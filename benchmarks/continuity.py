"""Measure airport continuity of an endpoint ledger without external truth.

An aircraft takes off from the airport where it last landed. A different
airport means the aircraft was moved by road or ship, a whole flight went
untracked, or one of the two endpoints was assigned to the wrong airport.
Every landing followed by a takeoff of the same aircraft within --hours is a
labelled pair, which makes the check available for every aircraft class and
region, unlike the BTS evaluation.

Mismatches are binned by the distance between the two airports:

* adjacent (< 5 km): neighbouring references such as a seaplane base or
  heliport beside a runway; nearly always an airport-assignment error.
* nearby (5-30 km): a wrong choice among a cluster of fields, or a short
  unobserved hop.
* remote (>= 30 km): usually an untracked flight.

Consecutive landings (missed takeoff) and consecutive takeoffs (missed
landing) are reported as sequence breaks.

    python benchmarks/continuity.py --events data/eval/v6-f/events.sqlite \\
        --airports data/db/airports.sqlite --aircraft data/db/aircraft.sqlite
"""

from __future__ import annotations

import argparse
import json
import math
import sqlite3
from collections import Counter, defaultdict
from pathlib import Path

BINS = ((5.0, "adjacent"), (30.0, "nearby"), (math.inf, "remote"))


def distance_km(a: tuple[float, float], b: tuple[float, float]) -> float:
    lat1, lon1, lat2, lon2 = map(math.radians, (*a, *b))
    h = math.sin((lat2 - lat1) / 2) ** 2 + math.cos(lat1) * math.cos(lat2) * math.sin((lon2 - lon1) / 2) ** 2
    return 12742.0 * math.asin(math.sqrt(min(1.0, h)))


def connect(path: Path) -> sqlite3.Connection:
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def load_airports(path: Path) -> dict[int, tuple[str, str, float, float]]:
    db = connect(path)
    kinds = dict(db.execute("SELECT airport_type_id, label FROM airport_types"))
    return {
        i: (code, kinds[t], lat / 1e7, lon / 1e7)
        for i, code, t, lat, lon in db.execute("SELECT airport_id, code, airport_type_id, lat_e7, lon_e7 FROM airports")
    }


def load_classes(path: Path | None) -> dict[int, str]:
    if path is None:
        return {}
    db = connect(path)
    return dict(db.execute("SELECT a.icao_id, t.label FROM aircraft a JOIN aircraft_types t USING(aircraft_type_id)"))


def evaluate(events: Path, airports: dict, classes: dict, start: int | None, end: int | None, hours: float, top: int) -> dict:
    db = connect(events)
    where, args = [], []
    if start is not None:
        where.append("ts >= ?"); args.append(start)
    if end is not None:
        where.append("ts < ?"); args.append(end)
    sql = "SELECT icao_id, ts, is_landing, airport_id FROM endpoints"
    if where:
        sql += " WHERE " + " AND ".join(where)
    sql += " ORDER BY icao_id, ts"
    limit = hours * 3600
    totals: Counter = Counter()
    by_class: dict[str, Counter] = defaultdict(Counter)
    by_types: dict[tuple[str, str], Counter] = defaultdict(Counter)
    pairs: Counter = Counter()
    previous = None
    for row in db.execute(sql, args):
        icao, ts, landing, airport = row
        if previous and previous[0] == icao and ts - previous[1] <= limit:
            _, _, was_landing, prior = previous
            if was_landing and not landing:
                cls = classes.get(icao, "unknown")
                a, b = airports.get(prior), airports.get(airport)
                if prior == airport:
                    outcome = "same"
                elif a is None or b is None:
                    outcome = "unknown"
                else:
                    d = distance_km(a[2:], b[2:])
                    outcome = next(name for limit_km, name in BINS if d < limit_km)
                    if outcome != "remote":
                        pairs[(a[0], a[1], b[0], b[1], outcome)] += 1
                totals[outcome] += 1
                by_class[cls][outcome] += 1
                if a and b:
                    kinds = tuple(sorted((a[1], b[1])))
                    by_types[kinds][outcome] += 1
            elif was_landing and landing:
                totals["landing_landing"] += 1
            elif not was_landing and not landing:
                totals["takeoff_takeoff"] += 1
        previous = row
    linked = sum(totals[k] for k in ("same", "adjacent", "nearby", "remote", "unknown"))

    def rates(counter: Counter) -> dict:
        n = sum(counter[k] for k in ("same", "adjacent", "nearby", "remote", "unknown"))
        return {"pairs": n, **{k: round(100 * counter[k] / n, 3) if n else None for k in ("same", "adjacent", "nearby", "remote")}}

    return {
        "events": str(events),
        "summary": {**rates(totals), "landing_landing": totals["landing_landing"], "takeoff_takeoff": totals["takeoff_takeoff"]},
        "by_class": {k: rates(v) for k, v in sorted(by_class.items(), key=lambda kv: -sum(kv[1].values()))},
        "by_airport_types": {
            "/".join(k): rates(v)
            for k, v in sorted(by_types.items(), key=lambda kv: -(kv[1]["adjacent"] + kv[1]["nearby"]))
            if v["adjacent"] + v["nearby"]
        },
        "top_mismatches": [
            {"landing": f"{a} ({at})", "takeoff": f"{b} ({bt})", "bin": kind, "count": n}
            for (a, at, b, bt, kind), n in pairs.most_common(top)
        ],
        "linked": linked,
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--events", type=Path, action="append", required=True)
    parser.add_argument("--airports", type=Path, required=True)
    parser.add_argument("--aircraft", type=Path)
    parser.add_argument("--start", type=int, help="first endpoint unix time")
    parser.add_argument("--end", type=int, help="end unix time (exclusive)")
    parser.add_argument("--hours", type=float, default=48.0, help="maximum landing -> takeoff gap")
    parser.add_argument("--top", type=int, default=25)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    airports = load_airports(args.airports)
    classes = load_classes(args.aircraft)
    reports = [evaluate(e, airports, classes, args.start, args.end, args.hours, args.top) for e in args.events]
    for report in reports:
        s = report["summary"]
        print(f"{report['events']}: pairs={s['pairs']} same={s['same']}% adjacent={s['adjacent']}% "
              f"nearby={s['nearby']}% remote={s['remote']}% L->L={s['landing_landing']} T->T={s['takeoff_takeoff']}")
    if args.report:
        args.report.write_text(json.dumps(reports if len(reports) > 1 else reports[0], indent=1))
    else:
        print(json.dumps([{k: r[k] for k in ("by_class", "by_airport_types", "top_mismatches")} for r in reports], indent=1))


if __name__ == "__main__":
    main()
