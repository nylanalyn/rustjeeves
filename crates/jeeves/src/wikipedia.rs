//! Bounded English Wikipedia article introductions through the public MediaWiki Action API.

use jeeves_abi::WikipediaResponse;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENDPOINT: &str = "https://en.wikipedia.org/w/api.php";
const MAX_QUERY_CHARS: usize = 160;
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_TITLE_CHARS: usize = 120;
const MAX_EXTRACT_CHARS: usize = 500;
const CACHE_CAP: usize = 128;
const CACHE_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Clone)]
struct Cached {
    inserted: Instant,
    response: WikipediaResponse,
}

static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();

/// Search English Wikipedia and return the first article's introductory extract.
pub fn lookup(query: &str) -> WikipediaResponse {
    let Some(query) = normalize_query(query) else {
        return failure("invalid_query");
    };
    let cache_key = query.to_lowercase();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut cache) = cache.lock() {
        cache.retain(|_, entry| entry.inserted.elapsed() < CACHE_TTL);
        if let Some(entry) = cache.get(&cache_key) {
            return entry.response.clone();
        }
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
    let response = agent
        .get(ENDPOINT)
        .query("action", "query")
        .query("generator", "search")
        .query("gsrsearch", &query)
        .query("gsrnamespace", "0")
        .query("gsrlimit", "1")
        .query("prop", "extracts|info|pageprops")
        .query("ppprop", "disambiguation")
        .query("exintro", "1")
        .query("explaintext", "1")
        .query("exsentences", "2")
        .query("inprop", "url")
        .query("redirects", "1")
        .query("format", "json")
        .query("formatversion", "2")
        .call();
    let mut response = match response {
        Ok(response) => response,
        Err(ureq::Error::StatusCode(404)) => {
            return cache_response(cache, cache_key, failure("not_found"))
        }
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
    let mut result = parse_response(&value);
    if result.error.is_none() && is_disambiguation(&value) {
        if let Some(title) = result.title.clone() {
            result.options = disambiguation_options(&agent, &title);
        }
    }
    if result.error.as_deref() == Some("unavailable") {
        result
    } else {
        cache_response(cache, cache_key, result)
    }
}

fn parse_response(value: &Value) -> WikipediaResponse {
    if value.get("error").is_some() {
        return failure("unavailable");
    }
    let Some(page) = value
        .get("query")
        .and_then(|query| query.get("pages"))
        .and_then(Value::as_array)
        .and_then(|pages| pages.first())
    else {
        return failure("not_found");
    };
    let Some(page_id) = page.get("pageid").and_then(Value::as_u64) else {
        return failure("not_found");
    };
    let title = clean(
        page.get("title").and_then(Value::as_str).unwrap_or(""),
        MAX_TITLE_CHARS,
    );
    let extract = clean(
        page.get("extract").and_then(Value::as_str).unwrap_or(""),
        MAX_EXTRACT_CHARS,
    );
    if title.is_empty() || extract.is_empty() {
        return failure("not_found");
    }
    WikipediaResponse {
        title: Some(title),
        extract: Some(extract),
        url: Some(format!("https://en.wikipedia.org/?curid={page_id}")),
        error: None,
        options: Vec::new(),
    }
}

const MAX_OPTIONS: usize = 8;

fn is_disambiguation(value: &Value) -> bool {
    value
        .get("query")
        .and_then(|query| query.get("pages"))
        .and_then(Value::as_array)
        .and_then(|pages| pages.first())
        .and_then(|page| page.get("pageprops"))
        .is_some_and(|props| props.get("disambiguation").is_some())
}

/// The articles a disambiguation page lists, in page order. Best effort: an empty list simply
/// falls back to the page's own "may refer to" extract.
fn disambiguation_options(agent: &ureq::Agent, title: &str) -> Vec<String> {
    let Ok(mut response) = agent
        .get(ENDPOINT)
        .query("action", "parse")
        .query("page", title)
        .query("prop", "wikitext")
        .query("redirects", "1")
        .query("format", "json")
        .query("formatversion", "2")
        .call()
    else {
        return Vec::new();
    };
    let Ok(body) = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
    else {
        return Vec::new();
    };
    serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("parse")?
                .get("wikitext")?
                .as_str()
                .map(bullet_links)
        })
        .unwrap_or_default()
}

/// The first wiki link of each bulleted line (`* [[Mercury (planet)]], …`), skipping links into
/// other namespaces, de-duplicated and bounded.
fn bullet_links(wikitext: &str) -> Vec<String> {
    let mut links = Vec::new();
    for line in wikitext.lines().filter(|line| line.starts_with('*')) {
        let Some(start) = line.find("[[") else {
            continue;
        };
        let rest = &line[start + 2..];
        let Some(end) = rest.find("]]") else {
            continue;
        };
        let target = rest[..end]
            .split(['|', '#'])
            .next()
            .unwrap_or("")
            .trim()
            .replace('_', " ");
        let namespaced = target.split_once(':').is_some_and(|(prefix, _)| {
            matches!(
                prefix.trim().to_ascii_lowercase().as_str(),
                "file"
                    | "image"
                    | "category"
                    | "wiktionary"
                    | "wikt"
                    | "special"
                    | "help"
                    | "template"
                    | "portal"
                    | "wikipedia"
                    | "wp"
                    | "commons"
                    | "q"
                    | "s"
                    | "voy"
            )
        });
        if target.is_empty() || namespaced || links.contains(&target) {
            continue;
        }
        links.push(clean(&target, MAX_TITLE_CHARS));
        if links.len() >= MAX_OPTIONS {
            break;
        }
    }
    links
}

fn normalize_query(query: &str) -> Option<String> {
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = query.chars().count();
    (count > 0 && count <= MAX_QUERY_CHARS && !query.chars().any(char::is_control)).then_some(query)
}

fn clean(value: &str, max_chars: usize) -> String {
    let clean = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= max_chars {
        clean
    } else {
        let mut bounded = clean
            .chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>();
        bounded.push('…');
        bounded
    }
}

fn cache_response(
    cache: &Mutex<HashMap<String, Cached>>,
    key: String,
    response: WikipediaResponse,
) -> WikipediaResponse {
    if let Ok(mut cache) = cache.lock() {
        if cache.len() >= CACHE_CAP {
            if let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.inserted)
                .map(|(key, _)| key.clone())
            {
                cache.remove(&oldest);
            }
        }
        cache.insert(
            key,
            Cached {
                inserted: Instant::now(),
                response: response.clone(),
            },
        );
    }
    response
}

fn failure(kind: &str) -> WikipediaResponse {
    WikipediaResponse {
        error: Some(kind.into()),
        ..WikipediaResponse::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_bounds_article_intro() {
        let value = serde_json::json!({
            "query": {
                "pages": [{
                    "pageid": 12590,
                    "title": "Grace Hopper",
                    "extract": "Grace Brewster Hopper was an American computer scientist.  She was a pioneer of computer programming.",
                    "fullurl": "https://en.wikipedia.org/wiki/Grace_Hopper"
                }]
            }
        });
        assert_eq!(
            parse_response(&value),
            WikipediaResponse {
                title: Some("Grace Hopper".into()),
                extract: Some(
                    "Grace Brewster Hopper was an American computer scientist. She was a pioneer of computer programming."
                        .into()
                ),
                url: Some("https://en.wikipedia.org/?curid=12590".into()),
                error: None,
                options: Vec::new(),
            }
        );
    }

    #[test]
    fn missing_and_malformed_pages_are_safe_errors() {
        assert_eq!(
            parse_response(&serde_json::json!({"query": {"pages": []}})).error,
            Some("not_found".into())
        );
        assert_eq!(
            parse_response(&serde_json::json!({"error": {"code": "ratelimited"}})).error,
            Some("unavailable".into())
        );
    }

    #[test]
    fn disambiguation_bullets_become_options_in_page_order() {
        let wikitext = "'''Mercury''' most commonly refers to:\n\
            * [[Mercury (planet)]], the nearest planet\n\
            * [[Mercury (element)|Mercury]], a metal\n\
            ** [[Mercury (mythology)]]\n\
            * [[File:Hg.png|thumb]]\n\
            * [[Mercury: The Musical]]\n\
            * [[Mercury (planet)#Orbit]] again\n\
            * plain text without a link\n";
        assert_eq!(
            bullet_links(wikitext),
            [
                "Mercury (planet)",
                "Mercury (element)",
                "Mercury (mythology)",
                "Mercury: The Musical"
            ]
        );
    }

    #[test]
    fn validates_and_normalizes_queries() {
        assert_eq!(
            normalize_query("  Grace   Hopper  "),
            Some("Grace Hopper".into())
        );
        assert_eq!(normalize_query(""), None);
        assert_eq!(normalize_query(&"x".repeat(MAX_QUERY_CHARS + 1)), None);
    }
}
