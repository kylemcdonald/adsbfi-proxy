"""Drift diagnostics; short-window real stock changes are not labeled errors."""
import argparse,json,sqlite3,datetime,collections,statistics
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('folders',nargs='+',type=Path);p.add_argument('--warmup-days',type=int,default=1);p.add_argument('--report',type=Path,default=Path('/media/kyle/External-1/cache/airport-activity/endpoint-v2/scores.json'));a=p.parse_args()
refs=sqlite3.connect('file:/media/kyle/External-1/cache/adsb-metadata-api/airports.sqlite?mode=ro',uri=True);codes={r[0]:r[1] for r in refs.execute('SELECT airport_id,code FROM airports')}
catalog=json.load(open('/media/kyle/External-1/cache/airport-activity/catalog.json'));top={r['id'] for r in catalog['airports']}
report=[]
for folder in a.folders:
 for window in sorted(folder.iterdir()):
  if not (window/'run.json').exists():continue
  db=sqlite3.connect((window/'events.sqlite').resolve().as_uri()+'?mode=ro',uri=True)
  first=db.execute("SELECT MIN(slot_ts),MAX(slot_ts) FROM processed_files") if False else None
  state=sqlite3.connect((window/'state.sqlite').resolve().as_uri()+'?mode=ro',uri=True);lo,hi=state.execute('SELECT MIN(slot_ts),MAX(slot_ts) FROM processed_files').fetchone();start=lo+a.warmup_days*86400;end=hi-1800 # one-hour lag exclusion; evaluate identical time bounds
  is_v2=db.execute("SELECT 1 FROM sqlite_master WHERE name='endpoints'").fetchone()
  if is_v2:
   rows=db.execute("SELECT airport_id,icao_id,ts,CASE kind WHEN 'landing' THEN 1 ELSE -1 END FROM endpoints WHERE ts>=? AND ts<?",(start,end))
  else:
   rows=db.execute('SELECT takeoff_airport_id,icao_id,takeoff_ts,-1 FROM events WHERE takeoff_ts>=? AND takeoff_ts<? UNION ALL SELECT landing_airport_id,icao_id,landing_ts,1 FROM events WHERE landing_ts>=? AND landing_ts<?',(start,end,start,end))
  net=collections.Counter();movements=collections.Counter();tails=collections.Counter();daily=collections.defaultdict(collections.Counter)
  for airport,icao,ts,delta in rows:
   if airport not in top:continue
   net[airport]+=delta;movements[airport]+=1;tails[(airport,icao)]+=delta;daily[airport][ts//86400]+=delta
  result=dict(variant=folder.name,window=window.name,days=(end-start)/86400,total_movements=sum(movements.values()),absolute_net=sum(abs(n) for n in net.values()),excess_tail_imbalance=sum(max(0,abs(n)-1) for n in tails.values()),airports=[])
  result['abs_net_per_1000']=1000*result['absolute_net']/max(1,result['total_movements']);result['excess_tail_per_1000']=1000*result['excess_tail_imbalance']/max(1,result['total_movements'])
  for aid in sorted(top):
   result['airports'].append(dict(code=codes.get(aid),net=net[aid],movements=movements[aid],net_per_day=net[aid]/result['days'],daily=[dict(day=k,net=v) for k,v in sorted(daily[aid].items())]))
  report.append(result);print(folder.name,window.name,'drift/1000',round(result['abs_net_per_1000'],2),'repeat/1000',round(result['excess_tail_per_1000'],2),'movements',result['total_movements'])
  print([(r['code'],round(r['net_per_day'],1)) for r in result['airports'] if r['code'] in ['KMIA','WIII','MMMX','RPLL','LTFM','KTEB']])
a.report.write_text(json.dumps(report,indent=2))
