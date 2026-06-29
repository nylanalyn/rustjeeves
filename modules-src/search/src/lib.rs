//! Tavily-backed web search commands. Network access and credentials stay in the host.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, SearchQuery, SearchResponse,
    SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
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

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "search".into(),
            aliases: vec!["g".into(), "google".into()],
            description: "Search the web with Tavily.".into(),
            usage: "!search <query>".into(),
        }],
    })?)
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

fn lifecycle_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| cooldown_key(&request.subject.server, identity, identity))
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let values = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if values.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "cooldown_timestamps": values })
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

    let destination = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let user = if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    };
    let query = parts.next().unwrap_or("").trim();
    if query.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "usage",
                &["What should I search for, {user}? Try !g <query>."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if query.chars().count() > 400 {
        reply(
            &server,
            destination,
            &themed(
                "query_too_long",
                &["That search is too long, {user}; keep it under 400 characters."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    let now = timestamp()?;
    let key = cooldown_key(&server, &msg.user_id, &msg.nick);
    let remaining = COOLDOWN_SECS - now.saturating_sub(get_cooldown(&key)?);
    if now > 0 && remaining > 0 && remaining <= COOLDOWN_SECS {
        let seconds = remaining.to_string();
        reply(
            &server,
            destination,
            &themed(
                "cooldown",
                &["Please wait {seconds}s before searching again, {user}."],
                &[("seconds", &seconds), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    set_cooldown(&key, now)?;

    let raw = unsafe {
        web_search(serde_json::to_string(&SearchQuery {
            query: query.into(),
        })?)?
    };
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
        )?;
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
                &[
                    ("query", &display_query),
                    ("url", &fallback),
                    ("user", user),
                ],
            )?,
        )?;
    }
    Ok(())
}

fn truncate(value: &str, max: usize) -> String {
    let mut out: String = value
        .chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect();
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
