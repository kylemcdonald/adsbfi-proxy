# Endpoint v5

V5 targets abrupt changes in apparent airport inventory caused by changes in reception or position quality. It preserves independent airport identities and records actual inferred movements; it does not detrend inventory or enforce balance.

The detector uses generic evidence rules:

- Physical airframe information survives operator classifications such as “military.” Physical jet/turboprop descriptors support transport-flight recovery. A generic landplane descriptor does not exclude water or ultralight references: float conversions retain those descriptors. The full-history regression review rejected that exclusion because it lost real floatplane departures.
- Weak fixed-wing departure matches at small airports or water bases must agree with the received climb direction. A climb of at least 100 feet over 30–120 seconds that moves more than 0.5 km toward that reference rejects the weak match above 300 feet AGL. Ground and low observations, missing field elevation and rotorcraft are left to the existing rules. This uses the retained summary, not runway geometry or historical lookups.
- The first airborne samples remain available after long ground segments. An unusable early position cannot block later geographically plausible departure evidence. Coordinates are never shifted to manufacture a match.
- Corroborated movements measure flight height relative to the known airport elevation. An arrival additionally needs observed descent and inward movement plus a later departure; a departure needs observed climb and a previous arrival.
- Weak airborne arrivals at medium or large airports receive the same delayed destination confirmation previously limited to small airports. A later independently observed departure can resolve a nearby reference mismatch. Airport records are not merged.
- Local confirmation and necessary-movement observation gaps share the existing 48-hour retained context. Gap movements are inferred with their full observation-time interval and lower confidence; the compact ledger stores only the chosen time. No older heatmaps are consulted at runtime.
- A fixed-wing aircraft changing altitude while its reported position remains effectively frozen cannot establish an airport location. A two-point witness can survive subsequent stationary ground reports. A later trusted departure can still establish a necessary intervening arrival, with an uncertain time. Rotorcraft are exempt from this fixed-wing position check.
- A low final fragment ending in a ground report within 3 km of an airport can close a previously unclosed airborne segment. This reuses the existing 48-hour gap, altitude and feasible-travel checks and requires no historical lookup. The `arrival_ground_context` evidence retains the interval between the earlier flight and the ground report; a parked fragment after a confirmed arrival cannot add another landing.

The ingestion state remains bounded: one active track, recent flight, and pending arrival per aircraft, with 48-hour expiry for contextual records. First-airborne samples and frozen-position witnesses are bounded samples, persisted through the existing exact-coordinate point representation. Committed endpoint rows always precede the checkpoint that covers them.

## References and performance

Re-import aircraft references for v5. `aircraft.airframe` preserves the physical description separately from the operator label. The importer writes `airframe_index` in the same transaction: version 1 is a sequence of little-endian unsigned 32-bit words, containing a 24-bit ICAO ID and an upper flag byte (bit 0: land airframe metadata, not an airport exclusion; bit 1: jet/turboprop land airframe). This small derived index avoids scanning the complete registry on every rolling update. Updating aircraft rows outside the importer requires rebuilding this index.

The frozen history investigation uses the previous reference snapshot plus physical descriptions from the existing local registry, accepted only when the registry’s ICAO type exactly matches the frozen record. Existing operator labels are unchanged. This avoids attributing gains to unrelated metadata refreshes.

Create fresh v5 event and state databases. Do not mix v4 checkpoints with v5 inference. Existing live services are not switched by a local rebuild. The workstation replay may use a larger SQLite cache; the single-file rolling benchmarks use the normal cache.

## Validation and reproduction

The sibling `airport-activity` workspace contains `analysis/v5_*.py` and `deploy/rebuild_v5.py`: change detection, all-airport metrics, frozen before/after replays, held-out reception masking, restart parity, and the resumable complete replay. Airport codes and dates occur only in research/test selection, never as detector exceptions.

The full replay saves a matched-cutoff comparison before catching up the rest of the frozen backlog. Publication requires reviewing the completed all-airport result. The interactive results are published by the `airport-activity` viewer.

Reduced drift is a diagnostic, not proof of complete or perfectly timed inventory. Long reception gaps, inaccurate airport references, unrecognized position faults, and incomplete low flights still leave residual drift. Uncertainty intervals describe observation gaps rather than calibrated confidence intervals.
