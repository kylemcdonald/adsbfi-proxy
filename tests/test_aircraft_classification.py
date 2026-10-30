import unittest
import tempfile
import sqlite3
import struct
from pathlib import Path
from adsbfi_proxy.aircraft import classify_aircraft, AircraftRecord, write_aircraft_db

class AircraftClassificationTests(unittest.TestCase):
    def test_physical_airframe_survives_military_classification(self):
        records = {
            1: AircraftRecord(1, icao_type='AT45', short_type='L2T', military=True),
            2: AircraftRecord(2, icao_type='C172', short_type='L1P'),
            3: AircraftRecord(3, short_type='invalid'),
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'aircraft.sqlite'
            write_aircraft_db(path, records, {})
            with sqlite3.connect(path) as db:
                rows = db.execute('SELECT icao_id, airframe, label FROM aircraft JOIN aircraft_types USING(aircraft_type_id) ORDER BY icao_id').fetchall()
                payload = db.execute('SELECT payload FROM airframe_index WHERE version=1').fetchone()[0]
        self.assertEqual(rows[0], (1, 'L2T', 'military'))
        self.assertEqual(rows[1], (2, 'L1P', 'piston'))
        self.assertIsNone(rows[2][1])
        self.assertEqual(struct.unpack('<II', payload), (1 | (3 << 24), 2 | (1 << 24)))

    def test_airliner_numbers_are_not_piston_models(self):
        for manufacturer, model, icao, expected in [
            ('EMBRAER', 'ERJ 170-200 LR', 'E75L', 'commercial_jet'),
            ('AIRBUS', 'A-340-300', 'A343', 'commercial_jet'),
            ('SAAB', '340 B', 'SF34', 'commercial_turboprop'),
            ('Fairchild Dornier', 'Do.328-310', 'J328', 'commercial_jet'),
        ]:
            with self.subTest(model=model):
                self.assertEqual(classify_aircraft(manufacturer=manufacturer, model=model, icao_type=icao), expected)

    def test_piston_and_rotorcraft_remain_distinct(self):
        for manufacturer, model, expected in [
            ('CESSNA', '170', 'piston'), ('CESSNA', '172 SKYHAWK', 'piston'),
            (None, 'C172', 'piston'), (None, '172', 'piston'),
            ('PIPER', 'PA-28', 'piston'), ('CIRRUS', 'SR22', 'piston'),
            ('QUAD CITY', 'CHALLENGER II', 'piston'),
        ]:
            with self.subTest(model=model):
                self.assertEqual(classify_aircraft(manufacturer=manufacturer, model=model), expected)
        self.assertEqual(classify_aircraft(model='EC 130 B4', type_aircraft_code='6'), 'helicopter')
