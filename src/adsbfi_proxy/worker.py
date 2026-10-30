"""Independent download, archive audit, and sequential flight-building loops."""
import email.utils
import fcntl
import json
import logging
import os
import shutil
import sqlite3
import struct
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from contextlib import closing

from .config import SLOT, iso, latest_slot, slot_path, slot_ts

log = logging.getLogger(__name__)
MARKER = struct.pack('<I', 0x0E7F7C9D)


def valid_file(path):
    try:
        size = path.stat().st_size
        if size < 16 or size % 16:
            return False
        # Some archive volumes advertise MiB-sized blocks. A buffered 16-byte
        # header read then pulls in a full MiB per file unnecessarily.
        with path.open('rb', buffering=0) as f:
            os.posix_fadvise(f.fileno(), 0, 0, os.POSIX_FADV_RANDOM)
            header = f.read(16)
            slices = struct.unpack('<I', header[:4])[0]
            if not 1 <= slices <= 1800 or size < slices * 16 + 16:
                return False
            f.seek(slices * 16)
            return f.read(4) == MARKER
    except OSError:
        return False


def retry_after(value, now):
    try:
        return max(0, float(value))
    except (TypeError, ValueError):
        try:
            return max(0, email.utils.parsedate_to_datetime(value).timestamp() - now)
        except (TypeError, ValueError):
            return 60


def processed_max(config):
    path = config.db / 'build_state.sqlite'
    if not path.exists():
        return None
    with closing(sqlite3.connect(f'{path.as_uri()}?mode=ro', uri=True, timeout=30)) as db:
        return db.execute('SELECT MAX(slot_ts) FROM processed_files').fetchone()[0]


class Worker:
    def __init__(self, config):
        self.c = config
        self.stop = threading.Event()
        self.mutex = threading.Lock()
        self.status = {'mode': config.mode, 'started_at': iso(time.time())}
        self.next_request = 0
        self.process = None
        self.events_hold = None
        self.queue_path = config.db / 'mirror.sqlite'
        with closing(self.queue()) as db:
            db.executescript('''PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS missing (
                    slot INTEGER PRIMARY KEY, retry_at REAL NOT NULL DEFAULT 0,
                    attempts INTEGER NOT NULL DEFAULT 0, error TEXT);
                CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT);''')
        last = processed_max(config)
        self.first = config.start if config.start is not None else (last + SLOT if last is not None else latest_slot(time.time()))
        self.first = self.first // SLOT * SLOT
        with closing(self.queue()) as db, db:
            db.execute("INSERT OR IGNORE INTO settings VALUES('ingest_start',?)", (str(self.first),))
            self.first = int(db.execute("SELECT value FROM settings WHERE key='ingest_start'").fetchone()[0])

    def queue(self):
        return sqlite3.connect(self.queue_path, timeout=30)

    def hold_events(self):
        # An idle connection keeps events.sqlite-wal/-shm in place after the
        # builder exits, so read-only consumers running as other users (whose
        # sandbox cannot create the WAL index) can always open the ledger.
        path = self.c.db / 'events.sqlite'
        if self.events_hold is None and path.exists():
            self.events_hold = sqlite3.connect(path, timeout=30, check_same_thread=False)
            self.events_hold.execute('SELECT 1 FROM sqlite_master LIMIT 1').fetchall()

    def report(self, **values):
        with self.mutex:
            self.status.update(values)
            self.status['checked_at'] = iso(time.time())
            path = self.c.db / 'proxy-status.json'
            tmp = path.with_suffix('.tmp')
            tmp.write_text(json.dumps(self.status, indent=2) + '\n')
            tmp.replace(path)

    def next_slot(self):
        last = processed_max(self.c)
        return self.first if last is None else last + SLOT

    def queue_missing(self, slot):
        with closing(self.queue()) as db, db:
            db.execute('INSERT OR IGNORE INTO missing(slot) VALUES(?)', (slot,))

    def fetch(self, slot):
        destination = slot_path(self.c.cache, slot)
        if valid_file(self.c.locate(slot)):
            with closing(self.queue()) as db, db:
                db.execute('DELETE FROM missing WHERE slot=?', (slot,))
            return True
        self.queue_missing(slot)
        with closing(self.queue()) as db:
            retry, attempts = db.execute('SELECT retry_at, attempts FROM missing WHERE slot=?', (slot,)).fetchone()
        if retry > time.time():
            return False
        if self.stop.wait(max(0, self.next_request - time.monotonic())):
            return False
        destination.parent.mkdir(parents=True, exist_ok=True)
        if shutil.disk_usage(self.c.cache).free < self.c.min_free_bytes:
            raise RuntimeError('Cache volume is below ADSBFI_MIN_FREE_GB; downloads paused')
        relative = destination.relative_to(self.c.cache).as_posix()
        part = destination.with_suffix(destination.suffix + '.part')
        delay = min(3600, 60 * 2**min(attempts, 6))
        bases = [(self.c.base_url, 'primary')]
        if self.c.fallback_url and self.c.fallback_url != self.c.base_url:
            bases.append((self.c.fallback_url, 'fallback'))
        try:
            self.report(downloading_slot=iso(slot))
            for base, source in bases:
                url = f'{base}/{relative}'
                origin = urllib.parse.urlsplit(base)
                req = urllib.request.Request(url, headers={
                    'User-Agent': 'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/126.0 Safari/537.36',
                    'Referer': f'{origin.scheme}://{origin.netloc}/',
                })
                try:
                    expected = None
                    with urllib.request.urlopen(req, timeout=45) as response, part.open('wb') as output:
                        if response.status != 200:
                            raise ValueError(f'Unexpected HTTP status {response.status}')
                        expected = response.headers.get('Content-Length')
                        while chunk := response.read(1024 * 1024):
                            if self.stop.is_set():
                                raise InterruptedError('Stopping download')
                            output.write(chunk)
                        output.flush()
                        os.fsync(output.fileno())
                    if expected and part.stat().st_size != int(expected):
                        raise ValueError('Truncated heatmap download')
                    if not valid_file(part):
                        raise ValueError('Invalid heatmap header or record alignment')
                    part.replace(destination)
                    with closing(self.queue()) as db, db:
                        db.execute('DELETE FROM missing WHERE slot=?', (slot,))
                    self.report(last_download=iso(slot), last_download_at=iso(time.time()),
                                last_download_source=source, download_error=None)
                    log.info('Downloaded %s from %s (%s bytes)', iso(slot), source, destination.stat().st_size)
                    return True
                except urllib.error.HTTPError as exc:
                    part.unlink(missing_ok=True)
                    if exc.code == 429:
                        # Honor the upstream rate limit; do not immediately hit
                        # the mirror with the same request.
                        delay = max(delay, retry_after(exc.headers.get('Retry-After'), time.time()))
                        self.next_request = time.monotonic() + delay
                        raise
                    if source != bases[-1][1]:
                        log.info('Primary heatmap source failed for %s (%s); trying fallback', iso(slot), exc)
                        continue
                    raise
                except Exception as exc:
                    part.unlink(missing_ok=True)
                    if source != bases[-1][1]:
                        log.info('Primary heatmap source failed for %s (%s); trying fallback', iso(slot), exc)
                        continue
                    raise
            raise RuntimeError('No heatmap source was configured')
        except Exception as exc:
            if isinstance(exc, urllib.error.HTTPError):
                if exc.code == 429:
                    delay = max(delay, retry_after(exc.headers.get('Retry-After'), time.time()))
                    self.next_request = time.monotonic() + delay
                elif exc.code == 404:
                    # Publication lag is retried each minute; old holes stay visible
                    # and are retried daily, without starving other archive gaps.
                    delay = 60 if slot >= latest_slot(time.time()) - 2 * SLOT else 86400
            with closing(self.queue()) as db, db:
                db.execute('UPDATE missing SET attempts=attempts+1, retry_at=?, error=? WHERE slot=?',
                           (time.time() + delay, str(exc), slot))
            self.report(download_error=f'{iso(slot)}: {exc}')
            log.warning('Download %s: %s (retry in %.0fs)', iso(slot), exc, delay)
            return False
        finally:
            part.unlink(missing_ok=True)
            self.next_request = max(self.next_request, time.monotonic() + self.c.request_interval)

    def download_tick(self):
        target = latest_slot(time.time())
        next_slot = self.next_slot()
        self.report(target_slot=iso(target), next_build_slot=iso(next_slot))
        # Newest file gets first priority, regardless of historical backlogs.
        if self.c.mode == 'mirror' or target >= next_slot:
            self.fetch(target)
        for slot in range(next_slot, min(target + SLOT, next_slot + 4 * SLOT), SLOT):
            if self.stop.is_set():
                return
            self.fetch(slot)
        if self.c.mode == 'mirror':
            with closing(self.queue()) as db:
                pending = db.execute('SELECT slot FROM missing WHERE retry_at<=? AND slot<=? ORDER BY slot LIMIT 3',
                                     (time.time(), target)).fetchall()
            for (slot,) in pending:
                if self.stop.is_set():
                    return
                self.fetch(slot)
        with closing(self.queue()) as db:
            count, unavailable = db.execute('SELECT COUNT(*), SUM(error IS NOT NULL) FROM missing').fetchone()
        self.report(missing_files=count, unavailable_files=unavailable or 0,
                    last_poll_at=iso(time.time()))

    def audit(self):
        if self.c.mode != 'mirror':
            return
        start = self.c.mirror_start
        if start is None:
            # Discover the archive boundary once and persist it. A disappeared
            # oldest file must not move the mirror boundary forward on restart.
            with closing(self.queue()) as db:
                row = db.execute("SELECT value FROM settings WHERE key='mirror_start'").fetchone()
            if row:
                start = int(row[0])
            else:
                roots = [r for r in (self.c.cache, self.c.archive) if r is not None]
                found = [slot_ts(f.relative_to(r)) for r in roots
                         for f in [next(iter(sorted(r.glob('????/??/??/heatmap/[0-4][0-9].bin.ttf'))), None)] if f]
                if found:
                    start = min(found)
                else:
                    start = self.first
                with closing(self.queue()) as db, db:
                    db.execute("INSERT OR REPLACE INTO settings VALUES('mirror_start',?)", (str(start),))
        target = latest_slot(time.time())
        self.report(mirror_start=iso(start), audit_running=True)
        missing = 0
        for scanned, slot in enumerate(range(start // SLOT * SLOT, target + SLOT, SLOT), 1):
            if self.stop.is_set():
                return
            if not valid_file(self.c.locate(slot)):
                self.queue_missing(slot)
                missing += 1
            if scanned % 5000 == 0:
                self.report(audit_scanned_files=scanned, audit_through=iso(slot))
        self.report(audit_running=False, audit_completed_at=iso(time.time()), audit_missing_files=missing,
                    audit_through=iso(target), audit_scanned_files=max(0, (target - start // SLOT * SLOT) // SLOT + 1))
        log.info('Mirror audit complete: %s missing files', missing)

    def build_tick(self):
        first = self.next_slot()
        target = latest_slot(time.time())
        slots = []
        for slot in range(first, min(target + SLOT, first + 8 * SLOT), SLOT):
            if not valid_file(self.c.locate(slot)):
                break
            slots.append(slot)
        if not slots:
            self.report(build_running=False, processed_through=iso(first - SLOT),
                        lag_seconds=max(0, target - first + SLOT))
            return
        if shutil.disk_usage(self.c.db).free < self.c.min_free_bytes:
            raise RuntimeError('Database volume is below ADSBFI_MIN_FREE_GB; builder paused')
        command = [str(self.c.builder), 'build-events', '--cache-dir', str(self.c.cache),
                   *(['--archive-dir', str(self.c.archive)] if self.c.archive else []),
                   '--start', iso(first), '--end', iso(slots[-1] + SLOT), '--commit-every', '1', '--progress-every', '0']
        for flag, name in [('events', 'events'), ('airports', 'airports'), ('aircraft', 'aircraft'), ('state', 'build_state')]:
            command.extend([f'--{flag}-db', str(self.c.db / f'{name}.sqlite')])
        self.report(build_running=True, building_from=iso(first), building_through=iso(slots[-1]))
        # A file avoids pipe buffering/deadlocks and leaves diagnostics after failure.
        with (self.c.db / 'last-build.log').open('w+') as output:
            self.process = subprocess.Popen(command, stdout=output, stderr=subprocess.STDOUT)
            code = self.process.wait()
            self.process = None
            output.seek(0)
            result = output.read()
        if code:
            raise RuntimeError(f'Builder exited {code}: {result[-2000:]}')
        details = json.loads(result)
        if details.get('parse_error_count') or processed_max(self.c) != slots[-1]:
            raise RuntimeError('Builder did not checkpoint the complete batch')
        self.hold_events()
        self.report(build_running=False, build_error=None, last_build_at=iso(time.time()),
                    processed_through=iso(slots[-1]), lag_seconds=max(0, target - slots[-1]), build=details)
        log.info('Built through %s: %s new endpoints', iso(slots[-1]), details.get('inserted_endpoints', details.get('inserted_events', 0)))
        if self.c.mode == 'live':
            # Delete only files that are durably checkpointed, including remnants
            # left by an interrupted previous cleanup. Mirror mode never deletes.
            # The read-only archive is never touched.
            for path in self.c.cache.glob('????/??/??/heatmap/*.bin.ttf'):
                if slot_ts(path.relative_to(self.c.cache)) <= slots[-1]:
                    path.unlink(missing_ok=True)

    def loop(self, name, action, interval):
        while not self.stop.is_set():
            started = time.monotonic()
            try:
                action()
            except Exception:
                log.exception('%s failed', name)
                import traceback
                try:
                    self.report(**{f'{name}_error': traceback.format_exc()[-2000:]})
                except OSError:
                    log.exception('Cannot write status; retrying when the data volume is available')
            self.stop.wait(max(1, interval - (time.monotonic() - started)))

    def run(self):
        # flock releases automatically on crash; hold the FD for the process lifetime.
        with (self.c.db / 'proxy.lock').open('a') as lock:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise RuntimeError('An adsbfi-proxy worker already owns this database')
            self.hold_events()
            threads = [threading.Thread(target=self.loop, args=args) for args in (
                ('download', self.download_tick, self.c.interval),
                ('build', self.build_tick, 5),
                ('audit', self.audit, 6 * 3600),
            )]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
            if self.events_hold is not None:
                self.events_hold.close()
            self.report(stopped_at=iso(time.time()))
