use chrono::{DateTime, Duration, LocalResult, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::{America::New_York, Tz};

pub(crate) const MARTA_TIMEZONE: Tz = New_York;

pub(crate) fn parse_event_time(value: &str) -> Option<DateTime<Tz>> {
    let naive = NaiveDateTime::parse_from_str(value.trim(), "%m/%d/%Y %I:%M:%S %p").ok()?;
    localize(naive)
}

pub(crate) fn parse_next_arrival(
    event: &DateTime<Tz>,
    next_arr: &str,
    waiting_seconds: Option<i64>,
) -> Option<DateTime<Tz>> {
    let next_time = NaiveTime::parse_from_str(next_arr.trim(), "%I:%M:%S %p").ok()?;
    let event_naive = event.naive_local();
    let base = event_naive.date().and_time(next_time);

    let candidates = [
        base - Duration::days(1),
        base,
        base + Duration::days(1),
    ];

    let chosen = if let Some(wait) = waiting_seconds {
        let target = event_naive + Duration::seconds(wait);
        candidates
            .into_iter()
            .min_by_key(|candidate| (*candidate - target).num_seconds().abs())?
    } else {
        candidates
            .into_iter()
            .filter(|candidate| *candidate >= event_naive - Duration::minutes(2))
            .min_by_key(|candidate| (*candidate - event_naive).num_seconds().abs())?
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

fn localize(naive: NaiveDateTime) -> Option<DateTime<Tz>> {
    match MARTA_TIMEZONE.from_local_datetime(&naive) {
        LocalResult::Single(dt) => Some(dt),
        LocalResult::Ambiguous(first, _) => Some(first),
        LocalResult::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

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
        assert_eq!(next.date_naive().to_string(), "2026-09-16");
        assert_eq!(next.hour(), 0);
        assert_eq!(next.minute(), 4);
    }
}
