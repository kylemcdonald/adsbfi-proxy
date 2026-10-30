"""Controlled warm-state single-file benchmark; writes only to --output."""
import argparse,json,sqlite3,subprocess,time,statistics
from pathlib import Path
p=argparse.ArgumentParser();p.add_argument('--binary',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--repeats',type=int,default=5);p.add_argument('--references',type=Path,default=Path('/media/kyle/External-1/cache/adsb-metadata-api'));a=p.parse_args();a.output.mkdir(parents=True,exist_ok=True)
source=a.references;cache='/media/kyle/External-1/cache/adsbx'
base=[str(a.binary.resolve()),'build-events','--cache-dir',cache,'--airports-db',str(source/'airports.sqlite'),'--aircraft-db',str(source/'aircraft.sqlite'),'--progress-every','0']
def run(folder,start,end):
 folder.mkdir(exist_ok=True)
 cmd=base+['--events-db',str(folder/'events.sqlite'),'--state-db',str(folder/'state.sqlite'),'--start',start,'--end',end]
 t=time.perf_counter();out=subprocess.run(['/usr/bin/time','-f','%U %S %M',*cmd],capture_output=True,text=True,check=True);wall=time.perf_counter()-t
 user,system,rss=map(float,out.stderr.strip().splitlines()[-1].split());details=json.loads(out.stdout)
 return dict(wall=wall,cpu=user+system,max_rss_kib=rss,details=details)
seed=a.output/'seed'
if not (seed/'state.sqlite').exists():run(seed,'2026-09-18T12:00:00Z','2026-09-18T15:00:00Z')
results=[]
for i in range(a.repeats):
 d=a.output/f'run-{i}';d.mkdir(exist_ok=True)
 for name in ['events.sqlite','state.sqlite']:
  if (d/name).exists():raise SystemExit(f'Use fresh output directory: {d/name}')
  with sqlite3.connect((seed/name).resolve().as_uri()+'?mode=ro',uri=True) as src,sqlite3.connect(d/name) as dst:src.backup(dst)
 r=run(d,'2026-09-18T15:00:00Z','2026-09-18T15:30:00Z');assert r['details']['files_processed']==1;results.append(r);print(i,r['wall'],r['cpu'],r['max_rss_kib'],flush=True)
summary=dict(binary=str(a.binary),results=results,median_wall=statistics.median(r['wall'] for r in results),median_cpu=statistics.median(r['cpu'] for r in results),peak_rss_kib=max(r['max_rss_kib'] for r in results))
(a.output/'results.json').write_text(json.dumps(summary,indent=2));print(json.dumps(summary,indent=2))
