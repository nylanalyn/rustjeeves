//! Keyless dictionary lookups. dictionaryapi.dev is asked first (it has phonetics and synonyms);
//! when it is down or doesn't know the term, English Wiktionary's REST definitions are used
//! instead (they cover phrases such as "ice cream"). The host owns both fixed endpoints and exposes
//! only bounded, sanitized definitions to WASM modules.

use jeeves_abi::{DictionaryResponse, DictionarySense};
use serde_json::Value;
use std::time::Duration;

const ENDPOINT: &str = "https://api.dictionaryapi.dev/api/v2/entries/en/";
const WIKTIONARY_ENDPOINT: &str = "https://en.wiktionary.org/api/rest_v1/page/definition/";
const MAX_WORD_CHARS: usize = 64;
const MAX_WORDS: usize = 3;
const MAX_SYNONYMS: usize = 6;
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
            .user_agent(concat!(
                "rustjeeves-bot/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    );
    let primary = match fetch_json(&agent, &format!("{ENDPOINT}{}", encode_path(word))) {
        Ok(value) => parse_response(&value),
        Err(kind) => failure(kind),
    };
    if primary.error.is_none() {
        return primary;
    }
    let fallback = match fetch_json(
        &agent,
        &format!(
            "{WIKTIONARY_ENDPOINT}{}",
            encode_path(&word.replace(' ', "_"))
        ),
    ) {
        Ok(value) => parse_wiktionary(word, &value),
        Err(kind) => failure(kind),
    };
    // Report "not found" only when neither source knows the term.
    if fallback.error.is_none() || primary.error.as_deref() == Some("unavailable") {
        fallback
    } else {
        primary
    }
}

fn fetch_json(agent: &ureq::Agent, url: &str) -> Result<Value, &'static str> {
    let mut response = match agent.get(url).call() {
        Ok(response) => response,
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

/// Wiktionary REST definitions: `{"en": [{"partOfSpeech", "definitions": [{"definition": html}]}]}`.
fn parse_wiktionary(word: &str, value: &Value) -> DictionaryResponse {
    let senses = value
        .get("en")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|entry| {
            let part = entry
                .get("partOfSpeech")
                .and_then(Value::as_str)
                .map(|value| clean(&value.to_lowercase(), 32))
                .unwrap_or_default();
            entry
                .get("definitions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |definition| {
                    let text = clean(&strip_html(definition.get("definition")?.as_str()?), 240);
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
        word: Some(clean(word, MAX_WORD_CHARS)),
        senses,
        ..DictionaryResponse::default()
    }
}

/// Drop tags and decode the handful of entities Wiktionary definitions use.
pub(crate) fn strip_html(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for character in html.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => text.push(character),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().count() <= MAX_WORD_CHARS
        && word.split(' ').count() <= MAX_WORDS
        && !word.contains("  ")
        && word
            .chars()
            .all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | ' '))
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
    // Synonyms appear per meaning and per definition; keep the first few distinct ones.
    let mut synonyms = Vec::new();
    for meaning in entry
        .get("meanings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let per_definition = meaning
            .get("definitions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|definition| definition.get("synonyms"));
        for list in std::iter::once(meaning.get("synonyms"))
            .flatten()
            .chain(per_definition)
        {
            for synonym in list
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                let synonym = clean(synonym, 40);
                if !synonym.is_empty() && !synonyms.contains(&synonym) {
                    synonyms.push(synonym);
                }
            }
        }
    }
    synonyms.truncate(MAX_SYNONYMS);
    DictionaryResponse {
        word,
        phonetic,
        senses,
        error: None,
        synonyms,
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => (*byte as char).to_string(),
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
    fn collects_synonyms_and_parses_wiktionary_fallback() {
        let value: Value = serde_json::from_str(
            r#"[{"word":"happy","meanings":[{"partOfSpeech":"adjective","synonyms":["glad"],
                "definitions":[{"definition":"Content.","synonyms":["cheerful","glad"]}]}]}]"#,
        )
        .unwrap();
        assert_eq!(parse_response(&value).synonyms, ["glad", "cheerful"]);

        let wiktionary: Value = serde_json::from_str(
            r#"{"en":[{"partOfSpeech":"Noun","definitions":[{"definition":""},
                {"definition":"A <a href=\"/wiki/frozen\">frozen</a> dessert &amp; treat."}]}]}"#,
        )
        .unwrap();
        let response = parse_wiktionary("ice cream", &wiktionary);
        assert_eq!(response.word.as_deref(), Some("ice cream"));
        assert_eq!(response.senses[0].part_of_speech, "noun");
        assert_eq!(response.senses[0].definition, "A frozen dessert & treat.");
    }

    #[test]
    fn validates_and_encodes_words() {
        assert!(valid_word("mother-in-law"));
        assert!(valid_word("don't"));
        assert!(valid_word("ice cream"));
        assert!(valid_word("put up with"));
        assert!(!valid_word("far too many words"));
        assert!(!valid_word("double  space"));
        assert!(!valid_word("rust/../../secret"));
        assert_eq!(encode_path("don't"), "don%27t");
    }
}
