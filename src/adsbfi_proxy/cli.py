import argparse
import json
import logging
import signal

from .config import Config


def main():
    parser = argparse.ArgumentParser(prog='adsbfi-proxy')
    sub = parser.add_subparsers(dest='command', required=True)
    sub.add_parser('worker', help='Continuously download and build movement endpoints')
    serve = sub.add_parser('serve', help='Read-only search API and browser')
    serve.add_argument('--host', default='::')
    serve.add_argument('--port', type=int, default=8082)
    sub.add_parser('status')
    sub.add_parser('import-airports')
    sub.add_parser('import-aircraft')
    owners = sub.add_parser('import-aircraft-owners', help='Add ADS-B Exchange and OpenSky owner/operator names to aircraft.sqlite')
    owners.add_argument('--download', action='store_true', help='Download the current source databases first')
    back = sub.add_parser('backfill', help='Offline resumable build into a separate versioned database')
    back.add_argument('--output', required=True)
    back.add_argument('--start', required=True)
    back.add_argument('--end', required=True)
    back.add_argument('--batch-files', type=int, default=24)
    back.add_argument('--pause', type=float, default=.1)
    back.add_argument('--builder')
    back.add_argument('--commit-every', type=int, default=1)
    args = parser.parse_args()
    config = Config.env()
    if args.command == 'worker':
        from .worker import Worker
        config.validate()
        config.db.mkdir(parents=True, exist_ok=True)
        config.cache.mkdir(parents=True, exist_ok=True)
        logging.basicConfig(level=logging.INFO, format='%(asctime)s %(levelname)s %(message)s')
        worker = Worker(config)
        # Graceful stop waits for the current build/checkpoint to finish.
        for sig in (signal.SIGTERM, signal.SIGINT):
            signal.signal(sig, lambda *_: worker.stop.set())
        worker.run()
    elif args.command == 'backfill':
        from .backfill import backfill
        print(json.dumps(backfill(config,args.output,args.start,args.end,args.batch_files,args.pause,args.builder,args.commit_every),indent=2))
    elif args.command == 'serve':
        from .server import serve
        serve(config, args.host, args.port)
    elif args.command == 'status':
        status = config.db / 'proxy-status.json'
        print(status.read_text() if status.exists() else f'No worker status yet at {status}')
    elif args.command == 'import-airports':
        from .airports import import_airports
        print(json.dumps(import_airports(), indent=2))
    elif args.command == 'import-aircraft-owners':
        from .aircraft_owners import main as import_owners
        import_owners(['--download'] if args.download else [])
    else:
        from .aircraft import import_aircraft
        print(json.dumps(import_aircraft(), indent=2))


if __name__ == '__main__':
    main()
