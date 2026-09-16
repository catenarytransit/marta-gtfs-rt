//! MARTA rail realtime -> GTFS-Realtime converter.
//!
//! The expensive work happens once in [`MartaScheduleIndex::build`]. A continuously running
//! process should keep one [`MartaGtfsRt`] alive across polls so the static GTFS pattern index
//! and short-lived `TRAIN_ID -> trip_id` assignments are reused.

mod index;
mod matcher;
mod models;
mod time;

pub use index::MartaScheduleIndex;
pub use matcher::MatcherConfig;
pub use models::MartaTrainRow;

use crate::index::{normalize_line, normalize_station_name, TripIndex};
use crate::matcher::{group_live_rows, match_train};
use crate::time::{parse_delay_seconds, parse_event_time, parse_next_arrival};
use chrono::NaiveDate;
use gtfs_realtime::trip_update::{StopTimeEvent, StopTimeUpdate};
use gtfs_realtime::{
    FeedEntity, FeedHeader, FeedMessage, Position, TripDescriptor, TripUpdate, VehicleDescriptor,
    VehiclePosition,
};
use gtfs_structures::Gtfs;
use std::collections::HashMap;
use std::error::Error;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MARTA_RAIL_REALTIME_URL: &str = "https://developerservices.itsmarta.com:18096/itsmarta/railrealtimearrivals/developerservices/traindata";

pub type MartaError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug)]
pub struct MartaConfig {
    pub realtime_url: String,
    pub matcher: MatcherConfig,
    /// A train ID can be reused when a physical train turns back. Cached assignments are only
    /// hints and are revalidated against stop order + schedule times before reuse.
    pub assignment_ttl_seconds: i64,
}

impl Default for MartaConfig {
    fn default() -> Self {
        Self {
            realtime_url: MARTA_RAIL_REALTIME_URL.to_string(),
            matcher: MatcherConfig::default(),
            assignment_ttl_seconds: 20 * 60,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MartaDiagnostics {
    pub received_rows: usize,
    pub ignored_non_realtime_rows: usize,
    pub matched_trains: usize,
    pub unmatched_trains: usize,
    pub unmatched_train_ids: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct MartaGtfsRtResults {
    pub trip_updates: FeedMessage,
    pub vehicle_positions: FeedMessage,
    pub alerts: FeedMessage,
    pub diagnostics: MartaDiagnostics,
}

#[derive(Clone, Debug)]
struct CachedAssignment {
    trip_index: TripIndex,
    service_date: NaiveDate,
    line: String,
    direction: String,
    destination: String,
    last_seen_unix: i64,
}

/// Stateful converter recommended for a continuously running poller/service.
pub struct MartaGtfsRt {
    index: MartaScheduleIndex,
    config: MartaConfig,
    assignments: Mutex<HashMap<String, CachedAssignment>>,
}

impl MartaGtfsRt {
    /// Build the rail-only static index once. MARTA's rail routes use GTFS route_type=1
    /// (`RouteType::Subway`), so buses and the streetcar are not indexed.
    pub fn new(gtfs: &Gtfs) -> Self {
        Self::with_config(gtfs, MartaConfig::default())
    }

    pub fn with_config(gtfs: &Gtfs, config: MartaConfig) -> Self {
        Self {
            index: MartaScheduleIndex::build(gtfs),
            config,
            assignments: Mutex::new(HashMap::new()),
        }
    }

    pub fn index(&self) -> &MartaScheduleIndex {
        &self.index
    }

    /// Fetch the MARTA REST endpoint. The API key is sent as a query parameter and is never
    /// stored in the converter.
    pub async fn fetch(
        &self,
        client: &reqwest::Client,
        api_key: &str,
    ) -> Result<MartaGtfsRtResults, MartaError> {
        let response = client
            .get(&self.config.realtime_url)
            .query(&[("apiKey", api_key)])
            .send()
            .await?
            .error_for_status()?;
        let rows = response.json::<Vec<MartaTrainRow>>().await?;
        Ok(self.process_rows(rows))
    }

    /// Convert an already-fetched MARTA JSON response. This is useful for tests, replay and
    /// deployments where HTTP fetching is handled outside the library.
    pub fn process_rows(&self, rows: Vec<MartaTrainRow>) -> MartaGtfsRtResults {
        let received_rows = rows.len();
        let (groups, ignored_non_realtime_rows) = group_live_rows(rows);
        let mut trip_update_entities = Vec::with_capacity(groups.len());
        let mut vehicle_entities = Vec::with_capacity(groups.len());
        let mut unmatched_train_ids = Vec::new();
        let mut matched_trains = 0usize;

        for group in groups {
            let Some(first) = group.first() else { continue; };
            let train_id = first.train_id.trim().to_string();
            let line = normalize_line(&first.line);
            let direction = first.direction.trim().to_ascii_uppercase();
            let destination = normalize_station_name(&first.destination);
            let latest_event_unix = group
                .iter()
                .filter_map(|row| parse_event_time(&row.event_time).map(|time| time.timestamp()))
                .max()
                .unwrap_or_else(current_unix_i64);

            let preferred = {
                let cache = self.assignments.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.get(&train_id).and_then(|cached| {
                    let age = latest_event_unix.saturating_sub(cached.last_seen_unix);
                    (age <= self.config.assignment_ttl_seconds
                        && cached.line == line
                        && cached.direction == direction
                        && cached.destination == destination)
                        .then_some((cached.trip_index, cached.service_date))
                })
            };

            let Some(matched) = match_train(
                &self.index,
                &group,
                &self.config.matcher,
                preferred,
            ) else {
                unmatched_train_ids.push(train_id);
                continue;
            };

            let trip = &self.index.trips[matched.trip_index];
            let descriptor = TripDescriptor {
                trip_id: Some(trip.trip_id.clone()),
                route_id: Some(trip.route_id.clone()),
                direction_id: trip.direction_id,
                start_time: None,
                start_date: Some(matched.service_date.format("%Y%m%d").to_string()),
                schedule_relationship: None,
                modified_trip: None,
            };
            let vehicle_descriptor = VehicleDescriptor {
                id: Some(train_id.clone()),
                label: Some(train_id.clone()),
                license_plate: None,
                wheelchair_accessible: None,
            };

            let mut stop_time_updates = Vec::with_capacity(matched.row_stop_positions.len());
            for &(row_index, stop_index) in &matched.row_stop_positions {
                let Some(row) = group.get(row_index) else { continue; };
                let Some(event_time) = parse_event_time(&row.event_time) else { continue; };
                let waiting_seconds = row.waiting_seconds.trim().parse::<i64>().ok();
                let Some(predicted_arrival) =
                    parse_next_arrival(&event_time, &row.next_arr, waiting_seconds)
                else {
                    continue;
                };
                let delay = parse_delay_seconds(&row.delay);
                let Some(stop_id) = trip.stop_ids.get(stop_index).cloned() else { continue; };
                let stop_sequence = trip.stop_sequences.get(stop_index).copied();

                stop_time_updates.push(StopTimeUpdate {
                    stop_sequence,
                    stop_id: Some(stop_id),
                    arrival: Some(StopTimeEvent {
                        delay: delay.and_then(|value| i32::try_from(value).ok()),
                        time: Some(predicted_arrival.timestamp()),
                        uncertainty: None,
                        scheduled_time: None,
                    }),
                    departure: None,
                    departure_occupancy_status: None,
                    schedule_relationship: None,
                    stop_time_properties: None,
                });
            }
            stop_time_updates.sort_by_key(|update| update.stop_sequence.unwrap_or(u32::MAX));

            trip_update_entities.push(FeedEntity {
                id: format!("marta-{train_id}-tu"),
                is_deleted: None,
                trip_update: Some(TripUpdate {
                    trip: descriptor.clone(),
                    vehicle: Some(vehicle_descriptor.clone()),
                    stop_time_update: stop_time_updates,
                    timestamp: u64::try_from(latest_event_unix).ok(),
                    delay: None,
                    trip_properties: None,
                }),
                vehicle: None,
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            });

            let position = group
                .iter()
                .filter_map(|row| {
                    let event = parse_event_time(&row.event_time)?;
                    let lat = row.latitude.as_deref()?.trim().parse::<f32>().ok()?;
                    let lon = row.longitude.as_deref()?.trim().parse::<f32>().ok()?;
                    Some((event.timestamp(), lat, lon))
                })
                .max_by_key(|(timestamp, _, _)| *timestamp)
                .map(|(_, lat, lon)| Position {
                    latitude: lat,
                    longitude: lon,
                    bearing: None,
                    odometer: None,
                    speed: None,
                });

            vehicle_entities.push(FeedEntity {
                id: format!("marta-{train_id}-vp"),
                is_deleted: None,
                trip_update: None,
                vehicle: Some(VehiclePosition {
                    trip: Some(descriptor),
                    vehicle: Some(vehicle_descriptor),
                    position,
                    current_stop_sequence: None,
                    stop_id: None,
                    current_status: None,
                    timestamp: u64::try_from(latest_event_unix).ok(),
                    congestion_level: None,
                    occupancy_status: None,
                    occupancy_percentage: None,
                    multi_carriage_details: vec![],
                }),
                alert: None,
                shape: None,
                stop: None,
                trip_modifications: None,
            });

            {
                let mut cache = self.assignments.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                cache.insert(
                    train_id,
                    CachedAssignment {
                        trip_index: matched.trip_index,
                        service_date: matched.service_date,
                        line,
                        direction,
                        destination,
                        last_seen_unix: latest_event_unix,
                    },
                );
                cache.retain(|_, cached| {
                    latest_event_unix.saturating_sub(cached.last_seen_unix)
                        <= self.config.assignment_ttl_seconds * 3
                });
            }
            matched_trains += 1;
        }

        let header = feed_header();
        let unmatched_trains = unmatched_train_ids.len();
        MartaGtfsRtResults {
            trip_updates: FeedMessage {
                header: header.clone(),
                entity: trip_update_entities,
            },
            vehicle_positions: FeedMessage {
                header: header.clone(),
                entity: vehicle_entities,
            },
            alerts: FeedMessage {
                header,
                entity: Vec::new(),
            },
            diagnostics: MartaDiagnostics {
                received_rows,
                ignored_non_realtime_rows,
                matched_trains,
                unmatched_trains,
                unmatched_train_ids,
            },
        }
    }
}

/// Compatibility/convenience function mirroring the style of the Amtrak/VIA libraries.
/// For repeated polling, prefer constructing [`MartaGtfsRt`] once so the index and assignment
/// cache are reused.
pub async fn fetch_marta_gtfs_rt(
    gtfs: &Gtfs,
    client: &reqwest::Client,
    api_key: &str,
) -> Result<MartaGtfsRtResults, MartaError> {
    MartaGtfsRt::new(gtfs).fetch(client, api_key).await
}

fn feed_header() -> FeedHeader {
    FeedHeader {
        gtfs_realtime_version: "2.0".to_string(),
        incrementality: None,
        timestamp: Some(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        ),
        feed_version: None,
    }
}

fn current_unix_i64() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
