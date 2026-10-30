import os
from contextlib import closing
import json
import sqlite3
import struct
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from adsbfi_proxy.config import Config, SLOT, iso, latest_slot, slot_path, timestamp
from adsbfi_proxy.schema import EVENTS_SCHEMA, AIRPORTS_SCHEMA, AIRCRAFT_SCHEMA, STATE_SCHEMA
from adsbfi_proxy.server import flights, flight_path
from adsbfi_proxy.worker import Worker, processed_max, retry_after, valid_file


LEGACY_EVENTS_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=OFF;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    event_id INTEGER PRIMARY KEY,
    icao_id INTEGER NOT NULL,
    takeoff_ts INTEGER NOT NULL,
    takeoff_airport_id INTEGER NOT NULL,
    landing_ts INTEGER NOT NULL,
    landing_airport_id INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_events_takeoff_time ON events(takeoff_ts,event_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_events_unique_pair
    ON events(icao_id, takeoff_ts, takeoff_airport_id, landing_ts, landing_airport_id);
CREATE INDEX IF NOT EXISTS idx_events_takeoff_airport_time
    ON events(takeoff_airport_id, takeoff_ts, icao_id, landing_airport_id, landing_ts);
CREATE INDEX IF NOT EXISTS idx_events_landing_airport_time
    ON events(landing_airport_id, landing_ts, icao_id, takeoff_airport_id, takeoff_ts);
CREATE INDEX IF NOT EXISTS idx_events_icao_takeoff
    ON events(icao_id, takeoff_ts);
"""


def heatmap(ts):
    # One time slice with a single high-altitude aircraft; cannot form a flight.
    ms=ts*1000
    return struct.pack('<4I',1,0,0,0)+struct.pack('<4I',0x0e7f7c9d,ms>>32,ms&0xffffffff,0)+struct.pack('<Iiii',0xa12345,40000000,-74000000,10000)


class ProxyTest(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory()
        self.root=Path(self.tmp.name)
        self.config=Config(self.root/'db', self.root/'cache', Path(os.environ.get('ADSBFI_TEST_BUILDER','rust/flight-builder/target/release/flight-builder')).resolve(), min_free_bytes=0)
        self.config.fallback_url=''
        self.config.db.mkdir();self.config.cache.mkdir()
        for name,schema in [('events',EVENTS_SCHEMA),('airports',AIRPORTS_SCHEMA),('aircraft',AIRCRAFT_SCHEMA),('build_state',STATE_SCHEMA)]:
            with closing(sqlite3.connect(self.config.db/f'{name}.sqlite')) as db, db:db.executescript(schema)
        with closing(sqlite3.connect(self.config.db/'airports.sqlite')) as db, db:
            db.execute("INSERT INTO airport_types VALUES(1,'large_airport')")
            db.executemany('INSERT INTO airports VALUES(?,?,?,?,?,?,?)',[(1,'KJFK','John F Kennedy',406413000,-737781000,1,13),(2,'KLAX','Los Angeles',339425000,-1184081000,1,125)])
        with closing(sqlite3.connect(self.config.db/'aircraft.sqlite')) as db, db:
            db.execute("INSERT INTO aircraft_types VALUES(1,'business_jet')")
            db.execute("INSERT INTO aircraft(icao_id,registration,n_number,aircraft_type_id,owner_name) VALUES(10560325,'N123AB','123AB',1,'Example Owner')")
        with closing(sqlite3.connect(self.config.db/'events.sqlite')) as db, db:
            db.executescript(LEGACY_EVENTS_SCHEMA)
            db.executemany('INSERT INTO events VALUES(?,?,?,?,?,?)',[(1,10560325,1700000000,1,1700003600,2),(2,10560325,1700000000,2,1700007200,1),(3,123,1700000500,1,1700007500,2)])

    def tearDown(self):self.tmp.cleanup()

    def test_search_filters_and_pagination(self):
        q={'start':'2023-11-14','end':'2023-11-16','airport':'KJFK','owner':'example','aircraft':'N123AB','limit':'1'}
        first=flights(self.config,q)
        self.assertEqual(first['flights'][0]['event_id'],2)
        second=flights(self.config,{**q,'cursor':first['next_cursor']})
        self.assertEqual(second['flights'][0]['event_id'],1)
        self.assertIsNone(second['next_cursor'])
        self.assertEqual(len(flights(self.config,{**q,'owner':"' OR 1=1--"})['flights']),0)
        self.assertEqual(len(flights(self.config,{**q,'owner':'','aircraft':'00007b'})['flights']),1)
        with self.assertRaises(ValueError):flights(self.config,{**q,'end':'2020-01-01'})

    def test_flight_path_metadata_and_unknown_id(self):
        data=flight_path(self.config,1)
        self.assertEqual(data['flight']['takeoff_airport'],'KJFK')
        self.assertAlmostEqual(data['flight']['takeoff_lat'],40.6413)
        self.assertEqual(data['coverage'],'unavailable')
        self.assertIsNone(flight_path(self.config,999))

    def test_slot_and_timezone(self):
        self.assertEqual(timestamp('2026-01-01T01:00:00+01:00'),timestamp('2026-01-01'))
        self.assertEqual(latest_slot(timestamp('2026-01-01T00:01:00Z')),timestamp('2025-12-31T23:00:00Z'))
        self.assertEqual(slot_path(self.config.cache,timestamp('2026-01-01T23:30:00Z')).name,'47.bin.ttf')
        self.assertEqual(retry_after('120',0),120)
        self.assertEqual(retry_after('Thu, 01 Jan 1970 00:02:00 GMT',0),120)

    def test_atomic_download_validation_and_retry(self):
        ts=timestamp('2026-01-01')
        payload=heatmap(ts)
        class Handler(BaseHTTPRequestHandler):
            mode=200
            def log_message(self,*args):pass
            def do_GET(self):
                self.send_response(self.mode)
                if self.mode==429:self.send_header('Retry-After','120')
                self.end_headers();self.wfile.write(payload if self.mode==200 else b'<html>Error</html>')
        server=ThreadingHTTPServer(('0.0.0.0',0),Handler)
        thread=threading.Thread(target=server.serve_forever);thread.start()
        try:
            self.config.base_url=f'http://127.0.0.1:{server.server_port}'
            worker=Worker(self.config)
            self.assertTrue(worker.fetch(ts));self.assertTrue(valid_file(slot_path(self.config.cache,ts)))
            self.assertFalse(list(self.config.cache.rglob('*.part')))
            Handler.mode=429;worker.next_request=0
            self.assertFalse(worker.fetch(ts+SLOT))
            self.assertGreater(worker.next_request-time.monotonic(),110)
            with worker.queue() as db:self.assertEqual(db.execute('SELECT COUNT(*) FROM missing').fetchone()[0],1)
        finally:server.shutdown();server.server_close();thread.join()

    def test_download_falls_back_to_secondary_source_after_403(self):
        ts=timestamp('2026-01-01')
        payload=heatmap(ts)
        class PrimaryHandler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                self.send_response(403)
                self.end_headers();self.wfile.write(b'<html>Error</html>')
        class FallbackHandler(BaseHTTPRequestHandler):
            def log_message(self,*args):pass
            def do_GET(self):
                self.send_response(200)
                self.end_headers();self.wfile.write(payload)
        primary=ThreadingHTTPServer(('0.0.0.0',0),PrimaryHandler)
        fallback=ThreadingHTTPServer(('0.0.0.0',0),FallbackHandler)
        thread=threading.Thread(target=primary.serve_forever);thread.start()
        thread2=threading.Thread(target=fallback.serve_forever);thread2.start()
        try:
            self.config.base_url=f'http://127.0.0.1:{primary.server_port}'
            self.config.fallback_url=f'http://127.0.0.1:{fallback.server_port}'
            worker=Worker(self.config)
            self.assertTrue(worker.fetch(ts))
            self.assertTrue(valid_file(slot_path(self.config.cache,ts)))
            self.assertEqual(worker.status.get('last_download_source'),'fallback')
            self.assertTrue(slot_path(self.config.cache,ts).relative_to(self.config.cache).as_posix().endswith('/heatmap/00.bin.ttf'))
        finally:
            primary.shutdown();fallback.shutdown();primary.server_close();fallback.server_close();thread.join();thread2.join()

    def test_archive_satisfies_downloads_builds_and_audits_without_copying(self):
        start=timestamp('2026-01-01');self.config.mode='mirror';self.config.mirror_start=start
        self.config.archive=self.root/'archive';self.config.base_url='http://127.0.0.1:9'
        for slot in [start,start+SLOT]:
            path=slot_path(self.config.archive,slot);path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(heatmap(slot))
        with patch('adsbfi_proxy.worker.latest_slot',return_value=start+SLOT):
            worker=Worker(self.config);self.assertTrue(worker.fetch(start));worker.audit()
        with worker.queue() as db:self.assertEqual(db.execute('SELECT COUNT(*) FROM missing').fetchone()[0],0)
        self.assertFalse(slot_path(self.config.cache,start).exists())
        self.assertEqual(self.config.locate(start),slot_path(self.config.archive,start))

    def test_audit_finds_old_holes_independent_of_processed_watermark(self):
        start=timestamp('2026-01-01');self.config.mode='mirror';self.config.mirror_start=start
        for slot in [start,start+2*SLOT]:
            path=slot_path(self.config.cache,slot);path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(heatmap(slot))
        with patch('adsbfi_proxy.worker.latest_slot',return_value=start+2*SLOT):
            worker=Worker(self.config);worker.audit()
        with worker.queue() as db:self.assertEqual(db.execute('SELECT slot FROM missing').fetchall(),[(start+SLOT,)])

    def test_builder_leaves_events_wal_index_for_readonly_consumers(self):
        ts=timestamp('2026-01-01');self.config.start=ts;self.config.mode='mirror'
        p=slot_path(self.config.cache,ts);p.parent.mkdir(parents=True);p.write_bytes(heatmap(ts))
        worker=Worker(self.config)
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts):worker.build_tick()
        try:
            for suffix in ['-wal','-shm']:self.assertTrue((self.config.db/f'events.sqlite{suffix}').exists())
        finally:worker.events_hold.close()

    def test_builder_resumes_and_keeps_summary_exact(self):
        ts=timestamp('2026-01-01');self.config.start=ts;self.config.mode='mirror'
        p=slot_path(self.config.cache,ts);p.parent.mkdir(parents=True);p.write_bytes(heatmap(ts))
        worker=Worker(self.config)
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts):worker.build_tick();worker.build_tick()
        self.assertEqual(processed_max(self.config),ts)
        with sqlite3.connect(self.config.db/'events.sqlite') as db:
            self.assertEqual(db.execute("SELECT value FROM meta WHERE key='endpoint_count'").fetchone()[0],'0')
        self.assertTrue(p.exists())
        # An invalid next file must neither advance checkpoint nor delete input.
        p=slot_path(self.config.cache,ts+SLOT);p.write_bytes(b'<html>upstream failure</html>')
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts+SLOT):worker.build_tick()
        self.assertEqual(processed_max(self.config),ts)

    def test_endpoints_and_interrupted_checkpoint_replay(self):
        ts=timestamp('2026-01-01');self.config.start=ts;self.config.mode='mirror'
        with sqlite3.connect(self.config.db/'build_state.sqlite') as db:
            db.execute('INSERT INTO processed_files VALUES(?,?,?)',('prior',ts-SLOT,iso(ts)))
        # A circuit returning to JFK: ground, climb, and ground observations.
        payload=struct.pack('<4I',3,0,0,0)+bytes(32)
        for offset,altitude in [(60,-123),(600,1200),(1500,-123)]:
            ms=(ts+offset)*1000
            payload+=struct.pack('<4I',0x0e7f7c9d,ms>>32,ms&0xffffffff,0)
            payload+=struct.pack('<Iiii',10560325,40641300,-73778100,altitude)
        p=slot_path(self.config.cache,ts);p.parent.mkdir(parents=True);p.write_bytes(payload)
        worker=Worker(self.config)
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts):worker.build_tick()
        with sqlite3.connect(self.config.db/'events.sqlite') as db:
            rows=db.execute('SELECT is_landing,ts,airport_id FROM endpoints ORDER BY ts').fetchall()
            self.assertEqual(rows,[(0,ts+60,1),(1,ts+1500,1)])
            self.assertEqual(dict(db.execute('SELECT key,value FROM meta'))['endpoint_count'],'2')
        # Simulate events committed but the track-state checkpoint was lost.
        with sqlite3.connect(self.config.db/'build_state.sqlite') as db:
            db.execute('DELETE FROM processed_files WHERE slot_ts=?',(ts,))
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts):worker.build_tick()
        with sqlite3.connect(self.config.db/'events.sqlite') as db:
            self.assertEqual(db.execute('SELECT COUNT(*) FROM endpoints').fetchone()[0],2)
            self.assertEqual(dict(db.execute('SELECT key,value FROM meta'))['endpoint_count'],'2')

    def test_live_cleanup_only_after_success(self):
        ts=timestamp('2026-01-01');self.config.start=ts;self.config.mode='live'
        p=slot_path(self.config.cache,ts);p.parent.mkdir(parents=True);p.write_bytes(heatmap(ts))
        worker=Worker(self.config)
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts):worker.build_tick()
        self.assertFalse(p.exists())
        p=slot_path(self.config.cache,ts+SLOT);p.write_bytes(heatmap(ts+SLOT))
        self.config.builder=Path('/bin/false')
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts+SLOT):
            with self.assertRaises(RuntimeError):worker.build_tick()
        self.assertTrue(p.exists())
        self.assertEqual(processed_max(self.config),ts)

    def test_missing_ingestion_slot_blocks_later_files(self):
        ts=timestamp('2026-01-01');self.config.start=ts
        p=slot_path(self.config.cache,ts+SLOT);p.parent.mkdir(parents=True);p.write_bytes(heatmap(ts+SLOT))
        worker=Worker(self.config)
        with patch('adsbfi_proxy.worker.latest_slot',return_value=ts+SLOT):worker.build_tick()
        self.assertIsNone(processed_max(self.config))

if __name__=='__main__':unittest.main()
