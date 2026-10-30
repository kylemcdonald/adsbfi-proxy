# Endpoint v2: implementation and validation results

Implemented in `adsbfi-proxy`. Full-history reingestion and the local airport viewer migration are complete: 479,174,282 independent endpoints through 2026-09-20 08:00 UTC, with all 100 viewer histories validated. Twelve unavailable half-hour source files (six hours) are disclosed. The production proxy worker/API and remote aircraft-embedding consumer have **not** been switched. The monthly shadow comparisons below are preserved; full-history results are in `../../airport-activity/DATA_QUALITY.md`.

## Accuracy diagnostics

Both variants read identical raw files and reference snapshots. Score the original top 100 airports on Jan 18–Feb 13, 2025 and Jul 22–Aug 17, 2026 (UTC), excluding the final hour. The four preceding days in each run were used during development and are excluded. No historical inventory offsets or fitted trends enter these metrics.

| Evaluation | Metric / 1,000 movements | Original | v2 | Change |
| --- | --- | ---: | ---: | ---: |
| holdout-winter | Absolute airport net | 10.25 | 6.99 | -31.8% |
| holdout-winter | Repeated aircraft imbalance | 11.29 | 5.49 | -51.3% |
| holdout-event | Absolute airport net | 11.57 | 7.25 | -37.3% |
| holdout-event | Repeated aircraft imbalance | 10.66 | 5.88 | -44.9% |

Absolute airport net is `sum(abs(landings − takeoffs)) / movements × 1000`. Repeated aircraft imbalance uses `sum(max(0, abs(net per aircraft/airport) − 1))` instead. They are diagnostics, **not measured precision/recall**. Real stock changes and asymmetric reception affect both. This is temporal validation plus inspected regressions, not a representative manually labeled holdout.

| Airport | Winter net/day original → v2 | Summer net/day original → v2 |
| --- | ---: | ---: |
| KMIA | -1.3 → +2.8 | -4.3 → -0.1 |
| MMMX | +67.5 → -11.2 | -106.2 → -4.3 |
| RPLL | -49.9 → -2.4 | +5.4 → +8.5 |
| WIII | +20.9 → +49.3 | +2.9 → +1.9 |
| LTFM | +25.1 → +12.7 | +169.7 → +270.6 |
| KTEB | +1.8 → +4.9 | -3.6 → +0.0 |

**Jakarta winter and Istanbul summer both regress substantially.** Do not treat these histories as reliable absolute counts. The independent ledger can expose one-sided observations previously hidden by pairing. Never infer the missing side merely to improve a drift score. Some other airports also regress; the interactive report exposes all 100.

## Performance

2026-09-18 15:00 UTC full-world heatmap, after six warmup files. Nine independent copied-checkpoint runs per binary; process startup, reference/state loading, parsing, inference and commits included. Bulk builders were stopped during the final comparison.

| Measurement | Original | v2 |
| --- | ---: | ---: |
| Median wall | 327.45 ms | 325.31 ms |
| Median CPU | 0.30 s | 0.28 s |
| Peak RSS | 122.69 MiB | 99.51 MiB |

Wall time is effectively unchanged; CPU falls about 7% and peak memory about 19%. This measures the development machine, not the low-resource deployment target. Internal inference/commit stages do more work, offset by less startup/state overhead. Month-long build timings were not controlled performance trials and should not be interpreted as a speedup.

Observed full-month builder times (not controlled trials): winter 201 → 246 seconds; summer 287 → 347 seconds. These backfills were about 21–22% slower even though the controlled one-file rolling update was unchanged.

Storage is the main resource tradeoff:

- holdout-winter: 0.50 → 1.17 GiB for the ledger. Independent endpoint rows and evidence cost more disk than paired rows.
- holdout-event: 0.66 → 1.52 GiB for the ledger. Independent endpoint rows and evidence cost more disk than paired rows.

## What changed

- Reject metadata/out-of-range coordinates in every Rust parser and the Python summarizer. Callsign bytes previously supplied fictitious altitude; the Calgary ground-only regression no longer produces endpoints.
- Store independently supported takeoffs and landings with episode uniqueness and source evidence; remove pending pairing and paired-row generation.
- Keep bounded terminal ground samples, prefer ground observations, and use aircraft-compatible airport candidates. Preserve water eligibility for unknown/general fixed-wing types and heliports for rotorcraft; small airports are not globally banned.
- Share spatial indexes, cap caches, index processed-file lookups, shrink parser over-allocation, preserve exact checkpoint coordinates and use exact-position cache keys.
- Add the insertion-ID endpoint feed/browser, fresh-schema/checkpoint guards, and offline resumable shadow backfill with reference snapshots and provenance.

## Tests and inspected evidence

- 7 Rust tests and 21 Python tests pass, including metadata decoding across six parsers, compatibility controls, checkpoint precision, cursor semantics, replay deduplication, input gaps, provenance, old-schema rejection, and live cleanup.
- Twelve complete heatmaps: continuous execution and restart after every file produce identical 84,292 endpoint records and 18,929 active track records. Endpoint insertion order can differ; movement identity and data match.
- Five original real-aircraft replays: Miami/Manila/Mexico City assignments corrected; the Jakarta takeoff discarded by pairing is preserved; the Istanbul high-start climb remains unresolved.
- Calgary example ICAO c1ea75: July 18 previously had four paired-flight rows; v2 has no endpoints that day. The parser regression directly tests the offending metadata record.
- Endpoint browser tested for paging, filtering, retired paired route, mobile layout and browser errors. Offline backfill and resume tested on this external disk with real files.
- July 19 Teterboro buildup from midnight: peak +68 → +71, and 21:30 UTC +48 → +49. This is relative movement, not known aircraft stock.

## Rejected complexity and limitations

A bounded trajectory extrapolator reduced Istanbul drift, but Jakarta winter changed from +1.4/day in the strict short-window ablation to +70.6/day with one direction sample and −47.3/day with two. Direction/coverage sensitivity was too high to ship. No runway/approach database or extrapolated endpoints are included.

Remaining limitations include high-start climbs, missing approach reception, short turns inside one heatmap, unknown/amphibious type ambiguity, historical reference changes, and heuristic confidence. The small-airport preference is not a reviewed site-identity map. There is no calibrated time interval or probability, and no guarantee that the historical minimum equals empty airport stock.

The implementation intentionally omits unproven geometry, canonical-site mergers, fitted drift correction and complex revision machinery. Rebuild into a new versioned ledger when inference changes. See [migration details](../ENDPOINT_V2.md).

## Reproduction

Experimental artifacts: `/media/kyle/External-1/cache/airport-activity/endpoint-v2`. Benchmark binary SHA-256: `c2f6efc69e3f1bb3e8a9a7fc4d297dea99965cd82cac7e503b0996dba823cd99`. Later source changes add regression tests only to the Rust builder; the release rebuild also passes full rolling parity.

```sh
python3 benchmarks/single_file.py --binary /path/to/binary --output /fresh/benchmark --repeats 9
python3 benchmarks/panel.py --long --binary /path/to/binary --output /fresh/panel
python3 benchmarks/score_panel.py --warmup-days 4 --report /path/to/scores.json /old/panel /new/panel
python3 benchmarks/rolling_parity.py --binary /path/to/binary --output /fresh/parity
python3 benchmarks/real_cases.py --binary /path/to/binary --output /fresh/cases
```

The separately stored full-archive backfill and local viewer publication are complete. Do not copy shadow databases over production. Switching the production worker and rebuilding the remote aircraft-embedding consumer remain deferred.
