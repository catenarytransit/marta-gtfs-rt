use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MartaTrainRow {
    #[serde(rename = "DESTINATION", default)]
    pub destination: String,
    #[serde(rename = "DIRECTION", default)]
    pub direction: String,
    #[serde(rename = "EVENT_TIME", default)]
    pub event_time: String,
    #[serde(rename = "IS_REALTIME", default)]
    pub is_realtime: String,
    #[serde(rename = "LINE", default)]
    pub line: String,
    #[serde(rename = "NEXT_ARR", default)]
    pub next_arr: String,
    #[serde(rename = "STATION", default)]
    pub station: String,
    #[serde(rename = "TRAIN_ID", default)]
    pub train_id: String,
    #[serde(rename = "WAITING_SECONDS", default)]
    pub waiting_seconds: String,
    #[serde(rename = "WAITING_TIME", default)]
    pub waiting_time: String,
    #[serde(rename = "DELAY", default)]
    pub delay: String,
    #[serde(rename = "LATITUDE", default)]
    pub latitude: Option<String>,
    #[serde(rename = "LONGITUDE", default)]
    pub longitude: Option<String>,
}

impl MartaTrainRow {
    pub fn is_live(&self) -> bool {
        self.is_realtime.trim().eq_ignore_ascii_case("true") && !self.train_id.trim().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters_scheduled_only_rows() {
        let live: MartaTrainRow = serde_json::from_str(r#"{
            "DESTINATION":"Airport","DIRECTION":"S","EVENT_TIME":"09/15/2026 10:40:05 PM",
            "IS_REALTIME":"true","LINE":"GOLD","NEXT_ARR":"10:51:31 PM",
            "STATION":"LAKEWOOD STATION","TRAIN_ID":"302","WAITING_SECONDS":"640",
            "WAITING_TIME":"10 min","DELAY":"T348S","LATITUDE":"33.758661","LONGITUDE":"-84.38765"
        }"#).unwrap();
        assert!(live.is_live());

        let scheduled: MartaTrainRow = serde_json::from_str(r#"{
            "DESTINATION":"Hamilton E. Holmes","DIRECTION":"W","EVENT_TIME":"09/15/2026 10:40:50 PM",
            "IS_REALTIME":"false","LINE":"BLUE","NEXT_ARR":"11:12:00 PM",
            "STATION":"EAST LAKE STATION","TRAIN_ID":"","WAITING_SECONDS":"1869",
            "WAITING_TIME":"31 min","DELAY":"T0S"
        }"#).unwrap();
        assert!(!scheduled.is_live());
    }
}
