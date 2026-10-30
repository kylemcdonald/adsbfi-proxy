# Endpoint v4

V4 fixes aircraft classification collisions, approach ordering, and low helicopter
flights. Use fresh event and state databases and reset consumer cursors after a
rebuild. The immutable endpoint feed and bounded timing intervals are unchanged.
Develop with an isolated `CARGO_TARGET_DIR`; the running production worker still
uses its previous binary until explicitly switched.

Deploy the corrected frozen aircraft reference database with the new ledger and
checkpoint, or rebuild classifications from the original reference inputs using
`adsbfi-proxy import-aircraft` in a fresh database directory. Updating Python code
alone does not repair classifications already stored in SQLite. Keep the
validated reference snapshot when reproducing the published comparisons.

- Bare numeric piston patterns no longer match numbers inside ERJ 170, A340 or
  Saab 340 model names. The frozen reference correction repairs those collisions
  without discarding the other imported aircraft metadata.
- Airborne arrival selection prefers the latest eligible observation. Earlier
  proximity to a small field must not outweigh later approach evidence. Observed
  motion away from a small field also disqualifies it within the former 3 km
  exemption; ground evidence retains priority.
- A rotorcraft can establish a low flight with a 500 ft observed height change
  when its endpoint has a ground report, or lies within 3 km of the airport and
  between −500 and +300 ft of field elevation. Other endpoints retain the
  1,500 ft threshold.
- Rotorcraft departure selection keeps the earliest eligible airborne point.
  Departure candidates must precede the flight peak: a short tour landing inside
  the eight-minute origin window cannot replace its liftoff observation.
- An unresolved helicopter approach can be confirmed by a later observed
  departure from the same nearby airport: the last airborne point must be within
  6 km and −500 to +1,500 ft of field elevation, with measured inward travel over
  30–480 seconds and a 10-minute to 24-hour gap. This is local corroboration;
  helicopters remain excluded from broad missing-flight gap recovery.
- A fixed-wing airborne small-field arrival is held provisionally. At the next
  qualifying segment, a different departure airport within 12 km can resolve the
  destination only if the earlier track independently satisfies the existing
  descending-approach confirmation checks. A strong touchdown within 1 km and
  300 ft above the small field is preserved. Unresolved candidates retain their
  original airport and are emitted after 48 hours of observation time. Ground
  arrivals are never deferred by this rule. IDs are not merged or aliased.

Corrected provisional arrivals use `arrival_destination_context` and carry the
observation interval from the last approach point to the later departure.
Consumers still checkpoint `endpoint_id`, never event timestamps. Recent history
can gain backdated rows. Absence from the feed during the confirmation window
is not evidence of absence from an airport.

`pending_landings` is a bounded rolling checkpoint table. Its contents survive
restarts; events commit before state, and episode uniqueness makes crash replay
idempotent. No full-history queries or approach-path database are used by the
rolling detector. Airport references and aircraft metadata are snapshots, and
unseen intermediate trips can still make continuity inference wrong. Half-hour
summaries can collapse intermediate movements of very short tours even when
their net airport count balances. Drift and
observation-time uncertainty remain visible; no detrending is applied.
