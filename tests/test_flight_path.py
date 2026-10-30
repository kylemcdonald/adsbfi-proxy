import struct
import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from adsbfi_proxy.config import Config, SLOT, slot_path
from adsbfi_proxy.flight_path import extract, geometry, read_positions


def payload(observations):
    result = struct.pack('<I', len(observations)) + bytes(len(observations) * 16 - 4)
    for ts, records in observations:
        ms = ts * 1000
        result += struct.pack('<4I', 0x0e7f7c9d, ms >> 32, ms & 0xffffffff, 0)
        for record in records:
            result += struct.pack('<Iiii', *record)
    return result


class FlightPathTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.config = Config(self.root, self.root, self.root)
        self.slot = 1699920000  # Midnight UTC.
        self.icao = 0xa12345

    def tearDown(self):
        self.temp.cleanup()

    def write(self, slot, data):
        path = slot_path(self.root, slot)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return path

    def record(self, aircraft=None, altitude=400):
        return (self.icao if aircraft is None else aircraft, 40000000, -74000000, altitude)

    def test_filters_time_aircraft_flags_and_altitude(self):
        ts = self.slot
        path = self.write(ts, payload([
            (ts, [self.record()]),
            (ts+60, [self.record(altitude=-123), self.record(0x1a12345), self.record(0x123456)]),
            (ts+120, [self.record(0x80a12345, 400), (self.icao, 1100000000, 0, 0)]),
            (ts+180, [self.record()]),
        ]))
        points = read_positions(path, ts, self.icao, ts+60, ts+120, time.monotonic()+5)
        self.assertEqual([p['timestamp'] for p in points], [ts+60, ts+120])
        self.assertTrue(points[0]['on_ground'])
        self.assertIsNone(points[0]['altitude_ft'])
        self.assertEqual(points[1]['altitude_ft'], 10000)
        self.assertEqual(points[1]['lon'], -74)

    def test_missing_file_then_repair_invalidates_cache_across_midnight(self):
        ts = self.slot + 47*SLOT
        self.write(ts, payload([(ts+1740, [self.record()])]))
        flight = dict(icao_id=self.icao, takeoff_ts=ts+1740, landing_ts=ts+1860)
        before = extract(self.config, flight)
        self.assertEqual(before['coverage'], 'partial')
        self.assertEqual(len(before['missing_slots']), 1)
        self.write(ts+SLOT, payload([(ts+1800, [self.record()]), (ts+1860, [self.record()])]))
        after = extract(self.config, flight)
        self.assertEqual(after['coverage'], 'available')
        self.assertEqual(len(after['points']), 3)
        self.assertEqual(len(after['geojson']['features']), 1)

    def test_unknown_altitude_is_not_a_negative_altitude_or_ground(self):
        from adsbfi_proxy.heatmap import summarize_heatmap_file
        ts=self.slot
        path=self.write(ts,payload([(ts,[self.record(altitude=-124)]),
                                  (ts+60,[self.record(altitude=-120)])]))
        points=read_positions(path,ts,self.icao,ts,ts+60,time.monotonic()+5)
        self.assertIsNone(points[0]['altitude_ft'])
        self.assertFalse(points[0]['on_ground'])
        self.assertEqual(points[1]['altitude_ft'],-3000)
        summary=summarize_heatmap_file(path)[0]
        self.assertIsNone(summary.first_alt_ft)
        self.assertFalse(summary.first_on_ground)
        self.assertEqual(summary.max_altitude_ft,-3000)

    def test_corrupt_file_and_missing_observations(self):
        ts = self.slot
        self.write(ts, b'bad')
        flight = dict(icao_id=self.icao, takeoff_ts=ts, landing_ts=ts+60)
        result = extract(self.config, flight)
        self.assertEqual(result['coverage'], 'unavailable')
        self.assertEqual(len(result['unreadable_slots']), 1)
        self.assertIsNone(result['bounds'])
        self.write(ts, payload([(ts, [self.record(123)])]))
        self.assertEqual(extract(self.config, flight)['points'], [])

    def test_gaps_and_dateline(self):
        points = [dict(timestamp=0, lon=179, lat=40), dict(timestamp=60, lon=-179, lat=41),
                  dict(timestamp=600, lon=-178, lat=42)]
        geojson, bounds, gaps = geometry(points)
        self.assertEqual(gaps, 1)
        self.assertEqual(bounds, [[179, 40], [182, 42]])
        self.assertEqual(geojson['features'][0]['geometry']['coordinates'], [[179, 40], [181, 41]])
        self.assertEqual(geojson['features'][1]['geometry']['type'], 'Point')

    def test_timeout_is_not_reported_as_corruption(self):
        ts = self.slot
        self.write(ts, payload([(ts, [self.record()])]))
        with patch('adsbfi_proxy.flight_path.read_positions', side_effect=TimeoutError('timeout')):
            with self.assertRaises(TimeoutError):
                extract(self.config, dict(icao_id=self.icao, takeoff_ts=ts, landing_ts=ts+60))
