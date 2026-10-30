"""Check the inspected World Cup day without detrending either ledger."""
import sqlite3,json,datetime
from pathlib import Path
root=Path('/media/kyle/External-1/cache/airport-activity/endpoint-v2')
refs=sqlite3.connect('file:/media/kyle/External-1/cache/adsb-metadata-api/airports.sqlite?mode=ro',uri=True);aid=refs.execute("SELECT airport_id FROM airports WHERE code='KTEB'").fetchone()[0]
start=int(datetime.datetime(2026,7,19,tzinfo=datetime.timezone.utc).timestamp());end=start+86400;target=start+21*3600+30*60;report={}
for name in ['baseline-long','reproducible-long']:
 db=sqlite3.connect((root/name/'holdout-event/events.sqlite').as_uri()+'?mode=ro',uri=True)
 if name=='baseline-long':rows=db.execute('SELECT takeoff_ts,-1 FROM events WHERE takeoff_airport_id=? AND takeoff_ts>=? AND takeoff_ts<? UNION ALL SELECT landing_ts,1 FROM events WHERE landing_airport_id=? AND landing_ts>=? AND landing_ts<? ORDER BY 1',(aid,start,end,aid,start,end)).fetchall()
 else:rows=db.execute("SELECT ts,CASE kind WHEN 'landing' THEN 1 ELSE -1 END FROM endpoints WHERE airport_id=? AND ts>=? AND ts<? ORDER BY ts",(aid,start,end)).fetchall()
 from collections import Counter
 changes=Counter()
 for ts,d in rows:changes[ts]+=d
 net=0;lo=0;hi=0;at_target=0;peak_ts=start
 for ts,d in sorted(changes.items()):
  net+=d;lo=min(lo,net)
  if net>hi:hi=net;peak_ts=ts
  if ts<=target:at_target=net
 report[name]=dict(movements=len(rows),day_net=net,minimum_from_midnight=lo,maximum_from_midnight=hi,intraday_range=hi-lo,peak_time=datetime.datetime.fromtimestamp(peak_ts,datetime.timezone.utc).isoformat(),net_at_2130=at_target)
print(json.dumps(report,indent=2));(root/'event-spike.json').write_text(json.dumps(report,indent=2))
