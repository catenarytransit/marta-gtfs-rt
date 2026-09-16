use crate::index::{normalize_line, normalize_station_name, MartaScheduleIndex, StationId, TripIndex};
use crate::models::MartaTrainRow;
use crate::time::{parse_delay_seconds, parse_event_time, parse_next_arrival};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime};
use chrono_tz::Tz;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub struct MatcherConfig {
    /// Maximum mean absolute difference between the schedule time reconstructed
    /// from MARTA and the GTFS stop_times for a candidate trip.
    pub max_mean_schedule_error_seconds: i64,
    /// Maximum single-stop difference allowed for a candidate trip.
    pub max_stop_schedule_error_seconds: i64,
    /// Looser bound used to validate an already cached TRAIN_ID -> trip assignment.
    pub cached_assignment_max_mean_error_seconds: i64,
    /// Minimum resolved realtime stops required for a new assignment.
    pub minimum_resolved_stops: usize,
}

impl Default for MatcherConfig {
    fn default() -> Self {
        Self {
            max_mean_schedule_error_seconds: 8 * 60,
            max_stop_schedule_error_seconds: 15 * 60,
            cached_assignment_max_mean_error_seconds: 10 * 60,
            minimum_resolved_stops: 2,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Observation {
    pub row_index: usize,
    pub station_candidates: Vec<StationId>,
    pub event_time: DateTime<Tz>,
    pub predicted_arrival: DateTime<Tz>,
    pub scheduled_local: NaiveDateTime,
}

#[derive(Clone, Debug)]
pub(crate) struct MatchScore {
    pub trip_index: TripIndex,
    pub service_date: NaiveDate,
    pub row_stop_positions: Vec<(usize, usize)>, // (realtime row index, trip stop index)
    pub mean_abs_error_seconds: i64,
    pub max_abs_error_seconds: i64,
    pub destination_matches: bool,
    pub skipped_pattern_stops: usize,
}

impl MatchScore {
    fn better_than(&self, other: &Self) -> bool {
        // Reconstructed schedule time is the strongest signal. Destination/headsign is only a
        // tie-breaker because rider-facing destination strings occasionally differ from GTFS.
        self.mean_abs_error_seconds < other.mean_abs_error_seconds
            || (self.mean_abs_error_seconds == other.mean_abs_error_seconds
                && self.max_abs_error_seconds < other.max_abs_error_seconds)
            || (self.mean_abs_error_seconds == other.mean_abs_error_seconds
                && self.max_abs_error_seconds == other.max_abs_error_seconds
                && self.destination_matches > other.destination_matches)
            || (self.mean_abs_error_seconds == other.mean_abs_error_seconds
                && self.max_abs_error_seconds == other.max_abs_error_seconds
                && self.destination_matches == other.destination_matches
                && self.skipped_pattern_stops < other.skipped_pattern_stops)
    }
}

pub(crate) fn build_observations(
    index: &MartaScheduleIndex,
    rows: &[MartaTrainRow],
) -> Vec<Observation> {
    let mut observations = rows
        .iter()
        .enumerate()
        .filter_map(|(row_index, row)| {
            let event_time = parse_event_time(&row.event_time)?;
            let waiting_seconds = row.waiting_seconds.trim().parse::<i64>().ok();
            let predicted_arrival = parse_next_arrival(&event_time, &row.next_arr, waiting_seconds)?;
            let delay_seconds = parse_delay_seconds(&row.delay)?;
            let scheduled_local = predicted_arrival.naive_local() - Duration::seconds(delay_seconds);
            let station_candidates = resolve_station(index, &row.station);
            if station_candidates.is_empty() {
                return None;
            }
            Some(Observation {
                row_index,
                station_candidates,
                event_time,
                predicted_arrival,
                scheduled_local,
            })
        })
        .collect::<Vec<_>>();

    observations.sort_by_key(|obs| obs.predicted_arrival.timestamp());

    // Keep one prediction per station occurrence. The API can occasionally repeat a row;
    // retaining duplicates would incorrectly require the GTFS pattern to visit a station twice.
    let mut seen = HashSet::<String>::new();
    observations.retain(|obs| {
        let key = normalize_station_name(&rows[obs.row_index].station);
        seen.insert(key)
    });
    observations
}

pub(crate) fn match_train(
    index: &MartaScheduleIndex,
    rows: &[MartaTrainRow],
    config: &MatcherConfig,
    preferred_trip: Option<(TripIndex, NaiveDate)>,
) -> Option<MatchScore> {
    let line = rows.first().map(|row| normalize_line(&row.line))?;
    let destination = rows
        .first()
        .map(|row| normalize_station_name(&row.destination))
        .unwrap_or_default();
    let observations = build_observations(index, rows);
    if observations.is_empty() {
        return None;
    }

    if let Some((trip_index, service_date)) = preferred_trip {
        if let Some(score) = score_specific_trip(
            index,
            trip_index,
            service_date,
            &line,
            &destination,
            &observations,
        ) {
            if score.mean_abs_error_seconds <= config.cached_assignment_max_mean_error_seconds
                && score.max_abs_error_seconds <= config.max_stop_schedule_error_seconds
            {
                return Some(score);
            }
        }
    }

    if observations.len() < config.minimum_resolved_stops {
        return None;
    }

    let candidate_dates = candidate_service_dates(&observations);

    // Choose the observation with the smallest static schedule posting list. This is the
    // equivalent of choosing the most selective database index: a branch-only station has a
    // much shorter posting list than a central transfer station such as Five Points.
    let anchor = observations
        .iter()
        .filter_map(|observation| {
            let postings = observation
                .station_candidates
                .iter()
                .map(|station_id| index.schedule_entry_count(&line, *station_id))
                .sum::<usize>();
            (postings > 0).then_some((postings, observation))
        })
        .min_by_key(|(postings, _)| *postings)
        .map(|(_, observation)| observation)?;

    let mut candidate_pairs = HashSet::<(TripIndex, NaiveDate)>::new();
    for service_date in candidate_dates {
        let Some(midnight) = service_date.and_hms_opt(0, 0, 0) else {
            continue;
        };
        let target_seconds = (anchor.scheduled_local - midnight).num_seconds();
        for station_id in &anchor.station_candidates {
            for posting in index.trip_candidates_at(
                &line,
                *station_id,
                target_seconds,
                config.max_stop_schedule_error_seconds,
            ) {
                candidate_pairs.insert((posting.trip_index as usize, service_date));
            }
        }
    }

    // HashSet gives cheap deduplication, then sort to make exact-tie behavior deterministic.
    let mut candidate_pairs = candidate_pairs.into_iter().collect::<Vec<_>>();
    candidate_pairs.sort_by(|(trip_a, date_a), (trip_b, date_b)| {
        date_a
            .cmp(date_b)
            .then_with(|| index.trips[*trip_a].trip_id.cmp(&index.trips[*trip_b].trip_id))
    });

    let mut best: Option<MatchScore> = None;
    for (trip_index, service_date) in candidate_pairs {
        let Some(score) = score_specific_trip(
            index,
            trip_index,
            service_date,
            &line,
            &destination,
            &observations,
        ) else {
            continue;
        };

        // The anchor posting already guarantees one stop is within the single-stop bound.
        // Full scoring verifies every resolved stop, service calendar, line and stop order.
        if score.mean_abs_error_seconds > config.max_mean_schedule_error_seconds
            || score.max_abs_error_seconds > config.max_stop_schedule_error_seconds
        {
            continue;
        }

        if best.as_ref().is_none_or(|current| score.better_than(current)) {
            best = Some(score);
        }
    }

    best
}

fn score_specific_trip(
    index: &MartaScheduleIndex,
    trip_index: TripIndex,
    service_date: NaiveDate,
    line: &str,
    destination: &str,
    observations: &[Observation],
) -> Option<MatchScore> {
    let trip = index.trips.get(trip_index)?;
    let pattern = index.patterns.get(trip.pattern_id)?;
    if pattern.line != line {
        return None;
    }
    if !index
        .services
        .get(&trip.service_id)
        .is_some_and(|service| service.runs_on(service_date))
    {
        return None;
    }
    let alignment = align_pattern(&pattern.stations, observations)?;
    let skipped_pattern_stops = count_skipped_stops(&alignment);
    let (mean, max) = score_schedule(trip, service_date, &alignment, observations)?;
    let destination_matches = destination.is_empty()
        || trip.headsign.as_deref().is_some_and(|headsign| headsign == destination);
    Some(MatchScore {
        trip_index,
        service_date,
        row_stop_positions: alignment
            .iter()
            .map(|(obs_index, stop_index)| (observations[*obs_index].row_index, *stop_index))
            .collect(),
        mean_abs_error_seconds: mean,
        max_abs_error_seconds: max,
        destination_matches,
        skipped_pattern_stops,
    })
}

fn align_pattern(
    pattern_stations: &[StationId],
    observations: &[Observation],
) -> Option<Vec<(usize, usize)>> {
    let mut result = Vec::with_capacity(observations.len());
    let mut pattern_cursor = 0usize;

    for (observation_index, observation) in observations.iter().enumerate() {
        while pattern_cursor < pattern_stations.len()
            && !observation
                .station_candidates
                .contains(&pattern_stations[pattern_cursor])
        {
            pattern_cursor += 1;
        }
        if pattern_cursor == pattern_stations.len() {
            return None;
        }
        result.push((observation_index, pattern_cursor));
        pattern_cursor += 1;
    }

    Some(result)
}

fn count_skipped_stops(alignment: &[(usize, usize)]) -> usize {
    alignment
        .windows(2)
        .map(|window| window[1].1.saturating_sub(window[0].1 + 1))
        .sum()
}

fn score_schedule(
    trip: &crate::index::IndexedTrip,
    service_date: NaiveDate,
    alignment: &[(usize, usize)],
    observations: &[Observation],
) -> Option<(i64, i64)> {
    let midnight = service_date.and_hms_opt(0, 0, 0)?;
    let mut total = 0i128;
    let mut count = 0i64;
    let mut max = 0i64;

    for &(observation_index, stop_index) in alignment {
        let scheduled_seconds = trip.arrival_seconds.get(stop_index).copied().flatten()?;
        let gtfs_local = midnight + Duration::seconds(scheduled_seconds as i64);
        let observed_local = observations.get(observation_index)?.scheduled_local;
        let difference = (observed_local - gtfs_local).num_seconds().abs();
        total += difference as i128;
        count += 1;
        max = max.max(difference);
    }

    if count == 0 {
        return None;
    }
    Some(((total / count as i128) as i64, max))
}

fn candidate_service_dates(observations: &[Observation]) -> Vec<NaiveDate> {
    let mut dates = HashSet::new();
    for observation in observations {
        let event_date = observation.event_time.date_naive();
        let scheduled_date = observation.scheduled_local.date();
        dates.insert(event_date);
        dates.insert(event_date - Duration::days(1));
        dates.insert(scheduled_date);
        dates.insert(scheduled_date - Duration::days(1));
    }
    let mut dates = dates.into_iter().collect::<Vec<_>>();
    dates.sort_unstable();
    dates
}

fn resolve_station(index: &MartaScheduleIndex, realtime_name: &str) -> Vec<StationId> {
    let normalized = normalize_station_name(realtime_name);
    if let Some(ids) = index.station_aliases.get(&normalized) {
        return ids.clone();
    }

    // Rare fallback for rider-facing abbreviations not present in static GTFS. This is
    // deliberately only used after O(1) alias lookup fails. Ambiguous matches are retained
    // and later resolved by line, stop order and schedule time.
    let mut ids = Vec::new();
    for (alias, alias_ids) in &index.station_aliases {
        if alias.len() >= 5
            && normalized.len() >= 5
            && (alias.starts_with(&normalized) || normalized.starts_with(alias))
        {
            ids.extend(alias_ids.iter().copied());
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

pub(crate) fn group_live_rows(rows: Vec<MartaTrainRow>) -> (Vec<Vec<MartaTrainRow>>, usize) {
    let mut ignored = 0usize;
    let mut groups = HashMap::<(String, String, String, String), Vec<MartaTrainRow>>::new();

    for row in rows {
        if !row.is_live() {
            ignored += 1;
            continue;
        }
        let key = (
            row.train_id.trim().to_string(),
            normalize_line(&row.line),
            row.direction.trim().to_ascii_uppercase(),
            normalize_station_name(&row.destination),
        );
        groups.entry(key).or_default().push(row);
    }

    (groups.into_values().collect(), ignored)
}
