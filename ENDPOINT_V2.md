# Endpoint v2

The canonical output is an independent movement ledger. A confirmed takeoff is
retained even if no landing can be identified, and vice versa. No trend removal,
airport balancing, or fabricated opposite endpoint is applied.

## Changes and deliberate limits

1. **Correct input decoding.** Metadata rows use latitude 1073741824 and are not
   positions. Validate latitude/longitude ranges in every Rust parser and Python
   summarizer. Previously callsign bytes could become huge altitudes and make
   ground-only tracks look like flights.
2. **Independent, replay-safe movements.** `(icao_id, kind, episode_start_ts)` is
   unique. Commit endpoint rows before rolling checkpoint state. Pairing no longer
   controls whether evidence exists; the pending-takeoff mechanism is removed.
3. **Bounded ground evidence and compatible references.** Keep first/last points
   plus terminal ground samples, with the existing eight-minute endpoint windows.
   Prefer ground evidence. Known commercial/business jets and commercial
   turboprops exclude water, heliport and ultralight references. Other fixed-wing
   aircraft retain seaplane eligibility; rotorcraft retain heliports. Small fields
   remain candidates, with a modest 1.5 km score penalty beyond 250 m from their
   reference center. This is a preference, not a reviewed airport merger.
4. **Uncertainty remains explicit.** Preserve supported single endpoints; retain
   supporting coordinates, source slot, evidence, heuristic confidence and version.
   An absent opposite endpoint remains unknown. We do not retain every rejected
   sample or claim calibrated uncertainty intervals. High-start climbs still often
   fail the conservative altitude gate. Trajectory extrapolation was tested and
   rejected because its gains were unstable across airports.
5. **Separate reproducible validation and backfill.** Benchmark single-file rolling
   updates, compare month-long shadow databases, test restarts/replays, and rebuild
   offline into a separately named directory. Production and downstream consumers
   are switched only after reviewing the results.

No approach/runway geometry database, historical trajectory lookup, learned model,
or full-history scan is needed for live ingestion. Airport indexes are shared,
checkpoint coordinates are lossless and exact-position
nearest-airport caches are capped at 50,000 entries each, active track windows are
bounded, and processed-file membership uses indexed queries. Database storage grows
with accepted movements; working state does not grow with years of raw heatmaps.
Live mode can discard raw files after durable checkpointing. Endpoint publication
can lag observation until a segment closes; this is not a real-time runway sensor.

## Offline migration

Freeze reference imports during a backfill. Build the release binary to a separate
location; do not overwrite the active service binary. Point `ADSBFI_DB_DIR` at the
existing reference databases and `ADSBFI_CACHE_DIR` at the local archive:

```sh
PYTHONPATH=src python3 -m adsbfi_proxy.cli backfill \
  --builder /path/to/new/flight-builder \
  --output /path/to/shadow-endpoints-v2 \
  --start 2020-01-01 --end 2026-09-19 \
  --batch-files 24 --pause 0.1
```

Choose the actual first available half-hour, not the illustrative date above.
The wrapper performs no downloads, checks complete inputs before each batch,
locks the output and resumes from the last committed slot. It checkpoints every
file by default; `--commit-every 24 --batch-files 96` amortizes checkpoint writes
for an offline backfill. A restart safely replays at most one uncommitted batch.
Missing/corrupt input stops the batch before checkpoint advancement. It checks
free disk and refuses changed binary hashes/reference size+mtime provenance on
resume. The manifest is provenance, not a cryptographic audit of every raw file.
Reference databases are snapshotted once with SQLite backup (also works on disks
without symlink support); the source event ledger
is never opened. Use the wrapper or normal worker for gap-safe ingestion; the
low-level builder accepts sparse ranges for diagnostic replays.

After a complete shadow build and review: stop the old worker; preserve its binary,
configuration and database directory; switch all four database paths together;
start the new worker from its committed checkpoint. Do not copy an old state DB
into a new ledger. Rebuild consumers from endpoint ID zero, then poll by insertion
ID. Keep the previous release/data for rollback. **No downstream deployment is
part of this change.**

## Validation

See `benchmarks/RESULTS.md` for measurements and remaining errors. The scripts in
`benchmarks/` use local archive/reference paths and write only to their requested
output directory. They are research utilities, not worker dependencies. Scores
measure imbalance, not endpoint precision/recall; actual fleet changes and missing
reception can affect them. Inspected examples are development checks, not an
independently labeled accuracy benchmark.
