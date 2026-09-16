# marta-gtfs-rt

Rust library that converts MARTA's rail realtime arrival REST response into GTFS-Realtime TripUpdates and VehiclePositions by matching the realtime station sequence back to MARTA's static GTFS schedule.

## Why this is stateful

For production, keep `MartaGtfsRt` alive in a continuously running process. It builds the rail schedule index once, then reuses it on every poll. It also keeps a short-lived `TRAIN_ID -> trip_id` assignment cache that is always revalidated against the current station sequence and schedule time.

A one-shot `fetch_marta_gtfs_rt()` helper is provided for compatibility with the style of `amtrak-gtfs-rt` and `via-rail-gtfsrt`, but it rebuilds the index on every call and therefore should not be used for a high-frequency poller.

## Matching algorithm

1. **Static preprocessing, once per GTFS load**
   - Keep only `route_type=1` (`RouteType::Subway`) routes.
   - Collapse platform stops onto their `parent_station` so northbound/southbound platform IDs represent one logical station for pattern matching.
   - Build normalized station-name aliases, including components of compound names such as `Lakewood-Ft. McPherson`.
   - Intern logical stations to compact integer IDs.
   - Group trips by `(line, ordered logical-station sequence)` into reusable stop patterns.
   - Keep each trip's GTFS stop IDs, stop sequences, scheduled arrival seconds and service calendar.
   - Build a per-line/per-station inverted schedule index sorted by GTFS arrival seconds. This is the main hot-path acceleration structure.

2. **Realtime preprocessing, every poll**
   - Discard every row where `IS_REALTIME != true` or `TRAIN_ID` is empty.
   - Group rows by `(TRAIN_ID, LINE, DIRECTION, DESTINATION)`.
   - Parse `EVENT_TIME`, `NEXT_ARR`, and `DELAY`.
   - Reconstruct the scheduled arrival as `NEXT_ARR - DELAY`.
   - Use `WAITING_SECONDS` only to disambiguate whether a time-of-day in `NEXT_ARR` belongs to today or tomorrow.

3. **Indexed trip candidate lookup**
   - Resolve each realtime station name to one or more logical station IDs and sort the rows by predicted arrival.
   - Choose the resolved station with the smallest static posting list (the most selective station).
   - Reconstruct its scheduled arrival time and binary-search that line/station's sorted schedule index within the configured error window.
   - This produces a small set of candidate `trip_id`s without scanning all trips on the line.

4. **Full pattern + trip validation**
   - Check both the scheduled calendar date and the previous GTFS service date, which handles trips represented with GTFS times above `24:00:00`.
   - Reject trips whose `service_id` is inactive according to `calendar.txt` and `calendar_dates.txt`.
   - Match all observed stations as an ordered subsequence of the candidate trip's logical station pattern. This naturally determines travel direction from stop order without hardcoding MARTA's `N/S/E/W` to GTFS `direction_id` values.
   - Compare every matched stop's reconstructed schedule time with the candidate GTFS `arrival_time`.
   - Select the smallest mean absolute schedule error; use maximum error, destination/headsign, and pattern gaps as tie-breakers.

5. **GTFS-Realtime output**
   - Emit a TripUpdate with the matched `trip_id`, `route_id`, service date, GTFS platform `stop_id`, stop sequence, predicted arrival timestamp and MARTA delay.
   - Emit a VehiclePosition using `TRAIN_ID` and MARTA latitude/longitude when available.

## Complexity

Let:

- `S` = total rail `stop_times` in static GTFS
- `T` = rail trips
- `R` = realtime rows for one train
- `A` = number of station-name aliases for the chosen anchor (normally 1)
- `N_s` = number of scheduled stop-time postings at that station
- `K` = postings found inside the configured schedule-time window
- `L` = candidate trip pattern length

Index construction is **O(S + T)** time and **O(S + T)** space. The extra inverted schedule index is also linear in `S`; its entries are compact `(scheduled_seconds, trip_index)` pairs.

For an uncached realtime train, station observations are sorted in **O(R log R)**. Candidate discovery is **O(A log N_s + K)** per candidate service date through binary search. Only those `K` nearby scheduled trips are then fully validated, for approximately **O(K * (L + R))**. This avoids a scan over every MARTA rail trip, every bus trip, or even every trip on the same rail line.

A valid cached `TRAIN_ID` assignment skips candidate discovery and is revalidated in **O(L + R)**.

## Usage

```rust
use marta_gtfs_rt::MartaGtfsRt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let gtfs = gtfs_structures::Gtfs::from_url_async(
        "https://itsmarta.com/google_transit_feed/google_transit.zip"
    ).await?;

    let converter = MartaGtfsRt::new(&gtfs);
    let client = reqwest::Client::new();
    let api_key = std::env::var("MARTA_API_KEY")?;

    let result = converter.fetch(&client, &api_key).await?;
    println!("matched {} trains", result.diagnostics.matched_trains);
    Ok(())
}
```

See `examples/poll.rs` for a continuously running poller that writes protobuf files.

## API key

The API key is supplied by the caller. It is not embedded in this crate and is not retained by `MartaGtfsRt`.
