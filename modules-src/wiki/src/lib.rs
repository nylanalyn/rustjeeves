//! Bounded Wikipedia article introductions (`!wiki`) and Wikiquote quotes (`!wq`) through
//! host-owned lookups.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, RandomBytesRequest, RandomBytesResponse, SendMessage,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    WikipediaQuery, WikipediaResponse, WikiquoteQuery, WikiquoteResponse,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};

const DEFAULT_COOLDOWN_SECONDS: i64 = 15;
const MAX_QUERY_CHARS: usize = 160;
const MAX_TITLE_CHARS: usize = 100;
const MAX_EXTRACT_CHARS: usize = 240;
const MAX_URL_CHARS: usize = 80;
const MAX_OPTIONS: usize = 6;
const MAX_QUOTE_CHARS: usize = 360;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn wikipedia_lookup(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn wikiquote(input: String) -> String;
    fn random_bytes(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 2,
        stats: vec![
            AchievementStat {
                id: "articles".into(),
                description: "Successful Wikipedia lookups".into(),
            },
            AchievementStat {
                id: "quotes".into(),
                description: "Wikiquote quotes fetched".into(),
            },
        ],
        achievements: [
            ("citation_found", "Citation Found", "articles", 1),
            ("rabbit_hole", "Down the Rabbit Hole", "articles", 25),
            ("encyclopedist", "Encyclopedist", "articles", 100),
            ("quotable", "Quotable", "quotes", 1),
            ("commonplace_book", "Commonplace Book", "quotes", 25),
        ]
        .into_iter()
        .map(|(id, name, stat, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: match (stat, threshold) {
                ("articles", 1) => "Look up a Wikipedia article.".into(),
                ("articles", _) => format!("Look up {threshold} Wikipedia articles."),
                (_, 1) => "Fetch a quote with !wq.".into(),
                _ => format!("Fetch {threshold} quotes with !wq."),
            },
            stat: stat.into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display_name: &str,
    target: &str,
    stat: &str,
) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "wiki".into(),
                aliases: vec!["wikipedia".into()],
                description: "Search Wikipedia and show a short article introduction.".into(),
                usage: "!wiki <topic>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "wq".into(),
                aliases: vec!["wikiquote".into()],
                description: "A random quote from a Wikiquote page, or today's quote of the day."
                    .into(),
                usage: "!wq [person, work, or topic]".into(),
                ..Default::default()
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "cooldown_seconds".into(),
            description: "Minimum delay between Wikipedia lookups by one user.".into(),
            default: DEFAULT_COOLDOWN_SECONDS.to_string(),
            kind: SettingKind::DurationSeconds { min: 0, max: 300 },
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }],
    })?)
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?
    };
    Ok(())
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse()?)
}

fn cooldown_seconds(server: &str, channel: Option<&str>) -> Result<i64, Error> {
    let value = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "cooldown_seconds".into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    };
    Ok(value.parse().unwrap_or(DEFAULT_COOLDOWN_SECONDS))
}

fn cooldown_key(server: &str, identity: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(identity))
}

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn lifecycle_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| cooldown_key(&request.subject.server, identity))
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let timestamps = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| entry.value.parse::<i64>())
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if timestamps.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "cooldown_timestamps": timestamps })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let mutations = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect();
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

/// A negative timestamp means this cooldown has already displayed its one warning.
fn get_cooldown(key: &str) -> Result<(i64, bool), Error> {
    let value = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    if value.is_empty() {
        Ok((0, false))
    } else {
        let timestamp = value.parse::<i64>()?;
        Ok((timestamp.saturating_abs(), timestamp < 0))
    }
}

fn set_cooldown(key: &str, value: i64) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.to_string(),
        })?)?
    };
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut parts = msg.text.trim().splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    if command != "!wiki" && command != "!wq" {
        return Ok(());
    }
    let quote = command == "!wq";
    let destination = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let user = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let rest = parts.next().unwrap_or("");
    let query = if quote && rest.trim().is_empty() {
        // Bare !wq is the quote of the day.
        Some(String::new())
    } else {
        normalize_query(rest)
    };
    let Some(query) = query else {
        reply(
            &env.server,
            destination,
            &themed(
                "wiki.usage",
                &["What should I look up, {user}? Try !wiki <topic>."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    };
    if msg.user_id.is_empty() {
        reply(
            &env.server,
            destination,
            &themed(
                "wiki.identity_unavailable",
                &["I can't verify your profile for a Wikipedia lookup right now, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    let now = timestamp()?;
    let key = cooldown_key(&env.server, &msg.user_id);
    let window = cooldown_seconds(
        &env.server,
        (!msg.is_private).then_some(msg.target.as_str()),
    )?;
    let (last_used, warned) = get_cooldown(&key)?;
    let remaining = window.saturating_sub(now.saturating_sub(last_used));
    if window > 0 && remaining > 0 && remaining <= window {
        if warned {
            return Ok(());
        }
        set_cooldown(&key, -last_used)?;
        reply(
            &env.server,
            destination,
            &themed(
                "wiki.cooldown",
                &["Please wait {seconds}s before another Wikipedia lookup, {user}."],
                &[("seconds", &remaining.to_string()), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    set_cooldown(&key, now)?;
    if quote {
        return Ok(reply_quote(
            &env.server,
            destination,
            user,
            &msg.user_id,
            &query,
        )?);
    }

    let raw = unsafe {
        wikipedia_lookup(serde_json::to_string(&WikipediaQuery {
            query: query.clone(),
        })?)?
    };
    let response: WikipediaResponse = serde_json::from_str(&raw)?;
    if let (Some(title), false) = (response.title.as_deref(), response.options.is_empty()) {
        // A disambiguation page: offer its main meanings instead of "X may refer to:".
        let options = response
            .options
            .iter()
            .take(MAX_OPTIONS)
            .map(|option| clean(option, MAX_TITLE_CHARS))
            .collect::<Vec<_>>()
            .join(" · ");
        let example = response
            .options
            .first()
            .map(String::as_str)
            .unwrap_or(title);
        reply(
            &env.server,
            destination,
            &themed(
                "wiki.ambiguous",
                &["{title} could mean several things: {options} — try !wiki {example}"],
                &[
                    ("title", &clean(title, MAX_TITLE_CHARS)),
                    ("options", &options),
                    ("example", &clean(example, MAX_TITLE_CHARS)),
                    ("user", user),
                ],
            )?,
        )?;
        award(&env.server, &msg.user_id, user, destination, "articles")?;
        return Ok(());
    }
    let (Some(title), Some(extract), Some(url)) = (
        response.title.as_deref(),
        response.extract.as_deref(),
        response.url.as_deref(),
    ) else {
        let (key, default) = match response.error.as_deref() {
            Some("not_found" | "invalid_query") | None => (
                "wiki.not_found",
                "I couldn't find a Wikipedia article for '{query}', {user}.",
            ),
            Some(_) => (
                "wiki.unavailable",
                "Wikipedia isn't answering right now, {user}.",
            ),
        };
        reply(
            &env.server,
            destination,
            &themed(key, &[default], &[("query", &query), ("user", user)])?,
        )?;
        return Ok(());
    };

    let title = clean(title, MAX_TITLE_CHARS);
    let extract = sentence_bounded(extract, MAX_EXTRACT_CHARS);
    let url = clean(url, MAX_URL_CHARS);
    reply(
        &env.server,
        destination,
        &themed(
            "wiki.result",
            &["Wikipedia: {title} — {extract} {url}"],
            &[
                ("title", &title),
                ("extract", &extract),
                ("url", &url),
                ("user", user),
            ],
        )?,
    )?;
    award(&env.server, &msg.user_id, user, destination, "articles")?;
    Ok(())
}

fn random_pick() -> Result<u64, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let bytes: [u8; 8] = serde_json::from_str::<RandomBytesResponse>(&raw)?
        .bytes
        .try_into()
        .map_err(|_| Error::msg("random_bytes returned the wrong byte count"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn reply_quote(
    server: &str,
    destination: &str,
    user: &str,
    user_id: &str,
    topic: &str,
) -> Result<(), Error> {
    let raw = unsafe {
        wikiquote(serde_json::to_string(&WikiquoteQuery {
            topic: topic.into(),
            pick: random_pick()?,
        })?)?
    };
    let response: WikiquoteResponse = serde_json::from_str(&raw)?;
    let title = clean(response.title.as_deref().unwrap_or(topic), MAX_TITLE_CHARS);
    let Some(quote) = response.quote.as_deref() else {
        let (key, default) = match response.error.as_deref() {
            Some("no_quotes") => (
                "wiki.quote_none",
                "I couldn't find a quote short enough for IRC on Wikiquote's {title} page, {user}.",
            ),
            Some("not_found" | "invalid_query") | None => (
                "wiki.quote_not_found",
                "Wikiquote has nothing for '{query}', {user}.",
            ),
            Some(_) => (
                "wiki.quote_unavailable",
                "Wikiquote isn't answering right now, {user}.",
            ),
        };
        return reply(
            server,
            destination,
            &themed(
                key,
                &[default],
                &[("title", &title), ("query", topic), ("user", user)],
            )?,
        );
    };
    let quote = clean(quote, MAX_QUOTE_CHARS);
    let source = clean(response.source.as_deref().unwrap_or(""), MAX_TITLE_CHARS);
    let url = clean(response.url.as_deref().unwrap_or(""), MAX_URL_CHARS);
    let (key, default, attribution) = if topic.is_empty() {
        (
            "wiki.quote_of_the_day",
            "Quote of the day: “{quote}” — {attribution}",
            source.clone(),
        )
    } else {
        (
            "wiki.quote",
            "“{quote}” — {attribution}",
            attribution(&title, &source),
        )
    };
    reply(
        server,
        destination,
        &themed(
            key,
            &[default],
            &[
                ("quote", &quote),
                ("attribution", &attribution),
                ("title", &title),
                ("source", &source),
                ("url", &url),
                ("user", user),
            ],
        )?,
    )?;
    award(server, user_id, user, destination, "quotes")
}

/// "Oscar Wilde", or "Discworld, Small Gods (1992)" when the quote belongs to a named work.
fn attribution(title: &str, source: &str) -> String {
    if source.is_empty() || source.eq_ignore_ascii_case(title) {
        title.into()
    } else {
        format!("{title}, {source}")
    }
}

fn normalize_query(value: &str) -> Option<String> {
    let query = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = query.chars().count();
    (count > 0 && count <= MAX_QUERY_CHARS && !query.chars().any(char::is_control)).then_some(query)
}

/// Fit `value` in `max_chars`, preferring to end on a sentence, then on a word, and marking any
/// cut with `…`.
fn sentence_bounded(value: &str, max_chars: usize) -> String {
    let clean = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if clean.chars().count() <= max_chars {
        return clean;
    }
    let window = clean.chars().take(max_chars).collect::<String>();
    // A sentence end in the back half of the window reads better than a mid-sentence cut.
    if let Some(end) = window
        .match_indices(". ")
        .map(|(index, _)| index + 1)
        .filter(|end| *end > window.len() / 2)
        .last()
    {
        return window[..end].to_string();
    }
    let cut = window
        .rfind(' ')
        .filter(|space| *space > window.len() / 2)
        .unwrap_or(window.len());
    let mut bounded = window[..cut].trim_end().to_string();
    // Leave room for the ellipsis within the budget.
    while bounded.chars().count() >= max_chars {
        bounded.pop();
    }
    bounded.push('…');
    bounded
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attributions_name_the_work_when_known() {
        assert_eq!(attribution("Oscar Wilde", ""), "Oscar Wilde");
        assert_eq!(
            attribution("Discworld", "Small Gods (1992)"),
            "Discworld, Small Gods (1992)"
        );
        assert_eq!(attribution("Mort", "mort"), "Mort");
    }

    #[test]
    fn validates_and_normalizes_queries() {
        assert_eq!(
            normalize_query("  Grace   Hopper "),
            Some("Grace Hopper".into())
        );
        assert_eq!(normalize_query(""), None);
        assert_eq!(normalize_query(&"x".repeat(MAX_QUERY_CHARS + 1)), None);
    }

    #[test]
    fn extracts_end_on_sentences_or_words() {
        let text =
            "Mercury is the first planet. It has no moons and a very thin atmosphere indeed.";
        assert_eq!(sentence_bounded(text, 50), "Mercury is the first planet.");
        assert!(sentence_bounded(text, 60).ends_with("very…"));
        assert_eq!(sentence_bounded("short", 60), "short");
        let words = "alpha beta gamma delta epsilon zeta eta theta";
        let cut = sentence_bounded(words, 20);
        assert!(cut.ends_with('…') && cut.chars().count() <= 20, "{cut}");
        assert!(!cut.contains("eps"), "{cut}");
    }

    #[test]
    fn bounds_article_fields() {
        assert_eq!(clean("  one   two ", 20), "one two");
        assert_eq!(clean("abcdefghij", 6), "abcde…");
    }

    #[test]
    fn cooldown_keys_are_unambiguous() {
        assert_ne!(cooldown_key("ab", "c"), cooldown_key("a", "bc"));
        assert_ne!(
            cooldown_key("network", "Alice"),
            cooldown_key("network", "alice")
        );
    }
}
