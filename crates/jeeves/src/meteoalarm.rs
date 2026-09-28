//! Official European weather warnings through MeteoAlarm, the European weather services' shared
//! CAP feeds.
//!
//! Each country's feed is fetched at most every five minutes and parsed once. A warning covers a
//! coordinate when one of its areas' polygons contains the point (Sweden, the UK) or, for
//! Germany, whose feed names areas only by DWD warn-cell code, when the point's warn cell (looked
//! up once from DWD's map server and cached) is listed. More countries can be added to
//! [`COUNTRIES`]; polygon countries need nothing else.

use crate::weather::parse_time;
use jeeves_abi::{WeatherAlert, WeatherAlertsResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const FEED: &str = "https://feeds.meteoalarm.org/api/v1/warnings/feeds-";
const DWD_WFS: &str = "https://maps.dwd.de/geoserver/dwd/ows";
const FEED_TTL: Duration = Duration::from_secs(5 * 60);
const CELL_TTL: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const CELL_CACHE_CAP: usize = 1024;
/// Germany's feed is about a megabyte on a busy day.
const MAX_FEED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TEXT_CHARS: usize = 120;

struct Country {
    slug: &'static str,
    /// Rough bounding box (south, north, west, east) deciding which feeds to consult; the
    /// polygon or warn-cell match decides coverage.
    bounds: (f64, f64, f64, f64),
    dwd_cells: bool,
}

const COUNTRIES: &[Country] = &[
    Country {
        slug: "germany",
        bounds: (47.2, 55.1, 5.8, 15.1),
        dwd_cells: true,
    },
    Country {
        slug: "sweden",
        bounds: (55.2, 69.1, 10.9, 24.2),
        dwd_cells: false,
    },
    Country {
        slug: "united-kingdom",
        bounds: (49.8, 60.9, -8.7, 1.8),
        dwd_cells: false,
    },
];

#[derive(Clone, Debug, PartialEq)]
struct Area {
    desc: String,
    /// Rings of (lat, lon).
    polygons: Vec<Vec<(f64, f64)>>,
    cells: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq)]
struct Warning {
    event: String,
    severity: String,
    level: u8,
    expires: i64,
    areas: Vec<Area>,
}

type FeedCache = HashMap<&'static str, (Instant, Vec<Warning>)>;
type CellCache = HashMap<(i64, i64), (Instant, Option<u64>)>;

static FEEDS: OnceLock<Mutex<FeedCache>> = OnceLock::new();
static CELLS: OnceLock<Mutex<CellCache>> = OnceLock::new();

pub fn alerts(lat: f64, lon: f64) -> WeatherAlertsResult {
    let now = chrono::Utc::now().timestamp();
    let mut result = WeatherAlertsResult::default();
    for country in COUNTRIES {
        let (south, north, west, east) = country.bounds;
        if !((south..=north).contains(&lat) && (west..=east).contains(&lon)) {
            continue;
        }
        let Some(warnings) = feed(country.slug) else {
            result.incomplete = true;
            continue;
        };
        let live = warnings
            .iter()
            .filter(|warning| warning.expires == 0 || warning.expires > now)
            .collect::<Vec<_>>();
        if live.is_empty() {
            continue;
        }
        let cell = if country.dwd_cells
            && live
                .iter()
                .any(|w| w.areas.iter().any(|a| !a.cells.is_empty()))
        {
            match dwd_cell(lat, lon) {
                Ok(cell) => cell,
                Err(()) => {
                    result.incomplete = true;
                    None
                }
            }
        } else {
            None
        };
        for warning in live {
            let Some(area) = warning.areas.iter().find(|area| {
                area.polygons.iter().any(|ring| contains(ring, lat, lon))
                    || cell.is_some_and(|cell| area.cells.contains(&cell))
            }) else {
                continue;
            };
            result.alerts.push(WeatherAlert {
                event: warning.event.clone(),
                severity: warning.severity.clone(),
                key: format!(
                    "meteoalarm|{}|{}|{}|{}",
                    country.slug, warning.event, area.desc, warning.level
                )
                .to_lowercase(),
                area: area.desc.clone(),
                level: warning.level,
                expires: warning.expires,
                source: "MeteoAlarm".into(),
            });
        }
    }
    // The feed can carry an alert and its update side by side; keep one per key.
    let mut seen = std::collections::HashSet::new();
    result.alerts.retain(|alert| seen.insert(alert.key.clone()));
    result
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(12)))
            .user_agent(concat!(
                "rustjeeves/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    )
}

fn feed(slug: &'static str) -> Option<Vec<Warning>> {
    let cache = FEEDS.get_or_init(Default::default);
    if let Some((at, warnings)) = cache.lock().unwrap().get(slug) {
        if at.elapsed() < FEED_TTL {
            return Some(warnings.clone());
        }
    }
    let mut response = agent().get(format!("{FEED}{slug}")).call().ok()?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_FEED_BYTES)
        .read_to_string()
        .ok()?;
    let warnings = parse_feed(&serde_json::from_str(&body).ok()?);
    cache
        .lock()
        .unwrap()
        .insert(slug, (Instant::now(), warnings.clone()));
    Some(warnings)
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_TEXT_CHARS)
        .collect()
}

/// MeteoAlarm awareness levels: 1 green, 2 yellow, 3 orange, 4 red → 0..=3.
fn awareness_level(info: &Value) -> u8 {
    info.get("parameter")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|parameter| parameter["valueName"] == "awareness_level")
        .and_then(|parameter| parameter["value"].as_str())
        .and_then(|value| value.split(';').next())
        .and_then(|level| level.trim().parse::<u8>().ok())
        .map_or(0, |level| level.clamp(1, 4) - 1)
}

/// "55.3,12.6 55.2,12.8 …" → [(55.3, 12.6), (55.2, 12.8), …].
fn parse_ring(text: &str) -> Vec<(f64, f64)> {
    text.split_whitespace()
        .filter_map(|pair| {
            let (lat, lon) = pair.split_once(',')?;
            Some((lat.parse().ok()?, lon.parse().ok()?))
        })
        .collect()
}

/// Identifiers named in CAP `references` ("sender,identifier,sent sender,identifier,sent").
fn referenced(alert: &Value) -> impl Iterator<Item = &str> {
    alert["references"]
        .as_str()
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|triple| triple.split(',').nth(1))
}

fn parse_feed(value: &Value) -> Vec<Warning> {
    let entries = value["warnings"].as_array().cloned().unwrap_or_default();
    // Updates and cancellations replace the messages they reference, which the feed may still
    // carry alongside them.
    let superseded = entries
        .iter()
        .flat_map(|entry| referenced(&entry["alert"]))
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>();
    let mut warnings = Vec::new();
    for entry in &entries {
        let alert = &entry["alert"];
        if alert["status"]
            .as_str()
            .is_some_and(|status| status != "Actual")
            || alert["msgType"] == "Cancel"
            || alert["identifier"]
                .as_str()
                .is_some_and(|id| superseded.contains(id))
        {
            continue;
        }
        let infos = alert["info"].as_array().cloned().unwrap_or_default();
        let Some(info) = infos
            .iter()
            .find(|info| {
                info["language"]
                    .as_str()
                    .is_some_and(|lang| lang.starts_with("en"))
            })
            .or_else(|| infos.first())
        else {
            continue;
        };
        let event = text(&info["event"]);
        if event.is_empty() {
            continue;
        }
        let areas = info["area"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|area| Area {
                desc: text(&area["areaDesc"]),
                polygons: area["polygon"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(parse_ring)
                    .filter(|ring| ring.len() >= 3)
                    .collect(),
                cells: area["geocode"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|code| code["valueName"] == "WARNCELLID")
                    .filter_map(|code| code["value"].as_str()?.parse().ok())
                    .collect(),
            })
            .filter(|area| !area.polygons.is_empty() || !area.cells.is_empty())
            .collect::<Vec<_>>();
        if areas.is_empty() {
            continue;
        }
        warnings.push(Warning {
            event,
            severity: text(&info["severity"]),
            level: awareness_level(info),
            expires: parse_time(info.get("expires")),
            areas,
        });
    }
    warnings
}

/// Ray casting over (lat, lon) vertices.
fn contains(ring: &[(f64, f64)], lat: f64, lon: f64) -> bool {
    let mut inside = false;
    let mut previous = ring[ring.len() - 1];
    for &(y, x) in ring {
        let (py, px) = previous;
        if (y > lat) != (py > lat) && lon < (px - x) * (lat - y) / (py - y) + x {
            inside = !inside;
        }
        previous = (y, x);
    }
    inside
}

/// The DWD district warn cell containing a point: Ok(None) outside Germany, Err on a failed
/// lookup (not cached, so the next check retries).
fn dwd_cell(lat: f64, lon: f64) -> Result<Option<u64>, ()> {
    let key = ((lat * 100.0).round() as i64, (lon * 100.0).round() as i64);
    let cache = CELLS.get_or_init(Default::default);
    if let Some((at, cell)) = cache.lock().unwrap().get(&key) {
        if at.elapsed() < CELL_TTL {
            return Ok(*cell);
        }
    }
    // This GeoServer reads EPSG:4326 points latitude first.
    let mut response = agent()
        .get(DWD_WFS)
        .query("service", "WFS")
        .query("version", "2.0.0")
        .query("request", "GetFeature")
        .query("typeName", "dwd:Warngebiete_Kreise")
        .query("outputFormat", "application/json")
        .query("propertyName", "WARNCELLID")
        .query("CQL_FILTER", format!("CONTAINS(SHAPE,POINT({lat} {lon}))"))
        .call()
        .map_err(|_| ())?;
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_string()
        .map_err(|_| ())?;
    let value: Value = serde_json::from_str(&body).map_err(|_| ())?;
    let cell = value
        .pointer("/features/0/properties/WARNCELLID")
        .and_then(Value::as_u64);
    let mut cache = cache.lock().unwrap();
    if cache.len() >= CELL_CACHE_CAP {
        cache.clear();
    }
    cache.insert(key, (Instant::now(), cell));
    Ok(cell)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_english_info_levels_polygons_and_cells() {
        let value = serde_json::json!({"warnings": [
            {"alert": {"status": "Actual", "msgType": "Alert", "info": [
                {"language": "sv-SE", "event": "Kuling", "area": [{"areaDesc": "Hav", "polygon": ["1,1 1,2 2,2"]}]},
                {"language": "en-GB", "event": "Near  gale", "severity": "Moderate",
                 "expires": "2026-09-24T02:00:00+00:00",
                 "parameter": [{"valueName": "awareness_level", "value": "3; orange; Severe"}],
                 "area": [{"areaDesc": "Sea", "polygon": ["55,12 55,13 56,13 56,12"]}]}
            ]}},
            {"alert": {"status": "Actual", "msgType": "Alert", "info": [
                {"language": "en", "event": "wind gusts", "severity": "Moderate",
                 "parameter": [{"valueName": "awareness_level", "value": "2; yellow; Moderate"}],
                 "area": [{"areaDesc": "Kreis Fulda", "geocode": [
                     {"valueName": "EMMA_ID", "value": "DE248"},
                     {"valueName": "WARNCELLID", "value": "106631000"}]}]}
            ]}},
            {"alert": {"identifier": "old", "status": "Actual", "msgType": "Alert", "info": [
                {"language": "en", "event": "fog", "area": [{"areaDesc": "X", "polygon": ["1,1 1,2 2,2"]}]}
            ]}},
            {"alert": {"status": "Actual", "msgType": "Cancel", "references": "dwd,old,2026-09-23T07:00:00+00:00",
                "info": [{"language": "en", "event": "fog", "area": [{"areaDesc": "X", "polygon": ["1,1 1,2 2,2"]}]}
            ]}}
        ]});
        let warnings = parse_feed(&value);
        assert_eq!(warnings.len(), 2);
        assert_eq!(warnings[0].event, "Near gale");
        assert_eq!(warnings[0].level, 2);
        assert_eq!(warnings[0].expires, 1_790_215_200);
        assert!(contains(&warnings[0].areas[0].polygons[0], 55.5, 12.5));
        assert!(!contains(&warnings[0].areas[0].polygons[0], 57.0, 12.5));
        assert_eq!(warnings[1].level, 1);
        assert_eq!(warnings[1].areas[0].cells, [106_631_000]);
    }
}
