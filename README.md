Current inference: [endpoint v5](ENDPOINT_V5.md), including physical-airframe evidence, preserved climb samples, generalized arrival confirmation and position-quality checks. Production workers run v5 against a compact ledger with callsigns, replayed from the full mirror; rebuild into fresh databases before changing inference or the ledger schema.

# adsbfi-proxy

A rolling SQLite ledger of independently inferred takeoffs and landings from
ADS-B Exchange heatmaps, with an optional archive mirror and movement browser.

**Endpoint v5 requires fresh event and checkpoint databases.** Re-import aircraft
references to populate physical airframe descriptors and the packed inference
index. Do not point the new builder at an existing v1/v2/v3/v4 installation.
See [inference, timing and validation](ENDPOINT_V5.md).

## Run

Python 3.12+ and Rust are required to install/build. The running worker and HTTP
server use only Python's standard library and the compiled Rust binary.

```sh
uv sync
cargo build --release --manifest-path rust/flight-builder/Cargo.toml
export ADSBFI_DB_DIR=/path/to/databases
export ADSBFI_CACHE_DIR=/path/to/heatmap-cache
export ADSBFI_MODE=mirror
uv run adsbfi-proxy worker
```

The service owns four databases in `ADSBFI_DB_DIR`:
`events.sqlite`, `build_state.sqlite`, `aircraft.sqlite`, and `airports.sqlite`.
Keep these together when moving an installation, and stop the worker before
moving them. Other applications can read the endpoint and reference databases
directly. The first builder run initializes summary metadata; subsequent updates
summarize only appended rows.

For a fresh install, populate the reference databases first:

```sh
uv sync --extra imports
uv run adsbfi-proxy import-airports
# Configure ADSBFI_FAA_ZIP and ADSBFI_AIRCRAFT_CACHE_DIR to existing inputs.
uv run adsbfi-proxy import-aircraft
# Optional first flight-processing slot; otherwise begin with the latest closed slot.
export ADSBFI_START=2026-09-01
```

`import-aircraft-owners` adds owner and operator names from ADS-B Exchange's
basic aircraft database and OpenSky's aircraft database to `aircraft.sqlite`,
in place and touching no other column (`import-aircraft` runs it too). With
`--download` it first fetches today's ADS-B Exchange file and, once a month,
OpenSky's newest `aircraft-database-complete-YYYY-MM.csv` snapshot into
`ADSBFI_AIRCRAFT_CACHE_DIR`:

```sh
uv run adsbfi-proxy import-aircraft-owners --download
```

| Column | Source |
| --- | --- |
| `adsbx_ownop` | ADS-B Exchange `ownop`: FAA registrants, including every co-owner of a fractional aircraft (the FAA `owner_name` holds only the first), and some foreign owners |
| `opensky_owner` | OpenSky owner (crowd-sourced; often the operator) |
| `opensky_operator`, `opensky_operator_icao` | OpenSky operator and its ICAO code |

A source's names are stored only while it lists the registration that
`aircraft.registration` has for the address. The FAA `owner_*` columns are
unchanged.

Run the independent search server:

```sh
uv run adsbfi-proxy serve --host :: --port 8082
```

Open <http://localhost:8082/>. Configure `ADSBFI_PASSWORD` for HTTP Basic
authentication behind HTTPS when publishing the browser/API. Without it the
server is open to its network. The embedding application's existing
`AIRCRAFT_EMBEDDING_PASSWORD` is also accepted as a deployment fallback.

The browser queries independent airport movements. Unknown opposite endpoints do
not erase accepted observations. `/api/endpoints` is the canonical incremental
feed; the old paired-flight browser/API can only read a retained v1 database.

## Updating and mirroring

The downloader checks every **60 seconds**. Source files cover **30-minute**
intervals, so polling every minute does not imply minute-resolution publication.
It allows two minutes after a slot closes for publication and retries a missing
recent slot each minute. Only complete files enter flight inference.

ADS-B Exchange began requiring a browser Turnstile session for `globe_history`
in October 2026. The worker therefore tries `ADSBFI_BASE_URL` first and falls
back to the public, format-compatible `ADSBFI_FALLBACK_URL`
(`https://globe.adsb.fi/globe_history` by default) on 403/404 or a network
error. Set `ADSBFI_FALLBACK_URL=` to disable the mirror, or point it at another
readsb `globe_history` instance. `proxy-status.json` records
`last_download_source` as `adsbx` or `adsbfi`.

- `live` mode downloads new slots, builds endpoints in chronological order, and
  deletes raw files only after a successful durable checkpoint. It retries gaps
  after downtime, starting immediately after the last processed slot.
- `mirror` mode retains raw files and audits the full cache every six hours,
  including holes older than the flight checkpoint. The first audit discovers
  and persists the oldest cached slot. `ADSBFI_MIRROR_START` explicitly overrides
  the archive boundary. An empty cache starts at `ADSBFI_START` or the current slot.
- One downloader gives the latest completed file priority. Backfill is sequential,
  with at least **10 seconds between requests**, up to four ingestion slots and
  three archive repairs per polling cycle. There are no parallel upstream fetches.
- Retries persist in `mirror.sqlite`; 429 responses honor `Retry-After` globally,
  other errors back off, and historical 404s are retried daily. Missing/unavailable
  files remain visible in status and do not disappear from the mirror queue.
- Downloads go to a temporary file, are checked for complete length and a binary
  heatmap header, flushed, and atomically renamed. Cache audits check headers and
  record alignment. This detects obvious corruption, not arbitrary bit changes;
  the upstream does not provide checksums here.
- The builder runs independently, carries bounded active tracks
  across slots/restarts, and refuses to jump over a missing ingestion slot.
  Old archive repairs do not rewind rolling state or retroactively rebuild
  flights. Use a separate database for a deliberate historical rebuild.
- The cache and database each reserve 5 GiB free space by default.

`ADSBFI_POLL_SECONDS`, `ADSBFI_REQUEST_SECONDS`, and `ADSBFI_MIN_FREE_GB` override
these defaults. Keep the cache on a mounted large volume; production units should
check the mount in `ExecStartPre` with `Restart=on-failure`, so a late-mounted
disk is retried and never silently replaced with directories on root.
A complete archive needs several TB. Existing caches are reused without downloads.

`uv run adsbfi-proxy status` prints the current worker status, latest processed
slot, ingestion lag, mirror audit results, retry count, and errors. Systemd journal
logs record downloads and builds. Worker liveness and data freshness are distinct:
check `checked_at`, `last_poll_at`, `processed_through`, and `lag_seconds`.

The worker uses an OS lock per database and graceful shutdown waits for the current
builder. Flight rows commit before track-state checkpoints; interruption between
those commits safely replays through the unique endpoint episode index. A malformed
file cannot advance its checkpoint.

## Endpoint data and API

`events.sqlite` contains `endpoints`, indexed by airport/time and aircraft/time.
It stores only what readers and replay deduplication need:

| Field | Meaning |
| --- | --- |
| `endpoint_id` | Append ID; checkpoint this, not event time |
| `icao_id` | Aircraft (24-bit ICAO address) |
| `ts` | Supporting observation UTC seconds |
| `is_landing` | 1 for landing, 0 for takeoff |
| `airport_id` | OurAirports ID |
| `callsign_id` | Callsign flown, from `callsigns`; NULL when none was reported |

`callsigns(callsign_id, callsign)` is an append-only dictionary of trimmed
callsigns, so each endpoint stores a small integer rather than text.

The unique index `idx_endpoints_aircraft_time (icao_id, ts, is_landing)`
deduplicates rows replayed after an interrupted checkpoint and serves aircraft
lookups; `idx_endpoints_airport_time (airport_id, ts, icao_id, is_landing)`
covers airport queries. The inference
version and ledger schema (`compact-2`) are recorded once in `meta`. Evidence classes,
confidence, recovery intervals and supporting positions are inference internals
and are not persisted.

Join the reference databases on `icao_id` and `airport_id`. Reference ownership
and airport metadata describe the imported snapshot, not historical validity.
`meta` maintains endpoint count and time bounds incrementally.

- `GET /api/endpoints?after_id=0&limit=100&airport=KMIA`
- Optional `aircraft` (ICAO hex or exact registration), `callsign` (exact),
  `kind` (`takeoff` or `landing`), `start` inclusive, `end` exclusive (UTC).
  Callsigns are not indexed: combine `callsign` with `aircraft`, or with
  `airport` and a `start`/`end` range. Searching a busy airport's full history
  for a callsign reads every row there and can exceed the 15-second query limit.
- `GET /api/airports?q=Miami`
- `GET /api/status` and `/health`

Persist `next_after_id` after applying a page transactionally. Continue while
`has_more`, then poll again from that cursor. Keep filters unchanged. A page uses
an insertion-ID watermark; out-of-order event times are expected. Reset cursors
when switching to a rebuilt database. Limits are 1–5000; default 100.
The API returns movement +1 for landing, −1 for takeoff. Missing reception alone
does not establish a movement; short aircraft histories can corroborate it using
other observations. Recovered movements use an observation bound as their
time, which is not necessarily a runway crossing.

### Callsigns

Heatmaps interleave identification records with positions from June 2020
onward: `lat` is `(1 << 30) | squawk` (the octal digits read as decimal) and
the eight callsign bytes fill the `lon`, altitude and speed fields. readsb
writes one when the callsign or squawk changes, and at least once a minute.
The builder accepts A–Z, 0–9 and inner spaces after trailing padding is
removed. Blank, all-zero, single-character and corrupted (`@`, punctuation,
leading space) values are discarded. Squawks are not stored.

Each track keeps a bounded tally of callsign runs (up to eight, with report
counts and first/last times) in its checkpoint. Reports within 15 minutes
extend a run. A movement takes the callsign reported most while airborne
between this track's takeoff and landing, or through the track's end or from
its start when the other side is unknown. Runs are prorated by their overlap
with that interval, so a callsign left over from the previous flight at the
gate does not outweigh the new one. Reports within ten minutes outside the
interval are a fallback when reception stops at liftoff or starts at
touchdown. Recent flights and pending landings keep their callsign, so
arrivals inferred later from a following departure receive it.

Endpoints and inference are unchanged from the compact-1 ledger. In a two-day
2026 replay, 99.6% of endpoints carry a callsign. Consecutive airliner
takeoff/landing pairs between different airports within six hours share a
callsign 98.9% of the time; most of the rest are unobserved intermediate legs.
A callsign changed in flight can still disagree between a flight's two
endpoints.

The new builder does not produce paired flights or support old consumer schemas.
`/api/flights` returns 410 on an endpoint-only database. Historical v1 read helpers remain
available for comparison; they are not part of the v5 ingestion path.

## Services and checks

Templates are in `deploy/`. Deploy code and compiled binary under
`/opt/adsbfi-proxy`, copy the service units to systemd, configure paths under
`/etc/adsbfi-proxy/`, and enable `adsbfi-proxy-worker` and `adsbfi-proxy-web`.
The `adsbfi-proxy-aircraft-owners` timer refreshes the owner/operator columns
daily.
Use user services with the same commands on a workstation. Run only one ingestion
worker per database. Keep the previous release and configuration for rollback;
stop the worker before switching releases. No embedding jobs are included.
Consumers running as another user may open the databases with `mode=ro`; the
worker keeps an idle connection to `events.sqlite` so its `-wal`/`-shm` files
persist between builds, which a read-only sandbox cannot create itself.

```sh
export CARGO_TARGET_DIR=/tmp/adsbfi-proxy-checks
cargo test --manifest-path rust/flight-builder/Cargo.toml
cargo build --release --manifest-path rust/flight-builder/Cargo.toml
ADSBFI_TEST_BUILDER="$CARGO_TARGET_DIR/release/flight-builder" PYTHONPATH=src python3 -m unittest discover -s tests -v
```

Data sources include ADS-B Exchange, OurAirports, FAA, tar1090/Mictronics, and
optionally OpenSky. Source datasets are not bundled with the application.
Generated data, credentials, caches, and machine-specific settings stay untracked.
