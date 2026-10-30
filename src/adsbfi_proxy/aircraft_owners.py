"""Owner and operator names from the ADS-B Exchange and OpenSky aircraft databases.

The FAA registry names US registrants only, and leaves fractional co-ownerships
blank. ADS-B Exchange's basic aircraft database carries an owner/operator
string (`ownop`: FAA registrants and co-owner lists for US aircraft, scattered
foreign owners), and OpenSky's crowd-sourced aircraft database carries owner,
operator and operator ICAO code fields worldwide. Both are stored verbatim in
their own columns of `aircraft`, never mixed into the FAA `owner_*` columns:

    adsbx_ownop            ADS-B Exchange ownop
    opensky_owner          OpenSky owner
    opensky_operator       OpenSky operator
    opensky_operator_icao  OpenSky operator ICAO code (callsign prefix)

A source's names are kept only when it lists the same registration for the
ICAO address as `aircraft.registration`, so names from an address's earlier
registration are dropped. The update is in place and touches only these
columns: classification, the airframe index and every other column are left
alone, so it is safe on a production database with read-only consumers open.
The database is left in rollback-journal mode, which read-only consumers can
open without creating any files.
"""
from __future__ import annotations

import argparse
import csv
import gzip
import json
import re
import shutil
import sqlite3
import urllib.request
from datetime import datetime, timezone
from pathlib import Path
from xml.etree import ElementTree

from .paths import ADSBX_BASIC_AIRCRAFT_DB, AIRCRAFT_CACHE_DIR, AIRCRAFT_DB, ensure_parent
from .schema import set_meta

ADSBX_BASIC_DB_URL = "https://downloads.adsbexchange.com/downloads/basic-ac-db.json.gz"
OPENSKY_BUCKET_URL = "https://s3.opensky-network.org/data-samples"
OPENSKY_SNAPSHOT = re.compile(r"^metadata/aircraft-database-complete-(\d{4}-\d{2})\.csv$")
OWNER_COLUMNS = ("adsbx_ownop", "opensky_owner", "opensky_operator", "opensky_operator_icao")
PLACEHOLDERS = re.compile(
    r"^(CANCELLED|NOT ASSIGNED|CERTIFICATE ISSUE PENDING|MODE S CODE RESERVED|REGISTRATION PENDING|"
    r"SALE REPORTED|N ?A|NONE|NOT AVAILABLE|UNKNOWN( OWNER)?|PRIVATE( OWNER)?)\b|^[^A-Z0-9]*$")
USER_AGENT = "adsbfi-proxy/0.1"


def hex_to_icao(value: str) -> int:
    """As heatmap.hex_to_icao, without its numpy import (production has no import extras)."""
    return int(value.strip().lower().removeprefix("~"), 16)


def clean_name(value: object) -> str | None:
    text = " ".join(str(value or "").split())
    if not text or PLACEHOLDERS.search(re.sub(r"[^A-Z0-9 ]+", " ", text.upper()).strip()):
        return None
    return text


def same_registration(a: str | None, b: str | None) -> bool:
    key = lambda value: re.sub(r"[^A-Z0-9]", "", (value or "").upper())
    return bool(key(a)) and key(a) == key(b)


def download(url: str, path: Path) -> Path:
    ensure_parent(path)
    part = path.with_name(path.name + ".part")
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=300) as response, part.open("wb") as output:
        shutil.copyfileobj(response, output)
    part.replace(path)
    return path


def latest_opensky_snapshot() -> tuple[str, str]:
    """(month, key) of the newest complete aircraft database in OpenSky's bucket."""
    request = urllib.request.Request(
        f"{OPENSKY_BUCKET_URL}?list-type=2&prefix=metadata/aircraft-database-complete-",
        headers={"User-Agent": USER_AGENT})
    with urllib.request.urlopen(request, timeout=60) as response:
        tree = ElementTree.parse(response)
    keys = [element.text or "" for element in tree.iter() if element.tag.endswith("}Key") or element.tag == "Key"]
    snapshots = sorted((match.group(1), key) for key in keys if (match := OPENSKY_SNAPSHOT.match(key)))
    if not snapshots:
        raise RuntimeError("No OpenSky aircraft database snapshot found")
    return snapshots[-1]


def fetch_sources(cache_dir: Path = AIRCRAFT_CACHE_DIR) -> tuple[Path, Path]:
    """Download today's ADS-B Exchange database and the newest OpenSky snapshot (once per month)."""
    adsbx = download(ADSBX_BASIC_DB_URL, cache_dir / ADSBX_BASIC_AIRCRAFT_DB.name)
    month, key = latest_opensky_snapshot()
    opensky = cache_dir / f"opensky-aircraft-database-{month}.csv"
    if not opensky.exists():
        download(f"{OPENSKY_BUCKET_URL}/{key}", opensky)
    return adsbx, opensky


def latest_opensky_file(cache_dir: Path = AIRCRAFT_CACHE_DIR) -> Path | None:
    files = sorted(cache_dir.glob("opensky-aircraft-database-*.csv"))
    return files[-1] if files else None


def read_adsbx(path: Path):
    """(icao_id, registration, ownop) from basic-ac-db.json.gz."""
    with gzip.open(path, "rt", encoding="utf-8") as handle:
        for line in handle:
            if not line.strip():
                continue
            row = json.loads(line)
            try:
                icao_id = hex_to_icao(row.get("icao") or "")
            except (TypeError, ValueError):
                continue
            ownop = clean_name(row.get("ownop"))
            if ownop:
                yield icao_id, row.get("reg"), ownop


def read_opensky(path: Path):
    """(icao_id, registration, owner, operator, operator ICAO) from an OpenSky CSV.

    The 2024+ "complete" snapshots quote with single quotes; older exports with
    double quotes."""
    with path.open(encoding="utf-8", newline="") as handle:
        quote = "'" if handle.readline().startswith("'") else '"'
        handle.seek(0)
        for row in csv.DictReader(handle, quotechar=quote):
            try:
                icao_id = hex_to_icao(row.get("icao24") or "")
            except (TypeError, ValueError):
                continue
            owner, operator = clean_name(row.get("owner")), clean_name(row.get("operator"))
            operator_icao = clean_name(row.get("operatorIcao") or row.get("operatoricao"))
            if owner or operator or operator_icao:
                yield icao_id, row.get("registration"), owner, operator, operator_icao


def ensure_columns(connection: sqlite3.Connection) -> None:
    existing = {row[1] for row in connection.execute("PRAGMA table_info(aircraft)")}
    for column in OWNER_COLUMNS:
        if column not in existing:
            connection.execute(f"ALTER TABLE aircraft ADD COLUMN {column} TEXT")


def import_owners(db_path: Path = AIRCRAFT_DB, adsbx_path: Path | None = ADSBX_BASIC_AIRCRAFT_DB,
                  opensky_path: Path | None = None) -> dict:
    connection = sqlite3.connect(db_path, timeout=60)
    try:
        registrations = dict(connection.execute("SELECT icao_id, registration FROM aircraft WHERE registration IS NOT NULL"))
        names: dict[int, list] = {}
        details: dict = {"updated_at": datetime.now(timezone.utc).isoformat(timespec="seconds")}
        if adsbx_path and adsbx_path.exists():
            rows = matched = 0
            for icao_id, registration, ownop in read_adsbx(adsbx_path):
                rows += 1
                if same_registration(registrations.get(icao_id), registration):
                    names.setdefault(icao_id, [None] * 4)[0] = ownop
                    matched += 1
            details["adsbx"] = {"path": str(adsbx_path), "rows_with_names": rows, "matched": matched}
        if opensky_path and opensky_path.exists():
            rows = matched = 0
            for icao_id, registration, owner, operator, operator_icao in read_opensky(opensky_path):
                rows += 1
                if same_registration(registrations.get(icao_id), registration):
                    names.setdefault(icao_id, [None] * 4)[1:] = [owner, operator, operator_icao]
                    matched += 1
            details["opensky"] = {"path": str(opensky_path), "rows_with_names": rows, "matched": matched}
        with connection:
            ensure_columns(connection)
            connection.execute(f"UPDATE aircraft SET {', '.join(f'{c} = NULL' for c in OWNER_COLUMNS)}"
                               f" WHERE {' OR '.join(f'{c} IS NOT NULL' for c in OWNER_COLUMNS)}")
            connection.executemany(
                f"UPDATE aircraft SET {', '.join(f'{c} = ?' for c in OWNER_COLUMNS)} WHERE icao_id = ?",
                [(*values, icao_id) for icao_id, values in names.items()])
            details["aircraft_with_names"] = len(names)
            set_meta(connection, "aircraft_owner_sources", json.dumps(details, sort_keys=True))
        # A rollback journal, not WAL: closing this last writer would delete the
        # -wal/-shm files, which mode=ro readers running as other users (in an
        # adsb-owned directory) cannot recreate, so they could not open it.
        connection.execute("PRAGMA journal_mode=DELETE")
        return details
    finally:
        connection.close()


def main(argv: list[str] | None = None) -> None:
    parser = argparse.ArgumentParser(description="Add ADS-B Exchange and OpenSky owner/operator names to aircraft.sqlite.")
    parser.add_argument("--db", type=Path, default=AIRCRAFT_DB)
    parser.add_argument("--cache-dir", type=Path, default=AIRCRAFT_CACHE_DIR)
    parser.add_argument("--download", action="store_true",
                        help="Download the current ADS-B Exchange database and the newest OpenSky snapshot first.")
    args = parser.parse_args(argv)
    if args.download:
        adsbx, opensky = fetch_sources(args.cache_dir)
    else:
        adsbx, opensky = args.cache_dir / ADSBX_BASIC_AIRCRAFT_DB.name, latest_opensky_file(args.cache_dir)
    print(json.dumps(import_owners(args.db, adsbx, opensky), indent=2))


if __name__ == "__main__":
    main()
