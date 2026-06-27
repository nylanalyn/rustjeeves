//! Web search through Tavily. Exposed as a narrow host function so WASM modules do not receive
//! unrestricted network access or API credentials.

use jeeves_abi::{SearchResponse, SearchResult};
use serde_json::{json, Value};
use std::time::Duration;

const ENDPOINT: &str = "https://api.tavily.com/search";
pub const API_KEY_CONFIG: &str = "tavily_api_key";
const MAX_QUERY_CHARS: usize = 400;
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;

/// Search Tavily for up to three ranked web results.
pub fn search(query: &str, configured_api_key: Option<&str>) -> SearchResponse {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return failure("invalid_query");
    }
    let api_key = configured_api_key
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("RUSTJEEVES_TAVILY_API_KEY").ok())
        .or_else(|| std::env::var("TAVILY_API_KEY").ok());
    let Some(api_key) = api_key else {
        return failure("not_configured");
    };
    if api_key.trim().is_empty() {
        return failure("not_configured");
    }

    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build(),
    );
    let request = json!({
        "query": query,
        "search_depth": "basic",
        "max_results": 3,
        "include_answer": false,
        "include_images": false,
        "include_raw_content": false
    });
    let response = agent
        .post(ENDPOINT)
        .header("Authorization", &format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .send(serde_json::to_string(&request).unwrap_or_default());
    let Ok(mut response) = response else {
        return failure("unavailable");
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
    parse_response(&value)
}

fn parse_response(value: &Value) -> SearchResponse {
    let results = value
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let title = item.get("title")?.as_str()?.trim();
            let url = item.get("url")?.as_str()?.trim();
            if title.is_empty() || !(url.starts_with("http://") || url.starts_with("https://")) {
                return None;
            }
            let snippet = item
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            Some(SearchResult {
                title: clean(title, 160),
                url: clean(url, 500),
                snippet: clean(snippet, 300),
            })
        })
        .take(3)
        .collect();
    SearchResponse {
        results,
        error: None,
    }
}

fn clean(input: &str, max_chars: usize) -> String {
    input
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn failure(kind: &str) -> SearchResponse {
    SearchResponse {
        results: Vec::new(),
        error: Some(kind.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_filters_results() {
        let value: Value = serde_json::from_str(
            r#"{"results":[
                {"title":" Rust \n Language ","url":"https://www.rust-lang.org/","content":"Fast and safe."},
                {"title":"bad","url":"javascript:alert(1)","content":"nope"}
            ]}"#,
        )
        .unwrap();
        let response = parse_response(&value);
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Rust Language");
        assert_eq!(response.results[0].snippet, "Fast and safe.");
    }

    #[test]
    fn missing_results_is_not_a_transport_error() {
        let response = parse_response(&json!({"results": []}));
        assert!(response.results.is_empty());
        assert!(response.error.is_none());
    }
}
