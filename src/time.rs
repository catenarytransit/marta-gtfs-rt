use jiff::civil::{DateTime, Time};
use jiff::tz::{TimeZone, TimeZoneDatabase};
use jiff::{SignedDuration, Span, Timestamp};
use std::sync::LazyLock;

// Use the bundled IANA data to preserve the old dependency's behavior on every host.
pub(crate) static MARTA_TIMEZONE: LazyLock<TimeZone> = LazyLock::new(|| {
    TimeZoneDatabase::bundled()
        .get("America/New_York")
        .expect("America/New_York is present in the bundled IANA time zone database")
});

pub(crate) fn parse_event_time(value: &str) -> Option<Timestamp> {
    let local = DateTime::strptime("%m/%d/%Y %I:%M:%S %p", value.trim()).ok()?;
    localize(local)
}

pub(crate) fn parse_next_arrival(
    event: &Timestamp,
    next_arr: &str,
    waiting_seconds: Option<i64>,
) -> Option<Timestamp> {
    let next_time = Time::strptime("%I:%M:%S %p", next_arr.trim()).ok()?;
    let event_local = MARTA_TIMEZONE.to_datetime(*event);
    let base = event_local.date().at(
        next_time.hour(),
        next_time.minute(),
        next_time.second(),
        0,
    );

    // Compare wall-clock times, as before: service dates and GTFS stop times
    // are local civil dates/times, including service-day overflow past 24:00.
    let day = Span::new().days(1);
    let candidates = [
        base.checked_sub(day).ok(),
        Some(base),
        base.checked_add(day).ok(),
    ];

    let chosen = if let Some(wait) = waiting_seconds {
        let target = event_local.checked_add(SignedDuration::from_secs(wait)).ok()?;
        candidates
            .into_iter()
            .flatten()
            .min_by_key(|candidate| candidate.duration_since(target).as_secs().abs())?
    } else {
        let earliest = event_local.checked_sub(SignedDuration::from_secs(120)).ok()?;
        candidates
            .into_iter()
            .flatten()
            .filter(|candidate| *candidate >= earliest)
            .min_by_key(|candidate| candidate.duration_since(event_local).as_secs().abs())?
    };

    localize(chosen)
}

pub(crate) fn parse_delay_seconds(value: &str) -> Option<i64> {
    let mut text = value.trim().to_ascii_uppercase();
    if text.is_empty() {
        return None;
    }
    if let Ok(seconds) = text.parse::<i64>() {
        return Some(seconds);
    }

    if let Some(rest) = text.strip_prefix('P') {
        text = rest.to_string();
    }
    if let Some(rest) = text.strip_prefix('T') {
        text = rest.to_string();
    }

    let sign = if let Some(rest) = text.strip_prefix('-') {
        text = rest.to_string();
        -1i64
    } else if let Some(rest) = text.strip_prefix('+') {
        text = rest.to_string();
        1i64
    } else {
        1i64
    };

    let mut number = String::new();
    let mut seconds = 0i64;
    let mut saw_unit = false;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            number.push(ch);
            continue;
        }
        if number.is_empty() {
            return None;
        }
        let amount = number.parse::<i64>().ok()?;
        number.clear();
        match ch {
            'H' => seconds += amount * 3600,
            'M' => seconds += amount * 60,
            'S' => seconds += amount,
            _ => return None,
        }
        saw_unit = true;
    }

    if !number.is_empty() {
        // MARTA normally emits T###S. Accept a bare numeric tail defensively.
        seconds += number.parse::<i64>().ok()?;
        saw_unit = true;
    }

    saw_unit.then_some(sign * seconds)
}

fn localize(local: DateTime) -> Option<Timestamp> {
    // Choose the first occurrence in a fall-back fold. In a spring-forward
    // gap, reject the input instead of silently adjusting the wall-clock time.
    let timestamp = MARTA_TIMEZONE.to_ambiguous_timestamp(local).earlier().ok()?;
    (MARTA_TIMEZONE.to_datetime(timestamp) == local).then_some(timestamp)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_marta_delay() {
        assert_eq!(parse_delay_seconds("T348S"), Some(348));
        assert_eq!(parse_delay_seconds("PT1M30S"), Some(90));
        assert_eq!(parse_delay_seconds("T-15S"), Some(-15));
    }

    #[test]
    fn next_arrival_crosses_midnight() {
        let event = parse_event_time("09/15/2026 11:59:50 PM").unwrap();
        let next = parse_next_arrival(&event, "12:04:00 AM", Some(250)).unwrap();
        assert_eq!(next, "2026-09-16T04:04:00Z".parse::<Timestamp>().unwrap());
        assert_eq!(
            parse_next_arrival(&event, "12:04:00 AM", None),
            Some(next)
        );
    }

    #[test]
    fn parses_eastern_standard_and_daylight_time() {
        for (input, expected) in [
            ("01/15/2026 1:04:00 PM", "2026-01-15T18:04:00Z"),
            ("07/15/2026 01:04:00 PM", "2026-07-15T17:04:00Z"),
        ] {
            assert_eq!(parse_event_time(input), Some(expected.parse::<Timestamp>().unwrap()));
        }
        assert!(parse_event_time("not a timestamp").is_none());
    }

    #[test]
    fn rejects_nonexistent_spring_forward_times() {
        assert!(parse_event_time("03/08/2026 02:30:00 AM").is_none());
        let event = parse_event_time("03/08/2026 01:55:00 AM").unwrap();
        assert!(parse_next_arrival(&event, "02:30:00 AM", Some(2100)).is_none());
    }

    #[test]
    fn fall_back_chooses_first_occurrence() {
        let expected = "2026-11-01T05:30:00Z".parse::<Timestamp>().unwrap();
        assert_eq!(parse_event_time("11/01/2026 01:30:00 AM"), Some(expected));
        let event = parse_event_time("11/01/2026 01:20:00 AM").unwrap();
        assert_eq!(parse_next_arrival(&event, "01:30:00 AM", Some(600)), Some(expected));
        assert_eq!(
            parse_event_time("11/01/2026 02:30:00 AM"),
            Some("2026-11-01T07:30:00Z".parse::<Timestamp>().unwrap())
        );
    }

    #[test]
    fn missing_wait_allows_recent_past_prediction() {
        let event = parse_event_time("07/15/2026 01:04:00 PM").unwrap();
        assert_eq!(
            parse_next_arrival(&event, "01:03:00 PM", None),
            Some("2026-07-15T17:03:00Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(
            parse_next_arrival(&event, "01:01:00 PM", None),
            Some("2026-07-16T17:01:00Z".parse::<Timestamp>().unwrap())
        );
    }
}
