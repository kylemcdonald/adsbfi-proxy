# Endpoint v3

V3 retains independent movement rows and adds bounded aircraft continuity. It
requires fresh event and checkpoint databases; do not mix v2 checkpoints with v3.
The production worker and remote downstream consumer must be switched separately
from the workstation validation/backfill.

The frozen workstation binary is
`/home/kyle/.cache/adsbfi-proxy/endpoints-v3-full/flight-builder`. The existing
production worker still uses its previous binary. On this machine, use an
isolated `CARGO_TARGET_DIR` for development builds so a test build cannot replace
the executable that the production worker launches.

## Inference changes

- Ground departure attribution uses the earliest retained ground location;
  arrival attribution uses the latest. An observed arrival can resolve a later
  origin ambiguity between airport references within 4 km, for up to 24 hours.
  The two airport IDs remain distinct. This assumes no unseen intervening
  repositioning between those adjacent airports.
- Known jets and commercial turboprops may use a measured climb of at least
  300 ft over 30–480 seconds, within 5 km of a large airport and 3,000 ft above
  field elevation. The flight must still gain at least 1,500 ft overall.
- A previous supported arrival can corroborate a high-start climb within 20 km
  and 5,000 ft above that airport. A later departure can corroborate an earlier
  descending approach within 80 km and 10,000 ft above the destination. These
  movements carry the full interval between supporting observations.
- For known fixed-wing jet/turboprop categories, or a flight observed at or
  above 20,000 ft, a supported airport presence
  and a separate airborne observation can establish a necessary departure or
  arrival across a reception gap. These lower-confidence events are explicitly
  labeled `*_observation_gap`. The maximum gap is 24 hours, with a 1,200 km/h
  travel-speed ceiling plus 5 minutes for departure or 10 minutes for arrival.
  For arrival recovery, a distant observation must be at least 10,000 ft MSL;
  within 80 km, at least 1,500 ft above the field is also accepted. Within 20 km,
  at least 300 ft above field elevation is enough only when the flight has
  descended at least 1,500 ft from its observed maximum. This permits sparse
  final approaches at high airports without requiring two closely spaced
  reports. Other altitude/location checks remain in the builder.
  Unobserved intermediate stops are possible; the model does not reconstruct
  their route or fill an arbitrary missing event simply to balance a total.
- An airborne arrival assigned more than 3 km from a small airport is checked
  against observed motion. If the aircraft is moving away, up to two alternative
  nearby compatible references are considered. Ordinary large-airport turning
  approaches retain their existing treatment.
- A ground flag more than 3 km from a small airport is not enough to establish
  presence there. The same limit applies to airborne arrival matches for the
  known jet/commercial-turboprop categories: later airport observations can
  corroborate these approaches. Real raw tracks sometimes carry ground flags
  while still airborne. Recovery retains the latest measured airborne point
  when the final report has a ground flag or missing altitude.
- Decode heatmap altitude `-124` as unknown, separately from `-123` (ground).
  Previously the unknown sentinel became a false -3,100 ft altitude. This is a
  format correctness fix; it was not the cause of the audited August Bogotá
  endpoints. Both Rust parser paths and the Python readers now agree.

These are general rules, not airport/date-specific patches, reference aliases,
runway/approach databases, or fitted drift corrections. Helicopters do not use
continuity recovery. Unknown aircraft categories need an observed altitude of
at least 20,000 ft to use the broad gap rules. This avoids depending entirely on
incomplete type metadata without treating all unknown tracks as airliners.

## Timestamps and evidence

`ts`, coordinates and altitude identify the supporting observation, not an exact
runway crossing. New `time_start_ts` and `time_end_ts` fields describe observation
bounds on recovered movements. A recovered arrival uses the earlier bound as
`ts`; a recovered departure uses the later bound. A direct endpoint has equal
bounds: this means timing uncertainty was not modeled, not that touchdown was
observed to the second. Bounds do not account for incorrect identity or airport
assignment. Confidence is a heuristic, not a calibrated probability.

Evidence values: `ground`, `low_altitude`, `climb`, `origin_context`,
`arrival_corroborated`, `departure_corroborated`, `arrival_observation_gap`,
`departure_observation_gap`. The append-ID API supports all these filters and
returns both integer bounds and UTC strings. Recovered events can be appended
long after their representative timestamps; consumers must checkpoint IDs.

## Rolling resources and restart safety

Only active tracks and at most 48 hours of recent aircraft context are retained.
Context is looked up by ICAO primary key, never by scanning historical endpoints.
Expiry applies at lookup and during cleanup, so checkpoint frequency cannot
change inference. Old inactive context is removed and dirty rows are written in
ICAO order. No full-archive heatmaps are needed for subsequent updates.

The private v3 checkpoint format uses lossless, 33-byte little-endian observation
records (timestamp, latitude, longitude, altitude and two flags). This avoids
repeated float/JSON formatting for every active aircraft. Invalid records fail
instead of silently resetting state. The state connection uses an 8 MiB page
cache; event storage retains SQLite's ordinary small cache for rolling operation.
Larger caches used in the workstation backfill are reset before publication.

Events commit before checkpoints. A crash between the commits replays safely
through the unique aircraft/kind/episode key. A real four-day test compares
96-file batches with restarting after every half-hour file, including context
expiry; endpoints, active tracks, recent context and processed-file keys match.

## Validation artifacts

The local airport-activity repository contains the replay, masking, runtime and
restart-parity scripts. Artifacts live outside Git in
`/media/kyle/External-1/cache/airport-activity/endpoint-v3/`. Read the final report
there and the local viewer's endpoint-v3 explainer for the measured results.
The masking experiment compares against an unmasked detector reference; it is
not an independently labeled truth set. Residual drift and missing source files
remain visible. The full ledger and both airport identities are preserved for
before/after analysis.
