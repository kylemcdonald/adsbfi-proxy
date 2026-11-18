from __future__ import annotations

import argparse
import csv
import datetime as dt
import math
import re
import sqlite3
import urllib.request
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

from .paths import AIRPORTS_CSV, AIRPORTS_DB, RUNWAYS_CSV
from .schema import AIRPORTS_SCHEMA, connect, set_meta, vacuum


AIRPORTS_URL = "https://davidmegginson.github.io/ourairports-data/airports.csv"
RUNWAYS_URL = "https://davidmegginson.github.io/ourairports-data/runways.csv"
AIRPORTS_SOURCE_PAGE = "https://ourairports.com/data/"
CLOSED_TYPES = {"closed", "closed_airport"}


@dataclass(frozen=True)
class Airport:
    airport_id: int
    code: str
    name: str
    airport_type: str
    lat: float
    lon: float
    elevation_ft: float | None
    cos_lat_rad: float
    distance_km: float = 0.0


@dataclass(frozen=True)
class AirportEndpoint:
    airport_id: int
    airport_type: str
    elevation_ft: float | None
    distance_km: float


class AirportIndex:
    def __init__(self, airports: list[Airport], cell_degrees: float = 0.1) -> None:
        self.airports = airports
        self.cell_degrees = cell_degrees
        self.grid: dict[tuple[int, int], list[Airport]] = defaultdict(list)
        for airport in airports:
            self.grid[self.cell_for(airport.lat, airport.lon)].append(airport)

    @classmethod
    def from_db(cls, connection: sqlite3.Connection, exclude_types: set[str] | None = None) -> "AirportIndex":
        exclude_types = {value.lower() for value in (exclude_types or set())}
        rows = connection.execute(
            """
            SELECT a.airport_id, a.code, a.name, a.lat_e7, a.lon_e7, a.elevation_ft, t.label AS airport_type
            FROM airports a
            JOIN airport_types t ON t.airport_type_id = a.airport_type_id
            ORDER BY a.airport_id
            """
        ).fetchall()
        airports = [
            Airport(
                airport_id=int(row["airport_id"]),
                code=row["code"],
                name=row["name"],
                airport_type=row["airport_type"],
                lat=int(row["lat_e7"]) / 1e7,
                lon=int(row["lon_e7"]) / 1e7,
                elevation_ft=float(row["elevation_ft"]) if row["elevation_ft"] is not None else None,
                cos_lat_rad=math.cos(math.radians(int(row["lat_e7"]) / 1e7)),
            )
            for row in rows
            if row["airport_type"].lower() not in CLOSED_TYPES
            and row["airport_type"].lower() not in exclude_types
        ]
        return cls(airports)

    def cell_for(self, lat: float, lon: float) -> tuple[int, int]:
        return (math.floor(lat / self.cell_degrees), math.floor(lon / self.cell_degrees))

    def nearest(self, lat: float, lon: float, max_distance_km: float) -> AirportEndpoint | None:
        lat_radius = max_distance_km / 111.0
        phi1 = math.radians(lat)
        cos_phi1 = math.cos(phi1)
        cos_lat = max(0.15, abs(cos_phi1))
        lon_radius = max_distance_km / (111.0 * cos_lat)
        min_cell = self.cell_for(lat - lat_radius, lon - lon_radius)
        max_cell = self.cell_for(lat + lat_radius, lon + lon_radius)
        best: AirportEndpoint | None = None
        for lat_cell in range(min_cell[0], max_cell[0] + 1):
            for lon_cell in range(min_cell[1], max_cell[1] + 1):
                for airport in self.grid.get((lat_cell, lon_cell), ()):
                    distance = haversine_from_query_km(lat, lon, cos_phi1, airport)
                    if distance <= max_distance_km and (best is None or distance < best.distance_km):
                        best = AirportEndpoint(
                            airport_id=airport.airport_id,
                            airport_type=airport.airport_type,
                            elevation_ft=airport.elevation_ft,
                            distance_km=distance,
                        )
        return best


class NearestAirportCache:
    def __init__(self, index: AirportIndex, max_distance_km: float) -> None:
        self.index = index
        self.max_distance_km = max_distance_km
        self.cache: dict[tuple[int, int], AirportEndpoint | None] = {}

    def nearest(self, lat: float, lon: float) -> AirportEndpoint | None:
        key = (round(lat, 3), round(lon, 3))
        if key not in self.cache:
            self.cache[key] = self.index.nearest(lat, lon, self.max_distance_km)
        return self.cache[key]


def download_airports(destination: Path = AIRPORTS_CSV, refresh: bool = False, url: str = AIRPORTS_URL) -> Path:
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists() and not refresh:
        return destination

    request = urllib.request.Request(url, headers={"User-Agent": "adsbfi-proxy/0.1"})
    with urllib.request.urlopen(request, timeout=120) as response:
        destination.write_bytes(response.read())
    return destination


def parse_float(value: str | None) -> float | None:
    value = (value or "").strip()
    if not value:
        return None
    try:
        return float(value)
    except ValueError:
        return None


def parse_int(value: str | None) -> int | None:
    parsed = parse_float(value)
    return None if parsed is None else int(round(parsed))


# OurAirports gives fields without an official identifier a placeholder such as "US-0370".
PLACEHOLDER_IDENT = re.compile(r"[A-Z]{2}-[0-9]+")
AIRPORT_CODE = re.compile(r"[A-Z0-9]{3,4}")


def airport_codes(rows: list[dict]) -> list[str]:
    """Return each airport's code: its ident, or a real code instead of a placeholder ident.

    A placeholder takes the field's gps_code, else its local_code, when that code
    is not any other airport's ident and no other placeholder claims it too.
    """
    idents = [(row.get("ident") or "").strip() for row in rows]
    taken = set(idents)
    proposals: dict[int, str] = {}
    for index, (row, ident) in enumerate(zip(rows, idents)):
        if not PLACEHOLDER_IDENT.fullmatch(ident):
            continue
        for column in ("gps_code", "local_code"):
            candidate = (row.get(column) or "").strip().upper()
            if AIRPORT_CODE.fullmatch(candidate) and candidate not in taken:
                proposals[index] = candidate
                break
    claims: dict[str, int] = defaultdict(int)
    for candidate in proposals.values():
        claims[candidate] += 1
    return [
        proposals[index] if index in proposals and claims[proposals[index]] == 1 else ident
        for index, ident in enumerate(idents)
    ]


def read_runways(csv_path: Path, airport_ids: set[int]) -> list[tuple]:
    """Runway centrelines with both ends located, excluding closed and water runways."""
    rows = []
    with csv_path.open("r", encoding="utf-8-sig", newline="") as file_handle:
        for row in csv.DictReader(file_handle):
            airport_id = parse_int(row.get("airport_ref"))
            surface = (row.get("surface") or "").strip().upper()
            ends = [parse_float(row.get(f"{end}_{axis}_deg")) for end in ("le", "he") for axis in ("latitude", "longitude")]
            if (
                airport_id not in airport_ids
                or (row.get("closed") or "").strip() == "1"
                or "WATER" in surface
                or any(value is None for value in ends)
            ):
                continue
            rows.append((airport_id, *(int(round(value * 1e7)) for value in ends), surface or None))
    return rows


def import_airports(
    db_path: Path = AIRPORTS_DB,
    csv_path: Path = AIRPORTS_CSV,
    refresh: bool = False,
    runways_path: Path | None = None,
) -> dict:
    csv_path = download_airports(csv_path, refresh=refresh)
    runways_path = download_airports(runways_path or csv_path.with_name(RUNWAYS_CSV.name), refresh=refresh, url=RUNWAYS_URL)
    airport_rows = []
    type_labels: dict[str, int] = {}

    with csv_path.open("r", encoding="utf-8-sig", newline="") as file_handle:
        source_rows = list(csv.DictReader(file_handle))
    for row, code in zip(source_rows, airport_codes(source_rows)):
        airport_id = parse_int(row.get("id"))
        name = (row.get("name") or "").strip()
        airport_type = (row.get("type") or "").strip().lower()
        lat = parse_float(row.get("latitude_deg"))
        lon = parse_float(row.get("longitude_deg"))
        if airport_id is None or not code or not name or not airport_type or lat is None or lon is None:
            continue
        if airport_type not in type_labels:
            type_labels[airport_type] = len(type_labels) + 1
        airport_rows.append(
            (
                # OurAirports' own numeric id stays fixed when a field's code changes.
                airport_id,
                code,
                name,
                int(round(lat * 1e7)),
                int(round(lon * 1e7)),
                type_labels[airport_type],
                parse_int(row.get("elevation_ft")),
            )
        )

    rows_with_ids = sorted(airport_rows)
    runway_rows = read_runways(runways_path, {row[0] for row in rows_with_ids})
    type_rows = [(type_id, label) for label, type_id in sorted(type_labels.items(), key=lambda item: item[1])]

    connection = connect(db_path, AIRPORTS_SCHEMA)
    try:
        with connection:
            connection.execute("DELETE FROM runways")
            connection.execute("DELETE FROM airports")
            connection.execute("DELETE FROM airport_types")
            connection.executemany("INSERT INTO airport_types(airport_type_id, label) VALUES (?, ?)", type_rows)
            connection.executemany(
                """
                INSERT INTO airports(airport_id, code, name, lat_e7, lon_e7, airport_type_id, elevation_ft)
                VALUES (?, ?, ?, ?, ?, ?, ?)
                """,
                rows_with_ids,
            )
            connection.executemany(
                "INSERT INTO runways(airport_id, le_lat_e7, le_lon_e7, he_lat_e7, he_lon_e7, surface) VALUES (?, ?, ?, ?, ?, ?)",
                runway_rows,
            )
            set_meta(connection, "airports_source_url", AIRPORTS_URL)
            set_meta(connection, "runways_source_url", RUNWAYS_URL)
            set_meta(connection, "runways_count", str(len(runway_rows)))
            set_meta(connection, "airports_source_page", AIRPORTS_SOURCE_PAGE)
            set_meta(connection, "airports_csv_path", str(csv_path))
            set_meta(connection, "airports_imported_at", dt.datetime.now(dt.timezone.utc).isoformat())
            set_meta(connection, "airports_count", str(len(rows_with_ids)))
    finally:
        connection.close()
    vacuum(db_path)
    return {"airports": len(rows_with_ids), "runways": len(runway_rows), "airport_types": len(type_rows), "db_path": str(db_path)}


def haversine_km(lat1: float, lon1: float, lat2: float, lon2: float) -> float:
    radius_km = 6371.0088
    phi1 = math.radians(lat1)
    phi2 = math.radians(lat2)
    d_phi = math.radians(lat2 - lat1)
    d_lambda = math.radians(lon2 - lon1)
    a = math.sin(d_phi / 2) ** 2 + math.cos(phi1) * math.cos(phi2) * math.sin(d_lambda / 2) ** 2
    return 2 * radius_km * math.atan2(math.sqrt(a), math.sqrt(1 - a))


def haversine_from_query_km(lat: float, lon: float, cos_phi1: float, airport: Airport) -> float:
    radius_km = 6371.0088
    d_phi = math.radians(airport.lat - lat)
    d_lambda = math.radians(airport.lon - lon)
    a = math.sin(d_phi / 2) ** 2 + cos_phi1 * airport.cos_lat_rad * math.sin(d_lambda / 2) ** 2
    return 2 * radius_km * math.atan2(math.sqrt(a), math.sqrt(1 - a))


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--db", type=Path, default=AIRPORTS_DB)
    parser.add_argument("--csv", type=Path, default=AIRPORTS_CSV)
    parser.add_argument("--refresh", action="store_true")
    args = parser.parse_args(argv)
    print(import_airports(args.db, args.csv, args.refresh))


if __name__ == "__main__":
    main()
