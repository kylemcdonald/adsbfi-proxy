#!/bin/sh
# Replay a date range into a fresh ledger: replay.sh OUTPUT START END [BINARY]
# Reads heatmaps from $ADSBFI_CACHE_DIR, falling back to $ADSBFI_ARCHIVE_DIR.
set -eu
out=$1; start=$2; end=$3
binary=${4:-$(dirname "$0")/../rust/flight-builder/target/release/flight-builder}
db=${ADSBFI_DB_DIR:-$(dirname "$0")/../data/db}
mkdir -p "$out"
cp "$binary" "$out/flight-builder"
"$out/flight-builder" build-events \
  --cache-dir "${ADSBFI_CACHE_DIR:-$(dirname "$0")/../data/heatmaps}" \
  ${ADSBFI_ARCHIVE_DIR:+--archive-dir "$ADSBFI_ARCHIVE_DIR"} \
  --events-db "$out/events.sqlite" --state-db "$out/state.sqlite" \
  --airports-db "$db/airports.sqlite" --aircraft-db "$db/aircraft.sqlite" \
  --start "$start" --end "$end" --commit-every 48 --progress-every 0 > "$out/build.json"
