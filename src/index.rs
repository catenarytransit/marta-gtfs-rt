use chrono::{Datelike, NaiveDate, Weekday};
use gtfs_structures::{DirectionType, Exception, Gtfs, RouteType};
use std::collections::{HashMap, HashSet};

pub(crate) type StationId = u32;
pub(crate) type PatternId = usize;
pub(crate) type TripIndex = usize;

#[derive(Clone, Debug)]
pub(crate) struct IndexedTrip {
    pub trip_id: String,
    pub route_id: String,
    pub service_id: String,
    pub headsign: Option<String>,
    pub direction_id: Option<u32>,
    pub pattern_id: PatternId,
    pub stop_ids: Vec<String>,
    pub stop_sequences: Vec<u32>,
    pub arrival_seconds: Vec<Option<u32>>,
}

#[derive(Clone, Debug)]
pub(crate) struct Pattern {
    pub line: String,
    pub stations: Vec<StationId>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ScheduledStopRef {
    pub seconds: u32,
    /// u32 keeps each posting compact (8 bytes with `seconds`) while the main vectors use usize.
    pub trip_index: u32,
}

#[derive(Clone, Debug)]
struct WeeklyCalendar {
    start_date: NaiveDate,
    end_date: NaiveDate,
    weekdays: u8,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ServiceRule {
    weekly: Option<WeeklyCalendar>,
    exceptions: HashMap<NaiveDate, bool>,
}

impl ServiceRule {
    pub fn runs_on(&self, date: NaiveDate) -> bool {
        if let Some(added) = self.exceptions.get(&date) {
            return *added;
        }

        let Some(weekly) = &self.weekly else {
            return false;
        };
        if date < weekly.start_date || date > weekly.end_date {
            return false;
        }

        let bit = match date.weekday() {
            Weekday::Mon => 1 << 0,
            Weekday::Tue => 1 << 1,
            Weekday::Wed => 1 << 2,
            Weekday::Thu => 1 << 3,
            Weekday::Fri => 1 << 4,
            Weekday::Sat => 1 << 5,
            Weekday::Sun => 1 << 6,
        };
        weekly.weekdays & bit != 0
    }
}

#[derive(Clone, Debug)]
pub struct MartaScheduleIndex {
    pub(crate) trips: Vec<IndexedTrip>,
    pub(crate) patterns: Vec<Pattern>,
    pub(crate) station_aliases: HashMap<String, Vec<StationId>>,
    /// Per line and logical station, stop-time entries sorted by GTFS seconds since
    /// service-day midnight. This is the hot-path index used to binary-search a small
    /// trip candidate set without touching unrelated rail lines.
    pub(crate) schedules_by_line_station: HashMap<String, HashMap<StationId, Vec<ScheduledStopRef>>>,
    pub(crate) services: HashMap<String, ServiceRule>,
    pub(crate) rail_route_count: usize,
}

#[derive(Hash, PartialEq, Eq)]
struct PatternKey {
    line: String,
    stations: Vec<StationId>,
}

impl MartaScheduleIndex {
    pub fn build(gtfs: &Gtfs) -> Self {
        let rail_routes: HashMap<&str, String> = gtfs
            .routes
            .values()
            .filter(|route| route.route_type == RouteType::Subway)
            .filter_map(|route| {
                let line = route
                    .short_name
                    .as_deref()
                    .or(route.long_name.as_deref())
                    .map(normalize_line)?;
                Some((route.id.as_str(), line))
            })
            .collect();

        let mut canonical_station_to_id = HashMap::<String, StationId>::new();
        let mut station_aliases = HashMap::<String, Vec<StationId>>::new();
        let mut next_station_id: StationId = 0;

        let mut patterns = Vec::<Pattern>::new();
        let mut pattern_lookup = HashMap::<PatternKey, PatternId>::new();
        let mut trips = Vec::<IndexedTrip>::new();
        let mut used_service_ids = HashSet::<String>::new();

        for trip in gtfs.trips.values() {
            let Some(line) = rail_routes.get(trip.route_id.as_str()) else {
                continue;
            };
            if trip.stop_times.len() < 2 {
                continue;
            }

            let mut stations = Vec::with_capacity(trip.stop_times.len());
            let mut stop_ids = Vec::with_capacity(trip.stop_times.len());
            let mut stop_sequences = Vec::with_capacity(trip.stop_times.len());
            let mut arrival_seconds = Vec::with_capacity(trip.stop_times.len());

            for stop_time in &trip.stop_times {
                let stop = &stop_time.stop;
                let canonical_key = stop
                    .parent_station
                    .as_deref()
                    .unwrap_or(stop.id.as_str())
                    .to_string();

                let station_id = *canonical_station_to_id.entry(canonical_key.clone()).or_insert_with(|| {
                    let id = next_station_id;
                    next_station_id += 1;
                    id
                });

                add_stop_aliases(&mut station_aliases, station_id, stop.name.as_deref());
                if let Some(parent_id) = stop.parent_station.as_deref() {
                    if let Some(parent) = gtfs.stops.get(parent_id) {
                        add_stop_aliases(&mut station_aliases, station_id, parent.name.as_deref());
                    }
                }

                stations.push(station_id);
                stop_ids.push(stop.id.clone());
                stop_sequences.push(stop_time.stop_sequence);
                arrival_seconds.push(stop_time.arrival_time.or(stop_time.departure_time));
            }

            let pattern_key = PatternKey {
                line: line.clone(),
                stations: stations.clone(),
            };

            let pattern_id = if let Some(existing) = pattern_lookup.get(&pattern_key) {
                *existing
            } else {
                let id = patterns.len();
                patterns.push(Pattern {
                    line: line.clone(),
                    stations: stations.clone(),
                });
                pattern_lookup.insert(pattern_key, id);
                id
            };

            trips.push(IndexedTrip {
                trip_id: trip.id.clone(),
                route_id: trip.route_id.clone(),
                service_id: trip.service_id.clone(),
                headsign: trip.trip_headsign.as_deref().map(normalize_station_name),
                direction_id: match trip.direction_id {
                    Some(DirectionType::Outbound) => Some(0),
                    Some(DirectionType::Inbound) => Some(1),
                    None => None,
                },
                pattern_id,
                stop_ids,
                stop_sequences,
                arrival_seconds,
            });
            used_service_ids.insert(trip.service_id.clone());
        }

        for ids in station_aliases.values_mut() {
            ids.sort_unstable();
            ids.dedup();
        }

        // Inverted schedule index: logical station -> sorted (scheduled seconds, trip).
        // It costs O(S) additional compact entries, but changes the realtime hot path from
        // scanning all trips in a pattern to O(log N + K) candidate discovery.
        let mut schedules_by_line_station =
            HashMap::<String, HashMap<StationId, Vec<ScheduledStopRef>>>::new();
        for (trip_index, trip) in trips.iter().enumerate() {
            let compact_trip_index = u32::try_from(trip_index)
                .expect("MARTA rail trip count exceeds u32::MAX");
            let pattern = &patterns[trip.pattern_id];
            let line_index = schedules_by_line_station
                .entry(pattern.line.clone())
                .or_default();
            for (stop_index, station_id) in pattern.stations.iter().copied().enumerate() {
                let Some(seconds) = trip.arrival_seconds.get(stop_index).copied().flatten() else {
                    continue;
                };
                line_index
                    .entry(station_id)
                    .or_default()
                    .push(ScheduledStopRef {
                        seconds,
                        trip_index: compact_trip_index,
                    });
            }
        }
        for line_index in schedules_by_line_station.values_mut() {
            for entries in line_index.values_mut() {
                entries.sort_unstable_by_key(|entry| entry.seconds);
            }
        }

        let mut services = HashMap::with_capacity(used_service_ids.len());
        for service_id in used_service_ids {
            let weekly = gtfs.calendar.get(&service_id).map(|calendar| {
                let mut weekdays = 0u8;
                if calendar.monday { weekdays |= 1 << 0; }
                if calendar.tuesday { weekdays |= 1 << 1; }
                if calendar.wednesday { weekdays |= 1 << 2; }
                if calendar.thursday { weekdays |= 1 << 3; }
                if calendar.friday { weekdays |= 1 << 4; }
                if calendar.saturday { weekdays |= 1 << 5; }
                if calendar.sunday { weekdays |= 1 << 6; }
                WeeklyCalendar {
                    start_date: calendar.start_date,
                    end_date: calendar.end_date,
                    weekdays,
                }
            });

            let mut exceptions = HashMap::new();
            if let Some(calendar_dates) = gtfs.calendar_dates.get(&service_id) {
                for exception in calendar_dates {
                    exceptions.insert(
                        exception.date,
                        matches!(exception.exception_type, Exception::Added),
                    );
                }
            }

            services.insert(service_id, ServiceRule { weekly, exceptions });
        }

        Self {
            trips,
            patterns,
            station_aliases,
            schedules_by_line_station,
            services,
            rail_route_count: rail_routes.len(),
        }
    }

    pub(crate) fn schedule_entry_count(&self, line: &str, station_id: StationId) -> usize {
        self.schedules_by_line_station
            .get(line)
            .and_then(|line_index| line_index.get(&station_id))
            .map_or(0, Vec::len)
    }

    pub(crate) fn trip_candidates_at(
        &self,
        line: &str,
        station_id: StationId,
        target_seconds: i64,
        tolerance_seconds: i64,
    ) -> &[ScheduledStopRef] {
        let Some(entries) = self
            .schedules_by_line_station
            .get(line)
            .and_then(|line_index| line_index.get(&station_id))
        else {
            return &[];
        };

        let tolerance = tolerance_seconds.max(0);
        let raw_lower = target_seconds.saturating_sub(tolerance);
        let raw_upper = target_seconds.saturating_add(tolerance);
        if raw_upper < 0 || raw_lower > u32::MAX as i64 {
            return &[];
        }
        let lower = raw_lower.clamp(0, u32::MAX as i64) as u32;
        let upper = raw_upper.clamp(0, u32::MAX as i64) as u32;

        let start = entries.partition_point(|entry| entry.seconds < lower);
        let end = entries.partition_point(|entry| entry.seconds <= upper);
        &entries[start..end]
    }

    pub fn rail_route_count(&self) -> usize {
        self.rail_route_count
    }

    pub fn indexed_trip_count(&self) -> usize {
        self.trips.len()
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }
}

pub(crate) fn normalize_line(value: &str) -> String {
    value.trim().to_ascii_uppercase()
}

pub(crate) fn normalize_station_name(value: &str) -> String {
    let upper = value.trim().to_ascii_uppercase();
    let mut cleaned = String::with_capacity(upper.len());
    let mut previous_space = true;

    for ch in upper.chars() {
        if ch.is_ascii_alphanumeric() {
            cleaned.push(ch);
            previous_space = false;
        } else if !previous_space {
            cleaned.push(' ');
            previous_space = true;
        }
    }

    let mut words: Vec<&str> = cleaned.split_whitespace().collect();
    while matches!(words.last(), Some(&"STATION") | Some(&"STA")) {
        words.pop();
    }

    words
        .into_iter()
        .map(|word| match word {
            "FT" => "FORT",
            "CTR" => "CENTER",
            _ => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn add_stop_aliases(
    aliases: &mut HashMap<String, Vec<StationId>>,
    station_id: StationId,
    name: Option<&str>,
) {
    let Some(name) = name else { return; };
    let normalized = normalize_station_name(name);
    if !normalized.is_empty() {
        aliases.entry(normalized.clone()).or_default().push(station_id);
    }

    // MARTA uses both the expanded and abbreviated form of Hamilton E. Holmes in
    // rider-facing systems.
    if normalized == "H E HOLMES" {
        aliases
            .entry("HAMILTON E HOLMES".to_string())
            .or_default()
            .push(station_id);
    } else if normalized == "HAMILTON E HOLMES" {
        aliases
            .entry("H E HOLMES".to_string())
            .or_default()
            .push(station_id);
    }

    // MARTA sometimes uses shortened station names in the realtime feed. Keep useful
    // components of compound GTFS names as aliases, e.g. "Lakewood-Ft. McPherson".
    for component in name.split(['/', '-', '&']) {
        let alias = normalize_station_name(component);
        if alias.len() >= 4 {
            aliases.entry(alias).or_default().push(station_id);
        }
    }

    // A few rider-facing names differ only by a generic suffix. This is safe because
    // aliases map to a list and ambiguity is resolved later by line + stop order + time.
    for suffix in [" CENTER", " PARK"] {
        if let Some(shorter) = normalized.strip_suffix(suffix) {
            if shorter.len() >= 4 {
                aliases.entry(shorter.to_string()).or_default().push(station_id);
            }
        }
    }
}
