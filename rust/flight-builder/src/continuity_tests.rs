use super::*;

#[test]
fn altitude_sentinels_distinguish_unknown_from_ground_and_negative_altitude() {
    assert_eq!(decode_altitude(-124), (None, false));
    assert_eq!(decode_altitude(-123), (None, true));
    assert_eq!(decode_altitude(-120), (Some(-3000.0), false));
    assert_eq!(
        decode_altitude((2500 << 16) | ((-124i32) & 65535)),
        (None, false)
    );
}

#[test]
fn unknown_type_needs_high_altitude_evidence_for_broad_gap_recovery() {
    for (high, rotor, expected) in [(10000.0, false, 0), (22000.0, false, 1), (22000.0, true, 0)] {
        let f = Fixture::new(&format!("unknown-{high}-{rotor}"));
        let mut b = f.builder();
        let mut s = flight();
        s.icao_id = 43;
        s.is_rotorcraft = rotor;
        s.first_points = vec![point(10000, 1.0, 8000.0), point(10060, 1.1, 8500.0)];
        s.max_altitude_ft = Some(high);
        b.recent.insert(43, Some(prior()));
        b.finalise_segment(&s, None).unwrap();
        let n: i64 = b
            .events_conn
            .query_row(
                "SELECT COUNT(*) FROM endpoint_audit WHERE evidence='departure_observation_gap'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, expected);
    }
}

#[test]
fn missing_or_false_ground_altitude_does_not_replace_last_measured_airborne_support() {
    let f = Fixture::new("last-measured-altitude");
    let b = f.builder();
    let p = RecentFlight {
        episode: 0,
        end_ts: 2010,
        max_altitude: 30000.0,
        points: vec![
            point(2000, 1.0, 22000.0),
            TrackPoint {
                on_ground: true,
                alt_ft: None,
                ..point(2010, 0.99, 0.0)
            },
        ],
        landing: None,
        callsign: None,
    };
    assert_eq!(b.gap_arrival(&p, anchor(1, 10000)).unwrap().ts, 2000);
}

#[test]
fn sparse_low_approach_needs_later_airport_presence_and_local_airborne_height() {
    let f = Fixture::new("sparse-low-approach");
    let b = f.builder();
    let mut p = RecentFlight {
        episode: 0,
        end_ts: 2000,
        max_altitude: 30000.0,
        points: vec![point(2000, 0.08, 900.0)],
        landing: None,
        callsign: None,
    };
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_some());
    p.max_altitude = 1000.0;
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_none());
    p.max_altitude = 30000.0;
    p.points[0].lon = 0.5;
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_none());
    p.points[0] = point(2000, 0.08, 100.0);
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_none());
}

#[test]
fn distant_small_field_ground_flag_needs_more_than_a_nearby_reference() {
    let f = Fixture::new("ground-distance");
    let db = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    db.execute_batch("INSERT INTO airport_types VALUES(2,'small_airport'); UPDATE airports SET airport_type_id=2 WHERE airport_id=1;").unwrap();
    let mut b = f.builder();
    let far = TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(1000, -0.04, 0.0)
    };
    assert!(b.choose_endpoint(42, false, 1000, &[far], false).is_none());
    let near = TrackPoint { lon: -0.01, ..far };
    assert_eq!(
        b.choose_endpoint(42, false, 1000, &[near], false)
            .unwrap()
            .airport_id,
        1
    );
}

struct Fixture {
    dir: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let dir = env::temp_dir().join(format!("adsbfi-v3-{}-{name}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let airports = Connection::open(dir.join("airports.sqlite")).unwrap();
        airports.execute_batch("CREATE TABLE airport_types(airport_type_id INTEGER,label TEXT); INSERT INTO airport_types VALUES(1,'large_airport'); CREATE TABLE airports(airport_id INTEGER,lat_e7 INTEGER,lon_e7 INTEGER,elevation_ft INTEGER,airport_type_id INTEGER,name TEXT); INSERT INTO airports VALUES(1,0,0,0,1,'First'),(2,0,150000,0,1,'Separate'),(3,0,2000000,0,1,'Distant');").unwrap();
        let aircraft = Connection::open(dir.join("aircraft.sqlite")).unwrap();
        aircraft.execute_batch("CREATE TABLE aircraft_types(aircraft_type_id INTEGER,label TEXT); INSERT INTO aircraft_types VALUES(1,'commercial_jet'); CREATE TABLE aircraft(icao_id INTEGER,aircraft_type_id INTEGER,airframe TEXT); INSERT INTO aircraft VALUES(42,1,'L2J');").unwrap();
        Self { dir }
    }
    fn builder(&self) -> EventBuilder {
        let references = Connection::open(self.dir.join("aircraft.sqlite")).unwrap();
        references.execute_batch("CREATE TABLE IF NOT EXISTS airframe_index(version INTEGER PRIMARY KEY,payload BLOB); DELETE FROM airframe_index;").unwrap();
        let mut data = Vec::new();
        let mut q = references
            .prepare("SELECT icao_id,airframe FROM aircraft WHERE airframe GLOB 'L??'")
            .unwrap();
        for row in q
            .query_map([], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
        {
            let (id, frame) = row.unwrap();
            let flags = 1 | if frame.ends_with('J') || frame.ends_with('T') {
                2
            } else {
                0
            };
            data.extend_from_slice(&(id | flags << 24).to_le_bytes());
        }
        references
            .execute("INSERT INTO airframe_index VALUES(1,?1)", [data])
            .unwrap();

        EventBuilder::new(
            &self.dir.join("events.sqlite"),
            &self.dir.join("airports.sqlite"),
            &self.dir.join("aircraft.sqlite"),
            &self.dir.join("state.sqlite"),
            ParserMode::DirectStreamFx,
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.dir).unwrap();
    }
}
fn point(ts: i64, lon: f64, alt: f64) -> TrackPoint {
    TrackPoint {
        ts,
        lat: 0.0,
        lon,
        alt_ft: Some(alt),
        on_ground: false,
    }
}
fn anchor(airport_id: i64, ts: i64) -> EndpointCandidate {
    EndpointCandidate {
        ts,
        airport_id,
        airport_elevation_ft: Some(0.0),
        confidence: 0.9,
        point: TrackPoint {
            on_ground: true,
            alt_ft: None,
            ..point(ts, 0.0, 0.0)
        },
    }
}
fn prior() -> RecentFlight {
    RecentFlight {
        episode: 0,
        end_ts: 1000,
        max_altitude: 10000.0,
        points: vec![point(1000, 0.0, 0.0)],
        landing: Some(anchor(1, 1000)),
        callsign: None,
    }
}
fn flight() -> TrackState {
    TrackState {
        icao_id: 42,
        is_rotorcraft: false,
        segment_start_ts: 10000,
        segment_end_ts: 12000,
        last_seen_ts: 12000,
        first_points: vec![point(10000, 0.04, 4000.0), point(10060, 0.06, 4500.0)],
        last_points: vec![point(12000, 1.0, 10000.0)],
        point_count: 200,
        max_altitude_ft: Some(10000.0),
        max_altitude_ts: Some(12000),
        source_first_file: None,
        source_last_file: None,
        open_start: false,
        callsigns: Vec::new(),
    }
}
#[test]
fn supported_departure_rejects_stale_remote_and_nonclimbing_tracks() {
    let f = Fixture::new("departure");
    let b = f.builder();
    let p = prior();
    let s = flight();
    assert_eq!(b.corroborated_departure(&s, &p).unwrap().airport_id, 1);
    let mut x = s.clone();
    x.first_points.iter_mut().for_each(|q| q.lon += 1.0);
    assert!(b.corroborated_departure(&x, &p).is_none());
    let mut x = s.clone();
    x.first_points[1].alt_ft = Some(1800.0);
    assert!(b.corroborated_departure(&x, &p).is_none());
    let mut x = s.clone();
    x.open_start = true;
    assert!(b.corroborated_departure(&x, &p).is_none());
    let mut p = prior();
    p.end_ts = 10000 - 48 * 3600 - 1;
    assert!(b.corroborated_departure(&s, &p).is_none());
}
#[test]
fn approach_requires_later_local_departure_and_observed_descent() {
    let f = Fixture::new("arrival");
    let b = f.builder();
    let p = RecentFlight {
        episode: 0,
        end_ts: 1300,
        max_altitude: 20000.0,
        points: vec![point(1000, 0.35, 4500.0), point(1300, 0.25, 3000.0)],
        landing: None,
        callsign: None,
    };
    let d = anchor(1, 10000);
    assert_eq!(b.corroborated_arrival(&p, d).unwrap().ts, 1300);
    let mut x = p.clone();
    x.points[0].alt_ft = Some(2800.0);
    assert!(b.corroborated_arrival(&x, d).is_none());
    let mut x = p.clone();
    x.points[0].lon = 0.15;
    assert!(b.corroborated_arrival(&x, d).is_none());
    let mut x = p.clone();
    x.points.iter_mut().for_each(|q| q.lon += 1.0);
    assert!(b.corroborated_arrival(&x, d).is_none());
    assert!(
        b.corroborated_arrival(&p, anchor(1, 1300 + 172801))
            .is_none()
    );
    let mut x = p.clone();
    x.landing = Some(anchor(2, 1300));
    assert!(b.corroborated_arrival(&x, d).is_none());
}
#[test]
fn committed_events_survive_replay_from_old_context_checkpoint_without_duplicates() {
    let f = Fixture::new("replay");
    {
        let mut b = f.builder();
        b.recent.insert(42, Some(prior()));
        b.recent_dirty.insert(42);
        b.save_state().unwrap();
    }
    {
        let mut b = f.builder();
        b.finalise_segment(&flight(), None).unwrap();
        b.commit_events().unwrap(); /* crash before state commit */
    }
    {
        let mut b = f.builder();
        b.finalise_segment(&flight(), None).unwrap();
        b.save_state().unwrap();
        let r: (i64, i64, i64, i64) = b
            .events_conn
            .query_row(
                "SELECT COUNT(*),MIN(ts),MIN(time_start_ts),MAX(time_end_ts) FROM endpoint_audit",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(r, (1, 10000, 1000, 10000));
        assert_eq!(
            b.events_conn
                .query_row(
                    "SELECT value FROM meta WHERE key='endpoint_count'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "1"
        );
    }
    {
        let mut b = f.builder();
        assert_eq!(b.prior(42).unwrap().unwrap().episode, 10000);
        b.observed_through = 12000 + 48 * 3600 + 1;
        b.save_state().unwrap();
        assert!(b.prior(42).unwrap().is_none());
    }
}
#[test]
fn airport_context_keeps_both_airport_identities_and_leaves_distant_airports_alone() {
    for (origin, lon, expected) in [(1, 0.015, 1), (2, 0.015, 2), (1, 0.2, 3)] {
        let f = Fixture::new(&format!("identity-{origin}-{expected}"));
        let mut b = f.builder();
        let mut p = prior();
        p.landing = Some(anchor(origin, 1000));
        b.recent.insert(42, Some(p));
        let mut s = flight();
        s.first_points = vec![TrackPoint {
            on_ground: true,
            alt_ft: None,
            ..point(10000, lon, 0.0)
        }];
        b.finalise_segment(&s, None).unwrap();
        assert_eq!(
            b.events_conn
                .query_row(
                    "SELECT airport_id FROM endpoint_audit WHERE kind='takeoff'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            expected
        );
    }
}
#[test]
fn context_distance_wraps_the_dateline() {
    assert!(distance_to(&point(0, 179.99, 1000.0), 0.0, -179.99) < 3.0);
}
#[test]
fn checkpoint_blob_is_exact_and_rejects_truncation() {
    let p = TrackPoint {
        ts: 1700000001,
        lat: 52.04886245727539,
        lon: -1.125862956047058,
        alt_ft: Some(1275.0),
        on_ground: false,
    };
    let ground = TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..p
    };
    let bytes = points_blob(&[p, ground]);
    let decoded = parse_points_blob(&bytes).unwrap();
    assert_eq!(decoded[0].lat.to_bits(), p.lat.to_bits());
    assert_eq!(decoded[0].lon.to_bits(), p.lon.to_bits());
    assert_eq!(decoded[0].ts, p.ts);
    assert_eq!(decoded[1].alt_ft, None);
    assert!(decoded[1].on_ground);
    assert!(parse_points_blob(&bytes[..bytes.len() - 1]).is_err());
    let mut bad = bytes;
    bad[32] = 255;
    assert!(parse_points_blob(&bad).is_err());
    assert!(parse_points_blob(&points_blob(&[TrackPoint { lat: f64::NAN, ..p }])).is_err());
}
#[test]
fn observation_gaps_require_airborne_evidence_and_feasible_travel_time() {
    let f = Fixture::new("observation-gap");
    let b = f.builder();
    let mut p = prior();
    p.landing = None;
    p.points = vec![point(1000, 10.0, 30000.0)];
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_some());
    assert!(b.gap_arrival(&p, anchor(1, 1100)).is_none());
    assert!(b.gap_arrival(&p, anchor(1, 180000)).is_none());
    p.points[0].on_ground = true;
    assert!(b.gap_arrival(&p, anchor(1, 10000)).is_none());
    let mut s = flight();
    s.first_points = vec![point(10000, 10.0, 30000.0)];
    assert!(b.gap_departure(&s, &prior()).is_some());
    s.first_points[0].lon = 100.0;
    assert!(b.gap_departure(&s, &prior()).is_none());
    s.first_points[0] = point(10000, 0.04, 500.0);
    assert!(b.gap_departure(&s, &prior()).is_none());
}
#[test]
fn higher_climb_detection_is_limited_to_observed_local_climbs_and_known_aircraft() {
    let f = Fixture::new("observed-climb");
    let mut b = f.builder();
    let mut s = flight();
    s.first_points = vec![point(10000, 0.04, 2000.0), point(10060, 0.06, 2500.0)];
    assert!(b.observed_departure(&s).is_some());
    s.icao_id = 43;
    assert!(b.observed_departure(&s).is_none());
    s.icao_id = 42;
    s.first_points[1].alt_ft = Some(1800.0);
    assert!(b.observed_departure(&s).is_none());
    s.first_points[1].alt_ft = Some(2500.0);
    s.first_points[0].lon = 0.09;
    assert!(b.observed_departure(&s).is_none());
}
#[test]
fn distant_small_airfield_is_not_an_arrival_when_aircraft_is_moving_away() {
    let f = Fixture::new("approach-direction");
    {
        let c = Connection::open(f.dir.join("airports.sqlite")).unwrap();
        c.execute_batch("INSERT INTO airport_types VALUES(2,'small_airport'); INSERT INTO airports VALUES(4,0,-1300000,0,2,'Small field');").unwrap();
    }
    let mut b = f.builder();
    let points = vec![point(1000, -0.10, 1400.0), point(1100, -0.08, 1000.0)];
    assert_eq!(
        b.choose_endpoint(42, false, 1100, &points, false)
            .unwrap()
            .airport_id,
        1
    );
    let points = vec![point(1000, -0.19, 1400.0), point(1100, -0.15, 1000.0)];
    assert_eq!(
        b.choose_endpoint(42, false, 1100, &points, false)
            .unwrap()
            .airport_id,
        4
    );
}

#[test]
fn context_expiry_does_not_depend_on_checkpoint_frequency() {
    let f = Fixture::new("expiry");
    let mut b = f.builder();
    b.recent.insert(42, Some(prior()));
    b.recent_dirty.insert(42);
    b.observed_through = 1000 + 48 * 3600;
    assert!(b.prior(42).unwrap().is_some());
    b.observed_through += 1;
    assert!(b.prior(42).unwrap().is_none());
    b.save_state().unwrap();
    assert_eq!(
        b.state_conn
            .query_row("SELECT COUNT(*) FROM recent_flights", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn latest_approach_point_outweighs_an_earlier_nearby_small_field() {
    let f = Fixture::new("latest-approach");
    let db = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    db.execute_batch("INSERT INTO airport_types VALUES(2,'small_airport'); UPDATE airports SET airport_type_id=2 WHERE airport_id=1; DELETE FROM airports WHERE airport_id=2; UPDATE airports SET lon_e7=1000000 WHERE airport_id=3;").unwrap();
    let mut b = f.builder();
    let e = b
        .choose_endpoint(
            42,
            false,
            12000,
            &[point(11940, 0.0, 900.0), point(12000, 0.07, 500.0)],
            false,
        )
        .unwrap();
    assert_eq!((e.airport_id, e.ts), (3, 12000));
}

#[test]
fn low_rotorcraft_flight_requires_ground_or_close_low_endpoint() {
    let f = Fixture::new("rotor-height");
    let mut b = f.builder();
    let mut s = flight();
    s.is_rotorcraft = true;
    s.max_altitude_ft = Some(800.0);
    s.max_altitude_ts = Some(11000);
    s.last_points = vec![TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(12000, 0.0, 0.0)
    }];
    assert!(b.infer_landing(&s).is_some());
    s.is_rotorcraft = false;
    assert!(b.infer_landing(&s).is_none());
    s.is_rotorcraft = true;
    s.last_points = vec![point(12000, 0.0, 500.0)];
    assert!(b.infer_landing(&s).is_none());
    s.last_points = vec![point(12000, 0.0, 200.0)];
    assert!(b.infer_landing(&s).is_some());
}

#[test]
fn rotor_recovery_rejects_remote_high_stale_and_outbound_observations() {
    let f = Fixture::new("rotor-context");
    let b = f.builder();
    let mut p = prior();
    p.landing = None;
    p.end_ts = 1200;
    p.points = vec![point(1140, 0.07, 1100.0), point(1200, 0.04, 1100.0)];
    let d = anchor(1, 4000);
    assert!(b.rotor_arrival(&p, d).is_some());
    let mut x = p.clone();
    x.points[1].alt_ft = Some(2500.0);
    assert!(b.rotor_arrival(&x, d).is_none());
    let mut x = p.clone();
    x.points[1].lon = 0.1;
    assert!(b.rotor_arrival(&x, d).is_none());
    let mut x = p.clone();
    x.points.reverse();
    x.points[0].ts = 1140;
    x.points[1].ts = 1200;
    assert!(b.rotor_arrival(&x, d).is_none());
    assert!(b.rotor_arrival(&p, anchor(1, 1200 + 86401)).is_none());
}

#[test]
fn tentative_small_airport_landing_survives_restart_and_uses_supported_destination() {
    let f = Fixture::new("pending-restart");
    let db = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    db.execute_batch("INSERT INTO airport_types VALUES(2,'small_airport'); UPDATE airports SET airport_type_id=2 WHERE airport_id=1; DELETE FROM airports WHERE airport_id=2; UPDATE airports SET lon_e7=1000000 WHERE airport_id=3;").unwrap();
    let mut approach = flight();
    approach.max_altitude_ts = Some(11000);
    approach.last_points = vec![point(11940, -0.03, 1600.0), point(12000, 0.005, 900.0)];
    let landing = EndpointCandidate {
        point: approach.last_points[1],
        ts: 12000,
        ..anchor(1, 12000)
    };
    {
        let mut b = f.builder();
        b.finalise_segment(&approach, Some(landing)).unwrap();
        b.save_state().unwrap();
        assert_eq!(b.pending_landings.len(), 1);
        assert_eq!(
            b.events_conn
                .query_row(
                    "SELECT COUNT(*) FROM endpoint_audit WHERE kind='landing'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
    {
        let mut b = f.builder();
        assert_eq!(b.pending_landings.len(), 1);
        let mut s = flight();
        s.segment_start_ts = 16000;
        s.segment_end_ts = 18000;
        s.max_altitude_ft = Some(20000.0);
        s.max_altitude_ts = Some(18000);
        s.first_points = vec![
            TrackPoint {
                on_ground: true,
                alt_ft: None,
                ..point(16000, 0.1, 0.0)
            },
            point(16060, 0.11, 1000.0),
        ];
        s.last_points = vec![point(18000, 1.0, 20000.0)];
        b.finalise_segment(&s, None).unwrap();
        b.save_state().unwrap();
        let row:(i64,i64,i64,String)=b.events_conn.query_row("SELECT airport_id,time_start_ts,time_end_ts,evidence FROM endpoint_audit WHERE kind='landing'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
        assert_eq!(row, (3, 12000, 16000, "arrival_destination_context".into()));
        assert!(b.pending_landings.is_empty());
    }
}

#[test]
fn pending_landing_expires_without_inventing_a_different_airport() {
    let f = Fixture::new("pending-expiry");
    let mut b = f.builder();
    b.pending_landings.insert(
        42,
        PendingLanding {
            episode: 0,
            endpoint: anchor(1, 1200),
            callsign: None,
        },
    );
    b.flush_stale(1200 + 48 * 3600).unwrap();
    assert_eq!(b.pending_landings.len(), 1);
    b.flush_stale(1200 + 48 * 3600 + 1).unwrap();
    assert!(b.pending_landings.is_empty());
    assert_eq!(
        b.events_conn
            .query_row("SELECT airport_id FROM endpoints", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn strong_small_field_touchdown_is_not_reassigned_to_later_departure_airport() {
    let f = Fixture::new("strong-small-touchdown");
    let mut b = f.builder();
    let old = EndpointCandidate {
        point: point(1200, 0.0, 100.0),
        ts: 1200,
        ..anchor(1, 1200)
    };
    let p = RecentFlight {
        episode: 0,
        end_ts: 1200,
        max_altitude: 10000.0,
        points: vec![point(1140, -0.03, 1600.0), old.point],
        landing: Some(old),
        callsign: None,
    };
    b.recent.insert(42, Some(p));
    b.pending_landings.insert(
        42,
        PendingLanding {
            episode: 0,
            endpoint: old,
            callsign: None,
        },
    );
    let mut s = flight();
    s.first_points = vec![TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(10000, 0.015, 0.0)
    }];
    b.finalise_segment(&s, None).unwrap();
    let row: (i64, String) = b
        .events_conn
        .query_row(
            "SELECT airport_id,evidence FROM endpoint_audit WHERE kind='landing'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(row, (1, "low_altitude".into()));
}

#[test]
fn pending_resolution_replay_after_event_commit_is_idempotent() {
    let f = Fixture::new("pending-crash");
    {
        let mut b = f.builder();
        b.pending_landings.insert(
            42,
            PendingLanding {
                episode: 0,
                endpoint: anchor(1, 1200),
                callsign: None,
            },
        );
        b.pending_dirty.insert(42);
        b.save_state().unwrap();
    }
    {
        let mut b = f.builder();
        b.flush_stale(1200 + 48 * 3600 + 1).unwrap();
        b.commit_events().unwrap();
    }
    {
        let mut b = f.builder();
        b.flush_stale(1200 + 48 * 3600 + 1).unwrap();
        b.save_state().unwrap();
        assert_eq!(
            b.events_conn
                .query_row("SELECT COUNT(*) FROM endpoints", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert!(b.pending_landings.is_empty());
    }
}

#[test]
fn short_rotor_flight_keeps_liftoff_before_its_later_ground_report() {
    let f = Fixture::new("rotor-liftoff");
    let mut b = f.builder();
    let mut s = flight();
    s.is_rotorcraft = true;
    s.first_points = vec![point(10000, -0.015, 100.0), point(10060, -0.002, 350.0)];
    s.max_altitude_ft = Some(900.0);
    s.max_altitude_ts = Some(10200);
    assert_eq!(b.infer_takeoff(&s).unwrap().ts, 10000);
    // A tour landing within the eight-minute origin window is not a takeoff.
    s.first_points.push(TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(10400, 0.0, 0.0)
    });
    assert_eq!(b.infer_takeoff(&s).unwrap().ts, 10000);
    // A ground observation before the climb still takes priority.
    s.first_points.push(TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(10030, 0.0, 0.0)
    });
    assert_eq!(b.infer_takeoff(&s).unwrap().ts, 10030);
    s.is_rotorcraft = false;
    assert!(b.infer_takeoff(&s).is_none());
}

#[test]
fn v5_preserves_first_airborne_observations_after_long_ground_segment() {
    let mut raw = SummaryBuilder::new(10000, 0.0, 180.0, None, true);
    raw.add(10800, 0.0, 0.02, Some(1800.0), false);
    raw.add(10860, 0.0, 0.03, Some(2200.0), false);
    let summary = raw.finish(42);
    let mut state = flight();
    state.segment_start_ts = 10000;
    state.first_points.clear();
    state.last_points.clear();
    state.add_summary(&summary, Path::new("sample"), 480);
    assert!(state.first_points.iter().any(|p| p.ts == 10800));
    assert!(state.first_points.iter().any(|p| p.ts == 10860));
    let f = Fixture::new("v5-first-airborne");
    let b = f.builder();
    state.max_altitude_ft = Some(2400.0);
    assert_eq!(
        b.corroborated_departure(&state, &prior())
            .unwrap()
            .airport_id,
        1
    );
}

#[test]
fn v5_bad_early_position_does_not_hide_credible_local_climb() {
    let f = Fixture::new("v5-invalid-origin");
    let b = f.builder();
    let mut s = flight();
    s.first_points = vec![
        point(10000, 180.0, 25.0),
        point(10060, 0.02, 1700.0),
        point(10120, 0.04, 2300.0),
    ];
    s.max_altitude_ft = Some(2400.0);
    let e = b.corroborated_departure(&s, &prior()).unwrap();
    assert_eq!(e.ts, 10060);
    s.first_points = vec![point(10000, 180.0, 3000.0), point(10060, 0.08, 6000.0)];
    let e = b.gap_departure(&s, &prior()).unwrap();
    assert_eq!(e.ts, 10060);
}

#[test]
fn v5_physical_airframe_and_operator_class_are_independent() {
    let f = Fixture::new("v5-physical");
    let c = Connection::open(f.dir.join("aircraft.sqlite")).unwrap();
    c.execute_batch("INSERT INTO aircraft_types VALUES(2,'military'),(3,'piston'); INSERT INTO aircraft VALUES(43,2,'L2T'),(44,3,'L1P'),(45,3,'A1P');").unwrap();
    let mut b = f.builder();
    assert!(b.transport_aircraft_ids.contains(&43));
    assert_eq!(b.airport_cache_for(false, 44).profile, 1);
    assert_eq!(b.airport_cache_for(false, 45).profile, 1);
    assert!(!b.transport_aircraft_ids.contains(&44));
}

#[test]
fn v5_weak_arrival_at_medium_airport_can_be_confirmed_elsewhere() {
    let f = Fixture::new("v5-medium-arrival");
    let mut b = f.builder();
    let mut arrival = flight();
    arrival.segment_start_ts = 10000;
    arrival.segment_end_ts = 12000;
    arrival.last_seen_ts = 12000;
    arrival.first_points = vec![point(10000, 1.0, 15000.0)];
    arrival.last_points = vec![point(11940, 0.04, 1500.0), point(12000, 0.025, 800.0)];
    arrival.max_altitude_ft = Some(20000.0);
    arrival.max_altitude_ts = Some(11000);
    b.finalise_segment(&arrival, None).unwrap();
    assert!(b.pending_landings.contains_key(&42));
    let mut departure = flight();
    departure.segment_start_ts = 16000;
    departure.segment_end_ts = 18000;
    departure.first_points = vec![
        TrackPoint {
            on_ground: true,
            alt_ft: None,
            ..point(16000, 0.0, 0.0)
        },
        point(16060, 0.02, 2000.0),
    ];
    departure.max_altitude_ts = Some(17000);
    departure.last_points = vec![point(18000, 2.0, 20000.0)];
    b.finalise_segment(&departure, None).unwrap();
    let (airport,evidence):(i64,String)=b.events_conn.query_row("SELECT airport_id,evidence FROM endpoint_audit WHERE kind='landing' AND episode_start_ts=10000",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
    assert_eq!(airport, 1);
    assert_eq!(evidence, "arrival_destination_context");
}

#[test]
fn v5_frozen_vertical_position_is_not_airport_evidence() {
    let points = vec![point(10000, 0.0, 3000.0), point(10060, 0.00001, 1000.0)];
    assert!(EventBuilder::frozen_position(&points));
    let f = Fixture::new("v5-frozen");
    let mut b = f.builder();
    assert!(
        b.choose_endpoint(42, false, 10060, &points, false)
            .is_none()
    );
    let moving = vec![point(10000, 0.05, 3000.0), point(10060, 0.0, 1000.0)];
    assert!(!EventBuilder::frozen_position(&moving));
    assert!(
        b.choose_endpoint(42, false, 10060, &moving, false)
            .is_some()
    );
    // A hovering rotorcraft does not share the fixed-wing exclusion.
    assert!(b.choose_endpoint(42, true, 10060, &points, false).is_some());
}

#[test]
fn v5_frozen_witness_survives_stationary_ground_reports_but_clears_on_movement() {
    let mut raw = SummaryBuilder::new(10000, 0.0, 0.0, Some(4000.0), false);
    raw.add(10060, 0.0, 0.00001, Some(1000.0), false);
    for ts in (10120..11200).step_by(60) {
        raw.add(ts, 0.0, 0.00001, None, true);
    }
    let summary = raw.finish(42);
    assert!(summary.frozen_pair.is_some());
    let mut s = flight();
    s.segment_start_ts = 10000;
    s.first_points.clear();
    s.last_points.clear();
    s.add_summary(&summary, Path::new("sample"), 480);
    assert!(EventBuilder::frozen_position(&s.last_points));
    assert!(s.last_points.len() <= 5);
    let next = SummaryBuilder::new(11800, 0.0, 0.1, Some(1000.0), false).finish(42);
    s.add_summary(&next, Path::new("next"), 480);
    assert!(!EventBuilder::frozen_position(&s.last_points));
}

#[test]
fn v5_gap_recovery_is_bounded_and_preserves_separate_airports() {
    let f = Fixture::new("v5-bound");
    let b = f.builder();
    let mut s = flight();
    let p = prior();
    s.segment_start_ts = 1000 + 36 * 3600;
    s.first_points = vec![point(s.segment_start_ts, 1.0, 20000.0)];
    assert_eq!(b.gap_departure(&s, &p).unwrap().airport_id, 1);
    s.segment_start_ts = 1000 + 49 * 3600;
    s.first_points[0].ts = s.segment_start_ts;
    assert!(b.gap_departure(&s, &p).is_none());
}

#[test]
fn v5_float_conversions_retain_water_arrivals_and_airborne_departures() {
    let f = Fixture::new("v5-float-conversion");
    let airports = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    airports.execute_batch("INSERT INTO airport_types VALUES(4,'seaplane_base'); INSERT INTO airports VALUES(4,0,600000,0,4,'Water');").unwrap();
    let aircraft = Connection::open(f.dir.join("aircraft.sqlite")).unwrap();
    aircraft.execute_batch("INSERT INTO aircraft_types VALUES(2,'piston'); INSERT INTO aircraft VALUES(43,2,'L1P');").unwrap();
    let mut b = f.builder();
    let ground = TrackPoint {
        on_ground: true,
        alt_ft: None,
        ..point(10000, 0.06, 0.0)
    };
    assert_eq!(
        b.choose_endpoint(43, false, 10000, &[ground], false)
            .unwrap()
            .airport_id,
        4
    );
    assert_eq!(
        b.choose_endpoint(43, false, 10000, &[point(10000, 0.06, 150.0)], false)
            .unwrap()
            .airport_id,
        4
    );
    // Landplane descriptors also describe float-converted aircraft. A missing
    // ground report must not exclude their airborne departures from water.
    for (takeoff, lon, height) in [(false, 0.06, 1200.0), (true, 0.072, 500.0)] {
        assert_eq!(
            b.choose_endpoint(43, false, 10000, &[point(10000, lon, height)], takeoff)
                .unwrap()
                .airport_id,
            4
        );
    }
}

#[test]
fn v5_low_final_fragment_can_close_a_prior_airborne_gap_with_ground_presence() {
    let f = Fixture::new("v5-ground-gap");
    let mut b = f.builder();
    let mut s = flight();
    s.first_points = vec![point(10000, 0.04, 800.0), point(10060, 0.01, 100.0)];
    s.last_points = vec![anchor(1, 12000).point];
    s.max_altitude_ft = Some(800.0);
    s.max_altitude_ts = Some(10000);
    let p = RecentFlight {
        episode: 7000,
        end_ts: 8000,
        max_altitude: 20000.0,
        points: vec![point(8000, 1.0, 15000.0)],
        landing: None,
        callsign: None,
    };
    assert!(b.infer_landing(&s).is_none());
    b.recent.insert(42, Some(p));
    b.finalise_segment(&s, None).unwrap();
    let (airport,start,end,evidence):(i64,i64,i64,String)=b.events_conn.query_row("SELECT airport_id,time_start_ts,time_end_ts,evidence FROM endpoint_audit WHERE kind='landing'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).unwrap();
    assert_eq!((airport, start, end), (1, 8000, 12000));
    assert_eq!(evidence, "arrival_ground_context");
    s.segment_start_ts = 13000;
    s.segment_end_ts = 14000;
    s.first_points = vec![anchor(1, 13000).point];
    s.last_points = vec![anchor(1, 14000).point];
    s.max_altitude_ft = Some(100.0);
    s.max_altitude_ts = Some(13000);
    b.finalise_segment(&s, None).unwrap();
    assert_eq!(
        b.events_conn
            .query_row("SELECT COUNT(*) FROM endpoints", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn v5_ground_gap_requires_close_ground_and_unclosed_feasible_recent_flight() {
    let f = Fixture::new("v5-ground-gap-rejects");
    let mut b = f.builder();
    let mut s = flight();
    s.max_altitude_ft = Some(800.0);
    s.last_points = vec![anchor(1, 12000).point];
    let p = RecentFlight {
        episode: 7000,
        end_ts: 8000,
        max_altitude: 20000.0,
        points: vec![point(8000, 1.0, 15000.0)],
        landing: None,
        callsign: None,
    };
    assert!(b.grounded_arrival(&s, &p).is_some());
    let mut bad = p.clone();
    bad.landing = Some(anchor(1, 8000));
    assert!(b.grounded_arrival(&s, &bad).is_none());
    bad = p.clone();
    bad.end_ts = 12000 - 49 * 3600;
    bad.points[0].ts = bad.end_ts;
    assert!(b.grounded_arrival(&s, &bad).is_none());
    bad = p.clone();
    bad.points[0].lon = 150.0;
    assert!(b.grounded_arrival(&s, &bad).is_none());
    bad = p.clone();
    bad.points[0].alt_ft = Some(200.0);
    assert!(b.grounded_arrival(&s, &bad).is_none());
    s.last_points[0].on_ground = false;
    s.last_points[0].alt_ft = Some(100.0);
    assert!(b.grounded_arrival(&s, &p).is_none());
    s.last_points[0] = anchor(1, 12000).point;
    s.last_points[0].lon = 0.1;
    assert!(b.grounded_arrival(&s, &p).is_none());
    s.last_points[0] = anchor(1, 12000).point;
    s.is_rotorcraft = true;
    assert!(b.grounded_arrival(&s, &p).is_none());
}

#[test]
fn v5_weak_climb_toward_water_does_not_select_it_as_the_origin() {
    let f = Fixture::new("v5-outbound-direction");
    let airports = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    airports.execute_batch("DELETE FROM airports WHERE airport_id IN (2,3); INSERT INTO airport_types VALUES(4,'seaplane_base'); INSERT INTO airports VALUES(4,0,600000,0,4,'Water');").unwrap();
    let aircraft = Connection::open(f.dir.join("aircraft.sqlite")).unwrap();
    aircraft.execute_batch("INSERT INTO aircraft_types VALUES(2,'piston'); INSERT INTO aircraft VALUES(43,2,'L1P');").unwrap();
    let mut b = f.builder();
    let points = [
        point(10000, 0.035, 1200.0),
        point(10060, 0.05, 1500.0),
        point(10120, 0.065, 1800.0),
    ];
    assert_eq!(
        b.choose_endpoint(43, false, 10120, &points, true)
            .unwrap()
            .airport_id,
        1
    );
    // A real floatplane climbing away from the water reference remains there.
    let points = [
        point(10000, 0.075, 1200.0),
        point(10060, 0.09, 1500.0),
        point(10120, 0.105, 1800.0),
    ];
    assert_eq!(
        b.choose_endpoint(43, false, 10120, &points, true)
            .unwrap()
            .airport_id,
        4
    );
    // This fixed-wing direction check must not change hover/rotorcraft behavior.
    let points = [
        point(10000, 0.035, 1200.0),
        point(10060, 0.05, 1500.0),
        point(10120, 0.065, 1800.0),
    ];
    assert_eq!(
        b.choose_endpoint(43, true, 10120, &points, true)
            .unwrap()
            .airport_id,
        4
    );
    let points = [
        TrackPoint {
            on_ground: true,
            alt_ft: None,
            ..point(10000, 0.06, 0.0)
        },
        point(10060, 0.055, 1200.0),
        point(10120, 0.058, 1500.0),
    ];
    assert_eq!(
        b.choose_endpoint(43, false, 10120, &points, true)
            .unwrap()
            .airport_id,
        4
    );
}

fn callsign_words(text: &str) -> (i32, i32) {
    let mut b = [b' '; 8];
    b[..text.len()].copy_from_slice(text.as_bytes());
    (
        i32::from_le_bytes(b[..4].try_into().unwrap()),
        i32::from_le_bytes(b[4..].try_into().unwrap()),
    )
}
fn callsign(text: &str) -> Callsign {
    let (lon, alt) = callsign_words(text);
    decode_callsign(lon, alt).unwrap()
}
fn run(text: &str, first_ts: i64, last_ts: i64, count: u32) -> CallsignSpan {
    CallsignSpan {
        callsign: callsign(text),
        first_ts,
        last_ts,
        count,
    }
}

#[test]
fn callsign_records_carry_squawk_and_validated_identification() {
    assert!(is_callsign_record(1 << 30));
    assert!(is_callsign_record((1 << 30) | 7777));
    assert!(is_callsign_record((1 << 30) | 8888));
    assert!(!is_callsign_record(90_000_000));
    assert!(!is_callsign_record(-1_073_741_824));
    assert_eq!(callsign_text(callsign("UTOW517 ")), "UTOW517");
    assert_eq!(callsign_text(callsign("TALON 99")), "TALON 99");
    for junk in ["@@@@@@@@", "        ", "00000000", " BAL5IZX", "N       ", "VT-ANW  ", "ab12    "] {
        let (lon, alt) = callsign_words(junk);
        assert_eq!(decode_callsign(lon, alt), None, "{junk:?}");
    }
    assert_eq!(decode_callsign(0, 0), None);
}

#[test]
fn callsign_runs_merge_nearby_reports_and_stay_bounded() {
    let mut spans = Vec::new();
    add_callsign(&mut spans, run("EZY85DA", 0, 0, 1));
    add_callsign(&mut spans, run("EZY13MG", 60, 60, 1));
    add_callsign(&mut spans, run("EZY85DA", 120, 900, 12));
    // The same callsign on a later flight starts a separate run.
    add_callsign(&mut spans, run("EZY85DA", 5000, 5000, 1));
    assert_eq!(spans.len(), 3);
    assert_eq!((spans[0].first_ts, spans[0].last_ts, spans[0].count), (0, 900, 13));
    for i in 0..MAX_CALLSIGNS as i64 {
        add_callsign(&mut spans, run(&format!("JUNK{i}"), 9000 + i, 9000 + i, 1));
    }
    assert_eq!(spans.len(), MAX_CALLSIGNS);
    assert!(spans.iter().any(|s| s.count == 13));
    assert_eq!(spans.last().unwrap().callsign, callsign("JUNK7"));
    let decoded = parse_callsigns_blob(&callsigns_blob(&spans)).unwrap();
    assert_eq!(decoded, spans);
    assert!(parse_callsigns_blob(&callsigns_blob(&spans)[1..]).is_err());
}

#[test]
fn movement_callsign_is_the_flight_majority_excluding_ground_time() {
    // A callsign kept through a long ground stop and changed at liftoff.
    let stale = [run("UAL123", 0, 3000, 50), run("UAL456", 3100, 9000, 98)];
    assert_eq!(callsign_for(&stale, 3000, 9000), Some(callsign("UAL456")));
    // A brief decoding error or alternating feed does not win.
    let noisy = [run("EZY85DA", 0, 6000, 100), run("EZY13MG", 3000, 3000, 1)];
    assert_eq!(callsign_for(&noisy, 0, 6000), Some(callsign("EZY85DA")));
    // A landing ignores a callsign set at the gate afterwards.
    let gate = [run("SWA1", 0, 6000, 100), run("SWA2", 6600, 9000, 40)];
    assert_eq!(callsign_for(&gate, 0, 6000), Some(callsign("SWA1")));
    assert_eq!(callsign_for(&gate, 6600, 9000), Some(callsign("SWA2")));
    // Reception that stops at liftoff falls back to nearby reports; distant ones do not count.
    assert_eq!(callsign_for(&[run("N123AB", 2500, 2900, 7)], 3000, 3000), Some(callsign("N123AB")));
    assert_eq!(callsign_for(&[run("N123AB", 1000, 1000, 7)], 3000, 3000), None);
}

#[test]
fn endpoints_and_pending_landings_keep_their_flight_callsign_across_restart() {
    let f = Fixture::new("callsign-restart");
    let db = Connection::open(f.dir.join("airports.sqlite")).unwrap();
    db.execute_batch("INSERT INTO airport_types VALUES(2,'small_airport'); UPDATE airports SET airport_type_id=2 WHERE airport_id=1; DELETE FROM airports WHERE airport_id=2; UPDATE airports SET lon_e7=1000000 WHERE airport_id=3;").unwrap();
    let mut approach = flight();
    approach.max_altitude_ts = Some(11000);
    approach.last_points = vec![point(11940, -0.03, 1600.0), point(12000, 0.005, 900.0)];
    approach.callsigns = vec![run("ABC1", 10000, 12000, 33), run("ABC2", 12600, 13000, 5)];
    let landing = EndpointCandidate {
        point: approach.last_points[1],
        ts: 12000,
        ..anchor(1, 12000)
    };
    {
        let mut b = f.builder();
        b.finalise_segment(&approach, Some(landing)).unwrap();
        b.save_state().unwrap();
        assert_eq!(b.pending_landings[&42].callsign, Some(callsign("ABC1")));
    }
    let mut b = f.builder();
    let mut s = flight();
    s.segment_start_ts = 16000;
    s.segment_end_ts = 18000;
    s.max_altitude_ft = Some(20000.0);
    s.max_altitude_ts = Some(18000);
    s.first_points = vec![
        TrackPoint {
            on_ground: true,
            alt_ft: None,
            ..point(16000, 0.1, 0.0)
        },
        point(16060, 0.11, 1000.0),
    ];
    s.last_points = vec![point(18000, 1.0, 20000.0)];
    s.callsigns = vec![run("ABC1", 15000, 15900, 15), run("ABC2", 15950, 18000, 35)];
    b.finalise_segment(&s, None).unwrap();
    b.save_state().unwrap();
    let rows: Vec<(String, String)> = b
        .events_conn
        .prepare("SELECT kind,callsign FROM endpoint_audit ORDER BY ts")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(rows, [("landing".into(), "ABC1".into()), ("takeoff".into(), "ABC2".into())]);
    let ledger: Vec<String> = b
        .events_conn
        .prepare("SELECT c.callsign FROM endpoints e JOIN callsigns c USING(callsign_id) ORDER BY e.ts")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(ledger, ["ABC1", "ABC2"]);
    assert_eq!(b.events_conn.query_row("SELECT COUNT(*) FROM callsigns", [], |r| r.get::<_, i64>(0)).unwrap(), 2);
    // The completed flight's callsign is retained for later recovered arrivals.
    assert_eq!(b.prior(42).unwrap().unwrap().callsign, Some(callsign("ABC2")));
}
