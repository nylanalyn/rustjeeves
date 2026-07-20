//! Geocoding via the keyless Open-Meteo geocoding API. Exposed to modules as the `geocode` host
//! function so a `.wasm` plugin doesn't need network access of its own, and so the integration is
//! reusable by a future weather module.

use jeeves_abi::GeoResult;
use serde_json::Value;
use std::time::Duration;

/// Resolve a free-text location to a best-match result, or `None` if nothing matched / the request
/// failed. The part before the first comma is the search name; the remainder (e.g. "England") is a
/// hint used to disambiguate among results.
pub fn geocode(query: &str) -> Option<GeoResult> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let (name, hint) = match query.split_once(',') {
        Some((n, rest)) => (n.trim(), rest.trim()),
        None => (query, ""),
    };

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(6)))
            .build(),
    );
    let body = agent
        .get("https://geocoding-api.open-meteo.com/v1/search")
        .query("name", name)
        .query("count", "10")
        .query("language", "en")
        .query("format", "json")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()?;

    let v: Value = serde_json::from_str(&body).ok()?;
    pick_best(&v, hint)
}

/// Choose the best result from an Open-Meteo response given a region `hint`. Pure (no network) so
/// it can be unit-tested.
fn pick_best(v: &Value, hint: &str) -> Option<GeoResult> {
    let results = v.get("results")?.as_array()?;
    if hint.is_empty() {
        return to_result(results.first()?);
    }
    // Location input commonly carries several disambiguators (for example, "Melbourne,
    // Florida, US"). Matching the entire suffix against one API field always scores zero, which
    // silently falls back to the first result (often Melbourne, Australia). Score each component
    // instead, so every matching region/country makes the intended result more specific.
    let hints: Vec<String> = hint
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_lowercase)
        .collect();
    let score = |r: &Value| -> i32 {
        hints
            .iter()
            .filter(|hint| {
                ["admin1", "admin2", "country", "country_code"]
                    .iter()
                    .filter_map(|key| r.get(*key).and_then(|value| value.as_str()))
                    .any(|value| value.to_lowercase().contains(hint.as_str()))
            })
            .count() as i32
    };
    // Strict `>` keeps the earliest (most relevant) result on a score tie.
    let mut best: Option<(&Value, i32)> = None;
    for r in results {
        let s = score(r);
        if best.is_none_or(|(_, bs)| s > bs) {
            best = Some((r, s));
        }
    }
    to_result(best?.0)
}

fn to_result(r: &Value) -> Option<GeoResult> {
    Some(GeoResult {
        name: r.get("name")?.as_str()?.to_string(),
        admin1: r.get("admin1").and_then(|x| x.as_str()).map(String::from),
        admin2: r.get("admin2").and_then(|x| x.as_str()).map(String::from),
        country: r.get("country").and_then(|x| x.as_str()).map(String::from),
        lat: r.get("latitude")?.as_f64()?,
        lon: r.get("longitude")?.as_f64()?,
        timezone: r.get("timezone")?.as_str()?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the real Open-Meteo response for "Hackney".
    const SAMPLE: &str = r#"{"results":[
        {"name":"Hackney","admin1":"England","country":"United Kingdom","country_code":"GB","latitude":51.55,"longitude":-0.05,"timezone":"Europe/London"},
        {"name":"Hackney","admin1":"Eastern Cape","country":"South Africa","country_code":"ZA","latitude":-32.31,"longitude":26.64,"timezone":"Africa/Johannesburg"}
    ]}"#;

    #[test]
    fn hint_disambiguates_results() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let england = pick_best(&v, "England").unwrap();
        assert_eq!(england.country.as_deref(), Some("United Kingdom"));
        assert!((england.lat - 51.55).abs() < 0.01);
        assert_eq!(england.timezone, "Europe/London");

        let sa = pick_best(&v, "South Africa").unwrap();
        assert_eq!(sa.country.as_deref(), Some("South Africa"));
    }

    #[test]
    fn no_hint_takes_first_result() {
        let v: Value = serde_json::from_str(SAMPLE).unwrap();
        let r = pick_best(&v, "").unwrap();
        assert_eq!(r.country.as_deref(), Some("United Kingdom"));
    }

    #[test]
    fn multiple_hints_disambiguate_melbourne_florida() {
        let v: Value = serde_json::from_str(
            r#"{"results":[
                {"name":"Melbourne","admin1":"Victoria","country":"Australia","country_code":"AU","latitude":-37.81,"longitude":144.96,"timezone":"Australia/Melbourne"},
                {"name":"Melbourne","admin1":"Florida","admin2":"Brevard County","country":"United States","country_code":"US","latitude":28.08,"longitude":-80.61,"timezone":"America/New_York"}
            ]}"#,
        )
        .unwrap();
        let florida = pick_best(&v, "Brevard County, Florida, United States").unwrap();
        assert_eq!(florida.timezone, "America/New_York");
        assert!((florida.lat - 28.08).abs() < 0.01);
    }

    #[test]
    fn empty_results_is_none() {
        let v: Value = serde_json::from_str(r#"{"generationtime_ms":0.1}"#).unwrap();
        assert!(pick_best(&v, "").is_none());
    }
}
