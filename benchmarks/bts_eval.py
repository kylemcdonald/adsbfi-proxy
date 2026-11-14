"""Score an endpoint ledger against BTS On-Time Performance ground truth.

BTS reports every domestic flight of the major US carriers with its tail
number, origin, destination and wheels-off/wheels-on minute (local time). US
registrations map to ICAO addresses algorithmically, so each reported flight
gives two labelled movements: a takeoff at the origin and a landing at the
destination.

Two views are reported:

* endpoints: each gold takeoff/landing is matched one-to-one, by time, to a
  ledger endpoint of the same aircraft and kind within --window minutes.
  Airport accuracy and time error are measured on the matched pairs.
* flights: consecutive takeoff -> landing endpoints of an aircraft form a
  flight, matched to a gold flight when both times fall within --window.
  This mirrors the segmentation precision/recall/F1 and TO/LD airport
  accuracy reported by Goode (2026) for BTS 2026-03-01.

Precision counts every ledger endpoint of a gold aircraft between its first
gold wheels-off and last gold wheels-on (with --window margin). BTS reports
only domestic scheduled legs, so unmatched endpoints of a leg with a non-US
end (at that endpoint or its takeoff/landing partner) are left out of the
precision denominators. Repositioning, charter and international legs whose
foreign end went unobserved remain, so precision is still a lower bound.

    uv run --extra evaluation python benchmarks/bts_eval.py \\
        --events data/eval/v5/events.sqlite --airports data/db/airports.sqlite \\
        --bts data/bts/ontime_2026_3.zip --ourairports data/ref/ourairports-airports.csv \\
        --date 2026-03-01 --report data/eval/v5/bts-2026-03-01.json
"""

from __future__ import annotations

import argparse
import bisect
import csv
import datetime as dt
import io
import json
import sqlite3
import statistics
import zipfile
from collections import Counter, defaultdict
from functools import lru_cache
from pathlib import Path
from zoneinfo import ZoneInfo

# --- US registration -> ICAO address -------------------------------------------------

_CHARSET = "ABCDEFGHJKLMNPQRSTUVWXYZ"  # no I or O
_DIGITS = "0123456789"


def _suffix_offset(suffix: str) -> int:
    """Offset of a 0-2 letter suffix: '', A, AA..AZ, B, BA..."""
    if not suffix:
        return 0
    first = _CHARSET.index(suffix[0])
    offset = first * 25 + 1
    if len(suffix) == 2:
        offset += _CHARSET.index(suffix[1]) + 1
    return offset


@lru_cache(maxsize=None)
def _block_size(depth: int) -> int:
    """Registrations at and below a prefix with `depth` characters still free.

    The FAA orders each prefix as: itself, its 600 one/two-letter suffixes,
    then its ten digit children. The final character is one letter or digit.
    """
    if depth == 1:
        return 1 + 24 + 10
    return 1 + 600 + 10 * _block_size(depth - 1)


def icao_for_registration(registration: str) -> int | None:
    """FAA N-number to its assigned 24-bit address (A00001-ADF7C7)."""
    reg = registration.strip().upper()
    if not reg.startswith("N") or not 2 <= len(reg) <= 6:
        return None
    tail = reg[1:]
    if tail[0] not in "123456789" or not all(c in _DIGITS + _CHARSET for c in tail):
        return None
    # Characters remaining after the first digit: up to 4.
    address = 0xA00001 + (int(tail[0]) - 1) * _block_size(4)
    remaining = 4
    rest = tail[1:]
    while rest:
        char = rest[0]
        if char in _CHARSET:
            # Letter suffix (one or two letters) ends the registration.
            if len(rest) > 2 or (len(rest) == 2 and rest[1] not in _CHARSET):
                return None
            if remaining == 1 and len(rest) > 1:
                return None
            if remaining == 1:
                return address + 1 + _CHARSET.index(char)
            return address + _suffix_offset(rest)
        if remaining == 1:
            return address + 1 + 24 + int(char)
        address += 601 + int(char) * _block_size(remaining - 1)
        remaining -= 1
        rest = rest[1:]
    return address


# --- Ground truth ---------------------------------------------------------------------


US_COUNTRIES = {"US", "PR", "VI", "GU", "AS", "MP"}


def load_domestic_ids(ourairports_csv: Path, airports_db: Path) -> set[int]:
    db = sqlite3.connect(f"file:{airports_db}?mode=ro", uri=True)
    ids = dict(db.execute("SELECT code, airport_id FROM airports"))
    with ourairports_csv.open(newline="", encoding="utf-8") as handle:
        return {ids[r["ident"]] for r in csv.DictReader(handle) if r["iso_country"] in US_COUNTRIES and r["ident"] in ids}


def load_iata_map(ourairports_csv: Path, airports_db: Path):
    """IATA code -> (airport_id, lat, lon) using OurAirports idents."""
    db = sqlite3.connect(f"file:{airports_db}?mode=ro", uri=True)
    ids = dict(db.execute("SELECT code, airport_id FROM airports"))
    rank = {"large_airport": 0, "medium_airport": 1, "small_airport": 2}
    best = {}
    with ourairports_csv.open(newline="", encoding="utf-8") as handle:
        for row in csv.DictReader(handle):
            code = row["iata_code"].strip()
            if not code or row["ident"] not in ids:
                continue
            key = (rank.get(row["type"], 3), row["iso_country"] != "US")
            if code not in best or key < best[code][0]:
                best[code] = (key, ids[row["ident"]], float(row["latitude_deg"]), float(row["longitude_deg"]), row["ident"])
    return {code: value[1:] for code, value in best.items()}


class Zones:
    def __init__(self):
        from timezonefinder import TimezoneFinder

        self.finder = TimezoneFinder()
        self.cache = {}

    def get(self, lat, lon):
        key = (round(lat, 3), round(lon, 3))
        if key not in self.cache:
            self.cache[key] = ZoneInfo(self.finder.timezone_at(lat=lat, lng=lon) or "UTC")
        return self.cache[key]


def _local(date: dt.date, hhmm: str, zone: ZoneInfo) -> dt.datetime:
    value = int(hhmm)
    hour, minute = divmod(value, 100)
    day = date
    if hour == 24:
        hour, day = 0, date + dt.timedelta(days=1)
    return dt.datetime(day.year, day.month, day.day, hour, minute, tzinfo=zone)


def _closest(date: dt.date, hhmm: str, zone: ZoneInfo, target: float) -> float:
    """The UTC timestamp of local hhmm on date-1..date+1 closest to target."""
    candidates = [_local(date + dt.timedelta(days=d), hhmm, zone).timestamp() for d in (-1, 0, 1)]
    return min(candidates, key=lambda ts: abs(ts - target))


def load_bts(paths: list[Path], dates: set[dt.date], iata, zones: Zones):
    flights = []
    skipped = Counter()
    for path in paths:
        with zipfile.ZipFile(path) as archive:
            name = next(n for n in archive.namelist() if n.endswith(".csv"))
            with archive.open(name) as raw:
                for row in csv.DictReader(io.TextIOWrapper(raw, encoding="latin-1")):
                    date = dt.date.fromisoformat(row["FlightDate"])
                    if date not in dates:
                        continue
                    if row["Cancelled"] not in ("0.00", "0") or row["Diverted"] not in ("0.00", "0"):
                        skipped["cancelled_or_diverted"] += 1
                        continue
                    if not row["WheelsOff"] or not row["WheelsOn"] or not row["AirTime"]:
                        skipped["no_times"] += 1
                        continue
                    icao = icao_for_registration(row["Tail_Number"])
                    if icao is None:
                        skipped["unmapped_tail"] += 1
                        continue
                    origin, dest = iata.get(row["Origin"]), iata.get(row["Dest"])
                    if origin is None or dest is None:
                        skipped["unmapped_airport"] += 1
                        continue
                    origin_zone = zones.get(origin[1], origin[2])
                    scheduled = _local(date, row["CRSDepTime"], origin_zone).timestamp()
                    expected_off = scheduled + 60 * (float(row["DepDelay"] or 0) + float(row["TaxiOut"] or 0))
                    off = _closest(date, row["WheelsOff"], origin_zone, expected_off)
                    on = off + 60 * float(row["AirTime"])
                    flights.append(
                        dict(
                            icao=icao,
                            tail=row["Tail_Number"],
                            carrier=row["Reporting_Airline"],
                            flight=row["Flight_Number_Reporting_Airline"],
                            date=row["FlightDate"],
                            origin=row["Origin"],
                            dest=row["Dest"],
                            origin_id=origin[0],
                            dest_id=dest[0],
                            off=off,
                            on=on,
                        )
                    )
    return flights, skipped


# --- Predictions ----------------------------------------------------------------------


def load_endpoints(events_db: Path, icaos, start, end):
    db = sqlite3.connect(f"file:{events_db}?mode=ro", uri=True)
    rows = defaultdict(list)
    for icao in icaos:
        for ts, landing, airport in db.execute(
            "SELECT ts, is_landing, airport_id FROM endpoints WHERE icao_id=? AND ts>=? AND ts<? ORDER BY ts",
            (icao, start, end),
        ):
            rows[icao].append((ts, landing, airport))
    return rows


def match(gold, predicted, window):
    """Greedy one-to-one matching by absolute time difference."""
    pairs = sorted(
        (abs(p[0] - g[0]), gi, pi)
        for gi, g in enumerate(gold)
        for pi, p in enumerate(predicted)
        if abs(p[0] - g[0]) <= window
    )
    used_g, used_p, result = set(), set(), {}
    for _, gi, pi in pairs:
        if gi in used_g or pi in used_p:
            continue
        used_g.add(gi)
        used_p.add(pi)
        result[gi] = pi
    return result


def summarize_errors(errors):
    if not errors:
        return {}
    minutes = sorted(e / 60 for e in errors)
    absolute = sorted(abs(e) for e in minutes)
    return dict(
        n=len(minutes),
        bias_min=round(statistics.fmean(minutes), 3),
        median_min=round(statistics.median(minutes), 3),
        mae_min=round(statistics.fmean(absolute), 3),
        p90_abs_min=round(absolute[int(0.9 * (len(absolute) - 1))], 3),
        within_1min=round(sum(a <= 1 for a in absolute) / len(absolute), 4),
        within_2min=round(sum(a <= 2 for a in absolute) / len(absolute), 4),
    )


def international(endpoints, domestic):
    """Indices of endpoints on a leg with a non-US end."""
    flagged = set()
    for i, (ts, landing, airport) in enumerate(endpoints):
        if airport not in domestic:
            flagged.add(i)
        partner = i + 1 if not landing else i - 1
        if 0 <= partner < len(endpoints) and endpoints[partner][1] != landing:
            if endpoints[partner][2] not in domestic and abs(endpoints[partner][0] - ts) <= 24 * 3600:
                flagged.add(i)
    return flagged


def evaluate(flights, endpoints, window, domestic, details=None):
    by_icao = defaultdict(list)
    for flight in flights:
        by_icao[flight["icao"]].append(flight)
    report = {}
    totals = Counter()
    errors = {0: [], 1: []}
    flight_errors = {0: [], 1: []}
    for icao, gold_flights in by_icao.items():
        gold_flights.sort(key=lambda f: f["off"])
        lo = gold_flights[0]["off"] - window
        hi = gold_flights[-1]["on"] + window
        everything = endpoints.get(icao, [])
        foreign = international(everything, domestic)
        keep = [i for i, e in enumerate(everything) if lo <= e[0] <= hi]
        mine = [everything[i] for i in keep]
        abroad = {j for j, i in enumerate(keep) if i in foreign}
        if not any(e[1] == 0 for e in mine) and not any(e[1] == 1 for e in mine):
            totals["zero_endpoint_icaos"] += 1
        for kind, key, airport in ((0, "off", "origin_id"), (1, "on", "dest_id")):
            gold = [(f[key], f[airport], f) for f in gold_flights]
            pred_index = [j for j, e in enumerate(mine) if e[1] == kind]
            pred = [mine[j] for j in pred_index]
            matched = match(gold, pred, window)
            totals[f"gold_{kind}"] += len(gold)
            totals[f"pred_{kind}"] += len(pred)
            totals[f"abroad_{kind}"] += sum(1 for pi, j in enumerate(pred_index) if j in abroad and pi not in matched.values())
            totals[f"matched_{kind}"] += len(matched)
            for gi, pi in matched.items():
                correct = pred[pi][2] == gold[gi][1]
                totals[f"airport_ok_{kind}"] += correct
                errors[kind].append(pred[pi][0] - gold[gi][0])
                if details is not None:
                    details.append(dict(kind="landing" if kind else "takeoff", status="ok" if correct else "wrong_airport",
                                        icao=f"{icao:06x}", gold_ts=gold[gi][0], pred_ts=pred[pi][0],
                                        gold_airport=gold[gi][1], pred_airport=pred[pi][2],
                                        flight=f'{gold[gi][2]["carrier"]}{gold[gi][2]["flight"]} {gold[gi][2]["origin"]}-{gold[gi][2]["dest"]}'))
            if details is not None:
                for gi, g in enumerate(gold):
                    if gi not in matched:
                        details.append(dict(kind="landing" if kind else "takeoff", status="missed", icao=f"{icao:06x}",
                                            gold_ts=g[0], gold_airport=g[1],
                                            flight=f'{g[2]["carrier"]}{g[2]["flight"]} {g[2]["origin"]}-{g[2]["dest"]}'))
                used = set(matched.values())
                for pi, p in enumerate(pred):
                    if pi not in used:
                        details.append(dict(kind="landing" if kind else "takeoff",
                                            status="international" if pred_index[pi] in abroad else "extra", icao=f"{icao:06x}",
                                            pred_ts=p[0], pred_airport=p[2]))
        # Flights: consecutive takeoff -> landing.
        segments = [(a, b) for j, (a, b) in enumerate(zip(mine, mine[1:])) if a[1] == 0 and b[1] == 1]
        segment_abroad = [j in abroad or j + 1 in abroad for j, (a, b) in enumerate(zip(mine, mine[1:])) if a[1] == 0 and b[1] == 1]
        totals["pred_flights"] += len(segments)
        totals["gold_flights"] += len(gold_flights)
        if not segments:
            totals["zero_flight_icaos"] += 1
        pairs = sorted(
            (abs(s[0][0] - f["off"]) + abs(s[1][0] - f["on"]), fi, si)
            for fi, f in enumerate(gold_flights)
            for si, s in enumerate(segments)
            if abs(s[0][0] - f["off"]) <= window and abs(s[1][0] - f["on"]) <= window
        )
        used_f, used_s = set(), set()
        for _, fi, si in pairs:
            if fi in used_f or si in used_s:
                continue
            used_f.add(fi)
            used_s.add(si)
            f, s = gold_flights[fi], segments[si]
            totals["matched_flights"] += 1
            totals["flight_to_ok"] += s[0][2] == f["origin_id"]
            totals["flight_ld_ok"] += s[1][2] == f["dest_id"]
            flight_errors[0].append(s[0][0] - f["off"])
            flight_errors[1].append(s[1][0] - f["on"])
        totals["abroad_flights"] += sum(1 for si, flag in enumerate(segment_abroad) if flag and si not in used_s)
    report["gold_icaos"] = len(by_icao)
    report["gold_flights"] = len(flights)

    def pct(a, b):
        return round(100 * a / b, 2) if b else None

    for kind, label in ((0, "takeoff"), (1, "landing")):
        report[label] = dict(
            gold=totals[f"gold_{kind}"],
            predicted=totals[f"pred_{kind}"],
            matched=totals[f"matched_{kind}"],
            international=totals[f"abroad_{kind}"],
            recall=pct(totals[f"matched_{kind}"], totals[f"gold_{kind}"]),
            precision=pct(totals[f"matched_{kind}"], totals[f"pred_{kind}"] - totals[f"abroad_{kind}"]),
            precision_all=pct(totals[f"matched_{kind}"], totals[f"pred_{kind}"]),
            airport_accuracy=pct(totals[f"airport_ok_{kind}"], totals[f"matched_{kind}"]),
            correct_airport_recall=pct(totals[f"airport_ok_{kind}"], totals[f"gold_{kind}"]),
            time=summarize_errors(errors[kind]),
        )
    p = pct(totals["matched_flights"], totals["pred_flights"] - totals["abroad_flights"])
    r = pct(totals["matched_flights"], totals["gold_flights"])
    report["flights"] = dict(
        predicted=totals["pred_flights"],
        international=totals["abroad_flights"],
        matched=totals["matched_flights"],
        precision_all=pct(totals["matched_flights"], totals["pred_flights"]),
        precision=p,
        recall=r,
        f1=round(2 * p * r / (p + r), 2) if p and r else None,
        zero_flight_icaos_pct=pct(totals["zero_flight_icaos"], len(by_icao)),
        airport_accuracy_to=pct(totals["flight_to_ok"], totals["matched_flights"]),
        airport_accuracy_ld=pct(totals["flight_ld_ok"], totals["matched_flights"]),
        time_bias_to_min=round(statistics.fmean(flight_errors[0]) / 60, 3) if flight_errors[0] else None,
        time_bias_ld_min=round(statistics.fmean(flight_errors[1]) / 60, 3) if flight_errors[1] else None,
    )
    return report


def evaluate_goode(flights, events_db, day, tolerance):
    """Goode (2026), section 5.3: flights taking off on the UTC day, both times within tolerance.

    Gold flights are BTS flights with wheels-off on the UTC day. Test flights are
    consecutive takeoff -> landing endpoints of gold aircraft whose takeoff falls on
    that day. Every unmatched test flight counts against precision, including
    international and unreported legs.
    """
    start = dt.datetime.combine(day, dt.time(), dt.timezone.utc).timestamp()
    end = start + 86400
    gold = [f for f in flights if start <= f["off"] < end]
    by_icao = defaultdict(list)
    for f in gold:
        by_icao[f["icao"]].append(f)
    endpoints = load_endpoints(events_db, by_icao, start - 86400, end + 86400)
    totals = Counter()
    bias = ([], [])
    for icao, gold_flights in by_icao.items():
        rows = endpoints.get(icao, [])
        tests = [
            (a, b) for a, b in zip(rows, rows[1:])
            if not a[1] and b[1] and start <= a[0] < end and b[0] - a[0] <= 86400
        ]
        totals["test"] += len(tests)
        totals["zero"] += not tests
        pairs = sorted(
            (abs(t[0][0] - f["off"]) + abs(t[1][0] - f["on"]), fi, ti)
            for fi, f in enumerate(gold_flights)
            for ti, t in enumerate(tests)
            if abs(t[0][0] - f["off"]) <= tolerance and abs(t[1][0] - f["on"]) <= tolerance
        )
        used_f, used_t = set(), set()
        for _, fi, ti in pairs:
            if fi in used_f or ti in used_t:
                continue
            used_f.add(fi)
            used_t.add(ti)
            f, t = gold_flights[fi], tests[ti]
            totals["matched"] += 1
            totals["to_ok"] += t[0][2] == f["origin_id"]
            totals["ld_ok"] += t[1][2] == f["dest_id"]
            bias[0].append(t[0][0] - f["off"])
            bias[1].append(t[1][0] - f["on"])
    p = 100 * totals["matched"] / totals["test"] if totals["test"] else 0
    r = 100 * totals["matched"] / len(gold) if gold else 0
    return dict(
        protocol="goode-2026",
        day=day.isoformat(),
        tolerance_min=tolerance / 60,
        gold_icaos=len(by_icao),
        gold_flights=len(gold),
        test_flights=totals["test"],
        matched=totals["matched"],
        precision=round(p, 2),
        recall=round(r, 2),
        f1=round(2 * p * r / (p + r), 2) if p and r else None,
        zero_flight_icaos_pct=round(100 * totals["zero"] / len(by_icao), 2) if by_icao else None,
        airport_accuracy_to=round(100 * totals["to_ok"] / totals["matched"], 2) if totals["matched"] else None,
        airport_accuracy_ld=round(100 * totals["ld_ok"] / totals["matched"], 2) if totals["matched"] else None,
        time_bias_to_min=round(statistics.fmean(bias[0]) / 60, 2) if bias[0] else None,
        time_bias_ld_min=round(statistics.fmean(bias[1]) / 60, 2) if bias[1] else None,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--events", type=Path, required=True)
    parser.add_argument("--airports", type=Path, required=True)
    parser.add_argument("--bts", type=Path, nargs="+", required=True)
    parser.add_argument("--ourairports", type=Path, required=True)
    parser.add_argument("--date", nargs="+", required=True, help="FlightDate(s) or START..END (inclusive)")
    parser.add_argument("--window", type=float, default=20, help="Match window in minutes")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--details", type=Path, help="Write per-movement match rows (JSON lines)")
    parser.add_argument("--goode", action="store_true",
                        help="Score each date with Goode's protocol (UTC day, 60-minute tolerance unless --window is given)")
    args = parser.parse_args()
    dates = set()
    for value in args.date:
        if ".." in value:
            a, b = (dt.date.fromisoformat(x) for x in value.split(".."))
            dates.update(a + dt.timedelta(days=i) for i in range((b - a).days + 1))
        else:
            dates.add(dt.date.fromisoformat(value))
    iata = load_iata_map(args.ourairports, args.airports)
    if args.goode:
        # A UTC day's departures span the neighbouring local FlightDates.
        local = {d + dt.timedelta(days=k) for d in dates for k in (-1, 0, 1)}
        flights, _ = load_bts(args.bts, local, iata, Zones())
        tolerance = (args.window if "--window" in __import__("sys").argv else 60) * 60
        reports = [evaluate_goode(flights, args.events, d, tolerance) for d in sorted(dates)]
        text = json.dumps(reports if len(reports) > 1 else reports[0], indent=2)
        print(text)
        if args.report:
            args.report.write_text(text + "\n")
        return
    flights, skipped = load_bts(args.bts, dates, iata, Zones())
    window = args.window * 60
    icaos = {f["icao"] for f in flights}
    start = min(f["off"] for f in flights) - window
    end = max(f["on"] for f in flights) + window
    endpoints = load_endpoints(args.events, icaos, start, end)
    details = [] if args.details else None
    report = evaluate(flights, endpoints, window, load_domestic_ids(args.ourairports, args.airports), details)
    report["dates"] = sorted(d.isoformat() for d in dates)
    report["window_min"] = args.window
    report["skipped"] = dict(skipped)
    report["events"] = str(args.events)
    text = json.dumps(report, indent=2)
    print(text)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(text + "\n")
    if details is not None:
        with args.details.open("w") as handle:
            for row in details:
                handle.write(json.dumps(row) + "\n")


if __name__ == "__main__":
    main()
