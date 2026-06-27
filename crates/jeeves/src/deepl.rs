//! Text translation through DeepL. Credentials and unrestricted HTTP remain in the host.

use jeeves_abi::{TranslateQuery, TranslateResponse};
use serde_json::{json, Value};
use std::time::Duration;

pub const API_KEY_CONFIG: &str = "deepl_api_key";
const FREE_ENDPOINT: &str = "https://api-free.deepl.com/v2/translate";
const PAID_ENDPOINT: &str = "https://api.deepl.com/v2/translate";
const MAX_TEXT_CHARS: usize = 1_000;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;

pub fn translate(req: &TranslateQuery, configured_api_key: Option<&str>) -> TranslateResponse {
    let text = req.text.trim();
    let target = normalize_language(&req.target_lang);
    let source = req.source_lang.as_deref().map(normalize_language);
    if text.is_empty()
        || text.chars().count() > MAX_TEXT_CHARS
        || target.is_empty()
        || source.as_deref().is_some_and(str::is_empty)
    {
        return failure("invalid_request");
    }
    if source
        .as_deref()
        .is_some_and(|source| language_base(source) == language_base(&target))
    {
        return failure("same_language");
    }

    let api_key = configured_api_key
        .filter(|key| !key.trim().is_empty())
        .map(str::to_string)
        .or_else(|| std::env::var("RUSTJEEVES_DEEPL_API_KEY").ok())
        .or_else(|| std::env::var("DEEPL_AUTH_KEY").ok());
    let Some(api_key) = api_key else {
        return failure("not_configured");
    };
    let api_key = api_key.trim();
    if api_key.is_empty() {
        return failure("not_configured");
    }
    let endpoint = if api_key.ends_with(":fx") {
        FREE_ENDPOINT
    } else {
        PAID_ENDPOINT
    };

    let mut body = json!({
        "text": [text],
        "target_lang": target,
        "show_billed_characters": true
    });
    if let Some(source) = source {
        body["source_lang"] = Value::String(source);
    }
    let Ok(body) = serde_json::to_string(&body) else {
        return failure("invalid_request");
    };
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build(),
    );
    let response = agent
        .post(endpoint)
        .header("Authorization", &format!("DeepL-Auth-Key {api_key}"))
        .header("Content-Type", "application/json")
        .send(body);
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(403)) => return failure("authentication"),
        Err(ureq::Error::StatusCode(429)) => return failure("rate_limited"),
        Err(ureq::Error::StatusCode(456)) => return failure("quota_exceeded"),
        Err(ureq::Error::StatusCode(400)) => return failure("invalid_request"),
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
    parse_response(&value)
}

fn normalize_language(value: &str) -> String {
    let value = value.trim();
    if (2..=10).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || byte == b'-')
    {
        value.to_ascii_uppercase()
    } else {
        String::new()
    }
}

fn language_base(value: &str) -> &str {
    value.split('-').next().unwrap_or(value)
}

fn parse_response(value: &Value) -> TranslateResponse {
    let Some(first) = value
        .get("translations")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    else {
        return failure("unavailable");
    };
    let Some(text) = first.get("text").and_then(Value::as_str) else {
        return failure("unavailable");
    };
    TranslateResponse {
        text: Some(clean(text, MAX_TEXT_CHARS)),
        detected_source_language: first
            .get("detected_source_language")
            .and_then(Value::as_str)
            .map(|language| clean(language, 10)),
        error: None,
    }
}

fn clean(input: &str, max_chars: usize) -> String {
    input
        .chars()
        .filter(|character| !character.is_control())
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn failure(kind: &str) -> TranslateResponse {
    TranslateResponse {
        text: None,
        detected_source_language: None,
        error: Some(kind.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_translation_response() {
        let value = json!({
            "translations": [{"detected_source_language": "EN", "text": "Bonjour !"}]
        });
        let response = parse_response(&value);
        assert_eq!(response.text.as_deref(), Some("Bonjour !"));
        assert_eq!(response.detected_source_language.as_deref(), Some("EN"));
        assert!(response.error.is_none());
    }

    #[test]
    fn validates_language_codes() {
        assert_eq!(normalize_language("en-gb"), "EN-GB");
        assert!(normalize_language("not a language").is_empty());
        assert_eq!(language_base("EN-US"), "EN");
    }
}
