//! Keyless dictionary lookups through dictionaryapi.dev. The host owns the fixed endpoint and
//! exposes only bounded, sanitized definitions to WASM modules.

use jeeves_abi::{DictionaryResponse, DictionarySense};
use serde_json::Value;
use std::time::Duration;

const ENDPOINT: &str = "https://api.dictionaryapi.dev/api/v2/entries/en/";
const MAX_WORD_CHARS: usize = 64;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_SENSES: usize = 3;

pub fn lookup(word: &str) -> DictionaryResponse {
    let word = word.trim();
    if !valid_word(word) {
        return failure("invalid_word");
    }
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .build(),
    );
    let response = agent.get(format!("{ENDPOINT}{}", encode_path(word))).call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => return failure("not_found"),
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

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().count() <= MAX_WORD_CHARS
        && word
            .chars()
            .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
}

fn parse_response(value: &Value) -> DictionaryResponse {
    let Some(entry) = value.as_array().and_then(|entries| entries.first()) else {
        return failure("not_found");
    };
    let word = entry
        .get("word")
        .and_then(Value::as_str)
        .map(|value| clean(value, MAX_WORD_CHARS));
    let phonetic = entry
        .get("phonetic")
        .and_then(Value::as_str)
        .map(|value| clean(value, 80))
        .filter(|value| !value.is_empty());
    let senses = entry
        .get("meanings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|meaning| {
            let part = meaning
                .get("partOfSpeech")
                .and_then(Value::as_str)
                .map(|value| clean(value, 32))
                .unwrap_or_default();
            meaning
                .get("definitions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |definition| {
                    let text = definition.get("definition")?.as_str()?;
                    let text = clean(text, 240);
                    (!text.is_empty()).then(|| DictionarySense {
                        part_of_speech: part.clone(),
                        definition: text,
                    })
                })
        })
        .take(MAX_SENSES)
        .collect::<Vec<_>>();
    if senses.is_empty() {
        return failure("not_found");
    }
    DictionaryResponse {
        word,
        phonetic,
        senses,
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

fn encode_path(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' => (*byte as char).to_string(),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn failure(kind: &str) -> DictionaryResponse {
    DictionaryResponse {
        error: Some(kind.into()),
        ..DictionaryResponse::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bounds_senses() {
        let value: Value = serde_json::from_str(
            r#"[{"word":"rust","phonetic":"/rʌst/","meanings":[
                {"partOfSpeech":"noun","definitions":[
                    {"definition":"Oxidized iron."},{"definition":"A reddish coating."}]},
                {"partOfSpeech":"verb","definitions":[
                    {"definition":"To become oxidized."},{"definition":"Ignored fourth sense."}]}
            ]}]"#,
        )
        .unwrap();
        let response = parse_response(&value);
        assert_eq!(response.word.as_deref(), Some("rust"));
        assert_eq!(response.phonetic.as_deref(), Some("/rʌst/"));
        assert_eq!(response.senses.len(), 3);
        assert_eq!(response.senses[2].part_of_speech, "verb");
    }

    #[test]
    fn validates_and_encodes_words() {
        assert!(valid_word("mother-in-law"));
        assert!(valid_word("don't"));
        assert!(!valid_word("two words"));
        assert!(!valid_word("rust/../../secret"));
        assert_eq!(encode_path("don't"), "don%27t");
    }
}
