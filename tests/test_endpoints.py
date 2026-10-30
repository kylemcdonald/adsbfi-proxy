import json,os,sqlite3,struct,subprocess,unittest
from pathlib import Path
from unittest.mock import patch
import test_proxy
heatmap=test_proxy.heatmap
from adsbfi_proxy.server import endpoints
from adsbfi_proxy.config import timestamp,SLOT,slot_path
from adsbfi_proxy.backfill import backfill

class EndpointTests(test_proxy.ProxyTest):
    # Reuse setup/cleanup, not inherited test methods.
    def add_endpoint(self,ts=1700000000,kind='landing',airport=1,icao=10560325,callsign=None):
        with sqlite3.connect(self.config.db/'events.sqlite') as db:
            callsign_id=None
            if callsign:
                db.execute('INSERT OR IGNORE INTO callsigns(callsign) VALUES(?)',(callsign,))
                callsign_id=db.execute('SELECT callsign_id FROM callsigns WHERE callsign=?',(callsign,)).fetchone()[0]
            db.execute('INSERT INTO endpoints(icao_id,ts,is_landing,airport_id,callsign_id) VALUES(?,?,?,?,?)',(icao,ts,int(kind=='landing'),airport,callsign_id))
    def test_feed_checkpoints_ids_not_event_times(self):
        self.add_endpoint();self.add_endpoint(ts=1690000000,kind='takeoff')
        first=endpoints(self.config,{'limit':'1','airport':'KJFK'});self.assertTrue(first['has_more']);self.assertEqual(first['next_after_id'],1)
        second=endpoints(self.config,{'after_id':'1'});self.assertEqual(second['endpoints'][0]['kind'],'takeoff');self.assertEqual(second['next_after_id'],2)
        self.assertEqual(second['endpoints'][0]['movement'],-1)
        self.assertEqual(endpoints(self.config,{'after_id':'2'})['endpoints'],[])
    def test_feed_filters_and_validation(self):
        self.add_endpoint();self.add_endpoint(ts=1700003600,kind='takeoff',airport=2)
        self.assertEqual(len(endpoints(self.config,{'airport':'KJFK','kind':'landing','aircraft':'N123AB'})['endpoints']),1)
        self.assertEqual(endpoints(self.config,{'airport':'none'})['endpoints'],[])
        for q in [{'limit':'0'},{'after_id':'-1'},{'kind':'flight'},{'start':'2026-01-02','end':'2026-01-01'}]:
            with self.assertRaises(ValueError):endpoints(self.config,q)
    def test_feed_rows_contain_only_ledger_columns(self):
        self.add_endpoint()
        row=endpoints(self.config,{})['endpoints'][0]
        self.assertEqual(row['kind'],'landing');self.assertEqual(row['movement'],1)
        self.assertNotIn('is_landing',row);self.assertNotIn('evidence',row)
        self.assertIsNone(row['callsign']);self.assertNotIn('callsign_id',row)
    def test_feed_reports_and_filters_callsigns(self):
        self.add_endpoint(callsign='UAL123');self.add_endpoint(ts=1700003600,kind='takeoff',callsign='TALON 99');self.add_endpoint(ts=1700007200)
        self.assertEqual([r['callsign'] for r in endpoints(self.config,{})['endpoints']],['UAL123','TALON 99',None])
        self.assertEqual([r['ts'] for r in endpoints(self.config,{'callsign':' talon  99 '})['endpoints']],[1700003600])
        self.assertEqual(endpoints(self.config,{'callsign':'NONE1'})['endpoints'],[])
    def test_feed_serves_ledgers_without_callsigns(self):
        with sqlite3.connect(self.config.db/'events.sqlite') as db:
            db.executescript('DROP TABLE endpoints; DROP TABLE callsigns; CREATE TABLE endpoints(endpoint_id INTEGER PRIMARY KEY,icao_id,ts,is_landing,airport_id); INSERT INTO endpoints VALUES(1,10560325,1700000000,1,1);')
        self.assertIsNone(endpoints(self.config,{})['endpoints'][0]['callsign'])
        self.assertEqual(endpoints(self.config,{'callsign':'UAL123'})['endpoints'],[])
    def test_offline_backfill_resume_and_no_source_writes(self):
        start=timestamp('2026-01-01');out=self.root/'shadow'
        for ts in [start,start+SLOT]:
            p=slot_path(self.config.cache,ts);p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(heatmap(ts))
        backfill(self.config,out,'2026-01-01','2026-01-01T01:00:00Z',2,0,commit_every=2)
        with sqlite3.connect(out/'build_state.sqlite') as db:self.assertEqual(db.execute('SELECT COUNT(*) FROM processed_files').fetchone()[0],2)
        backfill(self.config,out,'2026-01-01','2026-01-01T01:00:00Z',1,0)
        with sqlite3.connect(self.config.db/'events.sqlite') as db:self.assertEqual(db.execute('SELECT COUNT(*) FROM endpoints').fetchone()[0],0)
        with self.assertRaises(ValueError):backfill(self.config,self.config.db,'2026-01-01','2026-01-02')
    def test_missing_shadow_input_stops_before_checkpoint(self):
        out=self.root/'shadow'
        with self.assertRaises(RuntimeError):backfill(self.config,out,'2026-01-01','2026-01-01T00:30:00Z',1,0)
        self.assertFalse((out/'build_state.sqlite').exists())
    def test_legacy_store_rejected_without_schema_mutation(self):
        old=self.root/'legacy.sqlite'
        with sqlite3.connect(old) as db:db.executescript('CREATE TABLE events(event_id INTEGER PRIMARY KEY); INSERT INTO events VALUES(1);')
        start=timestamp('2026-01-01');p=slot_path(self.config.cache,start);p.parent.mkdir(parents=True);p.write_bytes(heatmap(start))
        run=subprocess.run([str(self.config.builder),'build-events','--cache-dir',str(self.config.cache),'--events-db',str(old),'--state-db',str(self.root/'unused-state.sqlite'),'--airports-db',str(self.config.db/'airports.sqlite'),'--aircraft-db',str(self.config.db/'aircraft.sqlite'),'--start','2026-01-01','--end','2026-01-01T00:30:00Z'],capture_output=True,text=True)
        self.assertNotEqual(run.returncode,0)
        with sqlite3.connect(old) as db:self.assertEqual(db.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall(),[('events',)])

    def test_incompatible_checkpoint_is_rejected(self):
        start=timestamp('2026-01-01');p=slot_path(self.config.cache,start);p.parent.mkdir(parents=True);p.write_bytes(heatmap(start))
        with sqlite3.connect(self.config.db/'build_state.sqlite') as db:
            db.execute("UPDATE meta SET value='legacy' WHERE key='inference_version'")
        command=[str(self.config.builder),'build-events','--cache-dir',str(self.config.cache),'--events-db',str(self.config.db/'events.sqlite'),'--state-db',str(self.config.db/'build_state.sqlite'),'--airports-db',str(self.config.db/'airports.sqlite'),'--aircraft-db',str(self.config.db/'aircraft.sqlite'),'--start','2026-01-01','--end','2026-01-01T00:30:00Z']
        run=subprocess.run(command,capture_output=True,text=True)
        self.assertNotEqual(run.returncode,0);self.assertIn('incompatible checkpoint',run.stderr)
        with sqlite3.connect(self.config.db/'build_state.sqlite') as db:self.assertEqual(db.execute('SELECT COUNT(*) FROM processed_files').fetchone()[0],0)

    def test_backfill_rejects_changed_references_on_resume(self):
        start=timestamp('2026-01-01');out=self.root/'shadow'
        p=slot_path(self.config.cache,start);p.parent.mkdir(parents=True);p.write_bytes(heatmap(start))
        backfill(self.config,out,'2026-01-01','2026-01-01T00:30:00Z',1,0)
        ref=self.config.db/'airports.sqlite';stat=ref.stat();os.utime(ref,ns=(stat.st_atime_ns,stat.st_mtime_ns+1000000000))
        with self.assertRaisesRegex(ValueError,'provenance changed'):
            backfill(self.config,out,'2026-01-01','2026-01-01T00:30:00Z',1,0)

# Inherited tests are exercised once by ProxyTest discovery.
for name in list(vars(test_proxy.ProxyTest)):
    if name.startswith('test_') and name not in vars(EndpointTests):setattr(EndpointTests,name,None)
