//! Current weather via the keyless Open-Meteo forecast API and active US alerts from the National
//! Weather Service. Exposed to modules as host functions, reusing the `geocode`/`profile` plumbing
//! so a weather module needs no network access of its own.

use jeeves_abi::{WeatherAlert, WeatherAlertsResult, WeatherResult};
use serde_json::Value;
use std::time::Duration;

const MAX_NWS_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_NWS_ALERTS: usize = 16;
const MAX_ALERT_EVENT_CHARS: usize = 96;

/// Fetch current conditions for a coordinate, or `None` on failure.
pub fn weather(lat: f64, lon: f64) -> Option<WeatherResult> {
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
            "temperature_2m,apparent_temperature,relative_humidity_2m,weather_code,wind_speed_10m,is_day",
        )
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
        return WeatherAlertsResult::default();
    };
    let Ok(body) = response
        .body_mut()
        .with_config()
        .limit(MAX_NWS_RESPONSE_BYTES)
        .read_to_string()
    else {
        return WeatherAlertsResult::default();
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .map_or_else(WeatherAlertsResult::default, |value| parse_alerts(&value))
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
            Some(WeatherAlert {
                event: event.chars().take(MAX_ALERT_EVENT_CHARS).collect(),
                severity: properties
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("Unknown")
                    .chars()
                    .take(16)
                    .collect(),
            })
        })
        .take(MAX_NWS_ALERTS)
        .collect();
    WeatherAlertsResult { alerts }
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_current_block() {
        let v: Value = serde_json::from_str(
            r#"{"current":{"temperature_2m":33.5,"apparent_temperature":31.9,
                "relative_humidity_2m":27,"weather_code":0,"wind_speed_10m":14.8,"is_day":1}}"#,
        )
        .unwrap();
        let w = parse_current(&v).unwrap();
        assert_eq!(w.temp_c, 33.5);
        assert_eq!(w.code, 0);
        assert!(w.is_day);
        assert_eq!(w.humidity, 27.0);
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
    fn parses_only_actual_nws_alerts() {
        let value = serde_json::json!({
            "features": [
                {
                    "properties": {
                        "status": "Actual",
                        "event": "Tornado Warning",
                        "severity": "Extreme"
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
                }],
            }
        );
    }

    #[test]
    fn malformed_nws_response_has_no_alerts() {
        assert_eq!(
            parse_alerts(&serde_json::json!({"features": null})),
            WeatherAlertsResult::default()
        );
    }
}
