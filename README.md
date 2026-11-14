# adsbfi-proxy

adsbfi-proxy turns the public [adsb.fi](https://adsb.fi) heatmap archive into a
SQLite ledger of aircraft takeoffs and landings. Each row records one movement:
the aircraft, the airport, the time and the callsign.

The project has three parts:

- a Python worker that mirrors the 30-minute heatmap files politely, at most
  one request every 10 seconds;
- a Rust builder that infers movements from them incrementally;
- a small HTTP API and browser.

The worker keeps the ledger current within a few minutes of each file's
publication. Given an existing archive copy, it can replay the full history.

On scheduled US airline flights, the inferred airports are 99.9% correct.
Takeoff and landing times are within one minute of the reported wheels-off and
wheels-on minute for 97–99% of flights. See [Accuracy](#accuracy).

## How it works

### Input

adsb.fi publishes `globe_history/YYYY/MM/DD/heatmap/NN.bin.ttf` every 30 minutes.
This is [readsb](https://github.com/wiedehopf/readsb)'s heatmap format. Each
file holds 30-second slices of every tracked aircraft, with:

- position;
- barometric altitude, or a ground flag;
- ground speed;
- periodic callsign records.

The builder reads each file once, in order, and keeps a bounded track state per
aircraft across files and restarts.

### Tracks

An aircraft's reports form a track. A track is split when reports stop for more
than 20 minutes, or when the aircraft is on the ground at the end of a file.
Tracks shorter than two minutes or three reports are ignored.

### Takeoff and landing times

A movement's time is the moment the aircraft leaves or reaches the runway, as
far as reception shows it:

- **Takeoff** is the first airborne report after a ground report, which is the
  liftoff. If the track starts in the air, it is the first airborne report.
- **Landing** is the last airborne report before the first ground report that
  follows it, which is the touchdown.

When reception starts or ends above the runway, the time is extrapolated:

- A takeoff subtracts the climb from 200 ft above the field. It assumes
  2,500 ft/min for transport aircraft, 800 for other fixed-wing aircraft and
  1,000 for rotorcraft, and moves back at most 10 minutes.
- A landing adds the larger of two estimates: the remaining distance beyond
  2 km at approach speed (250, 150 or 120 km/h), or the remaining height at
  the descent rate. It moves forward at most 20 minutes.

Neither estimate crosses an observed ground report. Taxi and gate reports never
set the time.

Timing against reported wheels-off and wheels-on was measured before this rule
was adopted:

- the first airborne report falls within one minute of wheels-off for 98.9% of
  airline departures;
- the last airborne report before ground is likewise the best landing time.

### Airports

Airports come from [OurAirports](https://ourairports.com/data/). An endpoint
needs a supporting report within the first or last 8 minutes of a track. That
report must be on the ground, or less than 1,500 ft above a field within 12 km.
The builder chooses the nearest compatible airport and checks it against the
aircraft's motion:

- **Compatible references.** Each aircraft class sees only plausible fields:
  - rotorcraft may use any airport or heliport;
  - airliners and business jets never use heliports, seaplane bases or
    ultralight fields;
  - multi-engine and turbine landplanes never use seaplane bases, unless their
    type is commonly flown on floats (Twin Otter, Caravan, Kodiak, turbine
    Beaver and Otter);
  - single-engine pistons keep water candidates, because many are on floats.
- **Direction checks.**
  - An aircraft descending past a small field and moving away from it did not
    land there.
  - A weak climb that does not move away from a small field or water base did
    not depart from it.
  - A fixed-wing track whose altitude changes while its position is frozen
    cannot place an airport.
- **Recovered movements.** Reception is often missing near the ground. Three
  rules recover the movement anyway, and each requires a physically feasible
  gap:
  - When an aircraft's next departure is observed, the previous track's
    descent toward that airport is accepted as the arrival, within 48 hours.
  - A transport aircraft first seen in a steady climb, moving away from a
    major airport at a plausible gradient, is given a departure from it.
  - An observed arrival and the next observed departure establish the
    movement in between.
- **Continuity.** An aircraft takes off from the airport where it last landed.
  - When the takeoff is seen only in the air, the previous landing's airport
    wins over another airport within 15 km.
  - A small-field arrival is reassigned when the next departure is from a
    major airport beside it, and the arrival's final reports were closing on
    that airport.

Each endpoint is accepted on its own evidence. A missing opposite end never
removes an observed movement, and the ledger never invents a paired flight.

[docs/inference.md](docs/inference.md) gives the details, thresholds and
experiments.

## Accuracy

### BTS airline flights

The [BTS On-Time Performance](https://www.transtats.bts.gov/Fields.asp?gnoyr_VQ=FGJ)
data lists every domestic flight of the major US airlines with its tail number,
airports and wheels-off/on minute. US tail numbers map to ICAO addresses
algorithmically, so each flight labels a takeoff and a landing.

[Goode (2026)](https://symposium.opensky-network.org/preprints/papers/osn26-9326-goode.pdf)
used this data to evaluate [PlaneQuery](https://github.com/PlaneQuery/planequery-data).
PlaneQuery segments flights with readsb legs and chooses airports with a
neural network. Under his protocol:

- gold flights take off on the UTC day;
- a flight matches when both of its times fall within 60 minutes;
- every unmatched derived flight counts against precision.

`benchmarks/bts_eval.py --goode` reproduces the protocol. Results for
2026-03-01:

| System | ADS-B data | Flight P / R / F1 (%) | Airport TO / LD (%) | Time bias TO / LD (min) |
| --- | --- | --- | --- | --- |
| PlaneQuery (readsb legs + NN) | OpenSky, 3 days | 92.26 / 96.87 / 94.50 | 97.00 / 95.51 | 1.01 / 0.64 |
| PlaneQuery (NN) | ADS-B Exchange, 1 day | 88.67 / 96.03 / 92.20 | 99.99 / 96.21 | |
| PlaneQuery (NN) | ADSB.lol, 1 day | 89.02 / 92.80 / 90.87 | 99.39 / 95.53 | |
| ADS-B Exchange flights | ADS-B Exchange | 94.31 / 89.19 / 91.68 | 100.00 / 99.97 | 0.61 / 0.91 |
| **adsbfi-proxy** | adsb.fi | 91.68 / 95.49 / 93.55 | 99.96 / 99.94 | 0.13 / 0.29 |

The PlaneQuery and ADS-B Exchange rows are from the paper. Over 2026-03-01 to
03-06, adsbfi-proxy scores F1 92.6–93.9 with 99.9% airport accuracy on both ends.
In short:

- **Segmentation** is on a par with PlaneQuery's best configuration.
- **Airports** are wrong about once in 1,700 movements. PlaneQuery's best
  segmentation configuration gets 1 in 33 takeoffs and 1 in 22 landings wrong.
- **Times** are unbiased to within 20 seconds.

The comparison is not fully controlled:

- the ADS-B networks differ;
- our UTC-day gold set has 17,907 flights, against his 15,951;
- BTS omits charter, repositioning and international legs, so precision
  understates real performance for every system.

Our own protocol matches each takeoff and landing within 20 minutes and leaves
international legs out of precision. It also measures times, using the middle
of the BTS minute as the reference. Days 03-02 to 03-04 were used for
development; the other periods are held out.

| Period | Takeoff / landing recall | Within 1 min | Flight F1 |
| --- | --- | --- | --- |
| 2026-03-02..04 (dev) | 97.2 / 97.1% | 99.2 / 97.7% | 95.3 |
| 2026-03-05..06 | 97.2 / 97.1% | 99.3 / 97.4% | 95.2 |
| 2026-06-09..16 | 94.4 / 93.7% | 98.9 / 97.2% | 93.1 |

### Continuity, for all aircraft

BTS covers only airliners. For every aircraft, `benchmarks/continuity.py`
checks whether the takeoff that follows each landing within 48 hours is from
the same airport. A mismatch means one of three things:

- one endpoint names the wrong airport;
- a whole flight went untracked;
- the aircraft was moved by road.

On a 10-day replay, 99.41% of 912,959 landing→takeoff pairs agree:

| Class | Same airport |
| --- | --- |
| commercial jets | 99.94% |
| business jets | 99.92% |
| turboprops | 99.44% |
| pistons | 98.57% |
| helicopters | 93.60% |

Most helicopter mismatches are at heliports next to airports. Continuity rules
were accepted only when they also left BTS airport accuracy unchanged.

### Known limitations

- **Movements with no reception.** A movement with no reception near its
  airport, and no corroborating neighbour, is missed.
- **Neighbouring airports.** Some arrival and departure pairs are both
  assigned to a neighbouring field, such as Gulfport/Keesler AFB, Kahului/
  Kapalua or McAllen/Reynosa. Continuity cannot detect these.
- **Reference data.** Airport and aircraft references describe the imported
  snapshot, not historical validity. An ICAO address reassigned to a different
  aircraft is classified by its current registration.
- **Times.** A movement time is an observation-based estimate, not a runway
  crossing time.

## Running it

Python 3.12+ and Rust are needed to build. The worker and server use only the
Python standard library and the compiled builder.

```sh
uv sync --extra imports
cargo build --release --manifest-path rust/flight-builder/Cargo.toml

export ADSBFI_DB_DIR=/srv/adsbfi/db
export ADSBFI_CACHE_DIR=/srv/adsbfi/heatmaps
uv run adsbfi-proxy import-airports          # OurAirports
uv run adsbfi-proxy import-aircraft          # see "Aircraft references"
uv run adsbfi-proxy worker                   # download + build, forever
uv run adsbfi-proxy serve --port 8082        # API and browser
uv run adsbfi-proxy status
```

### Configuration

| Variable | Default | Meaning |
| --- | --- | --- |
| `ADSBFI_DB_DIR` | `data/db` | `events.sqlite`, `build_state.sqlite`, `aircraft.sqlite`, `airports.sqlite` |
| `ADSBFI_CACHE_DIR` | `data/heatmaps` | downloaded heatmaps, in the upstream layout |
| `ADSBFI_ARCHIVE_DIR` | none | read-only copy of `globe_history`; used instead of downloading, never modified |
| `ADSBFI_MODE` | `live` | `live` deletes files after they are built; `mirror` keeps them and audits for holes every six hours |
| `ADSBFI_START` | latest slot | first slot to build in a new database |
| `ADSBFI_MIRROR_START` | oldest file | earliest slot the mirror should hold |
| `ADSBFI_BASE_URL` | `https://globe.adsb.fi/globe_history` | upstream |
| `ADSBFI_FALLBACK_URL` | none | another readsb `globe_history`, tried on 403/404 or errors |
| `ADSBFI_POLL_SECONDS` / `ADSBFI_REQUEST_SECONDS` | 60 / 10 | polling interval / minimum seconds between requests |
| `ADSBFI_MIN_FREE_GB` | 5 | pause rather than fill a volume |
| `ADSBFI_PASSWORD` | none | HTTP Basic password for the server |

The worker downloads only complete files. It writes them atomically and checks
their length and header before use. Retries, 429 `Retry-After` and daily
retries of historical 404s persist in `mirror.sqlite`.

The builder never skips a missing slot. Endpoint rows commit before the
checkpoint that covers them, so an interruption replays safely.

To rebuild history from an archive into a fresh database, run the builder
directly:

```sh
rust/flight-builder/target/release/flight-builder build-events \
  --cache-dir "$ADSBFI_CACHE_DIR" --archive-dir /path/to/globe_history \
  --events-db "$ADSBFI_DB_DIR/events.sqlite" --state-db "$ADSBFI_DB_DIR/build_state.sqlite" \
  --airports-db "$ADSBFI_DB_DIR/airports.sqlite" --aircraft-db "$ADSBFI_DB_DIR/aircraft.sqlite" \
  --start 2023-01-01 --end 2026-11-01 --commit-every 48
```

Then start the worker on the same database. It continues from the checkpoint.
An inference change needs fresh event and state databases; the builder refuses
an incompatible checkpoint.

### Aircraft references

`import-aircraft` merges several sources into `aircraft.sqlite`:

- the [FAA registry](https://registry.faa.gov/database/ReleasableAircraft.zip)
  (`ADSBFI_FAA_ZIP`);
- these files in `ADSBFI_AIRCRAFT_CACHE_DIR`:
  - [tar1090-db](https://github.com/wiedehopf/tar1090-db) `aircraft.csv.gz`
    and `icao_aircraft_types2.js`;
  - the ADS-B Exchange basic aircraft database;
  - the OpenSky aircraft database.

Each aircraft gets a registration, an ICAO type, a physical airframe
descriptor (e.g. `L2J`) and a class: commercial jet, business jet, turboprop,
piston, helicopter, and so on. `import-aircraft-owners --download` refreshes
only the owner and operator names.

### Deployment

`deploy/` has systemd templates for the worker, the server and the daily owner
refresh. Run one worker per database. Read-only consumers may open the
databases with `mode=ro` while it runs. Stop the worker before moving or
replacing databases.

## Data and API

`events.sqlite`:

```text
endpoints(endpoint_id, icao_id, ts, is_landing, airport_id, callsign_id)
callsigns(callsign_id, callsign)
meta(key, value)        -- inference_version, ledger_schema, counts, time bounds
```

The other tables are:

- `icao_id`, the 24-bit ICAO address, joins `aircraft.sqlite`;
- `airport_id`, the OurAirports ID, joins `airports.sqlite`;
- `ts` is UTC seconds;
- `endpoint_id` increases with insertion, not event time, so consumers should
  checkpoint on it.

A movement takes the callsign flown between its takeoff and landing. Callsigns
are decoded from the identification records interleaved with heatmap positions.

API:

- `GET /api/endpoints?after_id=0&limit=100` takes these optional filters:
  - `airport`, an ICAO code;
  - `aircraft`, an ICAO hex address or a registration;
  - `callsign`;
  - `kind`, either `takeoff` or `landing`;
  - `start`/`end`, in UTC.

  The response has `next_after_id` and `has_more`.
- `GET /api/airports?q=Miami`, `GET /api/status`, `/health`.

## Development

```sh
cargo test --manifest-path rust/flight-builder/Cargo.toml
cargo build --release --manifest-path rust/flight-builder/Cargo.toml
ADSBFI_TEST_BUILDER=rust/flight-builder/target/release/flight-builder \
  PYTHONPATH=src python3 -m unittest discover -s tests
```

Evaluate an inference change before adopting it:

1. Replay a fresh ledger: `benchmarks/replay.sh OUT START END`.
2. Compare it with the previous version using `benchmarks/bts_eval.py` and
   `benchmarks/continuity.py` on held-out days.

`uv sync --extra evaluation` installs the evaluation dependency.

## Credits

Data sources:

- **adsb.fi**: heatmaps.
- **OurAirports**: public-domain airport data.
- **FAA**: aircraft registry.
- **tar1090-db/Mictronics, ADS-B Exchange and OpenSky**: aircraft databases.
- **BTS**: On-Time Performance data, used for evaluation.

None of these datasets is redistributed here. Thanks to Jonah Goode for the
BTS evaluation design and the PlaneQuery comparison.
