"""Compare one process with a restart after every file, on identical inputs."""
import argparse,json,sqlite3,subprocess
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);a=p.parse_args()
a.output.mkdir(parents=True,exist_ok=True)
refs=Path('/media/kyle/External-1/cache/adsb-metadata-api')
base=[str(a.binary.resolve()),'build-events','--cache-dir','/media/kyle/External-1/cache/adsbx','--airports-db',str(refs/'airports.sqlite'),'--aircraft-db',str(refs/'aircraft.sqlite'),'--progress-every','0']
from datetime import datetime,timezone
start=int(datetime(2026,9,18,12,tzinfo=timezone.utc).timestamp());end=start+6*3600
for variant in ['bulk','rolling']:
 out=a.output/variant;out.mkdir()
 bounds=[(start,end)] if variant=='bulk' else [(t,t+1800) for t in range(start,end,1800)]
 for lo,hi in bounds:
  subprocess.run(base+['--events-db',str(out/'events.sqlite'),'--state-db',str(out/'state.sqlite'),'--start',datetime.fromtimestamp(lo,timezone.utc).isoformat(),'--end',datetime.fromtimestamp(hi,timezone.utc).isoformat()],check=True,capture_output=True)
def rows(variant,db,query):
 with sqlite3.connect(a.output/variant/db) as c:return c.execute(query).fetchall()
checks={}
for name,db,query in [('endpoints','events.sqlite','SELECT icao_id,kind,ts,airport_id,episode_start_ts,lat,lon,altitude_ft,on_ground,confidence,source_slot,inference_version,evidence FROM endpoints ORDER BY icao_id,kind,episode_start_ts'),('tracks','state.sqlite','SELECT * FROM track_states ORDER BY icao_id'),('files','state.sqlite','SELECT path,slot_ts FROM processed_files ORDER BY path')]:
 b=rows('bulk',db,query);r=rows('rolling',db,query);checks[name]={'equal':b==r,'rows':len(b)}
print(json.dumps(checks,indent=2));(a.output/'results.json').write_text(json.dumps(checks,indent=2));assert all(c['equal'] for c in checks.values())
