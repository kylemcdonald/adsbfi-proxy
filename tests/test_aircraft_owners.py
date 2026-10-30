import gzip
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from adsbfi_proxy.aircraft import AircraftRecord, write_aircraft_db
from adsbfi_proxy.aircraft_owners import import_owners

OPENSKY = """'icao24','registration','owner','operator','operatorIcao','typecode'
'440a1b','OE-FHK','Globe Air Ag','','GAC','C510'
'4ca123','EI-ABC','Old Owner Ltd','','',''
'a70bff','N553QS','Freedom Soars Llc','','',''
"""


class AircraftOwnerTests(unittest.TestCase):
    def test_names_are_kept_only_for_the_current_registration(self):
        records = {
            0x440A1B: AircraftRecord(0x440A1B, registration='OE-FHK', icao_type='C510', short_type='L2J'),
            0x4CA123: AircraftRecord(0x4CA123, registration='EI-XYZ', icao_type='C510', short_type='L2J'),
            0xA70BFF: AircraftRecord(0xA70BFF, registration='N553QS', icao_type='C68A', short_type='L2J'),
        }
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            db = root / 'aircraft.sqlite'
            write_aircraft_db(db, records, {})
            adsbx = root / 'basic-ac-db.json.gz'
            with gzip.open(adsbx, 'wt') as handle:
                for row in [{'icao': 'A70BFF', 'reg': 'N553QS', 'ownop': 'AIVIA LLC, TANZBERGER ERIC DAVID'},
                            {'icao': '440A1B', 'reg': 'OE-FHK', 'ownop': 'CANCELLED/NOT ASSIGNED'}]:
                    handle.write(json.dumps(row) + '\n')
            opensky = root / 'opensky-aircraft-database-2025-08.csv'
            opensky.write_text(OPENSKY)
            details = import_owners(db, adsbx, opensky)
            # Rerunning replaces names rather than accumulating them.
            details = import_owners(db, adsbx, opensky)
            with sqlite3.connect(db) as connection:
                self.assertEqual(connection.execute('PRAGMA journal_mode').fetchone()[0], 'delete')
                rows = {row[0]: row[1:] for row in connection.execute(
                    'SELECT registration, adsbx_ownop, opensky_owner, opensky_operator, opensky_operator_icao'
                    ' FROM aircraft')}
        self.assertEqual(details['aircraft_with_names'], 2)
        self.assertEqual(rows['OE-FHK'], (None, 'Globe Air Ag', None, 'GAC'))
        self.assertEqual(rows['EI-XYZ'], (None, None, None, None))  # OpenSky lists an earlier registration
        self.assertEqual(rows['N553QS'], ('AIVIA LLC, TANZBERGER ERIC DAVID', 'Freedom Soars Llc', None, None))


if __name__ == '__main__':
    unittest.main()
