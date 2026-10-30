import argparse,json,pathlib,subprocess,sqlite3
p=argparse.ArgumentParser();p.add_argument('--binary',type=pathlib.Path,required=True);p.add_argument('--output',type=pathlib.Path,required=True);a=p.parse_args()
source=pathlib.Path('/media/kyle/External-1/cache/airport-activity/investigation/replay')
for f in sorted(source.glob('*/result.json')):
 d=json.loads(f.read_text());out=a.output/f.parent.name;out.mkdir(parents=True,exist_ok=True);args=d['command'][:];args[0]=str(a.binary.resolve())
 for flag,name in [('--events-db','events.sqlite'),('--state-db','state.sqlite')]:args[args.index(flag)+1]=str(out/name)
 r=subprocess.run(args,capture_output=True,text=True,check=True);(out/'run.json').write_text(r.stdout)
 c=sqlite3.connect(out/'events.sqlite');print(f.parent.name,c.execute('SELECT kind,ts,airport_id,evidence FROM endpoints ORDER BY ts').fetchall(),flush=True)
