use marta_gtfs_rt::MartaGtfsRt;
use prost::Message;
use std::error::Error;
use std::time::Duration;

const GTFS_URL: &str = "https://itsmarta.com/google_transit_feed/google_transit.zip";

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let api_key = std::env::var("MARTA_API_KEY")?;
    let gtfs = gtfs_structures::Gtfs::from_url_async(GTFS_URL).await?;
    let converter = MartaGtfsRt::new(&gtfs);
    let client = reqwest::Client::new();

    eprintln!(
        "indexed {} MARTA rail trips across {} stop patterns",
        converter.index().indexed_trip_count(),
        converter.index().pattern_count()
    );

    loop {
        match converter.fetch(&client, &api_key).await {
            Ok(result) => {
                tokio::fs::write(
                    "marta_trip_updates.pb",
                    result.trip_updates.encode_to_vec(),
                )
                .await?;
                tokio::fs::write(
                    "marta_vehicle_positions.pb",
                    result.vehicle_positions.encode_to_vec(),
                )
                .await?;
                eprintln!(
                    "matched {} trains; {} unmatched; ignored {} non-realtime rows",
                    result.diagnostics.matched_trains,
                    result.diagnostics.unmatched_trains,
                    result.diagnostics.ignored_non_realtime_rows
                );
            }
            Err(error) => eprintln!("MARTA realtime fetch failed: {error}"),
        }

        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}
