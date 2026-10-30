"""Bounded, resumable, offline shadow backfill. Never resets a database or downloads."""
import fcntl,hashlib,json,shutil,sqlite3,subprocess,time
from pathlib import Path
from contextlib import closing
from .config import SLOT,iso,timestamp
from .worker import valid_file

def backfill(config, output, start, end, batch_files=24, pause=.1, builder=None, commit_every=1):
    output=Path(output).expanduser().resolve()
    if output==config.db.resolve():raise ValueError('Shadow output must differ from the source/reference database directory')
    if not 1<=batch_files<=96 or pause<0:raise ValueError('batch_files must be 1–96; pause must be nonnegative')
    if not 1<=commit_every<=batch_files:raise ValueError('commit_every must be 1–batch_files')
    start,end=timestamp(start),timestamp(end)
    if start%SLOT or end%SLOT or start>=end:raise ValueError('Use increasing UTC half-hour boundaries')
    builder=Path(builder or config.builder).resolve()
    output.mkdir(parents=True,exist_ok=True)
    with (output/'proxy.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        digest=hashlib.sha256(builder.read_bytes()).hexdigest()
        manifest=output/'backfill.json'
        identity=dict(binary_sha256=digest,source_cache=str(config.cache.resolve()),references=str(config.db.resolve()),start=start,reference_fingerprints={name:dict(size=(config.db/name).stat().st_size,mtime_ns=(config.db/name).stat().st_mtime_ns) for name in ['airports.sqlite','aircraft.sqlite']})
        if manifest.exists():
            previous=json.loads(manifest.read_text())
            if any(previous.get(k)!=v for k,v in identity.items()):raise ValueError('Backfill provenance changed; use a new output directory')
        else:
            if any((output/name).exists() for name in ['events.sqlite','build_state.sqlite','airports.sqlite','aircraft.sqlite']):raise ValueError('Output has databases but no backfill manifest; use a fresh directory')
            temporary_manifest=output/'backfill.json.tmp'
            temporary_manifest.write_text(json.dumps(identity,indent=2));temporary_manifest.replace(manifest)
        for name in ['airports.sqlite','aircraft.sqlite']:
            target=config.db/name
            if not target.is_file():raise ValueError(f'Missing reference {target}')
            snapshot=output/name
            if not snapshot.exists():
                if shutil.disk_usage(output).free<config.min_free_bytes+target.stat().st_size:
                    raise RuntimeError('Insufficient space for reference snapshot')
                temporary=output/(name+'.tmp')
                with closing(sqlite3.connect(target.resolve().as_uri()+'?mode=ro',uri=True)) as source, closing(sqlite3.connect(temporary)) as dest:
                    source.backup(dest)
                    dest.execute('PRAGMA journal_mode=DELETE')
                temporary.replace(snapshot)
        cursor=start
        state=output/'build_state.sqlite'
        if state.exists():
            with sqlite3.connect(state.as_uri()+'?mode=ro',uri=True) as db:
                latest=db.execute('SELECT MAX(slot_ts) FROM processed_files').fetchone()[0]
                if latest is not None:cursor=max(cursor,latest+SLOT)
        while cursor<end:
            if shutil.disk_usage(output).free<config.min_free_bytes:raise RuntimeError('Shadow volume is below minimum free space')
            stop=min(end,cursor+batch_files*SLOT)
            for slot in range(cursor,stop,SLOT):
                file=config.locate(slot)
                if not file.exists() or not valid_file(file):raise RuntimeError(f'Missing/invalid input at {iso(slot)}; resume after it is available')
            command=[str(builder),'build-events','--cache-dir',str(config.cache),*(['--archive-dir',str(config.archive)] if config.archive else []),'--events-db',str(output/'events.sqlite'),'--state-db',str(state),'--airports-db',str(output/'airports.sqlite'),'--aircraft-db',str(output/'aircraft.sqlite'),'--start',iso(cursor),'--end',iso(stop),'--commit-every',str(commit_every),'--progress-every','0']
            with (output/'last-build.log').open('w+') as log:
                subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True)
                log.seek(0);details=json.loads(log.read())
            if details.get('parse_error_count'):raise RuntimeError('Builder reported parse errors; inspect last-build.log')
            with sqlite3.connect(state.as_uri()+'?mode=ro',uri=True) as db:
                latest=db.execute('SELECT MAX(slot_ts) FROM processed_files').fetchone()[0]
            if latest!=stop-SLOT:raise RuntimeError('Incomplete committed checkpoint')
            cursor=stop
            status=dict(**identity,requested_end=end,processed_through=cursor-SLOT,updated_at=time.time(),details=details)
            tmp=output/'backfill-status.tmp';tmp.write_text(json.dumps(status,indent=2));tmp.replace(output/'backfill-status.json')
            print(f'{iso(cursor)}: {details.get("inserted_endpoints",0)} endpoints',flush=True)
            if cursor<end and pause:time.sleep(pause)
        return dict(output=str(output),processed_through=iso(cursor-SLOT),binary_sha256=digest)
