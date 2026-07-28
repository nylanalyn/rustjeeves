//! Narrow, provider-neutral GIF search backed by KLIPY.

use jeeves_abi::{GifSearchResponse, GifSearchResult};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const API_KEY_CONFIG: &str = "klipy_api_key";
const API_ROOT: &str = "https://api.klipy.com/api/v1";
const PROVIDER: &str = "KLIPY";
const MAX_QUERY_CHARS: usize = 80;
const MAX_RESULTS: u32 = 12;
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(1);
const REQUEST_WINDOW: Duration = Duration::from_secs(60 * 60);
const MAX_REQUESTS_PER_WINDOW: usize = 90;

#[derive(Default)]
struct RequestGate {
    last: Option<Instant>,
    recent: VecDeque<Instant>,
}

static REQUEST_GATE: OnceLock<Mutex<RequestGate>> = OnceLock::new();

pub fn search(query: &str, limit: u32, configured_key: Option<&str>) -> GifSearchResponse {
    let query = query.trim();
    if query.is_empty()
        || query.chars().count() > MAX_QUERY_CHARS
        || !(1..=MAX_RESULTS).contains(&limit)
    {
        return failure("invalid_request");
    }
    let key = match api_key(configured_key) {
        Some(key) => key,
        None => return failure("not_configured"),
    };
    if !admit_request() {
        return failure("rate_limited");
    }
    let url = format!(
        "{API_ROOT}/{}/gifs/search?q={}&per_page={limit}",
        url_encode(&key),
        url_encode(query),
    );
    let value = match get_json(&url) {
        Ok(value) => value,
        Err(error) => return failure(error),
    };
    let results = parse_results(&value, limit as usize);
    if results.is_empty() {
        failure("not_found")
    } else {
        GifSearchResponse {
            results,
            provider: PROVIDER.into(),
            error: None,
        }
    }
}

fn api_key(configured: Option<&str>) -> Option<String> {
    configured
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("RUSTJEEVES_KLIPY_API_KEY").ok())
        .filter(|key| !key.trim().is_empty())
}

fn admit_request() -> bool {
    let now = Instant::now();
    let mut gate = REQUEST_GATE
        .get_or_init(|| Mutex::new(RequestGate::default()))
        .lock()
        .unwrap();
    while gate
        .recent
        .front()
        .is_some_and(|instant| now.duration_since(*instant) >= REQUEST_WINDOW)
    {
        gate.recent.pop_front();
    }
    if gate
        .last
        .is_some_and(|instant| now.duration_since(instant) < MIN_REQUEST_INTERVAL)
        || gate.recent.len() >= MAX_REQUESTS_PER_WINDOW
    {
        return false;
    }
    gate.last = Some(now);
    gate.recent.push_back(now);
    true
}

fn get_json(url: &str) -> Result<Value, &'static str> {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build(),
    );
    let mut response = match agent.get(url).call() {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(400)) => return Err("invalid_request"),
        Err(ureq::Error::StatusCode(401 | 403)) => return Err("authentication"),
        Err(ureq::Error::StatusCode(404)) => return Err("not_found"),
        Err(ureq::Error::StatusCode(429)) => return Err("rate_limited"),
        Err(_) => return Err("unavailable"),
    };
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|_| "unavailable")?;
    serde_json::from_str(&body).map_err(|_| "unavailable")
}

fn parse_results(value: &Value, limit: usize) -> Vec<GifSearchResult> {
    result_items(value)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let url = [
                "/media_formats/gif/url",
                "/media_formats/mediumgif/url",
                "/media_formats/tinygif/url",
                "/images/original/url",
                "/file/hd/gif/url",
                "/file/sd/gif/url",
                "/file/gif/url",
                "/file/url",
                "/gif/url",
                "/url",
            ]
            .iter()
            .find_map(|path| item.pointer(path).and_then(Value::as_str))
            .filter(|url| valid_media_url(url))?;
            let title = [
                "/content_description",
                "/title",
                "/name",
                "/contentDescription",
            ]
            .iter()
            .find_map(|path| item.pointer(path).and_then(Value::as_str))
            .map(|title| clean(title, 100))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| "GIF".into());
            Some(GifSearchResult {
                url: url.into(),
                title,
            })
        })
        .take(limit)
        .collect()
}

fn result_items(value: &Value) -> Option<&Vec<Value>> {
    value
        .get("results")
        .and_then(Value::as_array)
        .or_else(|| value.pointer("/data/data").and_then(Value::as_array))
        .or_else(|| value.get("data").and_then(Value::as_array))
}

fn valid_media_url(url: &str) -> bool {
    if url.len() > 2_048 || url.chars().any(char::is_control) {
        return false;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("").to_ascii_lowercase();
    host == "klipy.com"
        || host.ends_with(".klipy.com")
        || host == "klipy.co"
        || host.ends_with(".klipy.co")
}

fn clean(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

fn failure(error: &str) -> GifSearchResponse {
    GifSearchResponse {
        results: Vec::new(),
        provider: PROVIDER.into(),
        error: Some(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tenor_compatible_klipy_results() {
        let value = serde_json::json!({
            "results": [{
                "content_description": "Danger alarm",
                "media_formats": {
                    "gif": { "url": "https://media.klipy.com/danger.gif" }
                }
            }]
        });
        assert_eq!(
            parse_results(&value, 8),
            vec![GifSearchResult {
                url: "https://media.klipy.com/danger.gif".into(),
                title: "Danger alarm".into(),
            }]
        );
    }

    #[test]
    fn accepts_newer_nested_data_shape_and_rejects_foreign_urls() {
        let value = serde_json::json!({
            "data": { "data": [
                { "title": "Nope", "file": { "hd": { "gif": { "url": "https://example.com/nope.gif" } } } },
                { "title": "Okay", "file": { "hd": { "gif": { "url": "https://static.klipy.co/okay.gif" } } } }
            ]}
        });
        let results = parse_results(&value, 8);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Okay");
    }

    #[test]
    fn encodes_queries_and_validates_media_urls() {
        assert_eq!(url_encode("danger, danger"), "danger%2C%20danger");
        assert!(valid_media_url("https://cdn.klipy.com/a.gif"));
        assert!(!valid_media_url("http://cdn.klipy.com/a.gif"));
        assert!(!valid_media_url("https://klipy.com.example/a.gif"));
    }
}
