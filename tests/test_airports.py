import csv
import sqlite3
import tempfile
import unittest
from pathlib import Path

from adsbfi_proxy.airports import airport_codes, import_airports

AIRPORT_FIELDS = ["id", "ident", "type", "name", "latitude_deg", "longitude_deg", "elevation_ft", "gps_code", "local_code"]
RUNWAY_FIELDS = ["airport_ref", "surface", "closed", "le_latitude_deg", "le_longitude_deg", "he_latitude_deg", "he_longitude_deg"]


def write_csv(path, fields, rows):
    with path.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.DictWriter(handle, fieldnames=fields)
        writer.writeheader()
        writer.writerows(rows)


class AirportImportTests(unittest.TestCase):
    def test_placeholder_idents_take_real_codes_when_unambiguous(self):
        # kylemcdonald/adsbfi-proxy#3
        rows = [
            {"ident": "KBOS", "gps_code": "KBOS"},
            {"ident": "US-0370", "gps_code": "MA87", "local_code": "MA87"},
            {"ident": "US-0568", "gps_code": "", "local_code": "MA17"},
            {"ident": "US-0001", "gps_code": "KBOS"},  # taken by another ident
            {"ident": "US-0002", "gps_code": "XX9"},
            {"ident": "US-0003", "gps_code": "XX9"},  # claimed twice
            {"ident": "US-0004", "gps_code": "AB-1"},  # not a code
        ]
        self.assertEqual(airport_codes(rows), ["KBOS", "MA87", "MA17", "US-0001", "US-0002", "US-0003", "US-0004"])

    def test_import_keeps_ourairports_ids_and_land_runways(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            write_csv(root / "airports.csv", AIRPORT_FIELDS, [
                {"id": "3422", "ident": "KBOS", "type": "large_airport", "name": "Logan", "latitude_deg": "42.36197", "longitude_deg": "-71.0079", "gps_code": "KBOS"},
                {"id": "321938", "ident": "US-0370", "type": "seaplane_base", "name": "Boston Harbor", "latitude_deg": "42.352502", "longitude_deg": "-71.025832", "gps_code": "MA87"},
            ])
            write_csv(root / "runways.csv", RUNWAY_FIELDS, [
                {"airport_ref": "3422", "surface": "ASP", "closed": "0", "le_latitude_deg": "42.3566", "le_longitude_deg": "-71.0233", "he_latitude_deg": "42.3486", "he_longitude_deg": "-71.0082"},
                {"airport_ref": "3422", "surface": "ASP", "closed": "1", "le_latitude_deg": "42.3", "le_longitude_deg": "-71.0", "he_latitude_deg": "42.31", "he_longitude_deg": "-71.01"},
                {"airport_ref": "321938", "surface": "WATER", "closed": "0", "le_latitude_deg": "42.35", "le_longitude_deg": "-71.02", "he_latitude_deg": "42.36", "he_longitude_deg": "-71.03"},
                {"airport_ref": "3422", "surface": "TURF", "closed": "0", "le_latitude_deg": "", "le_longitude_deg": "", "he_latitude_deg": "", "he_longitude_deg": ""},
            ])
            result = import_airports(root / "airports.sqlite", root / "airports.csv", runways_path=root / "runways.csv")
            with sqlite3.connect(root / "airports.sqlite") as db:
                airports = db.execute("SELECT airport_id, code FROM airports ORDER BY airport_id").fetchall()
                runways = db.execute("SELECT airport_id, le_lat_e7, he_lon_e7 FROM runways").fetchall()
        self.assertEqual(result["runways"], 1)
        self.assertEqual(airports, [(3422, "KBOS"), (321938, "MA87")])
        self.assertEqual(runways, [(3422, 423566000, -710082000)])


if __name__ == "__main__":
    unittest.main()
