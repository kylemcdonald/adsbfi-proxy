"""Matched full-world windows, with one day of warmup and no production writes."""
import argparse,json,subprocess,sqlite3,time
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--long',action='store_true');a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True)
windows=[('development','2022-05-04','2022-05-08'),('holdout-winter','2025-01-14','2025-01-18'),('holdout-event','2026-07-18','2026-07-22')]
if a.long:windows=[('holdout-winter','2025-01-14','2025-02-14'),('holdout-event','2026-07-18','2026-08-18')]
for name,start,end in windows:
 out=a.output/name;out.mkdir(exist_ok=True)
 if (out/'run.json').exists():continue
 command=[str(a.binary.resolve()),'build-events','--cache-dir','/media/kyle/External-1/cache/adsbx','--events-db',str(out/'events.sqlite'),'--state-db',str(out/'state.sqlite'),'--airports-db','/media/kyle/External-1/cache/adsb-metadata-api/airports.sqlite','--aircraft-db','/media/kyle/External-1/cache/adsb-metadata-api/aircraft.sqlite','--start',start,'--end',end,'--commit-every','24','--progress-every','24']
 print(name,'started',flush=True)
 with (out/'stderr.log').open('w') as log:
  run=subprocess.run(command,stdout=subprocess.PIPE,stderr=log,text=True,check=True)
 (out/'run.json').write_text(run.stdout);print(name,json.loads(run.stdout)['elapsed_seconds'],flush=True)
