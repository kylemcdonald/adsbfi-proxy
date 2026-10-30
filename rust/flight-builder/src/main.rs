use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use memmap2::MmapOptions;
use rusqlite::{Connection, OptionalExtension, params};
use rustc_hash::FxBuildHasher;
use std::collections::hash_map::RandomState;
type HashMap<K, V, S = FxBuildHasher> = std::collections::HashMap<K, V, S>;
type HashSet<K, S = FxBuildHasher> = std::collections::HashSet<K, S>;
use std::env;
use std::fs::{self, File};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const SLICE_BEGIN_MARKER: i32 = 0x0e7f7c9d;
const MAX_ICAO_ID: u32 = 0x00ff_ffff;
const EARTH_RADIUS_KM: f64 = 6371.0088;

const INFERENCE_VERSION: &str = "endpoints-v5";
// Only the columns readers and replay deduplication need are stored.
// compact-2 adds a dictionary-encoded callsign to each endpoint.
const LEDGER_SCHEMA: &str = "compact-2";
// Checkpoints carry per-track callsign tallies from callsigns-1 onward.
const STATE_LAYOUT: &str = "callsigns-1";
const MAX_CALLSIGNS: usize = 8;

const EVENTS_SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
PRAGMA foreign_keys=OFF;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS callsigns (
    callsign_id INTEGER PRIMARY KEY,
    callsign TEXT NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS endpoints (
    endpoint_id INTEGER PRIMARY KEY,
    icao_id INTEGER NOT NULL,
    ts INTEGER NOT NULL,
    is_landing INTEGER NOT NULL CHECK(is_landing IN (0,1)),
    airport_id INTEGER NOT NULL,
    callsign_id INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_endpoints_aircraft_time ON endpoints(icao_id,ts,is_landing);
CREATE INDEX IF NOT EXISTS idx_endpoints_airport_time ON endpoints(airport_id,ts,icao_id,is_landing);
"#;

const STATE_SCHEMA: &str = r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS processed_files (
    path TEXT PRIMARY KEY,
    slot_ts INTEGER NOT NULL,
    processed_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_processed_files_slot ON processed_files(slot_ts);

CREATE TABLE IF NOT EXISTS pending_landings (
    icao_id INTEGER PRIMARY KEY, episode INTEGER NOT NULL, airport_id INTEGER NOT NULL,
    elevation REAL, confidence REAL NOT NULL, point BLOB NOT NULL, callsign INTEGER
);
CREATE TABLE IF NOT EXISTS recent_flights (
    icao_id INTEGER PRIMARY KEY, episode_start_ts INTEGER NOT NULL, end_ts INTEGER NOT NULL,
    max_altitude_ft REAL NOT NULL, points BLOB NOT NULL,
    airport_id INTEGER, elevation_ft REAL, confidence REAL, anchor_point BLOB NOT NULL,
    callsign INTEGER
);
CREATE INDEX IF NOT EXISTS idx_recent_flights_end ON recent_flights(end_ts);

CREATE TABLE IF NOT EXISTS track_states (
    icao_id INTEGER PRIMARY KEY,
    is_rotorcraft INTEGER NOT NULL,
    segment_start_ts INTEGER NOT NULL,
    segment_end_ts INTEGER NOT NULL,
    last_seen_ts INTEGER NOT NULL,
    first_points BLOB NOT NULL,
    last_points BLOB NOT NULL,
    point_count INTEGER NOT NULL,
    max_altitude_ft REAL,
    max_altitude_ts INTEGER,
    source_first_file TEXT,
    source_last_file TEXT,
    open_start INTEGER NOT NULL,
    callsigns BLOB NOT NULL
);

"#;

#[derive(Clone, Copy, Debug)]
struct TrackPoint {
    ts: i64,
    lat: f64,
    lon: f64,
    alt_ft: Option<f64>,
    on_ground: bool,
}

// A validated identification: trimmed ASCII, zero-padded, packed little endian.
type Callsign = u64;

// Reports of one callsign within a file or track. Counts approximate observed
// minutes because the heatmap repeats identification at least once a minute.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CallsignSpan {
    callsign: Callsign,
    first_ts: i64,
    last_ts: i64,
    count: u32,
}

#[derive(Clone, Debug)]
struct AircraftFileSummary {
    callsigns: Vec<CallsignSpan>,
    first_airborne: Option<TrackPoint>,
    airborne_early: Option<TrackPoint>,
    frozen_pair: Option<(TrackPoint, TrackPoint)>,
    early: Option<TrackPoint>,
    late: Option<TrackPoint>,
    icao_id: u32,
    first_ts: i64,
    first_lat: f64,
    first_lon: f64,
    first_alt_ft: Option<f64>,
    first_on_ground: bool,
    last_ts: i64,
    last_lat: f64,
    last_lon: f64,
    last_alt_ft: Option<f64>,
    last_on_ground: bool,
    max_altitude_ft: Option<f64>,
    max_altitude_ts: Option<i64>,
    point_count: i64,
    first_terminal: Option<TrackPoint>,
    last_terminal: Option<TrackPoint>,
}

#[derive(Clone, Debug)]
struct SummaryBuilder {
    departure_extra: Option<Box<(Option<TrackPoint>, Option<TrackPoint>)>>,
    frozen_pair: Option<Box<(TrackPoint, TrackPoint)>>,
    early: Option<TrackPoint>,
    trend_latest: TrackPoint,
    trend_previous: Option<TrackPoint>,
    first_ts: i64,
    first_lat: f64,
    first_lon: f64,
    first_alt_ft: Option<f64>,
    first_on_ground: bool,
    last_ts: i64,
    last_lat: f64,
    last_lon: f64,
    last_alt_ft: Option<f64>,
    last_on_ground: bool,
    max_altitude_ft: Option<f64>,
    max_altitude_ts: Option<i64>,
    point_count: i64,
    first_terminal: Option<TrackPoint>,
    last_terminal: Option<TrackPoint>,
}

#[derive(Clone, Copy, Debug)]
struct RawPoint {
    icao_id: u32,
    ts: i64,
    lat: f64,
    lon: f64,
    alt_ft: Option<f64>,
    on_ground: bool,
}

impl SummaryBuilder {
    fn new(ts: i64, lat: f64, lon: f64, alt_ft: Option<f64>, on_ground: bool) -> Self {
        Self {
            departure_extra: None,
            frozen_pair: None,
            early: None,
            trend_latest: TrackPoint {
                ts,
                lat,
                lon,
                alt_ft,
                on_ground,
            },
            trend_previous: None,
            first_ts: ts,
            first_lat: lat,
            first_lon: lon,
            first_alt_ft: alt_ft,
            first_on_ground: on_ground,
            last_ts: ts,
            last_lat: lat,
            last_lon: lon,
            last_alt_ft: alt_ft,
            last_on_ground: on_ground,
            max_altitude_ft: alt_ft,
            max_altitude_ts: alt_ft.map(|_| ts),
            point_count: 1,
            first_terminal: if on_ground {
                Some(TrackPoint {
                    ts,
                    lat,
                    lon,
                    alt_ft,
                    on_ground,
                })
            } else {
                None
            },
            last_terminal: if on_ground {
                Some(TrackPoint {
                    ts,
                    lat,
                    lon,
                    alt_ft,
                    on_ground,
                })
            } else {
                None
            },
        }
    }

    #[inline(never)]
    fn check_frozen(&mut self, point: TrackPoint) {
        if !point.on_ground {
            for p in [Some(self.trend_latest), self.trend_previous]
                .into_iter()
                .flatten()
            {
                if !p.on_ground
                    && point.ts - p.ts >= 30
                    && point.ts - p.ts <= 120
                    && p.alt_ft
                        .zip(point.alt_ft)
                        .is_some_and(|(a, b)| (a - b).abs() >= 500.0)
                    && (point.lat - p.lat).abs() < 0.001
                    && distance_to(&point, p.lat, p.lon) < 0.1
                {
                    if let Some(pair) = self.frozen_pair.as_mut() {
                        **pair = (p, point);
                    } else {
                        self.frozen_pair = Some(Box::new((p, point)));
                    }
                    break;
                }
            }
        }
    }

    #[inline(always)]
    fn add(&mut self, ts: i64, lat: f64, lon: f64, alt_ft: Option<f64>, on_ground: bool) {
        let point = TrackPoint {
            ts,
            lat,
            lon,
            alt_ft,
            on_ground,
        };
        if (self.first_on_ground || self.first_alt_ft.is_none()) && !on_ground && alt_ft.is_some() {
            let extra = self
                .departure_extra
                .get_or_insert_with(|| Box::new((None, None)));
            if let Some(first) = extra.0 {
                if extra.1.is_none() && ts - first.ts >= 60 && ts - first.ts <= 480 {
                    extra.1 = Some(point);
                }
            } else {
                extra.0 = Some(point);
            }
        }
        if self
            .frozen_pair
            .as_ref()
            .is_some_and(|pair| distance_to(&point, pair.1.lat, pair.1.lon) > 0.1)
        {
            self.frozen_pair = None;
        }
        if ts - self.trend_latest.ts >= 60 {
            self.check_frozen(point);
        }
        if self.early.is_none() && ts - self.first_ts >= 60 && ts - self.first_ts <= 480 {
            self.early = Some(point);
        }
        if ts - self.trend_latest.ts >= 60 {
            self.trend_previous = Some(self.trend_latest);
            self.trend_latest = point;
        }
        // Retain ground evidence separately from the bounded airborne trend samples.
        if on_ground {
            if ts - self.first_ts <= 480 && self.first_terminal.is_none() {
                self.first_terminal = Some(point);
            }
            self.last_terminal = Some(point);
        }
        self.last_ts = ts;
        self.last_lat = lat;
        self.last_lon = lon;
        self.last_alt_ft = alt_ft;
        self.last_on_ground = on_ground;
        self.point_count += 1;
        if let Some(altitude) = alt_ft {
            if self
                .max_altitude_ft
                .is_none_or(|current| altitude > current)
            {
                self.max_altitude_ft = Some(altitude);
                self.max_altitude_ts = Some(ts);
            }
        }
    }

    fn finish(mut self, icao_id: u32) -> AircraftFileSummary {
        self.check_frozen(TrackPoint {
            ts: self.last_ts,
            lat: self.last_lat,
            lon: self.last_lon,
            alt_ft: self.last_alt_ft,
            on_ground: self.last_on_ground,
        });
        AircraftFileSummary {
            callsigns: Vec::new(),
            first_airborne: self.departure_extra.as_ref().and_then(|x| x.0),
            airborne_early: self.departure_extra.as_ref().and_then(|x| x.1),
            frozen_pair: self.frozen_pair.map(|p| *p),
            early: self.early,
            late: if self.last_ts - self.trend_latest.ts >= 30 {
                Some(self.trend_latest)
            } else {
                self.trend_previous
            }
            .filter(|p| self.last_ts - p.ts <= 480),
            icao_id,
            first_ts: self.first_ts,
            first_lat: self.first_lat,
            first_lon: self.first_lon,
            first_alt_ft: self.first_alt_ft,
            first_on_ground: self.first_on_ground,
            last_ts: self.last_ts,
            last_lat: self.last_lat,
            last_lon: self.last_lon,
            last_alt_ft: self.last_alt_ft,
            last_on_ground: self.last_on_ground,
            max_altitude_ft: self.max_altitude_ft,
            max_altitude_ts: self.max_altitude_ts,
            point_count: self.point_count,
            first_terminal: self.first_terminal,
            last_terminal: self.last_terminal,
        }
    }
}

#[derive(Clone, Debug)]
struct TrackState {
    icao_id: u32,
    is_rotorcraft: bool,
    segment_start_ts: i64,
    segment_end_ts: i64,
    last_seen_ts: i64,
    first_points: Vec<TrackPoint>,
    last_points: Vec<TrackPoint>,
    point_count: i64,
    max_altitude_ft: Option<f64>,
    max_altitude_ts: Option<i64>,
    source_first_file: Option<String>,
    source_last_file: Option<String>,
    open_start: bool,
    callsigns: Vec<CallsignSpan>,
}

impl TrackState {
    fn add_summary(
        &mut self,
        summary: &AircraftFileSummary,
        source_file: &Path,
        endpoint_window_seconds: i64,
    ) {
        let first_point = TrackPoint {
            ts: summary.first_ts,
            lat: summary.first_lat,
            lon: summary.first_lon,
            alt_ft: summary.first_alt_ft,
            on_ground: summary.first_on_ground,
        };
        let last_point = TrackPoint {
            ts: summary.last_ts,
            lat: summary.last_lat,
            lon: summary.last_lon,
            alt_ft: summary.last_alt_ft,
            on_ground: summary.last_on_ground,
        };

        self.segment_end_ts = summary.last_ts;
        self.last_seen_ts = summary.last_ts;
        self.source_last_file = Some(source_file.display().to_string());
        self.point_count += summary.point_count;
        for span in &summary.callsigns {
            add_callsign(&mut self.callsigns, *span);
        }

        for point in [
            Some(first_point),
            summary
                .early
                .filter(|p| !p.on_ground && first_point.alt_ft.is_none_or(|a| a < 20000.0)),
            summary.first_terminal,
            summary.first_airborne,
            summary.airborne_early,
        ]
        .into_iter()
        .flatten()
        {
            let first_airborne_ts = self
                .first_points
                .iter()
                .filter(|p| !p.on_ground && p.alt_ft.is_some())
                .map(|p| p.ts)
                .min()
                .or(summary.first_airborne.map(|p| p.ts));
            if (point.ts <= self.segment_start_ts + endpoint_window_seconds
                || !point.on_ground
                    && first_airborne_ts.is_some_and(|ts| point.ts <= ts + endpoint_window_seconds))
                && !self.first_points.iter().any(|p| p.ts == point.ts)
            {
                self.first_points.push(point);
            }
        }
        for point in [
            summary
                .late
                .filter(|p| !p.on_ground && last_point.alt_ft.is_none_or(|a| a < 20000.0)),
            summary.last_terminal,
            Some(last_point),
        ]
        .into_iter()
        .flatten()
        {
            if !self.last_points.iter().any(|p| p.ts == point.ts) {
                self.last_points.push(point);
            }
        }
        self.first_points.sort_by_key(|p| p.ts);
        if let Some((a, b)) = summary.frozen_pair {
            for p in [a, b] {
                if !self.last_points.iter().any(|q| q.ts == p.ts) {
                    self.last_points.push(p);
                }
            }
        }
        self.last_points.sort_by_key(|p| p.ts);
        let cutoff = summary.last_ts - endpoint_window_seconds;
        // Retain only a two-point witness while the latest coordinate remains
        // frozen, including taxi/ground reports after an invalid descent.
        let witness = self
            .last_points
            .iter()
            .rev()
            .find(|p| !p.on_ground && p.alt_ft.is_some())
            .and_then(|last| {
                if distance_to(&last_point, last.lat, last.lon) > 0.1 {
                    return None;
                }
                self.last_points
                    .iter()
                    .rev()
                    .find(|p| {
                        !p.on_ground
                            && last.ts - p.ts >= 30
                            && last.ts - p.ts <= 120
                            && p.alt_ft
                                .is_some_and(|a| (a - last.alt_ft.unwrap()).abs() >= 500.0)
                            && distance_to(p, last.lat, last.lon) < 0.1
                    })
                    .map(|p| (p.ts, last.ts))
            });
        self.last_points
            .retain(|p| p.ts >= cutoff || witness.is_some_and(|(a, b)| p.ts == a || p.ts == b));

        if let Some(max_altitude) = summary.max_altitude_ft {
            if self
                .max_altitude_ft
                .is_none_or(|current| max_altitude > current)
            {
                self.max_altitude_ft = Some(max_altitude);
                self.max_altitude_ts = summary.max_altitude_ts;
            }
        }
    }

    fn row_tuple(
        &self,
    ) -> (
        i64,
        i64,
        i64,
        i64,
        i64,
        Vec<u8>,
        Vec<u8>,
        i64,
        Option<f64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        i64,
        Vec<u8>,
    ) {
        (
            self.icao_id as i64,
            self.is_rotorcraft as i64,
            self.segment_start_ts,
            self.segment_end_ts,
            self.last_seen_ts,
            points_blob(&self.first_points),
            points_blob(&self.last_points),
            self.point_count,
            self.max_altitude_ft,
            self.max_altitude_ts,
            self.source_first_file.clone(),
            self.source_last_file.clone(),
            self.open_start as i64,
            callsigns_blob(&self.callsigns),
        )
    }
}

#[derive(Clone, Copy, Debug)]
struct AirportEndpoint {
    small: bool,
    lat: f64,
    lon: f64,
    airport_id: i64,
    elevation_ft: Option<f64>,
    distance_km: f64,
    water: bool,
}

#[derive(Clone, Debug)]
struct Airport {
    airport_id: i64,
    lat: f64,
    lon: f64,
    elevation_ft: Option<f64>,
    cos_lat_rad: f64,
    heliport: bool,
    water: bool,
    ultralight: bool,
    small: bool,
}

struct AirportIndex {
    cell_degrees: f64,
    grid: HashMap<(i32, i32), Vec<Airport>>,
}

impl AirportIndex {
    fn from_db(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening airports db {}", path.display()))?;
        let mut stmt = conn.prepare(
            r#"
            SELECT a.airport_id, a.lat_e7, a.lon_e7, a.elevation_ft, t.label AS airport_type, a.name
            FROM airports a
            JOIN airport_types t ON t.airport_type_id = a.airport_type_id
            ORDER BY a.airport_id
            "#,
        )?;
        let mut rows = stmt.query([])?;
        let mut grid: HashMap<(i32, i32), Vec<Airport>> = HashMap::default();
        let cell_degrees = 0.1_f64;
        while let Some(row) = rows.next()? {
            let airport_type: String = row.get(4)?;
            let normalized = airport_type.to_lowercase();
            let name: String = row.get(5)?;
            if normalized == "closed" || normalized == "closed_airport" {
                continue;
            }
            let lat_e7: i64 = row.get(1)?;
            let lon_e7: i64 = row.get(2)?;
            let lat = lat_e7 as f64 / 1e7;
            let lon = lon_e7 as f64 / 1e7;
            let airport = Airport {
                airport_id: row.get(0)?,
                lat,
                lon,
                elevation_ft: row.get::<_, Option<i64>>(3)?.map(|value| value as f64),
                cos_lat_rad: lat.to_radians().cos(),
                heliport: normalized == "heliport",
                water: normalized == "seaplane_base",
                ultralight: name.to_lowercase().contains("ultralight"),
                small: normalized == "small_airport",
            };
            grid.entry(cell_for(cell_degrees, lat, lon))
                .or_default()
                .push(airport);
        }
        Ok(Self { cell_degrees, grid })
    }

    fn nearest(
        &self,
        lat: f64,
        lon: f64,
        max_distance_km: f64,
        profile: u8,
    ) -> Option<AirportEndpoint> {
        self.nearest_excluding(lat, lon, max_distance_km, profile, &[])
    }

    fn nearest_excluding(
        &self,
        lat: f64,
        lon: f64,
        max_distance_km: f64,
        profile: u8,
        excluded: &[i64],
    ) -> Option<AirportEndpoint> {
        let lat_radius = max_distance_km / 111.0;
        let phi1 = lat.to_radians();
        let cos_phi1 = phi1.cos();
        let cos_lat = 0.15_f64.max(cos_phi1.abs());
        let lon_radius = max_distance_km / (111.0 * cos_lat);
        let min_cell = cell_for(self.cell_degrees, lat - lat_radius, lon - lon_radius);
        let max_cell = cell_for(self.cell_degrees, lat + lat_radius, lon + lon_radius);
        let mut best: Option<AirportEndpoint> = None;
        let mut best_cost = f64::INFINITY;
        for lat_cell in min_cell.0..=max_cell.0 {
            for lon_cell in min_cell.1..=max_cell.1 {
                if let Some(airports) = self.grid.get(&(lat_cell, lon_cell)) {
                    for airport in airports {
                        if excluded.contains(&airport.airport_id)
                            || (profile != 0 && airport.heliport)
                            || (profile == 2 && (airport.water || airport.ultralight))
                        {
                            continue;
                        }
                        let distance = haversine_from_query_km(lat, lon, cos_phi1, airport);
                        // A modest type preference resolves nearby small reference entries,
                        // without excluding genuine small airports or changing the search radius.
                        let cost = distance
                            + if profile == 2 && airport.small && distance > 0.25 {
                                1.5
                            } else {
                                0.0
                            };
                        if distance <= max_distance_km && cost < best_cost {
                            best_cost = cost;
                            best = Some(AirportEndpoint {
                                small: airport.small,
                                water: airport.water,
                                lat: airport.lat,
                                lon: airport.lon,
                                airport_id: airport.airport_id,
                                elevation_ft: airport.elevation_ft,
                                distance_km: distance,
                            });
                        }
                    }
                }
            }
        }
        best
    }
}

struct NearestAirportCache {
    index: Arc<AirportIndex>,
    profile: u8,
    max_distance_km: f64,
    cache: HashMap<(u64, u64), Option<AirportEndpoint>>,
}

impl NearestAirportCache {
    fn new(index: Arc<AirportIndex>, max_distance_km: f64, profile: u8) -> Self {
        Self {
            index,
            profile,
            max_distance_km,
            cache: HashMap::default(),
        }
    }

    fn nearest(&mut self, lat: f64, lon: f64) -> Option<AirportEndpoint> {
        // Cache exact positions: quantized keys made inference depend on arrival order.
        let key = (lat.to_bits(), lon.to_bits());
        if self.cache.len() >= 50_000 {
            self.cache.clear();
        }
        if !self.cache.contains_key(&key) {
            let value = self
                .index
                .nearest(lat, lon, self.max_distance_km, self.profile);
            self.cache.insert(key, value);
        }
        self.cache.get(&key).copied().flatten()
    }
}

#[derive(Clone, Copy, Debug)]
struct EndpointCandidate {
    ts: i64,
    airport_id: i64,
    airport_elevation_ft: Option<f64>,
    confidence: f64,
    point: TrackPoint,
}

#[derive(Clone, Copy, Debug)]
enum ParserMode {
    SortVec,
    StreamWords,
    DirectSort,
    MmapSort,
    StreamWordsFx,
    DirectStreamFx,
}

impl ParserMode {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "sort-vec" => Ok(Self::SortVec),
            "stream-words" => Ok(Self::StreamWords),
            "direct-sort" => Ok(Self::DirectSort),
            "mmap-sort" => Ok(Self::MmapSort),
            "stream-words-fx" => Ok(Self::StreamWordsFx),
            "direct-stream-fx" => Ok(Self::DirectStreamFx),
            _ => Err(anyhow!(
                "unknown parser mode {value}; expected one of sort-vec, stream-words, direct-sort, mmap-sort, stream-words-fx, direct-stream-fx"
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SortVec => "sort-vec",
            Self::StreamWords => "stream-words",
            Self::DirectSort => "direct-sort",
            Self::MmapSort => "mmap-sort",
            Self::StreamWordsFx => "stream-words-fx",
            Self::DirectStreamFx => "direct-stream-fx",
        }
    }
}

#[derive(Debug)]
struct Args {
    cache_dir: PathBuf,
    archive_dir: Option<PathBuf>,
    events_db: PathBuf,
    airports_db: PathBuf,
    aircraft_db: PathBuf,
    state_db: PathBuf,
    start: Option<String>,
    end: Option<String>,
    max_files: Option<usize>,
    reset: bool,
    commit_every: usize,
    progress_every: usize,
    parser_mode: ParserMode,
}

impl Args {
    fn parse() -> Result<Self> {
        let mut args = env::args().skip(1);
        let command = args.next().unwrap_or_else(|| "build-events".to_string());
        if command != "build-events" {
            return Err(anyhow!("only build-events is supported"));
        }
        let data_dir = env::var_os("ADSBFI_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("data"));
        let db_dir = env::var_os("ADSBFI_DB_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("db"));
        let cache_dir = env::var_os("ADSBFI_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("heatmaps"));
        let mut parsed = Self {
            cache_dir,
            archive_dir: env::var_os("ADSBFI_ARCHIVE_DIR").map(PathBuf::from),
            events_db: db_dir.join("events.sqlite"),
            airports_db: db_dir.join("airports.sqlite"),
            aircraft_db: db_dir.join("aircraft.sqlite"),
            state_db: db_dir.join("build_state.sqlite"),
            start: None,
            end: None,
            max_files: None,
            reset: false,
            commit_every: 50,
            progress_every: 25,
            parser_mode: ParserMode::DirectStreamFx,
        };
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--cache-dir" => parsed.cache_dir = PathBuf::from(required_arg(&mut args, &arg)?),
                "--archive-dir" => {
                    parsed.archive_dir = Some(PathBuf::from(required_arg(&mut args, &arg)?))
                }
                "--events-db" => parsed.events_db = PathBuf::from(required_arg(&mut args, &arg)?),
                "--airports-db" => {
                    parsed.airports_db = PathBuf::from(required_arg(&mut args, &arg)?)
                }
                "--aircraft-db" => {
                    parsed.aircraft_db = PathBuf::from(required_arg(&mut args, &arg)?)
                }
                "--state-db" => parsed.state_db = PathBuf::from(required_arg(&mut args, &arg)?),
                "--start" => parsed.start = Some(required_arg(&mut args, &arg)?),
                "--end" => parsed.end = Some(required_arg(&mut args, &arg)?),
                "--max-files" => parsed.max_files = Some(required_arg(&mut args, &arg)?.parse()?),
                "--reset" => parsed.reset = true,
                "--commit-every" => parsed.commit_every = required_arg(&mut args, &arg)?.parse()?,
                "--progress-every" => {
                    parsed.progress_every = required_arg(&mut args, &arg)?.parse()?
                }
                "--parser-mode" => {
                    parsed.parser_mode = ParserMode::parse(&required_arg(&mut args, &arg)?)?
                }
                other => return Err(anyhow!("unknown argument {other}")),
            }
        }
        Ok(parsed)
    }
}

fn required_arg(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
    args.next()
        .ok_or_else(|| anyhow!("{flag} requires a value"))
}

#[derive(Clone, Debug)]
struct RecentFlight {
    episode: i64,
    end_ts: i64,
    max_altitude: f64,
    points: Vec<TrackPoint>,
    landing: Option<EndpointCandidate>,
    // Callsign in use through the end of this flight, for later arrivals.
    callsign: Option<Callsign>,
}
fn great_circle_distance(point: &TrackPoint, lat: f64, lon: f64) -> f64 {
    let a = ((point.lat - lat).to_radians() * 0.5).sin().powi(2)
        + point.lat.to_radians().cos()
            * lat.to_radians().cos()
            * ((point.lon - lon).to_radians() * 0.5).sin().powi(2);
    12742.0 * a.clamp(0.0, 1.0).sqrt().asin()
}
fn distance_to(point: &TrackPoint, lat: f64, lon: f64) -> f64 {
    let dy = point.lat - lat;
    let dx = ((point.lon - lon + 180.0).rem_euclid(360.0) - 180.0)
        * ((point.lat + lat) * 0.5).to_radians().cos();
    111.195 * (dx * dx + dy * dy).sqrt()
}

#[derive(Clone, Copy)]
struct PendingLanding {
    episode: i64,
    endpoint: EndpointCandidate,
    callsign: Option<Callsign>,
}

struct EventBuilder {
    pending_landings: HashMap<u32, PendingLanding>,
    pending_dirty: HashSet<u32>,
    small_airport_ids: HashSet<i64>,
    recent: HashMap<u32, Option<RecentFlight>>,
    recent_dirty: HashSet<u32>,
    observed_through: i64,
    airport_locations: HashMap<i64, (f64, f64, Option<f64>)>,
    large_airport_ids: HashSet<i64>,
    events_conn: Connection,
    state_conn: Connection,
    rotorcraft_ids: HashSet<u32>,
    all_airports: NearestAirportCache,
    non_heliport_airports: NearestAirportCache,
    land_airports: NearestAirportCache,
    land_aircraft_ids: HashSet<u32>,
    transport_aircraft_ids: HashSet<u32, FxBuildHasher>,
    has_checkpoint: bool,
    inserted_endpoints: i64,
    callsign_ids: HashMap<Callsign, i64>,
    states: HashMap<u32, TrackState>,
    state_order: Vec<u32>,
    processed_paths: HashSet<String>,
    processed_batch: Vec<(String, i64, String)>,
    max_gap_seconds: i64,
    endpoint_window_seconds: i64,
    min_segment_seconds: f64,
    min_climb_ft: f64,
    max_airport_agl_ft: f64,
    fallback_low_alt_ft: f64,
    airport_match_radius_km: f64,
    files_processed: i64,
    files_skipped: i64,
    telemetry_rows: i64,
    stale_segments: i64,
    ground_splits: i64,
    parse_errors: Vec<String>,
    in_event_transaction: bool,
    parse_seconds: f64,
    summary_seconds: f64,
    flush_seconds: f64,
    save_seconds: f64,
    parser_mode: ParserMode,
}

impl EventBuilder {
    fn new(
        events_db: &Path,
        airports_db: &Path,
        aircraft_db: &Path,
        state_db: &Path,
        parser_mode: ParserMode,
    ) -> Result<Self> {
        ensure_parent(events_db)?;
        ensure_parent(state_db)?;
        let events_conn = Connection::open(events_db)?;
        let version: Option<String> = events_conn
            .query_row(
                "SELECT value FROM meta WHERE key='inference_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        let has_tables: bool = events_conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')",
            [],
            |r| r.get(0),
        )?;
        if (has_tables && version.as_deref() != Some(INFERENCE_VERSION))
            || version.as_deref().is_some_and(|v| v != INFERENCE_VERSION)
        {
            return Err(anyhow!(
                "Use fresh events/state databases for endpoints-v5; rebuild separately before switching readers"
            ));
        }
        let ledger: Option<String> = events_conn
            .query_row("SELECT value FROM meta WHERE key='ledger_schema'", [], |r| r.get(0))
            .ok();
        if has_tables && ledger.as_deref() != Some(LEDGER_SCHEMA) {
            return Err(anyhow!(
                "Events database uses a different endpoint ledger schema; rebuild into a fresh database"
            ));
        }
        let state_conn = Connection::open(state_db)?;
        let state_version: Option<String> = state_conn
            .query_row(
                "SELECT value FROM meta WHERE key='inference_version'",
                [],
                |r| r.get(0),
            )
            .ok();
        let state_has_tables: bool = state_conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table')",
            [],
            |r| r.get(0),
        )?;
        let state_layout: Option<String> = state_conn
            .query_row("SELECT value FROM meta WHERE key='state_layout'", [], |r| r.get(0))
            .ok();
        if state_has_tables
            && (state_version.as_deref() != Some(INFERENCE_VERSION)
                || state_layout.as_deref() != Some(STATE_LAYOUT))
        {
            return Err(anyhow!(
                "Use fresh events/state databases: incompatible checkpoint version"
            ));
        }
        if has_tables != state_has_tables {
            return Err(anyhow!(
                "Events and checkpoint databases must be created and retained together"
            ));
        }
        events_conn.execute_batch(EVENTS_SCHEMA)?;
        #[cfg(test)]
        events_conn.execute_batch("CREATE TABLE IF NOT EXISTS endpoint_audit(icao_id,kind,ts,airport_id,episode_start_ts,confidence,evidence,time_start_ts,time_end_ts,callsign)")?;
        state_conn.execute_batch(STATE_SCHEMA)?;
        state_conn.execute_batch("PRAGMA cache_size=-8192")?;
        set_meta(&state_conn, "inference_version", INFERENCE_VERSION)?;
        set_meta(&state_conn, "state_layout", STATE_LAYOUT)?;
        let rotorcraft_ids = load_rotorcraft_ids(aircraft_db)?;

        let airport_index = Arc::new(AirportIndex::from_db(airports_db)?);
        let all_airports = NearestAirportCache::new(airport_index.clone(), 12.0, 0);
        let non_heliport_airports = NearestAirportCache::new(airport_index.clone(), 12.0, 1);
        let land_airports = NearestAirportCache::new(airport_index, 12.0, 2);
        let aircraft =
            Connection::open_with_flags(aircraft_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut query = aircraft.prepare("SELECT a.icao_id FROM aircraft a JOIN aircraft_types t USING(aircraft_type_id) WHERE t.label IN ('commercial_jet','business_jet','unknown_jet','commercial_turboprop')")?;
        let land_aircraft_ids = query
            .query_map([], |r| r.get::<_, u32>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        let mut transport_aircraft_ids: HashSet<u32, FxBuildHasher> =
            land_aircraft_ids.iter().copied().collect();
        // This import-time index avoids scanning the complete registry per slot.
        let frames: Vec<u8> = aircraft
            .query_row(
                "SELECT payload FROM airframe_index WHERE version=1",
                [],
                |r| r.get(0),
            )
            .context("aircraft references need a fresh v5 metadata import")?;
        if frames.len() % 4 != 0 {
            return Err(anyhow!("invalid physical-airframe index"));
        }
        for word in frames.chunks_exact(4) {
            let packed = u32::from_le_bytes(word.try_into().unwrap());
            let id = packed & MAX_ICAO_ID;
            let flags = packed >> 24;
            if flags & 2 != 0 {
                transport_aircraft_ids.insert(id);
            }
        }
        for id in aircraft.prepare("SELECT a.icao_id FROM aircraft a JOIN aircraft_types t USING(aircraft_type_id) WHERE t.label='turboprop'")?.query_map([],|r|r.get::<_,u32>(0))? {transport_aircraft_ids.insert(id?);}
        let has_checkpoint: bool =
            state_conn.query_row("SELECT EXISTS(SELECT 1 FROM processed_files)", [], |r| {
                r.get(0)
            })?;
        set_meta(&events_conn, "inference_version", INFERENCE_VERSION)?;
        set_meta(&events_conn, "ledger_schema", LEDGER_SCHEMA)?;
        let refs =
            Connection::open_with_flags(airports_db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let mut locations_query =
            refs.prepare("SELECT airport_id,lat_e7/1e7,lon_e7/1e7,elevation_ft FROM airports")?;
        let airport_locations = locations_query
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?, r.get(3)?))))?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        let large_airport_ids = refs.prepare("SELECT airport_id FROM airports JOIN airport_types USING(airport_type_id) WHERE label='large_airport'")?
            .query_map([],|r|r.get(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        let small_airport_ids = refs.prepare("SELECT airport_id FROM airports JOIN airport_types USING(airport_type_id) WHERE label='small_airport'")?
            .query_map([],|r|r.get(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
        let pending_landings = {
            let mut q=state_conn.prepare("SELECT icao_id,episode,airport_id,elevation,confidence,point,callsign FROM pending_landings")?;
            q.query_map([], |r| {
                let point = *parse_points_blob(&r.get::<_, Vec<u8>>(5)?)?
                    .first()
                    .ok_or(rusqlite::Error::InvalidQuery)?;
                Ok((
                    r.get(0)?,
                    PendingLanding {
                        episode: r.get(1)?,
                        endpoint: EndpointCandidate {
                            ts: point.ts,
                            airport_id: r.get(2)?,
                            airport_elevation_ft: r.get(3)?,
                            confidence: r.get(4)?,
                            point,
                        },
                        callsign: r.get::<_, Option<i64>>(6)?.map(|c| c as Callsign),
                    },
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?
        };
        let mut builder = Self {
            pending_landings,
            pending_dirty: HashSet::default(),
            small_airport_ids,
            recent: HashMap::default(),
            recent_dirty: HashSet::default(),
            observed_through: 0,
            airport_locations,
            large_airport_ids,
            events_conn,
            state_conn,
            rotorcraft_ids,
            all_airports,
            non_heliport_airports,
            land_airports,
            land_aircraft_ids,
            transport_aircraft_ids,
            has_checkpoint,
            inserted_endpoints: 0,
            callsign_ids: HashMap::default(),
            states: HashMap::default(),
            state_order: Vec::new(),
            processed_paths: HashSet::default(),
            processed_batch: Vec::new(),
            max_gap_seconds: 20 * 60,
            endpoint_window_seconds: 8 * 60,
            min_segment_seconds: 120.0,
            min_climb_ft: 1500.0,
            max_airport_agl_ft: 1500.0,
            fallback_low_alt_ft: 1500.0,
            airport_match_radius_km: 12.0,
            files_processed: 0,
            files_skipped: 0,
            telemetry_rows: 0,
            stale_segments: 0,
            ground_splits: 0,
            parse_errors: Vec::new(),
            in_event_transaction: false,
            parse_seconds: 0.0,
            summary_seconds: 0.0,
            flush_seconds: 0.0,
            save_seconds: 0.0,
            parser_mode,
        };
        let (states, state_order) = builder.load_states()?;
        builder.states = states;
        builder.state_order = state_order;
        // Query processed paths by primary key per file; never load the entire archive index.
        for (key, value) in [("summary_endpoint_id", "0"), ("endpoint_count", "0")] {
            builder.events_conn.execute(
                "INSERT OR IGNORE INTO meta(key,value) VALUES(?1,?2)",
                params![key, value],
            )?;
        }
        builder.begin_events()?;
        Ok(builder)
    }

    fn begin_events(&mut self) -> Result<()> {
        if !self.in_event_transaction {
            self.events_conn.execute_batch("BEGIN")?;
            self.in_event_transaction = true;
        }
        Ok(())
    }

    fn commit_events(&mut self) -> Result<()> {
        if self.in_event_transaction {
            self.update_summary()?;
            self.events_conn.execute_batch("COMMIT")?;
            self.in_event_transaction = false;
        }
        Ok(())
    }

    fn update_summary(&self) -> Result<()> {
        let cursor: i64 = self.events_conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='summary_endpoint_id'",
            [],
            |r| r.get(0),
        )?;
        let (count, lo, hi, last): (i64,Option<i64>,Option<i64>,Option<i64>) = self.events_conn.query_row("SELECT COUNT(*),MIN(ts),MAX(ts),MAX(endpoint_id) FROM endpoints WHERE endpoint_id>?1",[cursor],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
        self.events_conn.execute(
            "UPDATE meta SET value=CAST(value AS INTEGER)+?1 WHERE key='endpoint_count'",
            [count],
        )?;
        if let Some(v) = lo {
            self.events_conn.execute("INSERT INTO meta(key,value) VALUES('min_endpoint_ts',?1) ON CONFLICT(key) DO UPDATE SET value=MIN(CAST(value AS INTEGER),CAST(excluded.value AS INTEGER))",[v])?;
        }
        if let Some(v) = hi {
            self.events_conn.execute("INSERT INTO meta(key,value) VALUES('max_endpoint_ts',?1) ON CONFLICT(key) DO UPDATE SET value=MAX(CAST(value AS INTEGER),CAST(excluded.value AS INTEGER))",[v])?;
        }
        if let Some(v) = last {
            set_meta(&self.events_conn, "summary_endpoint_id", &v.to_string())?;
        }
        Ok(())
    }

    fn load_states(&self) -> Result<(HashMap<u32, TrackState>, Vec<u32>)> {
        let mut stmt = self.state_conn.prepare(
            r#"
            SELECT icao_id, is_rotorcraft, segment_start_ts, segment_end_ts, last_seen_ts,
                   first_points, last_points, point_count, max_altitude_ft,
                   max_altitude_ts, source_first_file, source_last_file, open_start, callsigns
            FROM track_states
            ORDER BY rowid
            "#,
        )?;
        let rows = stmt.query_map([], |row| {
            let icao_id = row.get::<_, i64>(0)? as u32;
            Ok((
                icao_id,
                TrackState {
                    icao_id,
                    is_rotorcraft: row.get::<_, i64>(1)? != 0,
                    segment_start_ts: row.get(2)?,
                    segment_end_ts: row.get(3)?,
                    last_seen_ts: row.get(4)?,
                    first_points: parse_points_blob(&row.get::<_, Vec<u8>>(5)?)?,
                    last_points: parse_points_blob(&row.get::<_, Vec<u8>>(6)?)?,
                    point_count: row.get(7)?,
                    max_altitude_ft: row.get(8)?,
                    max_altitude_ts: row.get(9)?,
                    source_first_file: row.get(10)?,
                    source_last_file: row.get(11)?,
                    open_start: row.get::<_, i64>(12)? != 0,
                    callsigns: parse_callsigns_blob(&row.get::<_, Vec<u8>>(13)?)?,
                },
            ))
        })?;
        let mut states = HashMap::default();
        let mut state_order = Vec::new();
        for row in rows {
            let (key, state) = row?;
            state_order.push(key);
            states.insert(key, state);
        }
        Ok((states, state_order))
    }

    fn airport_cache_for(&mut self, is_rotorcraft: bool, icao_id: u32) -> &mut NearestAirportCache {
        if is_rotorcraft {
            &mut self.all_airports
        } else if self.land_aircraft_ids.contains(&icao_id) {
            &mut self.land_airports
        } else {
            &mut self.non_heliport_airports
        }
    }

    // Fixed-wing vertical movement with a frozen position is unusable
    // positioning evidence. The altitude may still prove that it was airborne.
    fn frozen_position(points: &[TrackPoint]) -> bool {
        let Some(last) = points
            .iter()
            .filter(|p| !p.on_ground && p.alt_ft.is_some())
            .max_by_key(|p| p.ts)
        else {
            return false;
        };
        points.iter().any(|p| {
            !p.on_ground
                && last.ts - p.ts >= 30
                && last.ts - p.ts <= 120
                && p.alt_ft
                    .is_some_and(|a| (a - last.alt_ft.unwrap()).abs() >= 500.0)
                && distance_to(p, last.lat, last.lon) < 0.1
        })
    }

    fn choose_endpoint(
        &mut self,
        icao_id: u32,
        is_rotorcraft: bool,
        latest_ts: i64,
        points: &[TrackPoint],
        takeoff: bool,
    ) -> Option<EndpointCandidate> {
        let max_airport_agl_ft = self.max_airport_agl_ft;
        let fallback_low_alt_ft = self.fallback_low_alt_ft;
        let airport_match_radius_km = self.airport_match_radius_km;
        let approach = if takeoff {
            None
        } else {
            points.iter().max_by_key(|p| p.ts).and_then(|last| {
                points
                    .iter()
                    .filter(|p| last.ts - p.ts >= 30 && last.ts - p.ts <= 480)
                    .max_by_key(|p| p.ts)
                    .map(|previous| (*previous, *last))
            })
        };
        if !is_rotorcraft && Self::frozen_position(points) {
            return None;
        }
        let airport_cache = self.airport_cache_for(is_rotorcraft, icao_id);
        let mut best_point: Option<TrackPoint> = None;
        let mut best_airport: Option<AirportEndpoint> = None;
        let mut best_confidence = 0.0;
        let mut best_score: Option<f64> = None;

        for point in points {
            // A short flight can land inside the origin window. Its later ground
            // report must not replace the observed departure before the climb.
            if point.ts > latest_ts {
                continue;
            }
            // Reject a terminal observation separated from the next retained
            // observation by an implausible takeoff-speed position jump.
            if takeoff
                && points
                    .iter()
                    .filter(|q| q.ts - point.ts >= 15 && q.ts - point.ts <= 120)
                    .min_by_key(|q| q.ts)
                    .is_some_and(|q| {
                        distance_to(point, q.lat, q.lon) * 3600.0 / (q.ts - point.ts) as f64 > 900.0
                    })
            {
                continue;
            }
            let mut matched = airport_cache.nearest(point.lat, point.lon);

            // A descending aircraft can pass a nearby field en route to another.
            // Reject small-field matches when the final observations move away.
            // Ground/taxi evidence and large-airport turning approaches are preserved.
            if !point.on_ground {
                if let Some((previous, last)) = approach {
                    let mut excluded = [0; 3];
                    let mut count = 0;
                    while let Some(airport) = matched {
                        if !airport.small
                            || distance_to(&last, airport.lat, airport.lon)
                                <= distance_to(&previous, airport.lat, airport.lon) + 0.5
                        {
                            break;
                        }
                        excluded[count] = airport.airport_id;
                        count += 1;
                        if count == excluded.len() {
                            matched = None;
                            break;
                        }
                        matched = airport_cache.index.nearest_excluding(
                            point.lat,
                            point.lon,
                            airport_match_radius_km,
                            airport_cache.profile,
                            &excluded[..count],
                        );
                    }
                }
            }
            // A weak airborne climb toward a compact reference is not a
            // departure from that reference. Keep ground/close low observations.
            if takeoff
                && !is_rotorcraft
                && !point.on_ground
                && matched.is_some_and(|airport| {
                    (airport.small || airport.water)
                        && endpoint_agl(point, &airport).is_some_and(|h| h > 300.0)
                })
            {
                if let Some(next) = points
                    .iter()
                    .filter(|q| {
                        q.ts - point.ts >= 30
                            && q.ts - point.ts <= 120
                            && q.ts <= latest_ts
                            && !q.on_ground
                            && q.alt_ft
                                .zip(point.alt_ft)
                                .is_some_and(|(b, a)| b - a >= 100.0)
                    })
                    .min_by_key(|q| q.ts)
                {
                    let mut excluded = [0; 3];
                    let mut count = 0;
                    while let Some(airport) = matched {
                        if !(airport.small || airport.water)
                            || !endpoint_agl(point, &airport).is_some_and(|h| h > 300.0)
                            || distance_to(next, airport.lat, airport.lon) + 0.5
                                >= distance_to(point, airport.lat, airport.lon)
                        {
                            break;
                        }
                        excluded[count] = airport.airport_id;
                        count += 1;
                        if count == excluded.len() {
                            matched = None;
                            break;
                        }
                        matched = airport_cache.index.nearest_excluding(
                            point.lat,
                            point.lon,
                            airport_match_radius_km,
                            airport_cache.profile,
                            &excluded[..count],
                        );
                    }
                }
            }
            let Some(airport) = matched else {
                continue;
            };
            // A ground bit is not sufficient evidence several kilometres from
            // a small field. Such flags also occur in airborne approach tracks.
            // Commercial jets far from a small field need later corroboration
            // rather than an immediate nearest-reference arrival assignment.
            if airport.small
                && airport.distance_km > 3.0
                && (point.on_ground || airport_cache.profile == 2 && !takeoff)
            {
                continue;
            }
            let agl_ft = endpoint_agl(point, &airport);
            let mut low_enough = point.on_ground;
            if agl_ft.is_some_and(|value| value <= max_airport_agl_ft) {
                low_enough = true;
            }
            if point
                .alt_ft
                .is_some_and(|value| value <= fallback_low_alt_ft)
            {
                low_enough = true;
            }
            if !low_enough {
                continue;
            }

            let agl_score = agl_ft.map_or(1.0, |value| {
                (1.0 - value / max_airport_agl_ft).clamp(0.0, 1.0)
            });
            let distance_score =
                (1.0 - airport.distance_km / airport_match_radius_km).clamp(0.0, 1.0);
            let ground_bonus = if point.on_ground { 0.15 } else { 0.0 };
            let confidence = (0.55 * distance_score + 0.35 * agl_score + ground_bonus).min(1.0);
            let score = airport.distance_km + agl_ft.unwrap_or(0.0) / 1800.0;
            if best_point.is_none_or(|p| {
                (point.on_ground && !p.on_ground)
                    || (point.on_ground
                        && p.on_ground
                        && if takeoff {
                            point.ts < p.ts
                        } else {
                            point.ts > p.ts
                        })
                    || (!point.on_ground
                        && !p.on_ground
                        && if takeoff {
                            if is_rotorcraft {
                                point.ts < p.ts
                            } else {
                                best_score.is_none_or(|current| score < current)
                            }
                        } else {
                            point.ts > p.ts
                        })
            }) {
                best_point = Some(*point);
                best_airport = Some(airport);
                best_confidence = confidence;
                best_score = Some(score);
            }
        }

        let point = best_point?;
        let airport = best_airport?;
        Some(EndpointCandidate {
            ts: point.ts,
            airport_id: airport.airport_id,
            airport_elevation_ft: airport.elevation_ft,
            confidence: best_confidence,
            point,
        })
    }

    // Helicopters can complete a low flight without reaching the fixed-wing
    // climb threshold. Require ground evidence or a close, low endpoint.
    fn movement_climb(&self, state: &TrackState, candidate: &EndpointCandidate) -> f64 {
        let local_low = candidate
            .airport_elevation_ft
            .zip(candidate.point.alt_ft)
            .is_some_and(|(e, a)| a - e <= 300.0 && a - e >= -500.0)
            && self
                .airport_locations
                .get(&candidate.airport_id)
                .is_some_and(|p| distance_to(&candidate.point, p.0, p.1) <= 3.0);
        if state.is_rotorcraft && (candidate.point.on_ground || local_low) {
            500.0
        } else {
            self.min_climb_ft
        }
    }

    fn infer_takeoff(&mut self, state: &TrackState) -> Option<EndpointCandidate> {
        if state.open_start || state.first_points.is_empty() || state.max_altitude_ft.is_none() {
            return None;
        }
        let candidate = self.choose_endpoint(
            state.icao_id,
            state.is_rotorcraft,
            state.max_altitude_ts.unwrap_or(state.segment_end_ts),
            &state.first_points,
            true,
        )?;
        let endpoint_alt = endpoint_altitude_for(&candidate, &state.first_points);
        let climbed = state.max_altitude_ft? - endpoint_alt;
        let max_after = state
            .max_altitude_ts
            .is_none_or(|value| value >= candidate.ts);
        if climbed >= self.movement_climb(state, &candidate) && max_after {
            Some(candidate)
        } else {
            None
        }
    }

    fn infer_landing(&mut self, state: &TrackState) -> Option<EndpointCandidate> {
        if state.last_points.is_empty() || state.max_altitude_ft.is_none() {
            return None;
        }
        let candidate = self.choose_endpoint(
            state.icao_id,
            state.is_rotorcraft,
            state.segment_end_ts,
            &state.last_points,
            false,
        )?;
        let endpoint_alt = endpoint_altitude_for(&candidate, &state.last_points);
        let descended = state.max_altitude_ft? - endpoint_alt;
        let max_before = state
            .max_altitude_ts
            .is_none_or(|value| value <= candidate.ts);
        let near_segment_end = state.segment_end_ts - candidate.ts <= self.endpoint_window_seconds;
        if descended >= self.movement_climb(state, &candidate) && max_before && near_segment_end {
            Some(candidate)
        } else {
            None
        }
    }

    fn prior(&mut self, icao: u32) -> Result<Option<RecentFlight>> {
        if !self.recent.contains_key(&icao) {
            let prior = self.state_conn.prepare_cached("SELECT episode_start_ts,end_ts,max_altitude_ft,points,airport_id,elevation_ft,confidence,anchor_point,callsign FROM recent_flights WHERE icao_id=?1")?
                .query_row([icao], |r| {
                    let airport:Option<i64>=r.get(4)?;
                    let landing=if let Some(airport_id)=airport {
                        let points=parse_points_blob(&r.get::<_,Vec<u8>>(7)?)?;
                        let point=*points.first().ok_or(rusqlite::Error::InvalidQuery)?;
                        Some(EndpointCandidate{ts:point.ts,airport_id,airport_elevation_ft:r.get(5)?,confidence:r.get(6)?,point})
                    }else{None};
                    Ok(RecentFlight{episode:r.get(0)?,end_ts:r.get(1)?,max_altitude:r.get(2)?,points:parse_points_blob(&r.get::<_,Vec<u8>>(3)?)?,landing,callsign:r.get::<_,Option<i64>>(8)?.map(|c| c as Callsign)})
                }).optional()?;
            self.recent.insert(icao, prior);
        }
        // Apply expiry at lookup as well as checkpoint cleanup. Otherwise a
        // long backfill batch can use context already pruned by a rolling run.
        Ok(self
            .recent
            .get(&icao)
            .cloned()
            .flatten()
            .filter(|r| r.end_ts >= self.observed_through - 48 * 3600))
    }

    fn corroborated_departure(
        &self,
        state: &TrackState,
        prior: &RecentFlight,
    ) -> Option<EndpointCandidate> {
        if Self::frozen_position(&state.first_points) {
            return None;
        }
        let anchor = prior.landing?;
        if state.open_start
            || state.segment_start_ts <= prior.end_ts
            || state.segment_start_ts - prior.end_ts > 48 * 3600
        {
            return None;
        }
        let location = self.airport_locations.get(&anchor.airport_id)?;
        let first = *state
            .first_points
            .iter()
            .filter(|p| {
                !p.on_ground && p.alt_ft.is_some() && distance_to(p, location.0, location.1) <= 20.0
            })
            .min_by_key(|p| p.ts)?;
        let alt = first.alt_ft?;
        if first.on_ground
            || distance_to(&first, location.0, location.1) > 20.0
            || alt - location.2? > 5000.0
        {
            return None;
        }
        let rising = state.first_points.iter().any(|p| {
            p.ts - first.ts >= 30
                && p.ts - first.ts <= 480
                && p.alt_ft.is_some_and(|a| a - alt >= 300.0)
        });
        if !rising || state.max_altitude_ft? - location.2? < self.min_climb_ft {
            return None;
        }
        Some(EndpointCandidate {
            ts: first.ts,
            airport_id: anchor.airport_id,
            airport_elevation_ft: location.2,
            confidence: 0.65,
            point: first,
        })
    }

    fn corroborated_arrival(
        &self,
        prior: &RecentFlight,
        takeoff: EndpointCandidate,
    ) -> Option<EndpointCandidate> {
        if prior.landing.is_some()
            || takeoff.ts <= prior.end_ts
            || takeoff.ts - prior.end_ts > 48 * 3600
        {
            return None;
        }
        if Self::frozen_position(&prior.points) {
            return None;
        }
        let location = self.airport_locations.get(&takeoff.airport_id)?;
        let last = *prior
            .points
            .iter()
            .filter(|p| !p.on_ground && p.alt_ft.is_some())
            .max_by_key(|p| p.ts)?;
        let elevation = location.2?;
        let altitude = last.alt_ft?;
        if last.on_ground
            || altitude - elevation > 10000.0
            || altitude - elevation < -500.0
            || prior.max_altitude - elevation < self.min_climb_ft
            || distance_to(&last, location.0, location.1) > 80.0
        {
            return None;
        }
        let approaching = prior.points.iter().any(|p| {
            last.ts - p.ts >= 30
                && last.ts - p.ts <= 480
                && p.alt_ft.is_some_and(|a| a - altitude >= 150.0)
                && distance_to(p, location.0, location.1)
                    > distance_to(&last, location.0, location.1) + 0.5
        });
        if !approaching {
            return None;
        }
        Some(EndpointCandidate {
            ts: last.ts,
            airport_id: takeoff.airport_id,
            airport_elevation_ft: location.2,
            confidence: 0.6,
            point: last,
        })
    }

    fn rotor_arrival(
        &self,
        prior: &RecentFlight,
        departure: EndpointCandidate,
    ) -> Option<EndpointCandidate> {
        if prior.landing.is_some()
            || departure.ts - prior.end_ts < 600
            || departure.ts - prior.end_ts > 24 * 3600
        {
            return None;
        }
        let last = *prior
            .points
            .iter()
            .filter(|p| !p.on_ground && p.alt_ft.is_some())
            .max_by_key(|p| p.ts)?;
        let location = self.airport_locations.get(&departure.airport_id)?;
        let agl = last.alt_ft? - location.2?;
        if !(-500.0..=1500.0).contains(&agl) || distance_to(&last, location.0, location.1) > 6.0 {
            return None;
        }
        let approach = prior.points.iter().any(|p| {
            last.ts - p.ts >= 30
                && last.ts - p.ts <= 480
                && distance_to(p, location.0, location.1)
                    > distance_to(&last, location.0, location.1) + 0.5
        });
        if !approach {
            return None;
        }
        Some(EndpointCandidate {
            ts: last.ts,
            airport_id: departure.airport_id,
            airport_elevation_ft: location.2,
            confidence: 0.6,
            point: last,
        })
    }

    fn observed_departure(&mut self, state: &TrackState) -> Option<EndpointCandidate> {
        if state.open_start || !self.land_aircraft_ids.contains(&state.icao_id) {
            return None;
        }
        if Self::frozen_position(&state.first_points) {
            return None;
        }
        let first = *state
            .first_points
            .iter()
            .filter(|p| !p.on_ground && p.alt_ft.is_some())
            .min_by_key(|p| p.ts)?;
        let altitude = first.alt_ft?;
        if first.on_ground || state.max_altitude_ft? - altitude < self.min_climb_ft {
            return None;
        }
        let rising = state.first_points.iter().any(|p| {
            p.ts - first.ts >= 30
                && p.ts - first.ts <= 480
                && p.alt_ft.is_some_and(|a| a - altitude >= 300.0)
        });
        if !rising {
            return None;
        }
        let airport = self
            .airport_cache_for(false, state.icao_id)
            .nearest(first.lat, first.lon)?;
        if !self.large_airport_ids.contains(&airport.airport_id)
            || airport.distance_km > 5.0
            || altitude - airport.elevation_ft? > 3000.0
        {
            return None;
        }
        Some(EndpointCandidate {
            ts: first.ts,
            airport_id: airport.airport_id,
            airport_elevation_ft: airport.elevation_ft,
            confidence: 0.65,
            point: first,
        })
    }

    // Presence on the ground and a later/earlier airborne observation establish
    // a necessary movement, not a precise route or runway-crossing time. Keep
    // its entire observation gap so consumers can represent that uncertainty.
    fn gap_departure(&self, state: &TrackState, prior: &RecentFlight) -> Option<EndpointCandidate> {
        let anchor = prior.landing?;
        let location = self.airport_locations.get(&anchor.airport_id)?;
        let elevation = location.2?;
        let first = *state
            .first_points
            .iter()
            .filter(|p| {
                !p.on_ground
                    && p.alt_ft.is_some_and(|a| a - elevation >= 1500.0)
                    && (p.ts - prior.end_ts) as f64
                        >= great_circle_distance(p, location.0, location.1) / 1200.0 * 3600.0
                            + 300.0
                    && (p.alt_ft.unwrap() >= 5000.0
                        || distance_to(p, location.0, location.1) <= 20.0)
            })
            .min_by_key(|p| p.ts)?;
        let gap = first.ts - prior.end_ts;
        let distance = great_circle_distance(&first, location.0, location.1);
        if state.open_start
            || gap <= 0
            || gap > 48 * 3600
            || first.on_ground
            || first.alt_ft? - location.2? < 1500.0
            || (first.alt_ft? < 5000.0 && distance > 20.0)
        {
            return None;
        }
        if (gap as f64) < distance / 1200.0 * 3600.0 + 300.0 {
            return None;
        }
        Some(EndpointCandidate {
            ts: first.ts,
            airport_id: anchor.airport_id,
            airport_elevation_ft: location.2,
            confidence: 0.45,
            point: first,
        })
    }
    fn gap_arrival(
        &self,
        prior: &RecentFlight,
        takeoff: EndpointCandidate,
    ) -> Option<EndpointCandidate> {
        if prior.landing.is_some() {
            return None;
        }
        let last = *prior
            .points
            .iter()
            .filter(|p| !p.on_ground && p.alt_ft.is_some())
            .max_by_key(|p| p.ts)?;
        let location = self.airport_locations.get(&takeoff.airport_id)?;
        let gap = takeoff.ts - last.ts;
        let frozen = Self::frozen_position(&prior.points);
        let distance = if frozen {
            0.0
        } else {
            great_circle_distance(&last, location.0, location.1)
        };
        if gap <= 0 || gap > 48 * 3600 || last.on_ground {
            return None;
        }
        let altitude = last.alt_ft?;
        // Sparse final approaches may have only one retained airborne point.
        // Near the later departure airport, use height above that field;
        // otherwise require a high-altitude observation for broad gap recovery.
        let local_descent = distance <= 20.0
            && altitude - location.2? >= 300.0
            && prior.max_altitude - altitude >= self.min_climb_ft;
        if !frozen
            && altitude < 10000.0
            && !(distance <= 80.0 && altitude - location.2? > 1500.0)
            && !local_descent
        {
            return None;
        }
        if (gap as f64) < distance / 1200.0 * 3600.0 + 600.0 {
            return None;
        }
        Some(EndpointCandidate {
            ts: last.ts,
            airport_id: takeoff.airport_id,
            airport_elevation_ft: location.2,
            confidence: 0.45,
            point: last,
        })
    }

    // Reception can resume only on final approach, below the standalone flight
    // threshold. A later close ground report can confirm the earlier unclosed
    // airborne segment without waiting for another departure. Reuse the same
    // bounded gap, altitude, position-quality and feasible-travel checks.
    fn grounded_arrival(
        &mut self,
        state: &TrackState,
        prior: &RecentFlight,
    ) -> Option<EndpointCandidate> {
        if state.is_rotorcraft
            || prior.landing.is_some()
            || state.segment_start_ts <= prior.end_ts
            || !state.last_points.last().is_some_and(|p| p.on_ground)
        {
            return None;
        }
        let ground = self.choose_endpoint(
            state.icao_id,
            false,
            state.segment_end_ts,
            &state.last_points,
            false,
        )?;
        if !ground.point.on_ground
            || !self
                .airport_locations
                .get(&ground.airport_id)
                .is_some_and(|p| distance_to(&ground.point, p.0, p.1) <= 3.0)
        {
            return None;
        }
        self.gap_arrival(prior, ground)?;
        Some(ground)
    }

    fn finalise_segment(
        &mut self,
        state: &TrackState,
        landing_override: Option<EndpointCandidate>,
    ) -> Result<()> {
        let duration = state.segment_end_ts - state.segment_start_ts;
        if state.point_count < 3
            || (duration as f64) < self.min_segment_seconds
            || state.max_altitude_ft.is_none()
        {
            return Ok(());
        }
        let mut prior = self.prior(state.icao_id)?;
        let low_takeoff = self.infer_takeoff(state);
        let mut takeoff = low_takeoff.or_else(|| self.observed_departure(state));
        let observed_climb = low_takeoff.is_none() && takeoff.is_some();
        let landing = landing_override.or_else(|| self.infer_landing(state));
        let ground_recovery = if landing.is_none() && takeoff.is_none() {
            prior.as_ref().and_then(|p| self.grounded_arrival(state, p))
        } else {
            None
        };
        let landing = landing.or(ground_recovery);
        // Movements of this track take the callsign flown between them, or
        // through the track's end or from its start when one side is unknown.
        let flight_start = takeoff.map_or(state.segment_start_ts, |t| t.ts);
        let flight_end = landing.map_or(state.segment_end_ts, |l| l.ts);
        let departure_callsign = |ts: i64| callsign_for(&state.callsigns, ts, flight_end.max(ts));
        let arrival_callsign = |ts: i64| callsign_for(&state.callsigns, flight_start.min(ts), ts);
        if let Some(pending) = self.pending_landings.remove(&state.icao_id) {
            self.pending_dirty.insert(state.icao_id);
            let mut corrected = None;
            if let (Some(previous), Some(departure)) = (prior.as_ref(), takeoff) {
                let old = pending.endpoint;
                let weak_touchdown = old
                    .point
                    .alt_ft
                    .zip(old.airport_elevation_ft)
                    .is_none_or(|(a, e)| a - e > 300.0)
                    || self
                        .airport_locations
                        .get(&old.airport_id)
                        .is_some_and(|p| distance_to(&old.point, p.0, p.1) > 1.0);
                if previous.episode == pending.episode
                    && departure.airport_id != old.airport_id
                    && weak_touchdown
                    && self
                        .airport_locations
                        .get(&departure.airport_id)
                        .is_some_and(|p| distance_to(&old.point, p.0, p.1) <= 12.0)
                {
                    let mut approach = previous.clone();
                    approach.landing = None;
                    corrected = self.corroborated_arrival(&approach, departure);
                }
            }
            let endpoint = corrected.unwrap_or(pending.endpoint);
            self.insert_movement(
                state.icao_id,
                pending.episode,
                "landing",
                endpoint,
                endpoint.ts,
                if corrected.is_some() {
                    takeoff.unwrap().ts
                } else {
                    endpoint.ts
                },
                if corrected.is_some() {
                    "arrival_destination_context"
                } else {
                    "low_altitude"
                },
                pending.callsign,
            )?;
            if let Some(previous) = prior.as_mut() {
                if previous.episode == pending.episode {
                    previous.landing = Some(endpoint);
                }
            }
        }
        // Rotorcraft get only local approach confirmation, never broad gap recovery.
        if state.is_rotorcraft {
            if let (Some(previous), Some(departure)) = (prior.as_ref(), takeoff) {
                if let Some(arrival) = self.rotor_arrival(previous, departure) {
                    self.insert_movement(
                        state.icao_id,
                        previous.episode,
                        "landing",
                        arrival,
                        arrival.ts,
                        departure.ts,
                        "arrival_corroborated",
                        previous.callsign,
                    )?;
                }
            }
        }
        let mut anchored = false;
        if let Some(previous) = prior.as_ref() {
            if !state.is_rotorcraft {
                if let Some(departure) = takeoff {
                    if let Some(arrival) = self.corroborated_arrival(previous, departure) {
                        self.insert_movement(
                            state.icao_id,
                            previous.episode,
                            "landing",
                            arrival,
                            arrival.ts,
                            departure.ts,
                            "arrival_corroborated",
                            previous.callsign,
                        )?;
                    } else if self.transport_aircraft_ids.contains(&state.icao_id)
                        || previous.max_altitude >= 20000.0
                        || self
                            .airport_locations
                            .get(&departure.airport_id)
                            .is_some_and(|a| {
                                previous
                                    .points
                                    .last()
                                    .is_some_and(|p| distance_to(p, a.0, a.1) <= 20.0)
                            })
                    {
                        if let Some(arrival) = self.gap_arrival(previous, departure) {
                            self.insert_movement(
                                state.icao_id,
                                previous.episode,
                                "landing",
                                arrival,
                                arrival.ts,
                                departure.ts,
                                "arrival_observation_gap",
                                previous.callsign,
                            )?;
                        }
                    }
                    // Resolve an adjacent-airport origin ambiguity using this aircraft's previous
                    // arrival. The airport identities remain separate; no alias table.
                    if let Some(anchor) = previous.landing {
                        if let (Some(a), Some(b)) = (
                            self.airport_locations.get(&anchor.airport_id),
                            self.airport_locations.get(&departure.airport_id),
                        ) {
                            let near_airports = distance_to(
                                &TrackPoint {
                                    ts: 0,
                                    lat: a.0,
                                    lon: a.1,
                                    alt_ft: None,
                                    on_ground: true,
                                },
                                b.0,
                                b.1,
                            ) <= 4.0;
                            if departure.airport_id != anchor.airport_id
                                && near_airports
                                && state.segment_start_ts > previous.end_ts
                                && state.segment_start_ts - previous.end_ts <= 48 * 3600
                                && distance_to(&departure.point, a.0, a.1) <= 6.0
                            {
                                takeoff = Some(EndpointCandidate {
                                    airport_id: anchor.airport_id,
                                    airport_elevation_ft: a.2,
                                    ..departure
                                });
                                anchored = true;
                            }
                        }
                    }
                } else if let Some(departure) = self.corroborated_departure(state, previous) {
                    self.insert_movement(
                        state.icao_id,
                        state.segment_start_ts,
                        "takeoff",
                        departure,
                        previous.end_ts,
                        departure.ts,
                        "departure_corroborated",
                        departure_callsign(departure.ts),
                    )?;
                } else if self.transport_aircraft_ids.contains(&state.icao_id)
                    || state.max_altitude_ft.is_some_and(|a| a >= 20000.0)
                    || previous.landing.is_some_and(|anchor| {
                        self.airport_locations
                            .get(&anchor.airport_id)
                            .is_some_and(|a| {
                                state
                                    .first_points
                                    .first()
                                    .is_some_and(|p| distance_to(p, a.0, a.1) <= 20.0)
                            })
                    })
                {
                    if let Some(departure) = self.gap_departure(state, previous) {
                        self.insert_movement(
                            state.icao_id,
                            state.segment_start_ts,
                            "takeoff",
                            departure,
                            previous.end_ts,
                            departure.ts,
                            "departure_observation_gap",
                            departure_callsign(departure.ts),
                        )?;
                    }
                }
            }
        }
        if let Some(endpoint) = takeoff {
            let evidence = if anchored {
                "origin_context"
            } else if observed_climb {
                "climb"
            } else if endpoint.point.on_ground {
                "ground"
            } else {
                "low_altitude"
            };
            self.insert_movement(
                state.icao_id,
                state.segment_start_ts,
                "takeoff",
                endpoint,
                endpoint.ts,
                endpoint.ts,
                evidence,
                departure_callsign(endpoint.ts),
            )?;
        }
        let pending = landing.is_some_and(|e| {
            !state.is_rotorcraft
                && !e.point.on_ground
                && (self.small_airport_ids.contains(&e.airport_id)
                    || e.point
                        .alt_ft
                        .zip(e.airport_elevation_ft)
                        .is_none_or(|(a, h)| a - h > 300.0))
        });
        if let Some(endpoint) = landing {
            if pending {
                self.pending_dirty.insert(state.icao_id);
                self.pending_landings.insert(
                    state.icao_id,
                    PendingLanding {
                        episode: state.segment_start_ts,
                        endpoint,
                        callsign: arrival_callsign(endpoint.ts),
                    },
                );
            } else {
                self.insert_movement(
                    state.icao_id,
                    state.segment_start_ts,
                    "landing",
                    endpoint,
                    if ground_recovery.is_some() {
                        prior.as_ref().unwrap().end_ts
                    } else {
                        endpoint.ts
                    },
                    endpoint.ts,
                    if ground_recovery.is_some() {
                        "arrival_ground_context"
                    } else if endpoint.point.on_ground {
                        "ground"
                    } else {
                        "low_altitude"
                    },
                    arrival_callsign(endpoint.ts).or_else(|| {
                        ground_recovery
                            .and(prior.as_ref())
                            .and_then(|p| p.callsign)
                    }),
                )?;
            }
        }
        // A parked/ground-only fragment must not erase the last supported arrival.
        let preserve_ground = prior.as_ref().and_then(|r| r.landing).is_some_and(|a| {
            self.airport_locations.get(&a.airport_id).is_some_and(|p| {
                p.2.is_some_and(|e| state.max_altitude_ft.unwrap() - e < 500.0)
                    && state
                        .last_points
                        .last()
                        .is_some_and(|q| distance_to(q, p.0, p.1) < 6.0)
            })
        });
        if !preserve_ground {
            self.recent.insert(
                state.icao_id,
                Some(RecentFlight {
                    episode: state.segment_start_ts,
                    end_ts: state.segment_end_ts,
                    max_altitude: state.max_altitude_ft.unwrap(),
                    points: if landing.is_some() && !pending {
                        Vec::new()
                    } else {
                        state.last_points.clone()
                    },
                    landing,
                    callsign: arrival_callsign(flight_end),
                }),
            );
            self.recent_dirty.insert(state.icao_id);
        }
        Ok(())
    }

    fn insert_movement(
        &mut self,
        icao: u32,
        episode: i64,
        kind: &str,
        endpoint: EndpointCandidate,
        time_start: i64,
        time_end: i64,
        evidence: &str,
        callsign: Option<Callsign>,
    ) -> Result<()> {
        let callsign_id = callsign.map(|c| self.callsign_id(c)).transpose()?;
        // Deterministic replay after a crash reproduces the same aircraft, time
        // and direction, so the unique key also deduplicates replayed rows.
        let changed = self
            .events_conn
            .prepare_cached("INSERT OR IGNORE INTO endpoints(icao_id,ts,is_landing,airport_id,callsign_id) VALUES(?1,?2,?3,?4,?5)")?
            .execute(params![icao as i64, endpoint.ts, kind == "landing", endpoint.airport_id, callsign_id])?;
        self.inserted_endpoints += changed as i64;
        #[cfg(test)]
        if changed > 0 {
            // Tests inspect inference details that the compact ledger does not store.
            self.events_conn.execute(
                "INSERT INTO endpoint_audit VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![icao as i64, kind, endpoint.ts, endpoint.airport_id, episode, endpoint.confidence, evidence, time_start, time_end, callsign.map(callsign_text)],
            )?;
        }
        #[cfg(not(test))]
        let _ = (episode, time_start, time_end, evidence);
        Ok(())
    }

    // Dictionary IDs commit in the same transaction as the endpoints using them.
    fn callsign_id(&mut self, callsign: Callsign) -> Result<i64> {
        if let Some(id) = self.callsign_ids.get(&callsign) {
            return Ok(*id);
        }
        let text = callsign_text(callsign);
        let existing: Option<i64> = self
            .events_conn
            .prepare_cached("SELECT callsign_id FROM callsigns WHERE callsign=?1")?
            .query_row([&text], |r| r.get(0))
            .optional()?;
        let id = match existing {
            Some(id) => id,
            None => {
                self.events_conn
                    .prepare_cached("INSERT INTO callsigns(callsign) VALUES(?1)")?
                    .execute([&text])?;
                self.events_conn.last_insert_rowid()
            }
        };
        if self.callsign_ids.len() >= 1_000_000 {
            self.callsign_ids.clear();
        }
        self.callsign_ids.insert(callsign, id);
        Ok(id)
    }

    fn flush_stale(&mut self, current_ts: i64) -> Result<()> {
        // Expire by observation time, independent of batch/checkpoint boundaries.
        let expired: Vec<_> = self
            .pending_landings
            .iter()
            .filter(|(_, p)| current_ts - p.endpoint.ts > 48 * 3600)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            let p = self.pending_landings.remove(&id).unwrap();
            self.pending_dirty.insert(id);
            let e = p.endpoint;
            self.insert_movement(id, p.episode, "landing", e, e.ts, e.ts, "low_altitude", p.callsign)?;
        }
        self.compact_state_order();
        let stale_ids: Vec<u32> = self
            .state_order
            .iter()
            .filter_map(|icao_id| {
                let state = self.states.get(icao_id)?;
                if current_ts - state.last_seen_ts > self.max_gap_seconds {
                    Some(*icao_id)
                } else {
                    None
                }
            })
            .collect();
        for icao_id in stale_ids {
            if let Some(state) = self.states.remove(&icao_id) {
                self.finalise_segment(&state, None)?;
                self.stale_segments += 1;
            }
        }
        Ok(())
    }

    fn maybe_split_ground_segment(
        &mut self,
        state: &TrackState,
        summary: &AircraftFileSummary,
    ) -> Result<bool> {
        if !summary.last_on_ground || state.max_altitude_ts.is_none() {
            return Ok(false);
        }
        if state.max_altitude_ts.unwrap() > summary.last_ts - self.endpoint_window_seconds {
            return Ok(false);
        }
        let Some(landing) = self.infer_landing(state) else {
            return Ok(false);
        };
        self.finalise_segment(state, Some(landing))?;
        self.states.remove(&state.icao_id);
        self.ground_splits += 1;
        Ok(true)
    }

    fn process_file(&mut self, path: &Path, open_start_cutoff_ts: Option<i64>) -> Result<()> {
        let path_key = path.display().to_string();
        if self.processed_paths.contains(&path_key)
            || self.state_conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM processed_files WHERE path=?1)",
                [&path_key],
                |r| r.get::<_, bool>(0),
            )?
        {
            self.files_skipped += 1;
            return Ok(());
        }
        let slot_ts = slot_timestamp_from_path(path)?;
        self.observed_through = self.observed_through.max(slot_ts + 1800);
        let parse_started = Instant::now();
        let summaries = summarize_heatmap_file(path, self.parser_mode)?;
        self.parse_seconds += parse_started.elapsed().as_secs_f64();
        let mut last_observed_ts = slot_ts + 1800;
        if let Some(max_ts) = summaries.iter().map(|summary| summary.last_ts).max() {
            last_observed_ts = max_ts;
        }

        let summary_started = Instant::now();
        for summary in &summaries {
            self.telemetry_rows += summary.point_count;
            let mut needs_new_state = false;
            if let Some(state) = self.states.get(&summary.icao_id) {
                if summary.first_ts - state.last_seen_ts > self.max_gap_seconds {
                    let state = self.states.remove(&summary.icao_id).unwrap();
                    self.finalise_segment(&state, None)?;
                    self.stale_segments += 1;
                    needs_new_state = true;
                }
            } else {
                needs_new_state = true;
            }

            if needs_new_state {
                self.states.insert(
                    summary.icao_id,
                    TrackState {
                        icao_id: summary.icao_id,
                        is_rotorcraft: self.rotorcraft_ids.contains(&summary.icao_id),
                        segment_start_ts: summary.first_ts,
                        segment_end_ts: summary.last_ts,
                        last_seen_ts: summary.last_ts,
                        first_points: Vec::new(),
                        last_points: Vec::new(),
                        point_count: 0,
                        max_altitude_ft: None,
                        max_altitude_ts: None,
                        source_first_file: Some(path.display().to_string()),
                        source_last_file: Some(path.display().to_string()),
                        open_start: open_start_cutoff_ts
                            .is_some_and(|cutoff| summary.first_ts <= cutoff),
                        callsigns: Vec::new(),
                    },
                );
                self.state_order.push(summary.icao_id);
            }

            let state = self.states.get_mut(&summary.icao_id).unwrap();
            state.add_summary(summary, path, self.endpoint_window_seconds);
            let should_check_split = summary.last_on_ground
                && state
                    .max_altitude_ts
                    .is_some_and(|max_ts| max_ts <= summary.last_ts - self.endpoint_window_seconds);
            if should_check_split {
                let state_snapshot = state.clone();
                self.maybe_split_ground_segment(&state_snapshot, summary)?;
            }
        }
        self.summary_seconds += summary_started.elapsed().as_secs_f64();

        let flush_started = Instant::now();
        self.flush_stale(last_observed_ts)?;
        self.flush_seconds += flush_started.elapsed().as_secs_f64();
        let processed_at = Utc::now().to_rfc3339();
        self.processed_batch
            .push((path_key.clone(), slot_ts, processed_at));
        self.processed_paths.insert(path_key);
        self.files_processed += 1;
        Ok(())
    }

    fn save_state(&mut self) -> Result<()> {
        let started = Instant::now();
        self.compact_state_order();
        self.state_conn.execute_batch("BEGIN")?;
        self.state_conn.execute(
            "DELETE FROM recent_flights WHERE end_ts < ?1",
            [self.observed_through - 48 * 3600],
        )?;
        {
            let mut put = self.state_conn.prepare_cached("INSERT INTO recent_flights(icao_id,episode_start_ts,end_ts,max_altitude_ft,points,airport_id,elevation_ft,confidence,anchor_point,callsign) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(icao_id) DO UPDATE SET episode_start_ts=excluded.episode_start_ts,end_ts=excluded.end_ts,max_altitude_ft=excluded.max_altitude_ft,points=excluded.points,airport_id=excluded.airport_id,elevation_ft=excluded.elevation_ft,confidence=excluded.confidence,anchor_point=excluded.anchor_point,callsign=excluded.callsign")?;
            let mut ordered: Vec<_> = self.recent_dirty.iter().collect();
            ordered.sort_unstable();
            for id in ordered {
                if let Some(Some(r)) = self.recent.get(id) {
                    if r.end_ts < self.observed_through - 48 * 3600 {
                        continue;
                    }
                    let anchor = r.landing;
                    let point = anchor.map(|a| points_blob(&[a.point])).unwrap_or_default();
                    put.execute(params![
                        id,
                        r.episode,
                        r.end_ts,
                        r.max_altitude,
                        points_blob(&r.points),
                        anchor.map(|a| a.airport_id),
                        anchor.and_then(|a| a.airport_elevation_ft),
                        anchor.map(|a| a.confidence),
                        point,
                        r.callsign.map(|c| c as i64)
                    ])?;
                }
            }
        }
        {
            let mut put=self.state_conn.prepare_cached("INSERT INTO pending_landings VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(icao_id) DO UPDATE SET episode=excluded.episode,airport_id=excluded.airport_id,elevation=excluded.elevation,confidence=excluded.confidence,point=excluded.point,callsign=excluded.callsign")?;
            let mut remove = self
                .state_conn
                .prepare_cached("DELETE FROM pending_landings WHERE icao_id=?1")?;
            for id in &self.pending_dirty {
                if let Some(p) = self.pending_landings.get(id) {
                    let e = p.endpoint;
                    put.execute(params![
                        id,
                        p.episode,
                        e.airport_id,
                        e.airport_elevation_ft,
                        e.confidence,
                        points_blob(&[e.point]),
                        p.callsign.map(|c| c as i64)
                    ])?;
                } else {
                    remove.execute([id])?;
                }
            }
        }
        self.state_conn.execute("DELETE FROM track_states", [])?;
        {
            let mut stmt = self.state_conn.prepare(
                r#"
                INSERT INTO track_states(
                    icao_id, is_rotorcraft, segment_start_ts, segment_end_ts, last_seen_ts,
                    first_points, last_points, point_count, max_altitude_ft,
                    max_altitude_ts, source_first_file, source_last_file, open_start, callsigns
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
                "#,
            )?;
            for icao_id in &self.state_order {
                let Some(state) = self.states.get(icao_id) else {
                    continue;
                };
                let row = state.row_tuple();
                stmt.execute(params![
                    row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
                    row.11, row.12, row.13
                ])?;
            }
        }
        if !self.processed_batch.is_empty() {
            let mut stmt = self.state_conn.prepare(
                "INSERT OR IGNORE INTO processed_files(path, slot_ts, processed_at) VALUES (?1, ?2, ?3)",
            )?;
            for row in &self.processed_batch {
                stmt.execute(params![row.0, row.1, row.2])?;
            }
            self.processed_batch.clear();
        }
        set_meta(
            &self.state_conn,
            "active_track_states",
            &self.states.len().to_string(),
        )?;
        self.commit_events()?;
        self.state_conn.execute_batch("COMMIT")?;
        self.recent.clear();
        self.recent_dirty.clear();
        self.pending_dirty.clear();
        self.processed_paths.clear();
        self.has_checkpoint = true;
        self.begin_events()?;
        self.save_seconds += started.elapsed().as_secs_f64();
        Ok(())
    }

    fn compact_state_order(&mut self) {
        let mut seen = HashSet::with_capacity_and_hasher(self.states.len(), FxBuildHasher);
        let mut compacted = Vec::with_capacity(self.states.len());
        for icao_id in self.state_order.iter().rev() {
            if self.states.contains_key(icao_id) && seen.insert(*icao_id) {
                compacted.push(*icao_id);
            }
        }
        compacted.reverse();
        self.state_order = compacted;
    }

    fn run(
        &mut self,
        files: &[PathBuf],
        start_ts: Option<i64>,
        commit_every: usize,
        progress_every: usize,
    ) -> Result<serde_json::Value> {
        let started = Instant::now();
        let open_start_cutoff_ts = if !self.has_checkpoint {
            start_ts.map(|value| value + self.max_gap_seconds)
        } else {
            None
        };
        for (index, path) in files.iter().enumerate() {
            self.process_file(path, open_start_cutoff_ts)?;
            let one_based = index + 1;
            if one_based % commit_every == 0 {
                self.save_state()?;
            }
            if progress_every > 0 && one_based % progress_every == 0 {
                eprintln!(
                    "Processed {one_based}/{total} selected files ({new} new, {skipped} skipped), {events} inserted endpoints, {active} active tracks, {elapsed:.1}s",
                    total = files.len(),
                    new = self.files_processed,
                    skipped = self.files_skipped,
                    events = self.inserted_endpoints,
                    active = self.states.len(),
                    elapsed = started.elapsed().as_secs_f64(),
                );
            }
        }
        self.save_state()?;
        self.commit_events()?;

        let endpoint_count: i64 = self.events_conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key='endpoint_count'",
            [],
            |r| r.get(0),
        )?;
        let min_endpoint_ts: Option<i64> = self
            .events_conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='min_endpoint_ts'",
                [],
                |r| r.get(0),
            )
            .ok();
        let max_endpoint_ts: Option<i64> = self
            .events_conn
            .query_row(
                "SELECT CAST(value AS INTEGER) FROM meta WHERE key='max_endpoint_ts'",
                [],
                |r| r.get(0),
            )
            .ok();
        let elapsed = started.elapsed().as_secs_f64();
        let details = serde_json::json!({
            "selected_files": files.len(),
            "files_processed": self.files_processed,
            "files_skipped": self.files_skipped,
            "telemetry_rows": self.telemetry_rows,
            "inserted_endpoints": self.inserted_endpoints,
            "inference_version": INFERENCE_VERSION,
            "active_track_states": self.states.len(),
            "stale_segments": self.stale_segments,
            "ground_splits": self.ground_splits,
            "parse_error_count": self.parse_errors.len(),
            "parse_errors": self.parse_errors,
            "parser_mode": self.parser_mode.as_str(),
            "parse_seconds": self.parse_seconds,
            "summary_seconds": self.summary_seconds,
            "flush_seconds": self.flush_seconds,
            "save_seconds": self.save_seconds,
            "elapsed_seconds": elapsed,
            "seconds_per_selected_file": if files.is_empty() { serde_json::Value::Null } else { serde_json::json!(elapsed / files.len() as f64) },
            "endpoint_count": endpoint_count,
            "min_endpoint_ts": min_endpoint_ts,
            "max_endpoint_ts": max_endpoint_ts,
        });
        set_meta(
            &self.events_conn,
            "last_build_details",
            &serde_json::to_string(&details)?,
        )?;
        Ok(details)
    }
}

fn main() -> Result<()> {
    let args = Args::parse()?;
    if args.reset {
        remove_sqlite_family(&args.events_db)?;
        remove_sqlite_family(&args.state_db)?;
    }
    let start_ts = parse_time_arg(args.start.as_deref())?;
    let end_ts = parse_time_arg(args.end.as_deref())?;
    let mut files =
        iter_heatmap_files(&args.cache_dir, args.archive_dir.as_deref(), start_ts, end_ts)?;
    if let Some(max_files) = args.max_files {
        files.truncate(max_files);
    }
    if files.is_empty() {
        return Err(anyhow!(
            "No heatmap files matched the requested range."
        ));
    }
    let mut builder = EventBuilder::new(
        &args.events_db,
        &args.airports_db,
        &args.aircraft_db,
        &args.state_db,
        args.parser_mode,
    )?;
    let details = builder.run(&files, start_ts, args.commit_every, args.progress_every)?;
    println!("{}", serde_json::to_string_pretty(&details)?);
    Ok(())
}

fn load_rotorcraft_ids(path: &Path) -> Result<HashSet<u32>> {
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening aircraft db {}", path.display()))?;
    let mut stmt = conn.prepare(
        r#"
        SELECT a.icao_id
        FROM aircraft a
        JOIN aircraft_types t ON t.aircraft_type_id = a.aircraft_type_id
        WHERE t.label IN ('helicopter', 'gyroplane')
        "#,
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    let mut ids = HashSet::default();
    for row in rows {
        ids.insert(row? as u32);
    }
    Ok(ids)
}

fn summarize_heatmap_file(path: &Path, mode: ParserMode) -> Result<Vec<AircraftFileSummary>> {
    match mode {
        ParserMode::SortVec => summarize_sort_vec(path),
        ParserMode::StreamWords => summarize_streaming_words(path, RandomState::new()),
        ParserMode::DirectSort => summarize_direct_sort(path),
        ParserMode::MmapSort => summarize_mmap_sort(path),
        ParserMode::StreamWordsFx => summarize_streaming_words(path, FxBuildHasher),
        ParserMode::DirectStreamFx => summarize_direct_streaming(path, FxBuildHasher),
    }
}

fn summarize_sort_vec(path: &Path) -> Result<Vec<AircraftFileSummary>> {
    let raw = fs::read(path)?;
    if raw.len() < 16 {
        return Ok(Vec::new());
    }
    summarize_sort_vec_from_bytes(&raw)
}

fn summarize_mmap_sort(path: &Path) -> Result<Vec<AircraftFileSummary>> {
    let file = File::open(path)?;
    if file.metadata()?.len() < 16 {
        return Ok(Vec::new());
    }
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    summarize_sort_vec_from_bytes(&mmap)
}

fn summarize_sort_vec_from_bytes(raw: &[u8]) -> Result<Vec<AircraftFileSummary>> {
    let (words_i, words_u) = build_word_vectors(raw);
    let marker_indices: Vec<usize> = words_i
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            if *value == SLICE_BEGIN_MARKER {
                Some(index)
            } else {
                None
            }
        })
        .collect();
    if marker_indices.is_empty() {
        return Ok(Vec::new());
    }

    let mut points: Vec<RawPoint> = Vec::new();
    let mut callsigns = Vec::new();
    for (marker_offset, marker_index) in marker_indices.iter().copied().enumerate() {
        let block_start = marker_index + 4;
        let block_end = marker_indices
            .get(marker_offset + 1)
            .copied()
            .unwrap_or(words_i.len());
        let record_words = ((block_end.saturating_sub(block_start)) / 4) * 4;
        if record_words == 0 {
            continue;
        }
        let now = words_u[marker_index + 2] as f64 / 1000.0
            + words_u[marker_index + 1] as f64 * 4294967.296;
        let ts = python_round_to_i64(now);
        let end = block_start + record_words;
        let mut index = block_start;
        while index < end {
            let point0_u = words_u[index];
            let point1_i = words_i[index + 1];
            if valid_position(point1_i, words_i[index + 2]) {
                let point_id = point0_u & 0x01ff_ffff;
                if point_id <= MAX_ICAO_ID {
                    let lat = words_i[index + 1] as f32 / 1_000_000.0_f32;
                    let lon = words_i[index + 2] as f32 / 1_000_000.0_f32;
                    let point3 = words_i[index + 3];
                    let (alt_ft, on_ground) = decode_altitude(point3);
                    let lat64 = lat as f64;
                    let lon64 = lon as f64;
                    points.push(RawPoint {
                        icao_id: point_id,
                        ts,
                        lat: lat64,
                        lon: lon64,
                        alt_ft,
                        on_ground,
                    });
                }
            } else if is_callsign_record(point1_i) {
                push_callsign(&mut callsigns, point0_u, ts, words_i[index + 2], words_i[index + 3]);
            }
            index += 4;
        }
    }
    if points.is_empty() {
        return Ok(Vec::new());
    }
    points.sort_by_key(|point| point.icao_id);
    let mut summaries = Vec::new();
    let mut index = 0;
    while index < points.len() {
        let icao_id = points[index].icao_id;
        let first = points[index];
        let mut summary = SummaryBuilder::new(
            first.ts,
            first.lat,
            first.lon,
            first.alt_ft,
            first.on_ground,
        );
        index += 1;
        while index < points.len() && points[index].icao_id == icao_id {
            let point = points[index];
            summary.add(
                point.ts,
                point.lat,
                point.lon,
                point.alt_ft,
                point.on_ground,
            );
            index += 1;
        }
        summaries.push(summary.finish(icao_id));
    }
    attach_callsigns(&mut summaries, callsigns);
    Ok(summaries)
}

fn summarize_direct_sort(path: &Path) -> Result<Vec<AircraftFileSummary>> {
    let raw = fs::read(path)?;
    if raw.len() < 16 {
        return Ok(Vec::new());
    }
    let word_len = raw.len() / 4;
    let marker_indices = marker_indices_from_bytes(&raw);
    if marker_indices.is_empty() {
        return Ok(Vec::new());
    }

    let mut points: Vec<RawPoint> = Vec::new();
    let mut callsigns = Vec::new();
    for (marker_offset, marker_index) in marker_indices.iter().copied().enumerate() {
        let block_start = marker_index + 4;
        let block_end = marker_indices
            .get(marker_offset + 1)
            .copied()
            .unwrap_or(word_len);
        let record_words = ((block_end.saturating_sub(block_start)) / 4) * 4;
        if record_words == 0 {
            continue;
        }
        let now = read_u32_word(&raw, marker_index + 2) as f64 / 1000.0
            + read_u32_word(&raw, marker_index + 1) as f64 * 4294967.296;
        let ts = python_round_to_i64(now);
        let end = block_start + record_words;
        let mut index = block_start;
        while index < end {
            let point0_u = read_u32_word(&raw, index);
            let point1_i = read_i32_word(&raw, index + 1);
            if valid_position(point1_i, read_i32_word(&raw, index + 2)) {
                let point_id = point0_u & 0x01ff_ffff;
                if point_id <= MAX_ICAO_ID {
                    let lat = point1_i as f32 / 1_000_000.0_f32;
                    let lon = read_i32_word(&raw, index + 2) as f32 / 1_000_000.0_f32;
                    let point3 = read_i32_word(&raw, index + 3);
                    let (alt_ft, on_ground) = decode_altitude(point3);
                    points.push(RawPoint {
                        icao_id: point_id,
                        ts,
                        lat: lat as f64,
                        lon: lon as f64,
                        alt_ft,
                        on_ground,
                    });
                }
            } else if is_callsign_record(point1_i) {
                push_callsign(&mut callsigns, point0_u, ts, read_i32_word(&raw, index + 2), read_i32_word(&raw, index + 3));
            }
            index += 4;
        }
    }

    let mut summaries = summaries_from_points(points);
    attach_callsigns(&mut summaries, callsigns);
    Ok(summaries)
}

fn summarize_streaming_words<S: BuildHasher>(
    path: &Path,
    hasher: S,
) -> Result<Vec<AircraftFileSummary>> {
    let raw = fs::read(path)?;
    if raw.len() < 16 {
        return Ok(Vec::new());
    }
    let (words_i, words_u) = build_word_vectors(&raw);
    let marker_indices: Vec<usize> = words_i
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            if *value == SLICE_BEGIN_MARKER {
                Some(index)
            } else {
                None
            }
        })
        .collect();
    if marker_indices.is_empty() {
        return Ok(Vec::new());
    }

    let mut summaries: HashMap<u32, SummaryBuilder, S> =
        HashMap::with_capacity_and_hasher(32_768, hasher);
    let mut callsigns = Vec::new();
    for (marker_offset, marker_index) in marker_indices.iter().copied().enumerate() {
        let block_start = marker_index + 4;
        let block_end = marker_indices
            .get(marker_offset + 1)
            .copied()
            .unwrap_or(words_i.len());
        let record_words = ((block_end.saturating_sub(block_start)) / 4) * 4;
        if record_words == 0 {
            continue;
        }
        let now = words_u[marker_index + 2] as f64 / 1000.0
            + words_u[marker_index + 1] as f64 * 4294967.296;
        let ts = python_round_to_i64(now);
        let end = block_start + record_words;
        let mut index = block_start;
        while index < end {
            let point0_u = words_u[index];
            let point1_i = words_i[index + 1];
            if valid_position(point1_i, words_i[index + 2]) {
                let point_id = point0_u & 0x01ff_ffff;
                if point_id <= MAX_ICAO_ID {
                    let lat = point1_i as f32 / 1_000_000.0_f32;
                    let lon = words_i[index + 2] as f32 / 1_000_000.0_f32;
                    let (alt_ft, on_ground) = decode_altitude(words_i[index + 3]);
                    update_summary(
                        &mut summaries,
                        point_id,
                        ts,
                        lat as f64,
                        lon as f64,
                        alt_ft,
                        on_ground,
                    );
                }
            } else if is_callsign_record(point1_i) {
                push_callsign(&mut callsigns, point0_u, ts, words_i[index + 2], words_i[index + 3]);
            }
            index += 4;
        }
    }

    let mut summaries = finish_summary_map(summaries);
    attach_callsigns(&mut summaries, callsigns);
    Ok(summaries)
}

fn summarize_direct_streaming<S: BuildHasher>(
    path: &Path,
    hasher: S,
) -> Result<Vec<AircraftFileSummary>> {
    let raw = fs::read(path)?;
    if raw.len() < 16 {
        return Ok(Vec::new());
    }
    let word_len = raw.len() / 4;
    let marker_indices = marker_indices_from_bytes(&raw);
    if marker_indices.is_empty() {
        return Ok(Vec::new());
    }

    let mut summaries: HashMap<u32, SummaryBuilder, S> =
        HashMap::with_capacity_and_hasher(32_768, hasher);
    let mut callsigns = Vec::new();
    for (marker_offset, marker_index) in marker_indices.iter().copied().enumerate() {
        let block_start = marker_index + 4;
        let block_end = marker_indices
            .get(marker_offset + 1)
            .copied()
            .unwrap_or(word_len);
        let record_words = ((block_end.saturating_sub(block_start)) / 4) * 4;
        if record_words == 0 {
            continue;
        }
        let now = read_u32_word(&raw, marker_index + 2) as f64 / 1000.0
            + read_u32_word(&raw, marker_index + 1) as f64 * 4294967.296;
        let ts = python_round_to_i64(now);
        let end = block_start + record_words;
        let mut index = block_start;
        while index < end {
            let point0_u = read_u32_word(&raw, index);
            let point1_i = read_i32_word(&raw, index + 1);
            if valid_position(point1_i, read_i32_word(&raw, index + 2)) {
                let point_id = point0_u & 0x01ff_ffff;
                if point_id <= MAX_ICAO_ID {
                    let lat = point1_i as f32 / 1_000_000.0_f32;
                    let lon = read_i32_word(&raw, index + 2) as f32 / 1_000_000.0_f32;
                    let (alt_ft, on_ground) = decode_altitude(read_i32_word(&raw, index + 3));
                    update_summary(
                        &mut summaries,
                        point_id,
                        ts,
                        lat as f64,
                        lon as f64,
                        alt_ft,
                        on_ground,
                    );
                }
            } else if is_callsign_record(point1_i) {
                push_callsign(&mut callsigns, point0_u, ts, read_i32_word(&raw, index + 2), read_i32_word(&raw, index + 3));
            }
            index += 4;
        }
    }

    let mut summaries = finish_summary_map(summaries);
    attach_callsigns(&mut summaries, callsigns);
    Ok(summaries)
}

fn build_word_vectors(raw: &[u8]) -> (Vec<i32>, Vec<u32>) {
    let word_len = raw.len() / 4;
    let mut words_i = Vec::with_capacity(word_len);
    let mut words_u = Vec::with_capacity(word_len);
    for chunk in raw.chunks_exact(4) {
        let bytes = [chunk[0], chunk[1], chunk[2], chunk[3]];
        words_i.push(i32::from_le_bytes(bytes));
        words_u.push(u32::from_le_bytes(bytes));
    }
    (words_i, words_u)
}

fn marker_indices_from_bytes(raw: &[u8]) -> Vec<usize> {
    let word_len = raw.len() / 4;
    let mut marker_indices = Vec::new();
    for index in 0..word_len {
        if read_i32_word(raw, index) == SLICE_BEGIN_MARKER {
            marker_indices.push(index);
        }
    }
    marker_indices
}

#[inline]
fn read_i32_word(raw: &[u8], word_index: usize) -> i32 {
    let offset = word_index * 4;
    i32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

#[inline]
fn read_u32_word(raw: &[u8], word_index: usize) -> u32 {
    let offset = word_index * 4;
    u32::from_le_bytes([
        raw[offset],
        raw[offset + 1],
        raw[offset + 2],
        raw[offset + 3],
    ])
}

#[inline]
fn valid_position(lat: i32, lon: i32) -> bool {
    (-90_000_000..=90_000_000).contains(&lat) && (-180_000_000..=180_000_000).contains(&lon)
}

#[inline]
fn decode_altitude(point3: i32) -> (Option<f64>, bool) {
    let mut alt_signed = point3 & 65_535;
    if (alt_signed & 32_768) != 0 {
        alt_signed |= -65_536;
    }
    let on_ground = alt_signed == -123;
    let alt_ft = if on_ground || alt_signed == -124 {
        None
    } else {
        Some((alt_signed as f32 * 25.0_f32) as f64)
    };
    (alt_ft, on_ground)
}

// readsb writes identification records with lat = (1 << 30) | squawk, the
// squawk's four octal digits read as a decimal number. The eight callsign
// bytes occupy the lon, altitude and speed fields.
#[inline]
fn is_callsign_record(lat: i32) -> bool {
    (lat as u32) & 0xffff_c000 == 0x4000_0000
}

// Accept the ADS-B identification alphabet (A-Z, 0-9, space) after trailing
// padding is removed. Decoding errors (@, punctuation, leading spaces), blank
// and all-zero identifications, and single characters are not callsigns.
fn decode_callsign(lon: i32, alt_gs: i32) -> Option<Callsign> {
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&lon.to_le_bytes());
    bytes[4..].copy_from_slice(&alt_gs.to_le_bytes());
    let len = bytes
        .iter()
        .rposition(|&c| c != b' ' && c != 0)
        .map_or(0, |i| i + 1);
    let text = &bytes[..len];
    if len < 2
        || !text[0].is_ascii_alphanumeric()
        || !text
            .iter()
            .all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b' ')
        || text.iter().all(|&c| c == b'0')
    {
        return None;
    }
    let mut packed = [0u8; 8];
    packed[..len].copy_from_slice(text);
    Some(u64::from_le_bytes(packed))
}

fn callsign_text(callsign: Callsign) -> String {
    let bytes = callsign.to_le_bytes();
    let len = bytes.iter().position(|&c| c == 0).unwrap_or(8);
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

#[inline]
fn push_callsign(
    callsigns: &mut Vec<(u32, i64, Callsign)>,
    id_word: u32,
    ts: i64,
    lon: i32,
    alt_gs: i32,
) {
    let id = id_word & 0x01ff_ffff;
    if id <= MAX_ICAO_ID {
        if let Some(callsign) = decode_callsign(lon, alt_gs) {
            callsigns.push((id, ts, callsign));
        }
    }
}

// Records are chronological per aircraft within a file. Identification for an
// aircraft without a usable position in this file is not retained.
fn attach_callsigns(summaries: &mut [AircraftFileSummary], mut observed: Vec<(u32, i64, Callsign)>) {
    observed.sort_by_key(|o| o.0);
    for group in observed.chunk_by(|a, b| a.0 == b.0) {
        if let Ok(i) = summaries.binary_search_by_key(&group[0].0, |s| s.icao_id) {
            for &(_, ts, callsign) in group {
                add_callsign(
                    &mut summaries[i].callsigns,
                    CallsignSpan {
                        callsign,
                        first_ts: ts,
                        last_ts: ts,
                        count: 1,
                    },
                );
            }
        }
    }
}

// Bounded tally of callsign runs. Reports within 15 minutes of a run extend
// it, so alternating feeds merge, while the same callsign on a later flight
// starts a new run. When full, the least reported run is dropped.
fn add_callsign(spans: &mut Vec<CallsignSpan>, span: CallsignSpan) {
    if let Some(s) = spans
        .iter_mut()
        .filter(|s| s.callsign == span.callsign)
        .max_by_key(|s| s.last_ts)
        .filter(|s| span.first_ts <= s.last_ts + 900)
    {
        s.first_ts = s.first_ts.min(span.first_ts);
        s.last_ts = s.last_ts.max(span.last_ts);
        s.count += span.count;
        return;
    }
    if spans.len() >= MAX_CALLSIGNS {
        let (i, _) = spans
            .iter()
            .enumerate()
            .min_by_key(|(_, s)| (s.count, s.last_ts))
            .unwrap();
        spans.remove(i);
    }
    spans.push(span);
}

// The callsign a movement was flown under: the one reported most while
// airborne on that side of it, between liftoff and the end of the track, or
// between the start of the track and touchdown. Tracks split at landings, so
// this is the flight's own majority. Ground time on the other side is
// excluded: a run's reports are assumed to be spread evenly, so a callsign
// kept through a long ground stop does not outweigh its successor. Brief
// decoding errors, alternating feeds and late corrections do not win. Reports
// within ten minutes outside the interval are a fallback, for reception that
// stops at liftoff or starts at touchdown.
fn callsign_for(spans: &[CallsignSpan], lo: i64, hi: i64) -> Option<Callsign> {
    for margin in [0, 600] {
        let (lo, hi) = (lo - margin, hi + margin);
        let mut scores: Vec<(Callsign, f64)> = Vec::new();
        for s in spans.iter().filter(|s| s.first_ts <= hi && s.last_ts >= lo) {
            // Reports repeat at least once a minute, so a run covers one more minute.
            let duration = (s.last_ts - s.first_ts + 60) as f64;
            let overlap = (s.last_ts.min(hi) - s.first_ts.max(lo) + 60) as f64;
            let score = s.count as f64 * overlap.min(duration) / duration;
            match scores.iter_mut().find(|(c, _)| *c == s.callsign) {
                Some(entry) => entry.1 += score,
                None => scores.push((s.callsign, score)),
            }
        }
        // Ties keep the earlier-recorded run, which is deterministic on replay.
        if let Some((c, _)) = scores
            .iter()
            .rev()
            .max_by(|a, b| a.1.total_cmp(&b.1))
        {
            return Some(*c);
        }
    }
    None
}

fn summaries_from_points(mut points: Vec<RawPoint>) -> Vec<AircraftFileSummary> {
    if points.is_empty() {
        return Vec::new();
    }
    points.sort_by_key(|point| point.icao_id);
    let mut summaries = Vec::new();
    let mut index = 0;
    while index < points.len() {
        let icao_id = points[index].icao_id;
        let first = points[index];
        let mut summary = SummaryBuilder::new(
            first.ts,
            first.lat,
            first.lon,
            first.alt_ft,
            first.on_ground,
        );
        index += 1;
        while index < points.len() && points[index].icao_id == icao_id {
            let point = points[index];
            summary.add(
                point.ts,
                point.lat,
                point.lon,
                point.alt_ft,
                point.on_ground,
            );
            index += 1;
        }
        summaries.push(summary.finish(icao_id));
    }
    summaries
}

#[inline(always)]
fn update_summary<S: BuildHasher>(
    summaries: &mut HashMap<u32, SummaryBuilder, S>,
    icao_id: u32,
    ts: i64,
    lat: f64,
    lon: f64,
    alt_ft: Option<f64>,
    on_ground: bool,
) {
    summaries
        .entry(icao_id)
        .and_modify(|summary| summary.add(ts, lat, lon, alt_ft, on_ground))
        .or_insert_with(|| SummaryBuilder::new(ts, lat, lon, alt_ft, on_ground));
}

fn finish_summary_map<S: BuildHasher>(
    mut summaries: HashMap<u32, SummaryBuilder, S>,
) -> Vec<AircraftFileSummary> {
    let mut keys: Vec<u32> = summaries.keys().copied().collect();
    keys.sort_unstable();
    keys.into_iter()
        .map(|icao_id| summaries.remove(&icao_id).unwrap().finish(icao_id))
        .collect()
}

// Each slot is read from the cache, else from a read-only archive with the same layout.
fn iter_heatmap_files(
    cache_dir: &Path,
    archive_dir: Option<&Path>,
    start_ts: Option<i64>,
    end_ts: Option<i64>,
) -> Result<Vec<PathBuf>> {
    if let (Some(start), Some(end)) = (start_ts, end_ts) {
        let mut files = Vec::new();
        let mut cursor = floor_to_slot(start);
        if cursor < start {
            cursor += 1800;
        }
        while cursor < end {
            let path = heatmap_path_for(cache_dir, cursor);
            if path.exists() {
                files.push(path);
            } else if let Some(archived) = archive_dir
                .map(|dir| heatmap_path_for(dir, cursor))
                .filter(|p| p.exists())
            {
                files.push(archived);
            }
            cursor += 1800;
        }
        return Ok(files);
    }
    Err(anyhow!("Rust builder currently requires --start and --end"))
}

fn heatmap_path_for(cache_dir: &Path, timestamp: i64) -> PathBuf {
    let dt = Utc.timestamp_opt(timestamp, 0).unwrap();
    let slot = dt.hour() * 2 + dt.minute() / 30;
    cache_dir
        .join(format!("{:04}", dt.year()))
        .join(format!("{:02}", dt.month()))
        .join(format!("{:02}", dt.day()))
        .join("heatmap")
        .join(format!("{slot:02}.bin.ttf"))
}

fn slot_timestamp_from_path(path: &Path) -> Result<i64> {
    let slot_text = path
        .file_stem()
        .and_then(|value| value.to_str())
        .and_then(|value| value.split('.').next())
        .ok_or_else(|| anyhow!("invalid slot filename {}", path.display()))?;
    let slot: i64 = slot_text.parse()?;
    // The upstream layout puts slots in a heatmap/ directory below the day.
    let path = match path.parent() {
        Some(parent) if parent.file_name().is_some_and(|n| n == "heatmap") => parent,
        _ => path,
    };
    let day: u32 = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|v| v.to_str())
        .unwrap()
        .parse()?;
    let month: u32 = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|v| v.to_str())
        .unwrap()
        .parse()?;
    let year: i32 = path
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .and_then(|p| p.file_name())
        .and_then(|v| v.to_str())
        .unwrap()
        .parse()?;
    let base = Utc
        .with_ymd_and_hms(year, month, day, 0, 0, 0)
        .unwrap()
        .timestamp();
    Ok(base + slot * 1800)
}

fn parse_time_arg(value: Option<&str>) -> Result<Option<i64>> {
    let Some(text) = value else {
        return Ok(None);
    };
    if text.contains('T') {
        let parsed = DateTime::parse_from_rfc3339(&text.replace('Z', "+00:00"))?;
        return Ok(Some(parsed.with_timezone(&Utc).timestamp()));
    }
    let date = NaiveDate::parse_from_str(text, "%Y-%m-%d")?;
    Ok(Some(
        date.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp(),
    ))
}

fn floor_to_slot(timestamp: i64) -> i64 {
    timestamp - timestamp.rem_euclid(1800)
}

fn cell_for(cell_degrees: f64, lat: f64, lon: f64) -> (i32, i32) {
    (
        (lat / cell_degrees).floor() as i32,
        (lon / cell_degrees).floor() as i32,
    )
}

fn haversine_from_query_km(lat: f64, lon: f64, cos_phi1: f64, airport: &Airport) -> f64 {
    let d_phi = (airport.lat - lat).to_radians();
    let d_lambda = (airport.lon - lon).to_radians();
    let a = (d_phi / 2.0).sin().powi(2)
        + cos_phi1 * airport.cos_lat_rad * (d_lambda / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * a.sqrt().atan2((1.0 - a).sqrt())
}

fn endpoint_agl(point: &TrackPoint, airport: &AirportEndpoint) -> Option<f64> {
    if point.on_ground {
        return Some(0.0);
    }
    if let (Some(alt), Some(elevation)) = (point.alt_ft, airport.elevation_ft) {
        return Some((alt - elevation).max(0.0));
    }
    point.alt_ft
}

fn endpoint_altitude_for(candidate: &EndpointCandidate, points: &[TrackPoint]) -> f64 {
    for point in points {
        if point.ts == candidate.ts {
            if let Some(altitude) = point.alt_ft {
                return altitude;
            }
        }
    }
    candidate.airport_elevation_ft.unwrap_or(0.0)
}

fn python_round_to_i64(value: f64) -> i64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction > 0.5 {
        floor as i64 + 1
    } else if fraction < 0.5 {
        floor as i64
    } else {
        let floor_i = floor as i64;
        if floor_i % 2 == 0 {
            floor_i
        } else {
            floor_i + 1
        }
    }
}

// Private v3 checkpoint encoding: 33 bytes per observation, little endian.
// Avoid formatting/parsing floating-point JSON for every active aircraft every file.
fn points_blob(points: &[TrackPoint]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(points.len() * 33);
    for p in points {
        bytes.extend_from_slice(&p.ts.to_le_bytes());
        bytes.extend_from_slice(&p.lat.to_le_bytes());
        bytes.extend_from_slice(&p.lon.to_le_bytes());
        bytes.extend_from_slice(&p.alt_ft.unwrap_or(0.0).to_le_bytes());
        bytes.push((p.on_ground as u8) | ((p.alt_ft.is_some() as u8) << 1));
    }
    bytes
}
fn parse_points_blob(bytes: &[u8]) -> rusqlite::Result<Vec<TrackPoint>> {
    if bytes.len() % 33 != 0 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    bytes
        .chunks_exact(33)
        .map(|b| {
            if b[32] > 3 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let point = TrackPoint {
                ts: i64::from_le_bytes(b[0..8].try_into().unwrap()),
                lat: f64::from_le_bytes(b[8..16].try_into().unwrap()),
                lon: f64::from_le_bytes(b[16..24].try_into().unwrap()),
                alt_ft: if b[32] & 2 != 0 {
                    Some(f64::from_le_bytes(b[24..32].try_into().unwrap()))
                } else {
                    None
                },
                on_ground: b[32] & 1 != 0,
            };
            if !point.lat.is_finite()
                || !point.lon.is_finite()
                || point.alt_ft.is_some_and(|a| !a.is_finite())
            {
                return Err(rusqlite::Error::InvalidQuery);
            }
            Ok(point)
        })
        .collect()
}

// Private checkpoint encoding: 28 bytes per callsign span, little endian.
fn callsigns_blob(spans: &[CallsignSpan]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(spans.len() * 28);
    for s in spans {
        bytes.extend_from_slice(&s.callsign.to_le_bytes());
        bytes.extend_from_slice(&s.first_ts.to_le_bytes());
        bytes.extend_from_slice(&s.last_ts.to_le_bytes());
        bytes.extend_from_slice(&s.count.to_le_bytes());
    }
    bytes
}
fn parse_callsigns_blob(bytes: &[u8]) -> rusqlite::Result<Vec<CallsignSpan>> {
    if bytes.len() % 28 != 0 || bytes.len() / 28 > MAX_CALLSIGNS {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(bytes
        .chunks_exact(28)
        .map(|b| CallsignSpan {
            callsign: u64::from_le_bytes(b[0..8].try_into().unwrap()),
            first_ts: i64::from_le_bytes(b[8..16].try_into().unwrap()),
            last_ts: i64::from_le_bytes(b[16..24].try_into().unwrap()),
            count: u32::from_le_bytes(b[24..28].try_into().unwrap()),
        })
        .collect())
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn ensure_parent(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn remove_sqlite_family(path: &Path) -> Result<()> {
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() {
            fs::remove_file(candidate)?;
        }
    }
    Ok(())
}

trait DateParts {
    fn year(&self) -> i32;
    fn month(&self) -> u32;
    fn day(&self) -> u32;
    fn hour(&self) -> u32;
    fn minute(&self) -> u32;
}

impl DateParts for DateTime<Utc> {
    fn year(&self) -> i32 {
        chrono::Datelike::year(self)
    }
    fn month(&self) -> u32 {
        chrono::Datelike::month(self)
    }
    fn day(&self) -> u32 {
        chrono::Datelike::day(self)
    }
    fn hour(&self) -> u32 {
        chrono::Timelike::hour(self)
    }
    fn minute(&self) -> u32 {
        chrono::Timelike::minute(self)
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    fn bytes(records: &[(u32, i32, i32, i32)]) -> Vec<u8> {
        let mut b = Vec::new();
        for n in [
            1_u32,
            0,
            0,
            0,
            SLICE_BEGIN_MARKER as u32,
            0,
            1_700_000_000,
            0,
        ] {
            b.extend(n.to_le_bytes());
        }
        for &(id, lat, lon, alt) in records {
            b.extend(id.to_le_bytes());
            b.extend(lat.to_le_bytes());
            b.extend(lon.to_le_bytes());
            b.extend(alt.to_le_bytes());
        }
        b
    }

    #[test]
    fn metadata_is_not_telemetry() {
        // Actual non-position metadata words from the Calgary holdout.
        assert!(!valid_position(1_073_741_824, 1_464_816_725));
        assert!(!valid_position(-1_073_741_824, 0));
        assert!(!valid_position(0, 180_000_001));
        assert!(valid_position(-90_000_000, -180_000_000));
        let raw = bytes(&[
            (0xc1ea75, 51_127_019, -114_005_337, -123),
            (0xc1ea75, 1_073_741_824, 1_464_816_725, 540_487_989),
        ]);
        let summary = summarize_sort_vec_from_bytes(&raw).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].point_count, 1);
        assert_eq!(summary[0].max_altitude_ft, None);
    }

    #[test]
    fn preserve_interior_ground_without_retaining_cruise() {
        let mut s = SummaryBuilder::new(0, 25.8, -80.4, Some(1300.0), false);
        s.add(240, 25.8058, -80.2697, None, true);
        s.add(360, 25.7984, -80.3563, Some(875.0), false);
        let result = s.finish(1);
        assert_eq!(result.last_terminal.unwrap().ts, 240);
        assert!(result.last_terminal.unwrap().on_ground);
        assert_eq!(result.point_count, 3);
        let mut cruise = SummaryBuilder::new(0, 0.0, 0.0, Some(30000.0), false);
        for i in 1..180 {
            cruise.add(i * 10, 0.0, 0.0, Some(30000.0), false);
        }
        let result = cruise.finish(2);
        assert!(result.first_terminal.is_none() && result.last_terminal.is_none());
    }

    fn airport(
        id: i64,
        lat: f64,
        lon: f64,
        heliport: bool,
        water: bool,
        small: bool,
        ultralight: bool,
    ) -> Airport {
        Airport {
            airport_id: id,
            lat,
            lon,
            elevation_ft: Some(0.0),
            cos_lat_rad: lat.to_radians().cos(),
            heliport,
            water,
            small,
            ultralight,
        }
    }
    fn index(airports: Vec<Airport>) -> AirportIndex {
        let mut grid = HashMap::<_, Vec<_>>::default();
        for a in airports {
            grid.entry(cell_for(0.1, a.lat, a.lon)).or_default().push(a);
        }
        AirportIndex {
            cell_degrees: 0.1,
            grid,
        }
    }
    #[test]
    fn aircraft_compatibility_preserves_floatplanes_and_heliports() {
        let i = index(vec![
            airport(1, 0.0, 0.0, true, false, false, false),
            airport(2, 0.0, 0.002, false, true, false, false),
            airport(3, 0.0, 0.01, false, false, false, false),
        ]);
        assert_eq!(i.nearest(0.0, 0.0, 12.0, 0).unwrap().airport_id, 1); // rotorcraft
        assert_eq!(i.nearest(0.0, 0.0, 12.0, 1).unwrap().airport_id, 2); // amphibious/unknown fixed wing retains water candidate
        assert_eq!(i.nearest(0.0, 0.0, 12.0, 2).unwrap().airport_id, 3); // known jets
    }
    #[test]
    fn small_airports_are_not_blanket_excluded() {
        let i = index(vec![
            airport(1, 0.0, 0.0, false, false, true, false),
            airport(2, 0.0, 0.07, false, false, false, false),
        ]);
        assert_eq!(i.nearest(0.0, 0.001, 12.0, 2).unwrap().airport_id, 1);
        let i = index(vec![
            airport(1, 0.0, 0.01, false, false, true, false),
            airport(2, 0.0, 0.017, false, false, false, false),
        ]);
        assert_eq!(i.nearest(0.0, 0.0, 12.0, 2).unwrap().airport_id, 2);
    }
    #[test]
    fn all_parsers_filter_metadata_identically() {
        let dir = env::temp_dir().join(format!("adsbfi-parser-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("input.bin");
        fs::write(
            &file,
            bytes(&[
                // Identification can precede the position in a slice.
                (42, (1 << 30) | 1200, 1_464_816_725, 540_487_989),
                (42, 40_000_000, -74_000_000, -123),
                (42, 1_073_741_824, 1_464_816_725, 540_487_989),
                (42, 1_073_741_824, 0x4040_4040, 0x4040_4040),
                // Non-ICAO addresses and aircraft without positions are not retained.
                (43 | 1 << 24, 1_073_741_824, 1_464_816_725, 540_487_989),
                (44, 1_073_741_824, 1_464_816_725, 540_487_989),
            ]),
        )
        .unwrap();
        for mode in [
            ParserMode::SortVec,
            ParserMode::StreamWords,
            ParserMode::DirectSort,
            ParserMode::MmapSort,
            ParserMode::StreamWordsFx,
            ParserMode::DirectStreamFx,
        ] {
            let summaries = summarize_heatmap_file(&file, mode).unwrap();
            assert_eq!(summaries.len(), 1);
            assert_eq!(summaries[0].point_count, 1);
            assert!(summaries[0].max_altitude_ft.is_none());
            assert_eq!(summaries[0].callsigns.len(), 1);
            let span = summaries[0].callsigns[0];
            assert_eq!(callsign_text(span.callsign), "UTOW517");
            assert_eq!((span.first_ts, span.last_ts, span.count), (1_700_000, 1_700_000, 2));
        }
        fs::remove_file(file).unwrap();
        fs::remove_dir(dir).unwrap();
    }
    #[test]
    fn checkpoint_preserves_exact_observation_coordinates() {
        let p = TrackPoint {
            ts: 1,
            lat: 52.04886245727539,
            lon: -1.125862956047058,
            alt_ft: Some(1275.0),
            on_ground: false,
        };
        let decoded = parse_points_blob(&points_blob(&[p])).unwrap();
        assert_eq!(decoded[0].lat.to_bits(), p.lat.to_bits());
        assert_eq!(decoded[0].lon.to_bits(), p.lon.to_bits());
    }
    #[test]
    fn airport_cache_does_not_alias_nearby_observations() {
        let i = Arc::new(index(vec![
            airport(1, 0.0, 0.0, false, false, false, false),
            airport(2, 0.0, 0.0004, false, false, false, false),
        ]));
        let mut c = NearestAirportCache::new(i, 12.0, 2);
        assert_eq!(c.nearest(0.0, 0.0).unwrap().airport_id, 1);
        assert_eq!(c.nearest(0.0, 0.0004).unwrap().airport_id, 2);
        assert_eq!(c.nearest(0.0, 0.0).unwrap().distance_km, 0.0);
    }
}

#[cfg(test)]
mod continuity_tests;
