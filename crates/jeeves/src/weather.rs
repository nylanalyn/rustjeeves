//! Current weather via the keyless Open-Meteo forecast API, and active official warnings: the US
//! National Weather Service here, European services through [`crate::meteoalarm`]. Exposed to modules as host functions, reusing the `geocode`/`profile` plumbing
//! so a weather module needs no network access of its own.

use jeeves_abi::{DailyWeather, WeatherAlert, WeatherAlertsResult, WeatherResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Reports and alerts are cached per ~1 km grid cell for ten minutes, so repeated `!w` calls
/// (and several people in one town) don't hit the providers each time.
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);
const CACHE_CAP: usize = 256;

type Cell = (i64, i64);

static WEATHER_CACHE: OnceLock<Mutex<HashMap<Cell, (Instant, WeatherResult)>>> = OnceLock::new();
static ALERT_CACHE: OnceLock<Mutex<HashMap<Cell, (Instant, WeatherAlertsResult)>>> =
    OnceLock::new();

fn cell(lat: f64, lon: f64) -> Cell {
    ((lat * 100.0).round() as i64, (lon * 100.0).round() as i64)
}

fn cached<T: Clone>(
    cache: &'static OnceLock<Mutex<HashMap<Cell, (Instant, T)>>>,
    key: Cell,
) -> Option<T> {
    cache
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap()
        .get(&key)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, value)| value.clone())
}

fn store<T>(cache: &'static OnceLock<Mutex<HashMap<Cell, (Instant, T)>>>, key: Cell, value: T) {
    let mut cache = cache
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if cache.len() >= CACHE_CAP {
        cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
        if cache.len() >= CACHE_CAP {
            cache.clear();
        }
    }
    cache.insert(key, (Instant::now(), value));
}

/// Rough bounding boxes for NWS coverage: the contiguous US, Alaska, Hawaii, and Puerto Rico /
/// US Virgin Islands. Anywhere else skips the NWS call entirely.
fn in_nws_coverage(lat: f64, lon: f64) -> bool {
    let boxes = [
        (24.0, 50.0, -125.0, -66.0),
        (51.0, 72.0, -180.0, -129.0),
        (18.5, 22.5, -161.0, -154.0),
        (17.5, 18.8, -67.5, -64.4),
    ];
    boxes.iter().any(|(south, north, west, east)| {
        (*south..=*north).contains(&lat) && (*west..=*east).contains(&lon)
    })
}

const MAX_NWS_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_NWS_ALERTS: usize = 16;
const MAX_ALERT_EVENT_CHARS: usize = 96;

/// Fetch current conditions and a three-day forecast for a coordinate, or `None` on failure.
pub fn weather(lat: f64, lon: f64) -> Option<WeatherResult> {
    let key = cell(lat, lon);
    if let Some(result) = cached(&WEATHER_CACHE, key) {
        return Some(result);
    }
    let result = fetch_weather(lat, lon)?;
    store(&WEATHER_CACHE, key, result.clone());
    Some(result)
}

fn fetch_weather(lat: f64, lon: f64) -> Option<WeatherResult> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(6)))
            .build(),
    );
    let body = agent
        .get("https://api.open-meteo.com/v1/forecast")
        .query("latitude", lat.to_string())
        .query("longitude", lon.to_string())
        .query(
            "current",
            "temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m,\
             wind_direction_10m,wind_gusts_10m,uv_index,is_day",
        )
        .query(
            "daily",
            "weather_code,temperature_2m_max,temperature_2m_min,precipitation_probability_max,\
             rain_sum,showers_sum,sunrise,sunset",
        )
        .query("timezone", "auto")
        .query("forecast_days", "3")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let v: Value = serde_json::from_str(&body).ok()?;
    let mut result = parse_current(&v)?;
    if let Some((us_aqi, pm2_5, pm10)) = air_quality(&agent, lat, lon) {
        result.us_aqi = us_aqi;
        result.pm2_5 = pm2_5;
        result.pm10 = pm10;
    }
    Some(result)
}

/// Fetch active alerts covering a coordinate from the US National Weather Service.
///
/// Coordinates outside NWS coverage and provider failures both produce no alerts so they never
/// suppress or replace a successful Open-Meteo weather report.
pub fn alerts(lat: f64, lon: f64) -> WeatherAlertsResult {
    let key = cell(lat, lon);
    if let Some(result) = cached(&ALERT_CACHE, key) {
        return result;
    }
    let mut result = if in_nws_coverage(lat, lon) {
        fetch_alerts(lat, lon)
    } else {
        WeatherAlertsResult::default()
    };
    let european = crate::meteoalarm::alerts(lat, lon);
    result.alerts.extend(european.alerts);
    result.incomplete |= european.incomplete;
    store(&ALERT_CACHE, key, result.clone());
    result
}

fn fetch_alerts(lat: f64, lon: f64) -> WeatherAlertsResult {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(6)))
            .user_agent(concat!(
                "rustjeeves/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    );
    let Ok(mut response) = agent
        .get("https://api.weather.gov/alerts/active")
        .query("point", format!("{lat},{lon}"))
        .header("Accept", "application/geo+json")
        .call()
    else {
        return unavailable();
    };
    let Ok(body) = response
        .body_mut()
        .with_config()
        .limit(MAX_NWS_RESPONSE_BYTES)
        .read_to_string()
    else {
        return unavailable();
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .map_or_else(unavailable, |value| parse_alerts(&value))
}

fn unavailable() -> WeatherAlertsResult {
    WeatherAlertsResult {
        alerts: Vec::new(),
        incomplete: true,
    }
}

/// US colour-equivalent levels: emergencies and extreme warnings are red, warnings orange,
/// watches yellow; advisories and statements are informational.
fn nws_level(event: &str, severity: &str) -> u8 {
    let lower = event.to_ascii_lowercase();
    if lower.contains("emergency") || (lower.ends_with("warning") && severity == "Extreme") {
        3
    } else if lower.ends_with("warning") {
        2
    } else if lower.ends_with("watch") {
        1
    } else {
        0
    }
}

/// "Adams; Brown; Clermont; Highland" → "Adams, Brown, Clermont…".
fn nws_area(area: &str) -> String {
    let counties = area
        .split(';')
        .map(str::trim)
        .filter(|county| !county.is_empty())
        .collect::<Vec<_>>();
    let mut text = counties
        .iter()
        .take(3)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    if counties.len() > 3 {
        text.push('…');
    }
    text.chars().take(120).collect()
}

/// The VTEC core ("KILN.TO.W.0023") identifies one warning across its updates.
fn vtec_core(properties: &Value) -> Option<String> {
    let vtec = properties
        .pointer("/parameters/VTEC/0")
        .and_then(Value::as_str)?;
    let fields = vtec.trim_matches('/').split('.').collect::<Vec<_>>();
    // /O.NEW.KILN.TO.W.0023.260928T2100Z-260928T2145Z/
    (fields.len() >= 6).then(|| fields[2..6].join("."))
}

pub(crate) fn parse_time(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_str)
        .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
        .map_or(0, |time| time.timestamp())
}

fn parse_alerts(value: &Value) -> WeatherAlertsResult {
    let alerts = value
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|feature| {
            let properties = feature.get("properties")?;
            if properties.get("status").and_then(Value::as_str) != Some("Actual") {
                return None;
            }
            let event = properties.get("event")?.as_str()?.trim();
            if event.is_empty() {
                return None;
            }
            let severity: String = properties
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("Unknown")
                .chars()
                .take(16)
                .collect();
            let area = nws_area(
                properties
                    .get("areaDesc")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
            );
            let level = nws_level(event, &severity);
            let key = match vtec_core(properties) {
                Some(core) => format!("nws|{core}|{level}"),
                None => format!("nws|{event}|{area}|{level}").to_lowercase(),
            };
            let ends = parse_time(properties.get("ends"));
            Some(WeatherAlert {
                event: event.chars().take(MAX_ALERT_EVENT_CHARS).collect(),
                severity,
                key,
                area,
                level,
                expires: if ends > 0 {
                    ends
                } else {
                    parse_time(properties.get("expires"))
                },
                source: "NWS".into(),
            })
        })
        .take(MAX_NWS_ALERTS)
        .collect();
    WeatherAlertsResult {
        alerts,
        incomplete: false,
    }
}

fn air_quality(
    agent: &ureq::Agent,
    lat: f64,
    lon: f64,
) -> Option<(Option<f64>, Option<f64>, Option<f64>)> {
    let body = agent
        .get("https://air-quality-api.open-meteo.com/v1/air-quality")
        .query("latitude", lat.to_string())
        .query("longitude", lon.to_string())
        .query("current", "us_aqi,pm2_5,pm10")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;
    let value: Value = serde_json::from_str(&body).ok()?;
    parse_air_quality(&value)
}

fn parse_air_quality(v: &Value) -> Option<(Option<f64>, Option<f64>, Option<f64>)> {
    let current = v.get("current")?;
    Some((
        current.get("us_aqi").and_then(Value::as_f64),
        current.get("pm2_5").and_then(Value::as_f64),
        current.get("pm10").and_then(Value::as_f64),
    ))
}

fn daily_value(v: &Value, field: &str, day: usize) -> Option<f64> {
    v.get("daily")?
        .get(field)?
        .as_array()?
        .get(day)?
        .as_f64()
        .filter(|value| value.is_finite())
}

fn daily_text(v: &Value, field: &str, day: usize) -> Option<String> {
    Some(
        v.get("daily")?
            .get(field)?
            .as_array()?
            .get(day)?
            .as_str()?
            .to_string(),
    )
}

fn day_rain(v: &Value, day: usize) -> Option<f64> {
    let non_negative = |value: Option<f64>| value.filter(|value| *value >= 0.0);
    let rain = non_negative(daily_value(v, "rain_sum", day));
    let showers = non_negative(daily_value(v, "showers_sum", day));
    match (rain, showers) {
        (Some(rain), Some(showers)) => Some(rain + showers),
        (Some(rain), None) => Some(rain),
        (None, Some(showers)) => Some(showers),
        (None, None) => None,
    }
}

/// The `daily` block as up to three local days.
fn parse_daily(v: &Value) -> Vec<DailyWeather> {
    let Some(dates) = v
        .get("daily")
        .and_then(|daily| daily.get("time"))
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    // "2026-09-28T06:52" → "06:52".
    let clock = |text: Option<String>| {
        text.and_then(|text| {
            text.split_once('T')
                .map(|(_, time)| time.chars().take(5).collect())
        })
    };
    dates
        .iter()
        .take(3)
        .enumerate()
        .filter_map(|(day, date)| {
            let date = date.as_str()?.to_string();
            let weekday = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d")
                .ok()?
                .format("%A")
                .to_string();
            Some(DailyWeather {
                weekday,
                code: daily_value(v, "weather_code", day).map_or(-1, |code| code as i64),
                max_c: daily_value(v, "temperature_2m_max", day),
                min_c: daily_value(v, "temperature_2m_min", day),
                precipitation_probability: daily_value(v, "precipitation_probability_max", day),
                rain_mm: day_rain(v, day),
                sunrise: clock(daily_text(v, "sunrise", day)),
                sunset: clock(daily_text(v, "sunset", day)),
                date,
            })
        })
        .collect()
}

fn parse_forecast_rain(v: &Value) -> Option<f64> {
    day_rain(v, 0)
}

/// Parse the `current` object of an Open-Meteo forecast response. Pure (no network) for testing.
fn parse_current(v: &Value) -> Option<WeatherResult> {
    let c = v.get("current")?;
    Some(WeatherResult {
        temp_c: c.get("temperature_2m")?.as_f64()?,
        apparent_c: c
            .get("apparent_temperature")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
        humidity: c
            .get("relative_humidity_2m")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
        wind_kmh: c
            .get("wind_speed_10m")
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0),
        code: c.get("weather_code").and_then(|x| x.as_i64()).unwrap_or(-1),
        is_day: c.get("is_day").and_then(|x| x.as_i64()).unwrap_or(1) != 0,
        us_aqi: None,
        pm2_5: None,
        pm10: None,
        forecast_rain_mm: parse_forecast_rain(v),
        wind_direction_deg: c.get("wind_direction_10m").and_then(Value::as_f64),
        gusts_kmh: c.get("wind_gusts_10m").and_then(Value::as_f64),
        uv_index: c.get("uv_index").and_then(Value::as_f64),
        daily: parse_daily(v),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_block() {
        let v: Value = serde_json::from_str(
            r#"{"current":{"temperature_2m":33.5,"apparent_temperature":31.9,
                "relative_humidity_2m":27,"weather_code":0,"wind_speed_10m":14.8,"is_day":1},
                "daily":{"rain_sum":[4.2],"showers_sum":[1.3]}}"#,
        )
        .unwrap();
        let w = parse_current(&v).unwrap();
        assert_eq!(w.temp_c, 33.5);
        assert_eq!(w.code, 0);
        assert!(w.is_day);
        assert_eq!(w.humidity, 27.0);
        assert_eq!(w.forecast_rain_mm, Some(5.5));
    }

    #[test]
    fn parses_three_local_days() {
        let v: Value = serde_json::from_str(
            r#"{"current":{"temperature_2m":18.3,"weather_code":3,"wind_direction_10m":90,
                "wind_gusts_10m":23.8,"uv_index":0.0,"is_day":0},
                "daily":{"time":["2026-09-28","2026-09-29","2026-09-30"],
                "weather_code":[3,51,53],"temperature_2m_max":[19.8,26.2,23.2],
                "temperature_2m_min":[14.2,16.8,17.3],"precipitation_probability_max":[25,51,91],
                "rain_sum":[0.0,1.2,4.0],"showers_sum":[0.0,0.0,0.5],
                "sunrise":["2026-09-28T06:52","2026-09-29T06:54","2026-09-30T06:55"],
                "sunset":["2026-09-28T18:44","2026-09-29T18:42","2026-09-30T18:40"]}}"#,
        )
        .unwrap();
        let w = parse_current(&v).unwrap();
        assert_eq!(w.wind_direction_deg, Some(90.0));
        assert_eq!(w.daily.len(), 3);
        assert_eq!(w.daily[0].weekday, "Monday");
        assert_eq!(w.daily[0].sunrise.as_deref(), Some("06:52"));
        assert_eq!(w.daily[2].code, 53);
        assert_eq!(w.daily[2].rain_mm, Some(4.5));
        assert_eq!(w.daily[1].precipitation_probability, Some(51.0));
        assert_eq!(w.forecast_rain_mm, Some(0.0));
    }

    #[test]
    fn nws_coverage_is_us_only() {
        assert!(in_nws_coverage(40.7, -74.0));
        assert!(in_nws_coverage(61.2, -149.9));
        assert!(in_nws_coverage(21.3, -157.8));
        assert!(!in_nws_coverage(51.5, -0.12));
        assert!(!in_nws_coverage(-33.9, 151.2));
    }

    #[test]
    fn missing_current_is_none() {
        let v: Value = serde_json::from_str(r#"{"error":true}"#).unwrap();
        assert!(parse_current(&v).is_none());
    }

    #[test]
    fn parses_optional_air_quality() {
        let v: Value =
            serde_json::from_str(r#"{"current":{"us_aqi":42,"pm2_5":8.1,"pm10":15.4}}"#).unwrap();
        assert_eq!(
            parse_air_quality(&v),
            Some((Some(42.0), Some(8.1), Some(15.4)))
        );
    }

    #[test]
    fn missing_daily_rain_does_not_suppress_current_weather() {
        let v: Value = serde_json::from_str(
            r#"{"current":{"temperature_2m":12.0,"weather_code":3,"is_day":1}}"#,
        )
        .unwrap();
        let weather = parse_current(&v).unwrap();
        assert_eq!(weather.forecast_rain_mm, None);
    }

    #[test]
    fn parses_only_actual_nws_alerts() {
        let value = serde_json::json!({
            "features": [
                {
                    "properties": {
                        "status": "Actual",
                        "event": "Tornado Warning",
                        "severity": "Extreme",
                        "areaDesc": "Adams; Brown; Clermont; Highland",
                        "ends": "2026-09-28T21:45:00+00:00",
                        "parameters": {"VTEC": ["/O.NEW.KILN.TO.W.0023.260928T2100Z-260928T2145Z/"]}
                    }
                },
                {
                    "properties": {
                        "status": "Test",
                        "event": "Required Weekly Test",
                        "severity": "Minor"
                    }
                },
                {
                    "properties": {
                        "status": "Actual",
                        "event": "",
                        "severity": "Severe"
                    }
                }
            ]
        });

        assert_eq!(
            parse_alerts(&value),
            WeatherAlertsResult {
                alerts: vec![WeatherAlert {
                    event: "Tornado Warning".into(),
                    severity: "Extreme".into(),
                    key: "nws|KILN.TO.W.0023|3".into(),
                    area: "Adams, Brown, Clermont…".into(),
                    level: 3,
                    expires: 1_790_631_900,
                    source: "NWS".into(),
                }],
                incomplete: false,
            }
        );
    }

    #[test]
    fn nws_levels_follow_warning_types() {
        assert_eq!(nws_level("Tornado Emergency", "Extreme"), 3);
        assert_eq!(nws_level("Severe Thunderstorm Warning", "Severe"), 2);
        assert_eq!(nws_level("Winter Storm Watch", "Moderate"), 1);
        assert_eq!(nws_level("Wind Advisory", "Minor"), 0);
    }

    #[test]
    fn malformed_nws_response_has_no_alerts() {
        assert_eq!(
            parse_alerts(&serde_json::json!({"features": null})),
            WeatherAlertsResult::default()
        );
    }
}
