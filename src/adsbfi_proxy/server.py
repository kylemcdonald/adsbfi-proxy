"""Small read-only JSON API; SQLite remains the interface for local applications."""
import hmac
import json
import os
import re
import sqlite3
import socket
import time
from contextlib import closing
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

from .config import ROOT, iso, timestamp
from .flight_path import extract, PathBusy


def connect(config):
    db = sqlite3.connect(f'{config.db.joinpath("events.sqlite").as_uri()}?mode=ro', uri=True, timeout=5)
    db.row_factory = sqlite3.Row
    for alias, name in [('ac', 'aircraft'), ('ap', 'airports')]:
        db.execute(f'ATTACH DATABASE ? AS {alias}', (f'{config.db.joinpath(name + ".sqlite").as_uri()}?mode=ro',))
    db.execute('PRAGMA query_only=ON')
    deadline = time.monotonic() + 15
    db.set_progress_handler(lambda: time.monotonic() > deadline, 10000)
    return db


def endpoints(config, query):
    """Append-only endpoint feed; IDs are the checkpoint, not inferred event time."""
    limit = int(query.get('limit', '100'))
    after = int(query.get('after_id', '0'))
    if not 1 <= limit <= 5000 or after < 0:
        raise ValueError('limit must be 1–5000 and after_id must be nonnegative')
    where, values = ['e.endpoint_id > ?'], [after]
    kind = query.get('kind')
    if kind:
        if kind not in ('takeoff','landing'): raise ValueError('Invalid kind')
        where.append('e.is_landing=?'); values.append(int(kind == 'landing'))
    start = timestamp(query['start']) if query.get('start') else None
    end = timestamp(query['end']) if query.get('end') else None
    if start is not None and end is not None and start >= end:
        raise ValueError('start must be before end')
    for value, comparison in [(start,'>='),(end,'<')]:
        if value is not None: where.append(f'e.ts {comparison} ?'); values.append(value)
    with closing(connect(config)) as db:
        watermark = db.execute('SELECT COALESCE(MAX(endpoint_id),0) FROM endpoints').fetchone()[0]
        where.append('e.endpoint_id<=?'); values.append(watermark)
        airport = query.get('airport','').strip()
        if airport:
            row=db.execute('SELECT airport_id FROM ap.airports WHERE code=? COLLATE NOCASE OR CAST(airport_id AS TEXT)=?',(airport,airport)).fetchone()
            if not row:return {'endpoints':[], 'next_after_id':watermark, 'has_more':False, 'snapshot_id':watermark}
            where.append('e.airport_id=?');values.append(row[0])
        # Ledgers before compact-2 have no callsigns; keep serving them.
        has_callsigns=db.execute("SELECT 1 FROM sqlite_master WHERE name='callsigns'").fetchone() is not None
        callsign=' '.join(query.get('callsign','').upper().split())
        if callsign:
            row=db.execute('SELECT callsign_id FROM callsigns WHERE callsign=?',(callsign,)).fetchone() if has_callsigns else None
            if not row:return {'endpoints':[], 'next_after_id':watermark, 'has_more':False, 'snapshot_id':watermark}
            where.append('e.callsign_id=?');values.append(row[0])
        aircraft=query.get('aircraft','').strip().upper()
        if aircraft:
            numeric=int(aircraft,16) if re.fullmatch('[0-9A-F]{6}',aircraft) else -1
            ids=[r[0] for r in db.execute('SELECT icao_id FROM ac.aircraft WHERE registration=? COLLATE NOCASE OR icao_id=?',(aircraft,numeric))]
            if numeric>=0 and numeric not in ids:ids.append(numeric)
            if not ids:return {'endpoints':[], 'next_after_id':watermark, 'has_more':False, 'snapshot_id':watermark}
            where.append('e.icao_id IN ('+','.join('?' for _ in ids)+')');values.extend(ids)
        sign=('cs.callsign','LEFT JOIN callsigns cs ON cs.callsign_id=p.callsign_id') if has_callsigns else ('NULL','')
        sql='WITH page AS (SELECT e.* FROM endpoints e WHERE '+' AND '.join(where)+f' ORDER BY e.endpoint_id LIMIT ?) SELECT p.*,{sign[0]} AS callsign,a.code AS airport,a.name AS airport_name,c.registration,c.model FROM page p LEFT JOIN ap.airports a ON a.airport_id=p.airport_id LEFT JOIN ac.aircraft c ON c.icao_id=p.icao_id {sign[1]} ORDER BY p.endpoint_id'
        rows=[dict(r) for r in db.execute(sql,[*values,limit+1])]
    more=len(rows)>limit;rows=rows[:limit]
    for row in rows:
        landing=row.pop('is_landing');row.pop('callsign_id',None)
        row['kind']='landing' if landing else 'takeoff'
        row['icao']=f"{row['icao_id']:06x}";row['time']=iso(row['ts']);row['movement']=1 if landing else -1
    return {'endpoints':rows, 'next_after_id':rows[-1]['endpoint_id'] if more else watermark, 'has_more':more, 'snapshot_id':watermark,
            'timestamp_semantics':'supporting observation, not exact runway crossing; recovered arrival uses the earlier bound, recovered departure the later bound'}


def flights(config, query):
    limit = int(query.get('limit', '100'))
    if not 1 <= limit <= 500:
        raise ValueError('limit must be between 1 and 500')
    start = timestamp(query['start']) if query.get('start') else int(time.time()) - 30 * 86400
    end = timestamp(query['end']) if query.get('end') else int(time.time()) + 1
    if start >= end:
        raise ValueError('start must be before end (UTC, end exclusive)')
    conditions = ['e.takeoff_ts >= ?', 'e.takeoff_ts < ?']
    values = [start, end]
    with closing(connect(config)) as db:
        airport_id = None
        airport = query.get('airport', '').strip()
        if airport:
            rows = db.execute('SELECT airport_id FROM ap.airports WHERE code=? COLLATE NOCASE OR CAST(airport_id AS TEXT)=?',
                              (airport, airport)).fetchall()
            if not rows:
                return {'flights': [], 'next_cursor': None}
            airport_id = rows[0][0]
        aircraft = query.get('aircraft', '').strip().upper()
        owner = query.get('owner', '').strip()
        if aircraft or owner:
            match, args = [], []
            if aircraft:
                numeric = int(aircraft, 16) if len(aircraft) == 6 and all(c in '0123456789ABCDEF' for c in aircraft) else -1
                match.append('(icao_id=? OR registration=? COLLATE NOCASE OR n_number=? COLLATE NOCASE)')
                args.extend([numeric, aircraft, aircraft.removeprefix('N')])
            if owner:
                match.append("owner_name LIKE ? ESCAPE '\\'")
                args.append('%' + owner.replace('\\', '\\\\').replace('%', '\\%').replace('_', '\\_') + '%')
            ids = [r[0] for r in db.execute('SELECT icao_id FROM ac.aircraft WHERE ' + ' AND '.join(match) + ' LIMIT 5001', args)]
            if aircraft and not owner and numeric >= 0 and numeric not in ids:
                ids.append(numeric)  # Unknown metadata must not hide observed ICAO addresses.
            if len(ids) > 5000:
                raise ValueError('Owner matches over 5,000 aircraft; use a more specific name')
            if not ids:
                return {'flights': [], 'next_cursor': None}
            conditions.append('e.icao_id IN (' + ','.join('?' for _ in ids) + ')')
            values.extend(ids)
        cursor = query.get('cursor')
        if cursor:
            ts, event_id = map(int, cursor.split(':'))
            conditions.append('(e.takeoff_ts, e.event_id) < (?, ?)')
            values.extend([ts, event_id])
        where = ' AND '.join(conditions)
        order = ' ORDER BY e.takeoff_ts DESC,e.event_id DESC LIMIT ?'
        if airport_id is not None:
            # Each branch uses a covering endpoint index. Limit before metadata
            # joins, then merge the two top-K sets (deduplicating round trips).
            takeoffs = 'SELECT e.* FROM events e INDEXED BY idx_events_takeoff_airport_time WHERE ' + where + ' AND e.takeoff_airport_id=?' + order
            landings = 'SELECT e.* FROM events e INDEXED BY idx_events_landing_airport_time WHERE ' + where + ' AND e.landing_airport_id=? AND e.landing_ts>=? AND e.landing_ts<?' + order
            prefix = 'WITH candidates AS (SELECT * FROM (' + takeoffs + ') UNION SELECT * FROM (' + landings + ')), page AS (SELECT * FROM candidates ORDER BY takeoff_ts DESC,event_id DESC LIMIT ?) '
            args = [*values, airport_id, limit + 1, *values, airport_id, start, end + 30 * 3600, limit + 1, limit + 1]
        else:
            prefix = 'WITH page AS (SELECT e.* FROM events e WHERE ' + where + order + ') '
            args = [*values, limit + 1]
        sql = prefix + '''SELECT e.*, a.registration, a.n_number, a.owner_name, a.model,
                        t.code AS takeoff_airport, t.name AS takeoff_name,
                        l.code AS landing_airport, l.name AS landing_name
                 FROM page e
                 LEFT JOIN ac.aircraft a ON a.icao_id=e.icao_id
                 LEFT JOIN ap.airports t ON t.airport_id=e.takeoff_airport_id
                 LEFT JOIN ap.airports l ON l.airport_id=e.landing_airport_id
                 ORDER BY e.takeoff_ts DESC,e.event_id DESC'''
        rows = [dict(r) for r in db.execute(sql, args)]
    more = len(rows) > limit
    rows = rows[:limit]
    for row in rows:
        row['icao'] = f'{row["icao_id"]:06x}'
        row['takeoff'] = iso(row['takeoff_ts'])
        row['landing'] = iso(row['landing_ts'])
    return {'flights': rows, 'next_cursor': f'{rows[-1]["takeoff_ts"]}:{rows[-1]["event_id"]}' if more else None,
            'start': iso(start), 'end': iso(end)}


def flight_path(config, event_id):
    with closing(connect(config)) as db:
        row = db.execute('''SELECT e.*, a.registration, a.n_number, a.owner_name, a.model,
            t.code AS takeoff_airport, t.name AS takeoff_name,
            t.lat_e7 / 10000000.0 AS takeoff_lat, t.lon_e7 / 10000000.0 AS takeoff_lon,
            l.code AS landing_airport, l.name AS landing_name,
            l.lat_e7 / 10000000.0 AS landing_lat, l.lon_e7 / 10000000.0 AS landing_lon
            FROM events e LEFT JOIN ac.aircraft a ON a.icao_id=e.icao_id
            LEFT JOIN ap.airports t ON t.airport_id=e.takeoff_airport_id
            LEFT JOIN ap.airports l ON l.airport_id=e.landing_airport_id
            WHERE e.event_id=?''', (event_id,)).fetchone()
    if row is None:
        return None
    flight = dict(row)
    flight.update(icao=f'{flight["icao_id"]:06x}', takeoff=iso(flight['takeoff_ts']), landing=iso(flight['landing_ts']))
    return {'flight': flight, **extract(config, flight)}


def serve(config, host, port):
    password = os.getenv('ADSBFI_PASSWORD', os.getenv('AIRCRAFT_EMBEDDING_PASSWORD', ''))
    class Handler(BaseHTTPRequestHandler):
        def send(self, code, body, content_type='application/json'):
            if not isinstance(body, bytes):
                body = json.dumps(body).encode()
            self.send_response(code)
            self.send_header('Content-Type', content_type)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('X-Content-Type-Options', 'nosniff')
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = urlsplit(self.path)
            if password:
                import base64
                auth = self.headers.get('Authorization', '')
                try:
                    provided = base64.b64decode(auth.removeprefix('Basic ')).decode().split(':', 1)[1] if auth.startswith('Basic ') else ''
                except (ValueError, UnicodeError):
                    provided = ''
                if not hmac.compare_digest(provided.encode(), password.encode()):
                    self.send_response(401)
                    self.send_header('WWW-Authenticate', 'Basic realm="adsbfi-proxy", charset="UTF-8"')
                    self.send_header('Content-Length', '0')
                    self.end_headers()
                    return
            query = {k: v[-1] for k, v in parse_qs(path.query).items()}
            try:
                if path.path in ('/health', '/api/status'):
                    with closing(connect(config)) as db:
                        metadata = dict(db.execute('SELECT key,value FROM meta'))
                    try:
                        status = json.loads((config.db / 'proxy-status.json').read_text())
                    except FileNotFoundError:
                        status = {}
                    self.send(200, {'database': metadata, 'worker': status})
                elif path.path == '/api/endpoints':
                    self.send(200, endpoints(config, query))
                elif path.path == '/api/flights':
                    with closing(connect(config)) as db:
                        if not db.execute("SELECT 1 FROM sqlite_master WHERE name='events'").fetchone():
                            self.send(410, {'error':'Paired flights retired. Use /api/endpoints and checkpoint endpoint_id.'}); return
                    self.send(200, flights(config, query))
                elif match := re.fullmatch(r'/api/flights/([1-9][0-9]{0,17})/path', path.path):
                    data = flight_path(config, int(match[1]))
                    self.send(200, data) if data is not None else self.send(404, {'error': 'Flight not found'})
                elif path.path == '/api/airports':
                    q = query.get('q', '').strip()
                    with closing(connect(config)) as db:
                        rows = db.execute('SELECT airport_id,code,name FROM ap.airports WHERE code LIKE ? OR name LIKE ? ORDER BY code=? COLLATE NOCASE DESC, code LIMIT 30',
                                          (q + '%', '%' + q + '%', q))
                        self.send(200, {'airports': [dict(r) for r in rows]})
                elif path.path in ('/', '/app.js', '/style.css'):
                    name, kind = {'/': ('index.html', 'text/html; charset=utf-8'), '/app.js': ('app.js', 'text/javascript'), '/style.css': ('style.css', 'text/css')}[path.path]
                    if path.path == '/':
                        with closing(connect(config)) as db:
                            if db.execute("SELECT value FROM meta WHERE key='inference_version' AND value IN ('endpoints-v2','endpoints-v3','endpoints-v4','endpoints-v5')").fetchone(): name = 'endpoints.html'
                    self.send(200, (ROOT / 'web' / name).read_bytes(), kind)
                else:
                    self.send(404, {'error': 'Not found'})
            except (PathBusy, TimeoutError) as exc:
                self.send(503, {'error': str(exc)})
            except (ValueError, OverflowError) as exc:
                self.send(400, {'error': str(exc)})
            except sqlite3.OperationalError:
                self.send(503, {'error': 'Query unavailable or too broad; narrow the filters and try again'})

    class Server(ThreadingHTTPServer):
        daemon_threads = True
        address_family = socket.AF_INET6 if ':' in host else socket.AF_INET

        def server_bind(self):
            if self.address_family == socket.AF_INET6:
                self.socket.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
            super().server_bind()
    print(f'Listening on {host}:{port}', flush=True)
    Server((host, port), Handler).serve_forever()
