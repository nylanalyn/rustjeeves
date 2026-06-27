//! Tavily-backed web search commands. Network access and credentials stay in the host.

use extism_pdk::*;
use jeeves_abi::{
    Event, EventEnvelope, KvGet, KvSet, SearchQuery, SearchResponse, SendMessage, ThemeReq,
};

const COOLDOWN_SECS: i64 = 20;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn web_search(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| (*s).into()).collect(),
        vars: vars
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
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
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn cooldown_key(server: &str, user_id: &str, nick: &str) -> String {
    format!(
        "cooldown:{}:{}",
        encode(server),
        encode(if user_id.is_empty() { nick } else { user_id })
    )
}

fn encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    value
        .bytes()
        .flat_map(|byte| {
            [
                HEX[(byte >> 4) as usize] as char,
                HEX[(byte & 0x0f) as usize] as char,
            ]
        })
        .collect()
}

fn get_cooldown(key: &str) -> Result<i64, Error> {
    let value = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    Ok(value.parse().unwrap_or(0))
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
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let mut parts = text.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    if !matches!(command.as_str(), "!g" | "!google" | "!search") {
        return Ok(());
    }

    let destination = if msg.is_private { &msg.nick } else { &msg.target };
    let user = if msg.display.is_empty() { &msg.nick } else { &msg.display };
    let query = parts.next().unwrap_or("").trim();
    if query.is_empty() {
        return reply(
            &server,
            destination,
            &themed(
                "usage",
                &["What should I search for, {user}? Try !g <query>."],
                &[("user", user)],
            )?,
        );
    }
    if query.chars().count() > 400 {
        return reply(
            &server,
            destination,
            &themed(
                "query_too_long",
                &["That search is too long, {user}; keep it under 400 characters."],
                &[("user", user)],
            )?,
        );
    }

    let now = timestamp()?;
    let key = cooldown_key(&server, &msg.user_id, &msg.nick);
    let remaining = COOLDOWN_SECS - now.saturating_sub(get_cooldown(&key)?);
    if now > 0 && remaining > 0 && remaining <= COOLDOWN_SECS {
        let seconds = remaining.to_string();
        return reply(
            &server,
            destination,
            &themed(
                "cooldown",
                &["Please wait {seconds}s before searching again, {user}."],
                &[("seconds", &seconds), ("user", user)],
            )?,
        );
    }
    set_cooldown(&key, now)?;

    let raw = unsafe { web_search(serde_json::to_string(&SearchQuery { query: query.into() })?)? };
    let response: SearchResponse = serde_json::from_str(&raw)?;
    if let Some(result) = response.results.first() {
        let title = truncate(&result.title, 100);
        let url = truncate(&result.url, 280);
        let snippet = truncate(&result.snippet, 180);
        reply(
            &server,
            destination,
            &themed(
                "result",
                &["{title} — {url}"],
                &[("title", &title), ("url", &url), ("snippet", &snippet)],
            )?,
        )
    } else {
        let fallback = format!("https://search.brave.com/search?q={}", url_encode(query));
        let key = match response.error.as_deref() {
            Some("not_configured") => "not_configured",
            Some(_) => "unavailable",
            None => "no_results",
        };
        let default = match key {
            "not_configured" => "Search needs a Tavily API key. Meanwhile: {url}",
            "unavailable" => "Search isn't answering right now. Try: {url}",
            _ => "I found no results for '{query}'. Try: {url}",
        };
        let display_query = truncate(query, 200);
        reply(
            &server,
            destination,
            &themed(
                key,
                &[default],
                &[("query", &display_query), ("url", &fallback), ("user", user)],
            )?,
        )
    }
}

fn truncate(value: &str, max: usize) -> String {
    let mut out: String = value.chars().filter(|c| !c.is_control()).take(max).collect();
    if value.chars().filter(|c| !c.is_control()).count() > max {
        out.push('…');
    }
    out
}

fn url_encode(value: &str) -> String {
    value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![byte as char]
            }
            b' ' => vec!['+'],
            _ => format!("%{byte:02X}").chars().collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_fallback_query() {
        assert_eq!(url_encode("rust & wasm"), "rust+%26+wasm");
    }

    #[test]
    fn storage_keys_are_unambiguous() {
        assert_ne!(cooldown_key("a:b", "c", ""), cooldown_key("a", "b:c", ""));
    }
}
