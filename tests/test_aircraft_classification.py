import unittest
import tempfile
import sqlite3
import struct
from pathlib import Path
import gzip
import json
from adsbfi_proxy.aircraft import classify_aircraft, AircraftRecord, write_aircraft_db, fill_airframes, designator_airframe, refresh_record_classification

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

    def test_reported_classification_errors(self):
        # IPS-Inequality/pjet#3: corporate airliner variants, manufacturer
        # names matching model patterns, and FAA engine codes overridden.
        for kwargs, expected in [
            (dict(manufacturer='YABORA INDUSTRIA AERONAUTICA S', model='ERJ 170-200 LR', icao_type='E75L'), 'commercial_jet'),
            (dict(manufacturer='EMBRAER-EMPRESA BRASILEIRA DE', model='ERJ 190-100 IGW', icao_type='E190'), 'commercial_jet'),
            (dict(manufacturer='AIRBUS', model='ACJ319 115X', icao_type='A319'), 'business_jet'),
            (dict(manufacturer='EMBRAER', model='Lineage 1000', icao_type='E190'), 'business_jet'),
            (dict(manufacturer='CESSNA', model='680A', icao_type='C68A'), 'business_jet'),
            (dict(manufacturer='HAWKER BEECHCRAFT CORP', model='G58'), 'piston'),
            (dict(manufacturer='RAYTHEON AIRCRAFT COMPANY', model='B300'), 'turboprop'),
        ]:
            with self.subTest(**kwargs):
                self.assertEqual(classify_aircraft(**kwargs), expected)

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

    def test_register_formatting_and_piston_descriptors(self):
        self.assertEqual(classify_aircraft(manufacturer='Cessna Aircraft Company', model='172M (Cessna)'), 'piston')
        self.assertEqual(classify_aircraft(icao_type='PA18', short_type='L1P', model='PA-18 "150"'), 'piston')
        self.assertEqual(classify_aircraft(icao_type='C337', short_type='L2P'), 'piston')
        self.assertEqual(classify_aircraft(icao_type='B350', short_type='L2T'), 'turboprop')
        self.assertEqual(classify_aircraft(manufacturer='PIPER', model='PA 46-350P', icao_type='P46T', short_type='L1T', type_engine_code='1'), 'turboprop')

    def test_corporate_suffixes_are_business_jets(self):
        # kylemcdonald/adsbfi-proxy#4
        for manufacturer, model, icao in [
            ('BOEING', '737-7BC(BBJ)', 'B737'),
            ('BOEING', '737-8ZE(BBJ2)', 'B738'),
            ('AIRBUS', 'A319-133ACJ', 'A319'),
            ('AIRBUS', 'A350-941ACJ', 'A359'),
        ]:
            with self.subTest(model=model):
                self.assertEqual(classify_aircraft(manufacturer=manufacturer, model=model, icao_type=icao), 'business_jet')
        self.assertEqual(classify_aircraft(manufacturer='BOEING', model='737-7H4', icao_type='B737'), 'commercial_jet')
        self.assertEqual(classify_aircraft(icao_type='B38M'), 'commercial_jet')
        self.assertNotEqual(classify_aircraft(model='B300'), 'commercial_jet')  # King Air model code, not a Boeing designator
        self.assertEqual(classify_aircraft(manufacturer='Cessna Aircraft Company', model='172M (Cessna)'), 'piston')

    def test_type_table_descriptor_replaces_stale_registry_descriptor(self):
        # kylemcdonald/adsbfi-proxy#2: reused registrations keep the old aircraft's descriptor.
        self.assertEqual(designator_airframe('L1P', 'L2T'), 'L2T')
        self.assertEqual(designator_airframe('H2T', 'L2T'), 'L2T')
        self.assertEqual(designator_airframe('A1T', 'L1T'), 'A1T')  # amphibious Caravan keeps its floats
        self.assertEqual(designator_airframe('L1P', 'L0-'), 'L1P')  # uninformative table entry
        self.assertEqual(designator_airframe(None, 'L2J'), 'L2J')
        self.assertEqual(designator_airframe(None, 'L0-'), 'L0-')
        table = {
            'B350': ['BEECHCRAFT Super King Air 350', 'L2T', 'M'],
            'BE20': ['BEECH 200 Super King Air', 'L2T', 'M'],
            'C700': ['CESSNA 700 Citation Longitude', 'L2J', 'M'],
            'H25B': ['HAWKER BEECHCRAFT Hawker 750/850', 'L2J', 'M'],
            'BE30': ['BEECH 300 Super King Air', 'L2T', 'L'],
        }
        records = {
            1: AircraftRecord(1, manufacturer='BEECHCRAFT', model='Super King Air 350', icao_type='B350', short_type='L1P'),
            2: AircraftRecord(2, manufacturer='BEECH', model='Super King Air 200', icao_type='BE20', short_type='H2T'),
            3: AircraftRecord(3, icao_type='C700'),
            4: AircraftRecord(4, icao_type='H25B', short_type='L2J'),
            5: AircraftRecord(5, model='B300', icao_type='BE30'),
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'types.js'
            path.write_bytes(gzip.compress(json.dumps(table).encode()))
            counts = fill_airframes(path, records)
        self.assertEqual(counts, {'airframes_from_type': 2, 'airframes_corrected_from_type': 2})
        self.assertEqual([records[i].short_type for i in range(1, 6)], ['L2T', 'L2T', 'L2J', 'L2J', 'L2T'])
        for record in records.values():
            refresh_record_classification(record)
        rotor = AircraftRecord(6, icao_type='H60', short_type='H2T', type_name='SIKORSKY UH-60 Black Hawk')
        refresh_record_classification(rotor)
        self.assertEqual(rotor.aircraft_type, 'helicopter')
        # kylemcdonald/adsbfi-proxy#5: no manufacturer, classified from the type name.
        self.assertEqual([records[i].aircraft_type for i in range(1, 6)],
                         ['turboprop', 'turboprop', 'business_jet', 'business_jet', 'turboprop'])
