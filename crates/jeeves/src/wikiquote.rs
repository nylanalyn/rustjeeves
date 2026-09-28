//! Wikiquote lookups for the `wikiquote` host function: a random quote from a topic's page, or
//! today's quote of the day. Pages are parsed from wikitext once and cached, so repeated `!wq`
//! calls for the same topic cost no further requests.

use chrono::Utc;
use jeeves_abi::{WikiquoteQuery, WikiquoteResponse};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const ENDPOINT: &str = "https://en.wikiquote.org/w/api.php";
const MAX_TOPIC_CHARS: usize = 120;
/// Popular pages (Discworld) are several hundred kilobytes of wikitext.
const MAX_RESPONSE_BYTES: u64 = 4 * 1024 * 1024;
const MIN_QUOTE_CHARS: usize = 12;
const MAX_QUOTE_CHARS: usize = 320;
const MAX_SOURCE_CHARS: usize = 100;
const MAX_QUOTES_PER_PAGE: usize = 2_000;
const CACHE_CAP: usize = 48;
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

/// Level-2 sections that are about the subject, unverified, or navigation rather than quotes.
const SKIPPED_SECTIONS: &[&str] = &[
    "about",
    "quotes about",
    "see also",
    "external links",
    "misattributed",
    "disputed",
    "sources",
    "references",
    "cast",
    "notes",
    "bibliography",
    "further reading",
];

/// Headings that name no particular work, so they make poor attributions.
const GENERIC_HEADINGS: &[&str] = &["quotes", "sourced", "attributed", "general", "quotations"];

#[derive(Clone)]
struct Quote {
    text: String,
    source: Option<String>,
}

#[derive(Clone)]
struct Page {
    title: String,
    quotes: Vec<Quote>,
}

struct Cached {
    inserted: Instant,
    page: Option<Page>,
}

static CACHE: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();

pub fn lookup(req: &WikiquoteQuery) -> WikiquoteResponse {
    let topic = req.topic.split_whitespace().collect::<Vec<_>>().join(" ");
    if topic.chars().count() > MAX_TOPIC_CHARS || topic.chars().any(char::is_control) {
        return failure("invalid_query");
    }
    let key = if topic.is_empty() {
        format!("qotd:{}", Utc::now().format("%Y-%m-%d"))
    } else {
        topic.to_lowercase()
    };
    let page = match cached(&key) {
        Some(page) => page,
        None => {
            let fetched = if topic.is_empty() {
                fetch_quote_of_the_day()
            } else {
                fetch_topic(&topic)
            };
            match fetched {
                Ok(page) => {
                    store(key, page.clone());
                    page
                }
                Err(kind) => return failure(kind),
            }
        }
    };
    let Some(page) = page else {
        return failure("not_found");
    };
    if page.quotes.is_empty() {
        return WikiquoteResponse {
            url: Some(page_url(&page.title)),
            title: Some(page.title),
            error: Some("no_quotes".into()),
            ..WikiquoteResponse::default()
        };
    }
    let quote = &page.quotes[(req.pick % page.quotes.len() as u64) as usize];
    WikiquoteResponse {
        url: Some(page_url(&page.title)),
        title: Some(page.title.clone()),
        quote: Some(quote.text.clone()),
        source: quote.source.clone(),
        error: None,
    }
}

fn cached(key: &str) -> Option<Option<Page>> {
    let mut cache = CACHE.get_or_init(Default::default).lock().ok()?;
    cache.retain(|_, entry| entry.inserted.elapsed() < CACHE_TTL);
    cache.get(key).map(|entry| entry.page.clone())
}

fn store(key: String, page: Option<Page>) {
    let Ok(mut cache) = CACHE.get_or_init(Default::default).lock() else {
        return;
    };
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
            page,
        },
    );
}

fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .user_agent(concat!(
                "rustjeeves-bot/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/nylanalyn/rustjeeves)"
            ))
            .build(),
    )
}

fn get(agent: &ureq::Agent, params: &[(&str, &str)]) -> Result<Value, &'static str> {
    let mut request = agent.get(ENDPOINT);
    for (key, value) in params {
        request = request.query(*key, *value);
    }
    let mut response = request
        .query("format", "json")
        .query("formatversion", "2")
        .call()
        .map_err(|_| "unavailable")?;
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|_| "unavailable")?;
    serde_json::from_str(&body).map_err(|_| "unavailable")
}

/// Ok(None) means the topic has no page; errors are provider failures (not cached).
fn fetch_topic(topic: &str) -> Result<Option<Page>, &'static str> {
    let agent = agent();
    let search = get(
        &agent,
        &[
            ("action", "query"),
            ("list", "search"),
            ("srsearch", topic),
            ("srnamespace", "0"),
            ("srlimit", "1"),
        ],
    )?;
    let Some(title) = search["query"]["search"][0]["title"].as_str() else {
        return Ok(None);
    };
    let parsed = get(
        &agent,
        &[
            ("action", "parse"),
            ("page", title),
            ("prop", "wikitext"),
            ("redirects", "1"),
        ],
    )?;
    let Some(wikitext) = parsed["parse"]["wikitext"].as_str() else {
        return Ok(None);
    };
    let title = parsed["parse"]["title"]
        .as_str()
        .unwrap_or(title)
        .to_string();
    Ok(Some(Page {
        title,
        quotes: if is_disambiguation(wikitext) {
            Vec::new()
        } else {
            page_quotes(wikitext)
        },
    }))
}

fn fetch_quote_of_the_day() -> Result<Option<Page>, &'static str> {
    let title = format!(
        "Wikiquote:Quote of the day/{}",
        Utc::now().format("%B %-d, %Y")
    );
    let parsed = get(
        &agent(),
        &[("action", "parse"), ("page", &title), ("prop", "wikitext")],
    )?;
    let Some(wikitext) = parsed["parse"]["wikitext"].as_str() else {
        return Ok(None);
    };
    Ok(quote_of_the_day(wikitext).map(|quote| Page {
        title: "Wikiquote:Quote of the day".into(),
        quotes: vec![quote],
    }))
}

fn is_disambiguation(wikitext: &str) -> bool {
    let lower = wikitext.to_ascii_lowercase();
    lower.contains("{{disambig") || lower.contains("{{dab")
}

/// `| quote = …` and `| author = …` from the quote-of-the-day template.
fn quote_of_the_day(wikitext: &str) -> Option<Quote> {
    let field = |name: &str| {
        wikitext.lines().find_map(|line| {
            let (key, value) = line.trim().trim_start_matches('|').split_once('=')?;
            (key.trim() == name).then(|| clean_markup(value))
        })
    };
    let text = field("quote").filter(|text| !text.is_empty())?;
    Some(Quote {
        text: bound(&text, MAX_QUOTE_CHARS * 2),
        source: field("author").filter(|author| !author.is_empty()),
    })
}

/// Top-level `*` bullets, attributed to the nearest work heading, skipping sections about the
/// subject and anything too short or too long for one IRC line.
fn page_quotes(wikitext: &str) -> Vec<Quote> {
    let mut quotes = Vec::new();
    let mut skipping = false;
    let mut skipping_subsection = false;
    let mut section: Option<String> = None;
    let mut subsection: Option<String> = None;
    for line in wikitext.lines() {
        let trimmed = line.trim();
        let level = trimmed.chars().take_while(|ch| *ch == '=').count();
        if level >= 2 && trimmed.ends_with('=') {
            let title = clean_markup(trimmed.trim_matches('='));
            let lower = title.to_lowercase();
            let named = (!GENERIC_HEADINGS.contains(&lower.as_str()) && !title.is_empty())
                .then(|| bound(&title, MAX_SOURCE_CHARS));
            let skipped = SKIPPED_SECTIONS
                .iter()
                .any(|skip| lower == *skip || lower.starts_with(&format!("{skip} ")))
                || lower.ends_with(" cast");
            match level {
                2 => {
                    skipping = skipped;
                    skipping_subsection = false;
                    section = named;
                    subsection = None;
                }
                3 => {
                    skipping_subsection = skipped;
                    subsection = named;
                }
                _ => {}
            }
            continue;
        }
        if skipping || skipping_subsection || !trimmed.starts_with('*') || trimmed.starts_with("**")
        {
            continue;
        }
        let text = strip_page_reference(&clean_markup(&trimmed[1..]));
        let length = text.chars().count();
        if (MIN_QUOTE_CHARS..=MAX_QUOTE_CHARS).contains(&length) {
            quotes.push(Quote {
                text,
                source: subsection.clone().or_else(|| section.clone()),
            });
            if quotes.len() >= MAX_QUOTES_PER_PAGE {
                break;
            }
        }
    }
    quotes
}

/// Drop a trailing page citation: "… (p. 2)", "(pp. 10–11)", "(Page 4)".
fn strip_page_reference(text: &str) -> String {
    if let Some(open) = text.rfind(" (") {
        let tail = text[open + 2..].to_ascii_lowercase();
        if tail.ends_with(')')
            && (tail.starts_with("p.") || tail.starts_with("pp.") || tail.starts_with("page"))
        {
            return text[..open].trim_end().into();
        }
    }
    text.into()
}

/// Wikitext → plain text: links keep their label, `{{w|…}}` keeps its text, other templates,
/// references, comments, files, and HTML tags are dropped.
fn clean_markup(input: &str) -> String {
    let mut text = remove_between(input, "<!--", "-->");
    text = remove_references(&text);
    text = replace_templates(&text);
    text = replace_links(&text);
    for br in ["<br />", "<br/>", "<br>", "<BR>"] {
        text = text.replace(br, " / ");
    }
    text = strip_tags(&text);
    let text = text
        .replace("'''", "")
        .replace("''", "")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    let text = text
        .chars()
        .filter(|ch| !matches!(ch, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'))
        .collect::<String>();
    let mut out = text.split_whitespace().collect::<Vec<_>>().join(" ");
    // Link removal leaves gaps like "harmony ."; close them.
    for (gap, closed) in [
        (" .", "."),
        (" ,", ","),
        (" ;", ";"),
        (" :", ":"),
        (" !", "!"),
        (" ?", "?"),
    ] {
        out = out.replace(gap, closed);
    }
    out.trim_matches(|ch: char| ch == '/' || ch.is_whitespace())
        .into()
}

fn remove_between(input: &str, open: &str, close: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find(open) {
        out.push_str(&rest[..start]);
        match rest[start..].find(close) {
            Some(end) => rest = &rest[start + end + close.len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

fn remove_references(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    while let Some(start) = rest.find("<ref") {
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let Some(tag_end) = tail.find('>') else {
            return out;
        };
        if tail[..tag_end].ends_with('/') {
            rest = &tail[tag_end + 1..];
        } else if let Some(close) = tail.find("</ref>") {
            rest = &tail[close + "</ref>".len()..];
        } else {
            return out;
        }
    }
    out.push_str(rest);
    out
}

/// Innermost-first template expansion so nested templates resolve.
fn replace_templates(input: &str) -> String {
    let mut text = input.to_string();
    for _ in 0..16 {
        let Some(close) = text.find("}}") else {
            break;
        };
        let Some(open) = text[..close].rfind("{{") else {
            text.replace_range(close..close + 2, "");
            continue;
        };
        let body = &text[open + 2..close];
        let parts = body.split('|').map(str::trim).collect::<Vec<_>>();
        let name = parts[0].to_ascii_lowercase();
        let keep = match name.as_str() {
            "w" | "wikipedia" | "nowrap" | "small" | "smaller" | "lang" | "abbr" | "sic"
            | "center" | "em" | "i" | "nobr" => parts
                .iter()
                .skip(1)
                .rfind(|part| !part.contains('='))
                .copied()
                .unwrap_or("")
                .to_string(),
            "'" => "'".into(),
            "--" | "mdash" | "—" => "—".into(),
            "ndash" | "–" => "–".into(),
            _ => String::new(),
        };
        // `lang` is `{{lang|fr|texte}}`; the last unnamed part is the text for every kept name.
        text.replace_range(open..close + 2, &keep);
    }
    text
}

fn replace_links(input: &str) -> String {
    let mut out = String::new();
    let mut rest = input;
    loop {
        let internal = rest.find("[[");
        let external = rest.find("[http");
        match (internal, external) {
            (Some(start), _) if external.is_none_or(|ext| start < ext) => {
                out.push_str(&rest[..start]);
                let tail = &rest[start + 2..];
                let Some(end) = tail.find("]]") else {
                    return out;
                };
                let inner = &tail[..end];
                let lower = inner.to_ascii_lowercase();
                if !(lower.starts_with("file:")
                    || lower.starts_with("image:")
                    || lower.starts_with("category:"))
                {
                    out.push_str(inner.rsplit('|').next().unwrap_or(inner));
                }
                rest = &tail[end + 2..];
            }
            (_, Some(start)) => {
                out.push_str(&rest[..start]);
                let tail = &rest[start + 1..];
                let Some(end) = tail.find(']') else {
                    return out;
                };
                if let Some((_, label)) = tail[..end].split_once(' ') {
                    out.push_str(label);
                }
                rest = &tail[end + 1..];
            }
            _ => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

fn strip_tags(input: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

fn bound(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.into();
    }
    let head = text.chars().take(max_chars).collect::<String>();
    let cut = head.rfind(' ').unwrap_or(head.len());
    format!("{}…", &head[..cut])
}

fn page_url(title: &str) -> String {
    let path = title
        .replace(' ', "_")
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'(' | b')' | b':' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect::<String>();
    format!("https://en.wikiquote.org/wiki/{path}")
}

fn failure(kind: &str) -> WikiquoteResponse {
    WikiquoteResponse {
        error: Some(kind.into()),
        ..WikiquoteResponse::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleans_wikitext_markup() {
        assert_eq!(
            clean_markup("'''If [[Nature]] had been''' {{w|The Decay of Lying|decaying}}<ref name=\"a\">cite</ref> [[w:Oscar Wilde|Wilde]]<ref name=\"b\"/> [http://x.org site] <br/>end{{citation needed}}"),
            "If Nature had been decaying Wilde site / end"
        );
        assert_eq!(
            clean_markup("[[File:X.jpg|thumb|caption]]Text <!-- note -->here"),
            "Text here"
        );
        assert_eq!(
            clean_markup("wide canvas<br /> So [[harmony]] ."),
            "wide canvas / So harmony."
        );
    }

    #[test]
    fn collects_quotes_with_their_works() {
        let wikitext = "\
Intro text.
== Books ==
::[[Mort]] (1987)
=== ''[[w:Small Gods|Small Gods]]'' (1992) ===
* Gravity is a habit that is hard to shake off. (p. 2)
** A note, not a quote.
* Short.
== Quotes ==
* Anybody can make history; only a great man can write it.
== Quotes about Pratchett ==
* He was a wonderful writer and a kind man.
== See also ==
* [[Discworld]] and other things
";
        let quotes = page_quotes(wikitext);
        assert_eq!(quotes.len(), 2);
        assert_eq!(
            quotes[0].text,
            "Gravity is a habit that is hard to shake off."
        );
        assert_eq!(quotes[0].source.as_deref(), Some("Small Gods (1992)"));
        assert_eq!(
            quotes[1].source, None,
            "generic headings aren't attributions"
        );
    }

    #[test]
    fn reads_the_quote_of_the_day_template() {
        let wikitext = "{{Wikiquote:Quote of the day/Template\n| image1 = X.jpg\n| quote = <!-- ⨀ -->''[[Shakespeare]]'s stage <br /> Must hold [[Mirror|the glass]] to every age''\n| author = Francis Turner Palgrave\n}}";
        let quote = quote_of_the_day(wikitext).unwrap();
        assert_eq!(
            quote.text,
            "Shakespeare's stage / Must hold the glass to every age"
        );
        assert_eq!(quote.source.as_deref(), Some("Francis Turner Palgrave"));
    }
}
