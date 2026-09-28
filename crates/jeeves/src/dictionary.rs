//! Keyless dictionary lookups. dictionaryapi.dev is asked first (it has phonetics and synonyms);
//! when it is down or doesn't know the term, English Wiktionary's REST definitions are used
//! instead (they cover phrases such as "ice cream"). The host owns both fixed endpoints and exposes
//! only bounded, sanitized definitions to WASM modules.

use jeeves_abi::{DictionaryResponse, DictionarySense, EtymologyResponse};
use serde_json::Value;
use std::time::Duration;

const ENDPOINT: &str = "https://api.dictionaryapi.dev/api/v2/entries/en/";
const WIKTIONARY_ENDPOINT: &str = "https://en.wiktionary.org/api/rest_v1/page/definition/";
const MAX_WORD_CHARS: usize = 64;
const MAX_WORDS: usize = 3;
const MAX_SYNONYMS: usize = 6;
const MAX_RESPONSE_BYTES: u64 = 256 * 1024;
const MAX_SENSES: usize = 3;
const WIKTIONARY_API: &str = "https://en.wiktionary.org/w/api.php";
const MAX_ETYMOLOGIES: usize = 2;
const MAX_ETYMOLOGY_CHARS: usize = 420;
const MAX_EXTRACT_BYTES: u64 = 1024 * 1024;

pub fn lookup(word: &str) -> DictionaryResponse {
    let word = word.trim();
    if !valid_word(word) {
        return failure("invalid_word");
    }
    let agent = agent();
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

/// The English etymology sections of a Wiktionary entry, trying the word as typed, then lower
/// case, then capitalised ("christmas" → "Christmas").
pub fn etymology(word: &str) -> EtymologyResponse {
    let word = word.trim();
    if !valid_word(word) {
        return EtymologyResponse {
            error: Some("invalid_word".into()),
            ..EtymologyResponse::default()
        };
    }
    let agent = agent();
    let lower = word.to_lowercase();
    let capitalised = {
        let mut chars = lower.chars();
        chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect::<String>())
            .unwrap_or_default()
    };
    let mut candidates = vec![word.to_string()];
    for candidate in [lower, capitalised] {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    let mut error = "not_found";
    for title in candidates {
        let response = agent
            .get(WIKTIONARY_API)
            .query("action", "query")
            .query("prop", "extracts")
            .query("explaintext", "1")
            .query("redirects", "1")
            .query("titles", &title)
            .query("format", "json")
            .query("formatversion", "2")
            .call();
        let Ok(mut response) = response else {
            error = "unavailable";
            continue;
        };
        let Some(value) = response
            .body_mut()
            .with_config()
            .limit(MAX_EXTRACT_BYTES)
            .read_to_string()
            .ok()
            .and_then(|body| serde_json::from_str::<Value>(&body).ok())
        else {
            error = "unavailable";
            continue;
        };
        let page = &value["query"]["pages"][0];
        let Some(extract) = page["extract"].as_str() else {
            continue;
        };
        let etymologies = english_etymologies(extract);
        if !etymologies.is_empty() {
            let found = page["title"].as_str().unwrap_or(&title).to_string();
            return EtymologyResponse {
                url: Some(format!(
                    "https://en.wiktionary.org/wiki/{}#English",
                    encode_path(&found.replace(' ', "_"))
                )),
                word: Some(found),
                etymologies,
                error: None,
            };
        }
    }
    EtymologyResponse {
        error: Some(error.into()),
        ..EtymologyResponse::default()
    }
}

/// Plain-text extract → the paragraphs under "Etymology" headings in the English section.
fn english_etymologies(extract: &str) -> Vec<String> {
    let heading = |line: &str| -> Option<(usize, String)> {
        let line = line.trim();
        let level = line.chars().take_while(|ch| *ch == '=').count();
        (level >= 2 && line.ends_with('=')).then(|| (level, line.trim_matches('=').trim().into()))
    };
    let mut in_english = false;
    let mut current: Option<Vec<&str>> = None;
    let mut found = Vec::new();
    let flush = |current: &mut Option<Vec<&str>>, found: &mut Vec<String>| {
        if let Some(lines) = current.take() {
            let joined = lines
                .join(" ")
                .replace(['\u{200E}', '\u{200F}', '\u{200B}'], "");
            let text = clean(&joined, MAX_ETYMOLOGY_CHARS * 2);
            if !text.is_empty() && found.len() < MAX_ETYMOLOGIES {
                found.push(bound(&text, MAX_ETYMOLOGY_CHARS));
            }
        }
    };
    for line in extract.lines() {
        if let Some((level, title)) = heading(line) {
            flush(&mut current, &mut found);
            if level == 2 {
                if in_english {
                    break;
                }
                in_english = title == "English";
            } else if in_english && title.starts_with("Etymology") {
                current = Some(Vec::new());
            }
        } else if let Some(lines) = current.as_mut() {
            if !line.trim().is_empty() {
                lines.push(line.trim());
            }
        }
    }
    flush(&mut current, &mut found);
    found
}

/// Cut at a sentence end where possible, otherwise at a word, marking the cut with "…".
fn bound(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.into();
    }
    let head = text.chars().take(max_chars).collect::<String>();
    if let Some(end) = head.rfind(". ").filter(|end| *end > max_chars / 2) {
        return head[..=end].into();
    }
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", head[..cut].trim_end_matches([',', ';', ':']))
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(8)))
            .user_agent(concat!(
                "rustjeeves-bot/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    )
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
    fn extracts_english_etymologies_only() {
        let extract = "== English ==\n\n\n=== Etymology 1 ===\nFrom Middle English boteler,\nfrom Old French.\n\n\n=== Noun ===\nA servant.\n\n=== Etymology 2 ===\nBorrowed from Dutch.\n\n== Dutch ==\n\n=== Etymology ===\nBorrowed from English.\n";
        assert_eq!(
            english_etymologies(extract),
            [
                "From Middle English boteler, from Old French.",
                "Borrowed from Dutch."
            ]
        );
        assert!(english_etymologies("== French ==\n=== Etymology ===\nLatin.").is_empty());
        let long = "word ".repeat(200);
        assert!(bound(&long, 50).ends_with('…'));
        assert_eq!(
            bound("One. Two three four five six seven.", 20),
            "One. Two three four…"
        );
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
