//! Daylight-saving-aware IANA timezone conversion for the narrow WASM `local_time` service.
//!
//! Zones may be named as IANA ids in any case ("europe/london"), common abbreviations ("PST",
//! "BST", "IST" — each mapped to a representative zone so daylight saving still applies), or
//! fixed UTC offsets ("UTC+5:30", "GMT-3", "+02:00").

use chrono::{DateTime, Datelike, FixedOffset, LocalResult, NaiveDate, Offset, TimeZone, Timelike};
use jeeves_abi::{LocalTimeResult, LocalWallTime};

/// Abbreviation → representative IANA zone.
const ABBREVIATIONS: &[(&str, &str)] = &[
    ("utc", "Etc/UTC"),
    ("gmt", "Etc/UTC"),
    ("z", "Etc/UTC"),
    ("zulu", "Etc/UTC"),
    ("et", "America/New_York"),
    ("est", "America/New_York"),
    ("edt", "America/New_York"),
    ("eastern", "America/New_York"),
    ("ct", "America/Chicago"),
    ("cst", "America/Chicago"),
    ("cdt", "America/Chicago"),
    ("central", "America/Chicago"),
    ("mt", "America/Denver"),
    ("mst", "America/Denver"),
    ("mdt", "America/Denver"),
    ("mountain", "America/Denver"),
    ("pt", "America/Los_Angeles"),
    ("pst", "America/Los_Angeles"),
    ("pdt", "America/Los_Angeles"),
    ("pacific", "America/Los_Angeles"),
    ("akst", "America/Anchorage"),
    ("akdt", "America/Anchorage"),
    ("hst", "Pacific/Honolulu"),
    ("ast", "America/Halifax"),
    ("adt", "America/Halifax"),
    ("nst", "America/St_Johns"),
    ("ndt", "America/St_Johns"),
    ("bst", "Europe/London"),
    ("uk", "Europe/London"),
    ("wet", "Europe/Lisbon"),
    ("west", "Europe/Lisbon"),
    ("cet", "Europe/Paris"),
    ("cest", "Europe/Paris"),
    ("eet", "Europe/Athens"),
    ("eest", "Europe/Athens"),
    ("msk", "Europe/Moscow"),
    ("ist", "Asia/Kolkata"),
    ("pkt", "Asia/Karachi"),
    ("sgt", "Asia/Singapore"),
    ("hkt", "Asia/Hong_Kong"),
    ("jst", "Asia/Tokyo"),
    ("kst", "Asia/Seoul"),
    ("awst", "Australia/Perth"),
    ("acst", "Australia/Adelaide"),
    ("acdt", "Australia/Adelaide"),
    ("aest", "Australia/Sydney"),
    ("aedt", "Australia/Sydney"),
    ("nzst", "Pacific/Auckland"),
    ("nzdt", "Pacific/Auckland"),
    ("sast", "Africa/Johannesburg"),
    ("wat", "Africa/Lagos"),
    ("eat", "Africa/Nairobi"),
    ("brt", "America/Sao_Paulo"),
    ("art", "America/Argentina/Buenos_Aires"),
];

#[derive(Clone, Copy, Debug)]
enum Zone {
    Named(chrono_tz::Tz),
    Fixed(FixedOffset),
}

/// Resolve a zone name, abbreviation, or UTC offset.
fn resolve(input: &str) -> Option<Zone> {
    let input = input.trim();
    if input.is_empty() || input.len() > 64 {
        return None;
    }
    if let Ok(tz) = input.parse::<chrono_tz::Tz>() {
        return Some(Zone::Named(tz));
    }
    let normalized = input.replace(' ', "_");
    if let Some(tz) = chrono_tz::TZ_VARIANTS
        .iter()
        .find(|tz| tz.name().eq_ignore_ascii_case(&normalized))
    {
        return Some(Zone::Named(*tz));
    }
    let lower = input.to_ascii_lowercase();
    if let Some((_, name)) = ABBREVIATIONS.iter().find(|(abbr, _)| *abbr == lower) {
        return name.parse().ok().map(Zone::Named);
    }
    parse_offset(&lower).map(Zone::Fixed)
}

/// "utc+5:30", "gmt-3", "+0200", "utc +02:00".
fn parse_offset(lower: &str) -> Option<FixedOffset> {
    let rest = lower
        .strip_prefix("utc")
        .or_else(|| lower.strip_prefix("gmt"))
        .unwrap_or(lower)
        .trim();
    let (sign, digits) = match rest.chars().next()? {
        '+' => (1, &rest[1..]),
        '-' | '−' => (-1, rest[rest.chars().next()?.len_utf8()..].trim_start()),
        _ => return None,
    };
    let digits = digits.trim();
    let (hours, minutes) = if let Some((hours, minutes)) = digits.split_once(':') {
        (hours.parse::<i32>().ok()?, minutes.parse::<i32>().ok()?)
    } else if digits.len() == 4 && digits.chars().all(|ch| ch.is_ascii_digit()) {
        (digits[..2].parse().ok()?, digits[2..].parse().ok()?)
    } else {
        (digits.parse::<i32>().ok()?, 0)
    };
    if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    FixedOffset::east_opt(sign * (hours * 3600 + minutes * 60))
}

fn describe<Tz: TimeZone>(dt: DateTime<Tz>, name: String) -> LocalTimeResult
where
    Tz::Offset: std::fmt::Display,
{
    let offset = dt.offset().fix();
    let abbreviation = dt.offset().to_string();
    LocalTimeResult {
        timezone: name,
        // chrono-tz renders abbreviations ("BST"); fixed offsets render as "+05:30".
        abbreviation,
        utc_offset: offset.to_string(),
        year: dt.year(),
        month: dt.month(),
        day: dt.day(),
        weekday: dt.format("%A").to_string(),
        hour_24: dt.hour(),
        minute: dt.minute(),
        unix_seconds: dt.timestamp(),
    }
}

/// The civil time in `timezone` at `unix_seconds`.
pub fn local_time(timezone: &str, unix_seconds: i64) -> Option<LocalTimeResult> {
    match resolve(timezone)? {
        Zone::Named(tz) => {
            let dt = tz.timestamp_opt(unix_seconds, 0).single()?;
            Some(describe(dt, tz.name().to_string()))
        }
        Zone::Fixed(offset) => {
            let dt = offset.timestamp_opt(unix_seconds, 0).single()?;
            Some(describe(dt, format!("UTC{offset}")))
        }
    }
}

/// Resolve a wall-clock time in `timezone` to an instant and describe it there.
pub fn local_time_at(timezone: &str, wall: LocalWallTime) -> Option<LocalTimeResult> {
    let naive = NaiveDate::from_ymd_opt(wall.year, wall.month, wall.day)?.and_hms_opt(
        wall.hour,
        wall.minute,
        0,
    )?;
    let pick = |result: LocalResult<i64>| match result {
        LocalResult::Single(instant) | LocalResult::Ambiguous(instant, _) => Some(instant),
        LocalResult::None => None,
    };
    let instant = match resolve(timezone)? {
        Zone::Named(tz) => {
            let at = |naive| pick(tz.from_local_datetime(&naive).map(|dt| dt.timestamp()));
            // A time skipped by a spring-forward change moves to the first valid minute after it.
            at(naive).or_else(|| at(naive + chrono::Duration::hours(1)))?
        }
        Zone::Fixed(offset) => pick(offset.from_local_datetime(&naive).map(|dt| dt.timestamp()))?,
    };
    local_time(timezone, instant)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_dst_and_fractional_offsets() {
        let winter = local_time("America/New_York", 1_704_067_200).unwrap(); // 2024-01-01 UTC
        let summer = local_time("America/New_York", 1_719_792_000).unwrap(); // 2024-07-01 UTC
        assert_eq!(winter.utc_offset, "-05:00");
        assert_eq!(summer.utc_offset, "-04:00");

        let half = local_time("Asia/Kathmandu", 1_704_067_200).unwrap();
        assert_eq!(half.utc_offset, "+05:45");
        assert!(local_time("Not/A_Zone", 0).is_none());
    }

    #[test]
    fn resolves_case_abbreviations_and_offsets() {
        let july = 1_719_792_000; // 2024-07-01 00:00 UTC
        assert_eq!(
            local_time("europe/london", july).unwrap().timezone,
            "Europe/London"
        );
        assert!(
            local_time("new york", july).is_none(),
            "city names are for the geocoder"
        );
        assert_eq!(
            local_time("America/New York", july).unwrap().utc_offset,
            "-04:00"
        );
        let pst = local_time("PST", july).unwrap();
        assert_eq!(pst.timezone, "America/Los_Angeles");
        assert_eq!(
            pst.utc_offset, "-07:00",
            "abbreviations follow daylight saving"
        );
        assert_eq!(local_time("utc", july).unwrap().hour_24, 0);
        let india = local_time("UTC+5:30", july).unwrap();
        assert_eq!((india.hour_24, india.minute), (5, 30));
        assert_eq!(local_time("gmt-3", july).unwrap().hour_24, 21);
        assert_eq!(local_time("+0200", july).unwrap().hour_24, 2);
        assert!(local_time("utc+15", july).is_none());
        assert!(local_time("paris", july).is_none());
    }

    #[test]
    fn wall_times_resolve_to_instants() {
        let wall = |year, month, day, hour, minute| LocalWallTime {
            year,
            month,
            day,
            hour,
            minute,
        };
        // 3pm in Los Angeles on 2024-07-01 is 22:00 UTC.
        let la = local_time_at("PST", wall(2024, 7, 1, 15, 0)).unwrap();
        assert_eq!(la.unix_seconds, 1_719_792_000 + 22 * 3600);
        assert_eq!((la.hour_24, la.minute), (15, 0));
        // 02:30 on the US spring-forward day doesn't exist; it moves to 03:30.
        let skipped = local_time_at("America/New_York", wall(2024, 3, 10, 2, 30)).unwrap();
        assert_eq!((skipped.hour_24, skipped.minute), (3, 30));
        assert!(local_time_at("UTC", wall(2024, 2, 30, 0, 0)).is_none());
    }
}
