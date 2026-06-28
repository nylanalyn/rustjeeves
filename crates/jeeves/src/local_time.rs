//! Daylight-saving-aware IANA timezone conversion for the narrow WASM `local_time` service.

use chrono::{Datelike, Offset, TimeZone, Timelike};
use jeeves_abi::LocalTimeResult;

pub fn local_time(timezone: &str, unix_seconds: i64) -> Option<LocalTimeResult> {
    let tz: chrono_tz::Tz = timezone.parse().ok()?;
    let dt = tz.timestamp_opt(unix_seconds, 0).single()?;
    Some(LocalTimeResult {
        timezone: timezone.to_string(),
        abbreviation: dt.format("%Z").to_string(),
        utc_offset: dt.offset().fix().to_string(),
        year: dt.year(),
        month: dt.month(),
        day: dt.day(),
        weekday: dt.format("%A").to_string(),
        hour_24: dt.hour(),
        minute: dt.minute(),
    })
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
}
