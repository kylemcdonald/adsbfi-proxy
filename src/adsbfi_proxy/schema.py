from __future__ import annotations

import sqlite3
from pathlib import Path

from .paths import ensure_parent


EVENTS_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=OFF;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS callsigns (
    callsign_id INTEGER PRIMARY KEY,
    callsign TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS endpoints (
    endpoint_id INTEGER PRIMARY KEY,
    icao_id INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    is_landing INTEGER NOT NULL CHECK(is_landing IN (0,1)),
    airport_id INTEGER NOT NULL,
    callsign_id INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_endpoints_aircraft_time ON endpoints(icao_id,ts,is_landing);
CREATE INDEX IF NOT EXISTS idx_endpoints_airport_time ON endpoints(airport_id,ts,icao_id,is_landing);

INSERT OR IGNORE INTO meta(key,value) VALUES('inference_version','endpoints-v6');
INSERT OR IGNORE INTO meta(key,value) VALUES('ledger_schema','compact-2');
"""

AIRPORTS_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS airport_types (
    airport_type_id INTEGER PRIMARY KEY,
    label TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS airports (
    airport_id INTEGER PRIMARY KEY,
    code TEXT NOT NULL UNIQUE,
    name TEXT NOT NULL,
    lat_e7 INTEGER NOT NULL,
    lon_e7 INTEGER NOT NULL,
    airport_type_id INTEGER NOT NULL,
    elevation_ft INTEGER,
    FOREIGN KEY (airport_type_id) REFERENCES airport_types(airport_type_id)
);

CREATE INDEX IF NOT EXISTS idx_airports_lat_lon ON airports(lat_e7, lon_e7);

-- Runway centrelines, used to measure how close a report is to a land airport.
CREATE TABLE IF NOT EXISTS runways (
    airport_id INTEGER NOT NULL,
    le_lat_e7 INTEGER NOT NULL,
    le_lon_e7 INTEGER NOT NULL,
    he_lat_e7 INTEGER NOT NULL,
    he_lon_e7 INTEGER NOT NULL,
    surface TEXT,
    FOREIGN KEY (airport_id) REFERENCES airports(airport_id)
);
CREATE INDEX IF NOT EXISTS idx_runways_airport ON runways(airport_id);
CREATE INDEX IF NOT EXISTS idx_airports_type_lat_lon ON airports(airport_type_id, lat_e7, lon_e7);
"""

AIRCRAFT_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS aircraft_types (
    aircraft_type_id INTEGER PRIMARY KEY,
    label TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS aircraft (
    icao_id INTEGER PRIMARY KEY,
    registration TEXT,
    n_number TEXT,
    manufacturer TEXT,
    model TEXT,
    icao_type TEXT,
    airframe TEXT,
    aircraft_type_id INTEGER NOT NULL,
    source_mask INTEGER NOT NULL DEFAULT 0,
    owner_name TEXT,
    owner_city TEXT,
    owner_state TEXT,
    owner_country TEXT,
    owner_type TEXT,
    year_mfr INTEGER,
    FOREIGN KEY (aircraft_type_id) REFERENCES aircraft_types(aircraft_type_id)
);

CREATE INDEX IF NOT EXISTS idx_aircraft_type ON aircraft(aircraft_type_id, icao_id);
CREATE INDEX IF NOT EXISTS idx_aircraft_registration ON aircraft(registration);

CREATE TABLE IF NOT EXISTS airframe_index (
    version INTEGER PRIMARY KEY,
    payload BLOB NOT NULL
);
INSERT OR IGNORE INTO airframe_index VALUES(1, X'');
"""

STATE_SCHEMA = """
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS processed_files (
    path TEXT PRIMARY KEY,
    slot_ts INTEGER NOT NULL,
    processed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_processed_files_slot ON processed_files(slot_ts);

CREATE TABLE IF NOT EXISTS pending_landings (
    icao_id INTEGER PRIMARY KEY, episode INTEGER NOT NULL, airport_id INTEGER NOT NULL,
    elevation REAL, confidence REAL NOT NULL, point BLOB NOT NULL, callsign INTEGER,
    ts INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS recent_flights (
    icao_id INTEGER PRIMARY KEY, episode_start_ts INTEGER NOT NULL, end_ts INTEGER NOT NULL,
    max_altitude_ft REAL NOT NULL, points BLOB NOT NULL,
    airport_id INTEGER, elevation_ft REAL, confidence REAL, anchor_point BLOB NOT NULL,
    callsign INTEGER, anchor_ts INTEGER
);
CREATE INDEX IF NOT EXISTS idx_recent_flights_end ON recent_flights(end_ts);

CREATE TABLE IF NOT EXISTS track_states (
    icao_id INTEGER PRIMARY KEY,
    is_rotorcraft INTEGER NOT NULL,
    segment_start_ts INTEGER NOT NULL,
    segment_end_ts INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL,
    first_points BLOB NOT NULL,
    last_points BLOB NOT NULL,
    point_count INTEGER NOT NULL,
    max_altitude_ft REAL,
    max_altitude_ts INTEGER,
    source_first_file TEXT,
    source_last_file TEXT,
    open_start INTEGER NOT NULL,
    callsigns BLOB NOT NULL,
    liftoff BLOB,
    touchdown BLOB
);

INSERT OR IGNORE INTO meta(key,value) VALUES('inference_version','endpoints-v6');
INSERT OR IGNORE INTO meta(key,value) VALUES('state_layout','transitions-1');
"""


def connect(path: Path, schema: str | None = None) -> sqlite3.Connection:
    ensure_parent(path)
    connection = sqlite3.connect(path)
    connection.row_factory = sqlite3.Row
    if schema:
        connection.executescript(schema)
    return connection


def connect_readonly(path: Path) -> sqlite3.Connection:
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    connection.row_factory = sqlite3.Row
    return connection


def set_meta(connection: sqlite3.Connection, key: str, value: str) -> None:
    connection.execute(
        "INSERT INTO meta(key, value) VALUES(?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        (key, value),
    )


def get_meta(connection: sqlite3.Connection, key: str) -> str | None:
    row = connection.execute("SELECT value FROM meta WHERE key = ?", (key,)).fetchone()
    return row["value"] if row else None


def vacuum(path: Path) -> None:
    connection = sqlite3.connect(path)
    try:
        connection.execute("PRAGMA optimize")
        connection.execute("VACUUM")
    finally:
        connection.close()  # `with` would only commit, leaving it open until garbage collection
