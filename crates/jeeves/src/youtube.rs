//! Narrow YouTube Data API v3 provider for search and video metadata lookup.

use jeeves_abi::{YoutubeResponse, YoutubeResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const API_KEY_CONFIG: &str = "youtube_api_key";
const API_ROOT: &str = "https://www.googleapis.com/youtube/v3";
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_CAP: usize = 256;

struct CachedVideo {
    inserted: Instant,
    result: YoutubeResult,
}

static CACHE: OnceLock<Mutex<HashMap<String, CachedVideo>>> = OnceLock::new();

pub fn lookup(ids: &[String], configured_key: Option<&str>) -> YoutubeResponse {
    let mut ids = ids
        .iter()
        .map(|id| id.trim())
        .filter(|id| valid_video_id(id))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let mut seen = std::collections::HashSet::new();
    ids.retain(|id| seen.insert(id.clone()));
    if ids.is_empty() || ids.len() > 50 {
        return failure("invalid_request");
    }
    let key = match api_key(configured_key) {
        Some(key) => key,
        None => return failure("not_configured"),
    };

    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let mut results = Vec::new();
    let mut missing = Vec::new();
    {
        let mut cache = cache.lock().unwrap();
        cache.retain(|_, value| value.inserted.elapsed() < CACHE_TTL);
        for id in &ids {
            match cache.get(id) {
                Some(value) => results.push(value.result.clone()),
                None => missing.push(id.clone()),
            }
        }
    }
    if !missing.is_empty() {
        let url = format!(
            "{API_ROOT}/videos?part=snippet,contentDetails,statistics&id={}&key={}",
            url_encode(&missing.join(",")),
            url_encode(&key)
        );
        let value = match get_json(&url) {
            Ok(value) => value,
            Err(error) => return failure(error),
        };
        let fetched = parse_videos(&value);
        {
            let mut cache = cache.lock().unwrap();
            for result in &fetched {
                cache.insert(
                    result.video_id.clone(),
                    CachedVideo {
                        inserted: Instant::now(),
                        result: result.clone(),
                    },
                );
            }
            if cache.len() > CACHE_CAP {
                let mut ages = cache
                    .iter()
                    .map(|(id, value)| (id.clone(), value.inserted))
                    .collect::<Vec<_>>();
                ages.sort_by_key(|(_, inserted)| *inserted);
                for (id, _) in ages.into_iter().take(cache.len() - CACHE_CAP) {
                    cache.remove(&id);
                }
            }
        }
        results.extend(fetched);
    }
    results.sort_by_key(|result| {
        ids.iter()
            .position(|id| id == &result.video_id)
            .unwrap_or(usize::MAX)
    });
    if results.is_empty() {
        failure("not_found")
    } else {
        YoutubeResponse {
            results,
            error: None,
        }
    }
}

pub fn search(query: &str, configured_key: Option<&str>) -> YoutubeResponse {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 200 {
        return failure("invalid_request");
    }
    let key = match api_key(configured_key) {
        Some(key) => key,
        None => return failure("not_configured"),
    };
    let url = format!(
        "{API_ROOT}/search?part=snippet&type=video&maxResults=1&q={}&key={}",
        url_encode(query),
        url_encode(&key)
    );
    let value = match get_json(&url) {
        Ok(value) => value,
        Err(error) => return failure(error),
    };
    let Some(id) = value
        .get("items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.pointer("/id/videoId"))
        .and_then(Value::as_str)
        .filter(|id| valid_video_id(id))
    else {
        return failure("not_found");
    };
    lookup(&[id.to_string()], Some(&key))
}

fn api_key(configured: Option<&str>) -> Option<String> {
    configured
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("RUSTJEEVES_YOUTUBE_API_KEY").ok())
        .or_else(|| std::env::var("YOUTUBE_API_KEY").ok())
        .filter(|key| !key.trim().is_empty())
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
        Err(ureq::Error::StatusCode(403 | 429)) => return Err("quota_exceeded"),
        Err(ureq::Error::StatusCode(404)) => return Err("not_found"),
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

fn parse_videos(value: &Value) -> Vec<YoutubeResult> {
    value
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = item.get("id")?.as_str()?;
            if !valid_video_id(id) {
                return None;
            }
            let title = clean(item.pointer("/snippet/title")?.as_str()?, 100);
            let channel = clean(item.pointer("/snippet/channelTitle")?.as_str()?, 80);
            let published_at = clean(item.pointer("/snippet/publishedAt")?.as_str()?, 40);
            let duration_seconds =
                parse_duration(item.pointer("/contentDetails/duration")?.as_str()?)?;
            let view_count = item
                .pointer("/statistics/viewCount")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let like_count = item
                .pointer("/statistics/likeCount")
                .and_then(Value::as_str)
                .and_then(|value| value.parse().ok());
            Some(YoutubeResult {
                video_id: id.into(),
                title,
                channel,
                view_count,
                like_count,
                duration_seconds,
                published_at,
            })
        })
        .collect()
}

fn parse_duration(value: &str) -> Option<u64> {
    let chars = value.strip_prefix('P')?.chars();
    let mut total = 0_u64;
    let mut number = 0_u64;
    let mut in_time = false;
    let mut saw_unit = false;
    for character in chars {
        match character {
            'T' => in_time = true,
            '0'..='9' => {
                number = number
                    .checked_mul(10)?
                    .checked_add(character.to_digit(10)? as u64)?
            }
            'D' => {
                total = total.checked_add(number.checked_mul(86_400)?)?;
                number = 0;
                saw_unit = true;
            }
            'H' if in_time => {
                total = total.checked_add(number.checked_mul(3_600)?)?;
                number = 0;
                saw_unit = true;
            }
            'M' if in_time => {
                total = total.checked_add(number.checked_mul(60)?)?;
                number = 0;
                saw_unit = true;
            }
            'S' if in_time => {
                total = total.checked_add(number)?;
                number = 0;
                saw_unit = true;
            }
            _ => return None,
        }
    }
    (saw_unit && number == 0).then_some(total)
}

fn valid_video_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn clean(value: &str, max_chars: usize) -> String {
    value
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn url_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b',') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn failure(error: &str) -> YoutubeResponse {
    YoutubeResponse {
        results: Vec::new(),
        error: Some(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_realistic_video_response() {
        let value = serde_json::json!({"items": [{
            "id": "dQw4w9WgXcQ",
            "snippet": {"title": "Rick &amp; Roll", "channelTitle": "Rick", "publishedAt": "2009-10-25T06:57:33Z"},
            "contentDetails": {"duration": "PT3M33S"},
            "statistics": {"viewCount": "1600000000", "likeCount": "18000000"}
        }]});
        let parsed = parse_videos(&value);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].title, "Rick & Roll");
        assert_eq!(parsed[0].duration_seconds, 213);
        assert_eq!(parsed[0].view_count, 1_600_000_000);
    }

    #[test]
    fn validates_ids_and_iso_durations() {
        assert!(valid_video_id("dQw4w9WgXcQ"));
        assert!(!valid_video_id("too-short"));
        assert_eq!(parse_duration("PT1H2M3S"), Some(3_723));
        assert_eq!(parse_duration("P1DT2M"), Some(86_520));
        assert_eq!(parse_duration("PT1"), None);
    }
}
