# Endpoint v6 and v7

V6 is the first inference run on adsb.fi heatmaps. It keeps the v5 rules and
adds three things: movement times at liftoff and touchdown rather than at the
first or last received report, departures recovered from the climb-out, and
airport choices checked against the airport where the aircraft last landed.
V6 was developed against two kinds of truth that need no hand labelling:
scheduled airline flights from BTS, and the continuity of each aircraft between
a landing and its next takeoff.

V7 keeps every v6 rule and corrects the references it reads; see
[V7](#v7).

## Ground truth

**BTS On-Time Performance** reports every domestic flight of the major US
carriers with its tail number, origin, destination and wheels-off/wheels-on
minute. US registrations map to ICAO addresses algorithmically, so each
reported flight gives a labelled takeoff and landing. This follows Goode,
*Evaluating and Improving Flight Derivation from ADS-B Data* (2026), who used
the same data to score segmentation and airport choice on 2026-03-01.
`benchmarks/bts_eval.py` reports:

- endpoint recall, precision and airport accuracy, with a one-to-one time
  match within ±20 minutes;
- time bias, mean absolute error, and the share within one minute. BTS minutes
  are truncated, so the reference is the middle of the minute;
- flight precision, recall and F1, from consecutive takeoff→landing pairs,
  comparable to Goode's segmentation metrics.

Precision excludes unmatched legs with a non-US end, because BTS lists only
domestic flights. Charter and repositioning legs remain, so precision is a
lower bound.

**Continuity.** An aircraft takes off from the airport where it last landed.
When the next takeoff is from a different airport, either the aircraft was moved
by road, a whole flight went untracked, or one of the two endpoints names the
wrong airport. `benchmarks/continuity.py` scores every landing followed within
48 hours by a takeoff of the same aircraft. It covers all aircraft classes and
regions, which BTS does not. It reports mismatches by distance:

| Bin | Distance | Usual cause |
| --- | --- | --- |
| adjacent | < 5 km | seaplane base or heliport beside a runway |
| nearby | 5–30 km | wrong field in a cluster |
| remote | ≥ 30 km | untracked flight |

Continuity is partly circular once inference uses it. Every continuity rule
below was therefore also required to leave BTS airport accuracy unchanged.

## Timing

Each 30-minute file summary keeps the liftoff transition and the touchdown
transition:

- liftoff is the first airborne report after a ground report, or the first
  airborne report of a track that starts in the air;
- touchdown is the last airborne report before the first ground report that
  follows it.

The track state carries both transitions across files and restarts.

Against BTS, the first airborne report is the best takeoff time: 98.9% fall
within one minute of the BTS minute. The last airborne report before ground is
the best landing time. Ground-flag flicker affects fewer than 0.1% of
movements.

V5 timed a movement at its chosen supporting report. On a long gate or taxi
segment, that report was often minutes from the runway: takeoffs came out 1.7
minutes early and landings 3 minutes late.

When reception starts or ends above the runway, the time is extrapolated:

- **Takeoff** subtracts the climb time from 200 ft AGL to the first report, at
  2,500 ft/min (transport), 800 ft/min (other fixed-wing) or 1,000 ft/min
  (rotorcraft). The extrapolation is capped at ten minutes and never precedes
  the last ground report.
- **Landing** adds the larger of two estimates: the remaining distance beyond
  2 km at approach speed (250, 150 or 120 km/h), or the remaining height at the
  descent rate. The extrapolation is capped at 20 minutes and never follows the
  first ground report.

A transition is used only near the chosen endpoint. A liftoff is used up to 15
minutes before or 45 minutes after the candidate. A touchdown is used up to 45
minutes before or 15 minutes after it, and only after the maximum altitude.

Recovered movements (corroborated, gap and corrected pending arrivals) receive
the same timing. Their ledger interval remains the observation gap.

## Climb-out departures

adsb.fi often has no low-altitude reception at an airport, and Marseille is a
clear example. A departure can still be inferred when the first airborne
reports of a transport aircraft are a climb leaving a major airport. All of
these must hold:

- the aircraft gains at least 300 ft within 60–300 seconds;
- the track's maximum altitude is at least 1,500 ft above that first report;
- a large or medium airport lies within 40 km, at a climb gradient of
  150–800 ft/km from it, and no more than 10,000 ft below the first report;
- the aircraft is moving away from that airport.

The nearest qualifying airport is chosen. A previous unclosed flight must have
had time to reach it. These departures also feed the existing corroborated
arrival for the previous flight.

## Continuity rules

These rules came from the continuity metric. The first version of climb-out
departures reduced commercial-jet continuity from 99.94% to 99.74%. The
nearest major airport was often the wrong neighbour: Changi→Seletar,
Itami→Kobe, Key West→NAS Key West and Fiumicino→Ciampino.

- **Climb-out anchor.** When the previous landing's airport also explains the
  climb (within 40 km, at most 800 ft/km, moving away), it is chosen over a
  nearer major airport.
- **Airborne origin anchor.** V5 moved an origin to the previous landing airport
  only when the two airports were within 4 km. An origin observed only in the
  air now yields to the previous landing airport within 15 km, if the departure
  point is within 12 km of it.
- **Minor field next to a major airport.** The direction of a correction
  matters: it moves whichever endpoint is likelier to be wrong. If the previous
  landing is at a small airport and the departure is from a large or medium one,
  the takeoff is not moved. Instead, the pending small-airport landing is
  reassigned to the departure airport, provided its last reports were closing on
  it. Moving the takeoff instead would have copied four wrong BTS arrivals
  (Rucker beside Wichita, Hustons beside South Bend) onto correct takeoffs.
  Reassigning the landing fixed seven BTS arrivals and broke none.
- **Floatless landplanes.** Multi-engine and turbine landplanes no longer match
  seaplane bases. On a production ledger, seaplane-base endpoints of these
  types were overwhelmingly at bases within 5 km of a runway. For example, a
  Beech 1900 had 8,441 such endpoints and 63 at remote bases. A type stays
  float-capable if it often uses remote bases: DHC-6, Caravan, Kodiak, the
  turbine Beaver and Otter, Porter, PAC 750 and the Air Tractor Fire Boss.
  Single-engine pistons, amphibians and seaplane airframes keep water
  candidates, as v5 decided. This removes 1,137 of 1,138 landplane
  seaplane-base endpoints from a 10-day replay. At Vancouver's CAM9 water
  aerodrome, the Beech 1900 and King Air movements from the YVR south terminal
  move to CYVR.

## Results

These are adsb.fi replays into fresh databases with the same references. V5 is
the v5 builder on the same input.

| Days | Version | TO recall / precision / airport | LD recall / precision / airport | TO / LD within 1 min | TO / LD bias (min) | Flight P / R / F1 |
| --- | --- | --- | --- | --- | --- | --- |
| Mar 2–4 (development) | v5 | 91.5 / 92.7 / 99.98 | 94.9 / 95.9 / 99.96 | 78.0 / 40.5% | −1.69 / +2.94 | 89.3 / 87.8 / 88.5 |
| | v6 | 97.2 / 96.8 / 99.95 | 97.1 / 96.8 / 99.93 | 99.2 / 97.7% | +0.12 / +0.29 | 95.2 / 95.4 / 95.3 |
| Mar 1 (held out) | v5 | 91.0 / 93.8 / 99.98 | 95.1 / 97.3 / 99.96 | 79.1 / 42.4% | −1.54 / +2.78 | 90.9 / 87.4 / 89.2 |
| | v6 | 97.3 / 98.1 / 99.94 | 97.4 / 98.1 / 99.93 | 99.1 / 97.9% | +0.13 / +0.29 | 96.8 / 95.7 / 96.3 |
| Mar 5–6 (held out) | v5 | 90.7 / 91.8 / 99.97 | 94.6 / 95.4 / 99.97 | 79.4 / 36.5% | −1.68 / +3.28 | 88.3 / 86.8 / 87.5 |
| | v6 | 97.2 / 96.5 / 99.94 | 97.1 / 96.5 / 99.93 | 99.3 / 97.4% | +0.10 / +0.30 | 95.0 / 95.4 / 95.2 |
| Jun 9–16 (held out) | v5 | 87.6 / 90.2 / 99.99 | 90.8 / 94.1 / 99.99 | 78.5 / 40.7% | −1.67 / +2.94 | 86.2 / 83.0 / 84.6 |
| | v6 | 94.4 / 95.3 / 99.93 | 93.7 / 95.3 / 99.93 | 98.9 / 97.2% | +0.11 / +0.30 | 93.9 / 92.3 / 93.1 |

Continuity over all aircraft (landing→takeoff pairs within 48 hours):

| Replay | Version | Pairs | Same airport | Adjacent | Nearby | Remote |
| --- | --- | --- | --- | --- | --- | --- |
| Feb 26–Mar 7 | v5 | 854,881 | 99.17% | 0.18% | 0.51% | 0.14% |
| | v6 | 912,959 | 99.41% | 0.15% | 0.30% | 0.15% |
| Jun 8–16 | v5 | 875,799 | 99.16% | 0.18% | 0.52% | 0.14% |
| | v6 | 937,662 | 99.41% | 0.14% | 0.30% | 0.15% |

By class on the March replay, same-airport rates are:

| Class | Same airport |
| --- | --- |
| commercial jets | 99.94% |
| business jets | 99.92% |
| commercial turboprops | 99.75% |
| turboprops | 99.44% (v5 99.15%) |
| pistons | 98.57% (v5 97.48%) |
| helicopters | 93.60% |

Most remaining helicopter mismatches are at heliports beside an airport.

### Comparison with Goode (2026)

Goode's paper (OpenSky Symposium 2026) scores flights differently from the
tables above:

- gold flights are BTS flights that take off on the UTC day;
- a test flight matches when both of its times fall within 60 minutes;
- every unmatched test flight of a gold aircraft counts against precision,
  including international and unreported legs.

`bts_eval.py --goode` reproduces this protocol. Our UTC-day set has 17,907
flights on 4,715 aircraft, against his 15,951 flights on 4,719, so his exact
gold filter is not reproduced. On 2026-03-01:

| System | ADS-B data | Flight P / R / F1 | TO / LD airport | TO / LD time bias (min) |
| --- | --- | --- | --- | --- |
| Goode, readsb legs + neural network | OpenSky, 3 days of context | 92.26 / 96.87 / 94.50 | 97.00 / 95.51 | 1.01 / 0.64 |
| Goode, neural network | ADS-B Exchange, single day | 88.67 / 96.03 / 92.20 | 99.99 / 96.21 | |
| Goode, neural network | ADSB.lol, single day | 89.02 / 92.80 / 90.87 | 99.39 / 95.53 | |
| ADS-B Exchange published flights | ADS-B Exchange | 94.31 / 89.19 / 91.68 | 100.00 / 99.97 | 0.61 / 0.91 |
| v5 rules | adsb.fi | 89.13 / 91.47 / 90.29 | 99.99 / 99.98 | −2.66 / +2.88 |
| **v6** | adsb.fi | 91.68 / 95.49 / 93.55 | 99.96 / 99.94 | +0.13 / +0.29 |

Over Mar 1–6, v6 scores F1 92.6–93.9 under this protocol. Segmentation is on a
par with his best configuration. Airport accuracy and timing are much better,
and v6 uses explicit rules with no trained model.

His talk's main points are reflected here:

- nearest-airport matching is the main airport failure, which motivates the
  continuity and floatless rules;
- low ground coverage hides takeoffs and landings, which motivates the
  climb-out departures;
- BTS is a practical ground truth for the segments it covers.

V6's airport accuracy on BTS is 0.03 points below v5's. V6 adds about 6% more
movements, in harder places where low reception is missing. The remaining
errors are mostly consistent pairs at an airport and its neighbour:

- Gulfport and Keesler AFB;
- Montgomery and Maxwell AFB;
- Ketchikan and Annette Island;
- Kahului and Kapalua;
- McAllen and Reynosa.

In each case both the arrival and the departure name the neighbour, so
continuity cannot detect them.

## V7

V7 changes what the builder knows about airports and aircraft, not how it
segments tracks or times movements.

- **Runways** ([#1](https://github.com/kylemcdonald/adsbfi-proxy/issues/1)).
  `import-airports` also loads OurAirports runway centrelines. A fixed-wing
  report within 0.5 km of a land airport's runway is measured to that runway
  rather than to the airport's reference point. A large airport's reference
  point can be farther from its own ramp than a neighbouring seaplane base is.
  FedEx feeder Caravans parked at Logan's cargo ramp, 0.3 km from runway 14,
  were assigned to the Boston Harbor seaplane base 0.8 km away. Morningstar's
  Caravans beside the 26L threshold at Vancouver went to CAM9. Floatplanes on
  the Fraser River, 0.85 km from the runway, keep CAM9. Approach and climb-out
  reports farther out still use reference points: applying runway distance at
  any range moved airline movements at Fort Myers, Little Rock and Amarillo to
  a smaller neighbour's runway. Rotorcraft also keep reference points.
- **Airframes from the ICAO type**
  ([#2](https://github.com/kylemcdonald/adsbfi-proxy/issues/2)). When a record's
  ICAO type is in the designator table, its airframe descriptor comes from the
  table. A registry descriptor that disagrees is usually left over from an
  earlier aircraft on a reused registration: a King Air 350 carried a Grob
  trainer's L1P, a Challenger 300 and a G450 were classed as piston. A registry
  descriptor is kept when it only adds amphibian or float gear (A1T for L1T).
  783 descriptors changed.
- **Type names** ([#5](https://github.com/kylemcdonald/adsbfi-proxy/issues/5)).
  A record that registry text cannot classify, or that has no manufacturer or
  model, is classified from its designator's name, e.g. "CESSNA 700 Citation
  Longitude". Unknown jets fell from 4,806 to 1,804 and business jets rose from
  35,928 to 38,517. Rotorcraft descriptors are
  left alone: "UH-60 Black Hawk" would otherwise read as a Hawk trainer.
- **Corporate airliners**
  ([#4](https://github.com/kylemcdonald/adsbfi-proxy/issues/4)). BBJ and ACJ are
  recognised as suffixes ("A319-133ACJ") and inside parentheses
  ("737-7BC(BBJ)").
- **Airport codes and ids**
  ([#3](https://github.com/kylemcdonald/adsbfi-proxy/issues/3)). An OurAirports
  placeholder ident such as `US-0370` is replaced by the field's `gps_code` or
  `local_code` (MA87) when no other airport uses it. `airport_id` is now the
  OurAirports numeric id, so it stays fixed when a code changes. This changed
  6,991 codes.

An experiment that also treated every rotorcraft descriptor as rotorcraft in the
builder, including military helicopters, lowered continuity (99.394% against
99.424% on Jun 8–16) and was dropped.

On the evaluation replays, v7 matches v6 on BTS exactly: no airline movement
changed. Continuity rises from 99.410% to 99.413% (Feb 26–Mar 7) and from
99.412% to 99.415% (Jun 8–16). Caravan movements at seaplane bases fall from
517 to 75 and from 751 to 346.

## Reproduction

```sh
export ADSBFI_ARCHIVE_DIR=/path/to/globe_history
sh benchmarks/replay.sh data/eval/v6 2026-02-26 2026-03-08
uv run --extra evaluation python benchmarks/bts_eval.py --events data/eval/v6/events.sqlite \
    --airports data/db/airports.sqlite --bts data/bts/ontime_2026_3.zip \
    --ourairports data/ref/ourairports-airports.csv --date 2026-03-02..2026-03-04
python benchmarks/continuity.py --events data/eval/v6/events.sqlite \
    --airports data/db/airports.sqlite --aircraft data/db/aircraft.sqlite
```

A replay of ten days takes under a minute. Leave a margin of one day on both
sides of the scored BTS dates. Flights that end after the replay are missing,
which lowers recall on the last day.

V7 needs fresh event and state databases (`endpoints-v7`, state layout
`transitions-1`). Import airports with runways and aircraft with this version
first: the aircraft reference must include `icao_type` and `airframe`. An
airports database without a `runways` table still works, without the runway
rule.
