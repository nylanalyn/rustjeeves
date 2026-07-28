//! Current observations from one operator-configured WeatherLink v2 station.
//!
//! Credentials remain in the native host. The WASM module receives only normalized observations
//! and safe error categories because WeatherLink records vary by station and sensor product.

use jeeves_abi::WeatherLinkResult;
use serde_json::Value;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const API_KEY_CONFIG: &str = "weatherlink_api_key";
pub const API_SECRET_CONFIG: &str = "weatherlink_api_secret";
pub const STATION_ID_CONFIG: &str = "weatherlink_station_id";
pub const STATION_NAME_CONFIG: &str = "weatherlink_station_name";

const CURRENT_ENDPOINT: &str = "https://api.weatherlink.com/v2/current";
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const CACHE_FOR: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Cached {
    station_id: String,
    api_key: String,
    fetched_at: Instant,
    result: WeatherLinkResult,
}

static CACHE: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();

pub fn current(
    api_key: Option<String>,
    api_secret: Option<String>,
    station_id: Option<String>,
    station_name: Option<String>,
) -> WeatherLinkResult {
    let Some(api_key) = nonempty(api_key) else {
        return failure("not_configured");
    };
    let Some(api_secret) = nonempty(api_secret) else {
        return failure("not_configured");
    };
    let Some(station_id) = nonempty(station_id) else {
        return failure("not_configured");
    };
    if !valid_station_id(&station_id) || api_key.chars().count() > 256 || api_secret.len() > 512 {
        return failure("invalid_configuration");
    }

    let cache = CACHE.get_or_init(|| Mutex::new(None));
    if let Some(cached) = cache
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned())
        .filter(|cached| {
            cached.station_id == station_id
                && cached.api_key == api_key
                && cached.fetched_at.elapsed() < CACHE_FOR
        })
    {
        return cached.result;
    }

    let endpoint = format!("{CURRENT_ENDPOINT}/{station_id}");
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build(),
    );
    let response = agent
        .get(&endpoint)
        .query("api-key", &api_key)
        .header("X-Api-Secret", &api_secret)
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(401 | 403)) => return failure("authentication"),
        Err(ureq::Error::StatusCode(404)) => return failure("station_not_found"),
        Err(ureq::Error::StatusCode(429)) => return failure("rate_limited"),
        Err(ureq::Error::StatusCode(400)) => return failure("invalid_configuration"),
        Err(_) => return failure("unavailable"),
    };
    let Ok(body) = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
    else {
        return failure("unavailable");
    };
    let Ok(value) = serde_json::from_str::<Value>(&body) else {
        return failure("unavailable");
    };
    let label = station_name
        .and_then(|value| {
            let value = value
                .trim()
                .chars()
                .filter(|ch| !ch.is_control())
                .take(80)
                .collect::<String>();
            (!value.is_empty()).then_some(value)
        })
        .unwrap_or_else(|| "local station".into());
    let Some(result) = parse_current(&value, label) else {
        return failure("no_observations");
    };
    if let Ok(mut guard) = cache.lock() {
        *guard = Some(Cached {
            station_id,
            api_key,
            fetched_at: Instant::now(),
            result: result.clone(),
        });
    }
    result
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn valid_station_id(value: &str) -> bool {
    value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

fn failure(kind: &str) -> WeatherLinkResult {
    WeatherLinkResult {
        error: Some(kind.into()),
        ..WeatherLinkResult::default()
    }
}

fn number(record: &Value, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| record.get(*name).and_then(Value::as_f64))
        .filter(|value| value.is_finite())
}

fn timestamp(record: &Value) -> i64 {
    record.get("ts").and_then(Value::as_i64).unwrap_or(0)
}

fn primary_score(structure: i64, record: &Value) -> i64 {
    let structure_score = match structure {
        23 | 10 | 6 | 2 | 1 => 100,
        _ => 0,
    };
    structure_score
        + i64::from(number(record, &["temp", "temp_out"]).is_some()) * 20
        + i64::from(number(record, &["hum", "hum_out"]).is_some()) * 10
        + i64::from(
            number(
                record,
                &[
                    "wind_speed_last",
                    "wind_speed_avg_last_1_min",
                    "wind_speed_avg_last_2_min",
                    "wind_speed",
                ],
            )
            .is_some(),
        ) * 5
}

fn parse_current(value: &Value, station: String) -> Option<WeatherLinkResult> {
    let sensors = value.get("sensors")?.as_array()?;
    let mut records = Vec::new();
    for sensor in sensors {
        let structure = sensor
            .get("data_structure_type")
            .and_then(Value::as_i64)
            .unwrap_or_default();
        let Some(record) = sensor
            .get("data")
            .and_then(Value::as_array)
            .and_then(|data| data.first())
        else {
            continue;
        };
        records.push((structure, record));
    }
    let (_, primary) = records
        .iter()
        .max_by_key(|(structure, record)| (primary_score(*structure, record), timestamp(record)))?;
    if number(primary, &["temp", "temp_out"]).is_none()
        && number(primary, &["hum", "hum_out"]).is_none()
    {
        return None;
    }

    let newest = |names: &[&str]| {
        records
            .iter()
            .filter_map(|(_, record)| number(record, names).map(|value| (timestamp(record), value)))
            .max_by_key(|(ts, _)| *ts)
            .map(|(_, value)| value)
    };
    let observed_at = (timestamp(primary) > 0).then(|| timestamp(primary));
    Some(WeatherLinkResult {
        station,
        observed_at,
        temp_f: number(primary, &["temp", "temp_out"]),
        apparent_f: number(
            primary,
            &["thw_index", "heat_index", "heat_index_out", "wind_chill"],
        ),
        humidity: number(primary, &["hum", "hum_out"]),
        wind_mph: number(
            primary,
            &[
                "wind_speed_last",
                "wind_speed_avg_last_1_min",
                "wind_speed_avg_last_2_min",
                "wind_speed",
            ],
        ),
        wind_gust_mph: number(
            primary,
            &["wind_speed_hi_last_10_min", "wind_speed_hi_last_2_min"],
        ),
        wind_dir_degrees: number(
            primary,
            &[
                "wind_dir_last",
                "wind_dir_scalar_avg_last_1_min",
                "wind_dir_scalar_avg_last_2_min",
                "wind_dir",
            ],
        ),
        pressure_inhg: newest(&["bar_sea_level", "bar", "pressure_last", "bar_absolute"]),
        rain_daily_in: newest(&["rainfall_daily_in", "rainfall_day_in"]),
        rain_rate_in_hr: newest(&["rain_rate_last_in", "rain_rate_hi_last_15_min_in"]),
        error: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_weatherlink_live_and_separate_barometer() {
        let value: Value = serde_json::from_str(
            r#"{
                "station_id": 374964,
                "sensors": [
                    {"data_structure_type": 15, "data": [{"ts": 200, "uptime": 42}]},
                    {"data_structure_type": 10, "data": [{
                        "ts": 190, "temp": 73.3, "hum": 42.7, "thw_index": 72.2,
                        "wind_speed_last": 4, "wind_speed_hi_last_10_min": 6,
                        "wind_dir_last": 195, "rainfall_daily_in": 0.12,
                        "rain_rate_last_in": 0.01
                    }]},
                    {"data_structure_type": 12, "data": [{
                        "ts": 195, "bar_sea_level": 29.61
                    }]}
                ]
            }"#,
        )
        .unwrap();
        let result = parse_current(&value, "Back Garden".into()).unwrap();
        assert_eq!(result.station, "Back Garden");
        assert_eq!(result.temp_f, Some(73.3));
        assert_eq!(result.humidity, Some(42.7));
        assert_eq!(result.wind_mph, Some(4.0));
        assert_eq!(result.pressure_inhg, Some(29.61));
        assert_eq!(result.rain_daily_in, Some(0.12));
        assert_eq!(result.observed_at, Some(190));
    }

    #[test]
    fn parses_legacy_outdoor_field_names() {
        let value: Value = serde_json::from_str(
            r#"{"sensors":[{"data_structure_type":2,"data":[{
                "ts":123,"temp_out":55.4,"hum_out":82,"wind_speed":3,
                "heat_index_out":55.1,"bar":30.079
            }]}]}"#,
        )
        .unwrap();
        let result = parse_current(&value, "Yard".into()).unwrap();
        assert_eq!(result.temp_f, Some(55.4));
        assert_eq!(result.humidity, Some(82.0));
        assert_eq!(result.pressure_inhg, Some(30.079));
    }

    #[test]
    fn rejects_health_only_response() {
        let value: Value = serde_json::from_str(
            r#"{"sensors":[{"data_structure_type":15,"data":[{"ts":123,"uptime":99}]}]}"#,
        )
        .unwrap();
        assert!(parse_current(&value, "Yard".into()).is_none());
    }
}
