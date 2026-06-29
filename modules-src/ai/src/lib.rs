//! Addressed AI chat responder. HTTP, endpoint selection, SOUL.md, and credentials remain in the
//! host; this module handles IRC addressing, policy settings, cooldowns, and themed replies.

use extism_pdk::*;
use jeeves_abi::{
    AiChatRequest, AiChatResponse, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, SendMessage, ServerQuery, SettingGet,
    SettingKind, SettingScope, SettingSpec, SettingsManifest, ThemeReq, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};

const MAX_PROMPT_CHARS: usize = 1_000;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn ai_chat(input: String) -> String;
    fn bot_nick(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all_scopes = || {
        vec![
            SettingScope::Global,
            SettingScope::Network,
            SettingScope::Channel,
        ]
    };
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Master switch for AI responses.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "channel_enabled".into(),
                description: "Respond when addressed by name in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "pm_enabled".into(),
                description: "Respond to unprefixed private messages.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
            SettingSpec {
                key: "aliases".into(),
                description: "Comma-separated additional names, such as jeeves.".into(),
                default: "jeeves".into(),
                kind: SettingKind::String { max_len: 200 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
            SettingSpec {
                key: "cooldown_seconds".into(),
                description: "Per-user delay between AI requests.".into(),
                default: "30".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "temperature_percent".into(),
                description: "Sampling temperature from 0 to 200 (0.0 to 2.0).".into(),
                default: "70".into(),
                kind: SettingKind::Integer { min: 0, max: 200 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_tokens".into(),
                description: "Maximum generated tokens per response.".into(),
                default: "256".into(),
                kind: SettingKind::Integer { min: 16, max: 1_024 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
        ],
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

fn setting(key: &str, server: &str, channel: Option<&str>) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    })
}

fn setting_bool(key: &str, server: &str, channel: Option<&str>) -> Result<bool, Error> {
    Ok(setting(key, server, channel)? == "true")
}

fn setting_i64(key: &str, server: &str, channel: Option<&str>, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
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

fn cooldown_key(server: &str, profile_id: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(profile_id))
}

fn cooldown_get(key: &str) -> Result<i64, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? }
        .parse()
        .unwrap_or(0))
}

fn cooldown_set(key: &str, timestamp: i64) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: timestamp.to_string(),
        })?)?
    };
    Ok(())
}

fn valid_alias(alias: &str) -> bool {
    !alias.is_empty()
        && alias.len() <= 32
        && alias
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_[]\\`^{}|".contains(&byte))
}

fn names(bot_nick: &str, aliases: &str) -> Vec<String> {
    std::iter::once(bot_nick)
        .chain(aliases.split(','))
        .map(str::trim)
        .filter(|name| valid_alias(name))
        .map(str::to_ascii_lowercase)
        .fold(Vec::new(), |mut names, name| {
            if names.len() < 10 && !names.contains(&name) {
                names.push(name);
            }
            names
        })
}

fn addressed_prompt<'a>(text: &'a str, names: &[String]) -> Option<&'a str> {
    let text = text.trim_start();
    for name in names {
        let Some(prefix) = text.get(..name.len()) else {
            continue;
        };
        if !prefix.eq_ignore_ascii_case(name) {
            continue;
        }
        let rest = text.get(name.len()..)?;
        let rest = if let Some(rest) = rest.strip_prefix(',') {
            rest
        } else if let Some(rest) = rest.strip_prefix(':') {
            rest
        } else {
            continue;
        };
        return Some(rest.trim());
    }
    None
}

fn select_prompt<'a>(
    is_private: bool,
    pm_enabled: bool,
    channel_enabled: bool,
    text: &'a str,
    names: &[String],
) -> Option<&'a str> {
    if is_private {
        pm_enabled.then(|| text.trim())
    } else if channel_enabled {
        addressed_prompt(text, names)
    } else {
        None
    }
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let channel = (!msg.is_private).then_some(msg.target.as_str());
    if !setting_bool("enabled", &server, channel)? {
        return Ok(());
    }
    let pm_enabled = msg.is_private && setting_bool("pm_enabled", &server, None)?;
    let channel_enabled = !msg.is_private && setting_bool("channel_enabled", &server, channel)?;
    if !pm_enabled && !channel_enabled {
        return Ok(());
    }

    let configured_bot_nick = unsafe {
        bot_nick(serde_json::to_string(&ServerQuery {
            server: server.clone(),
        })?)?
    };
    if !configured_bot_nick.is_empty() && msg.nick.eq_ignore_ascii_case(&configured_bot_nick) {
        return Ok(());
    }

    let aliases = if msg.is_private {
        String::new()
    } else {
        setting("aliases", &server, None)?
    };
    let names = names(&configured_bot_nick, &aliases);
    let Some(prompt) = select_prompt(
        msg.is_private,
        pm_enabled,
        channel_enabled,
        &msg.text,
        &names,
    ) else {
        return Ok(());
    };
    if msg.is_private && prompt.starts_with('!') {
        return Ok(());
    }
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
    if prompt.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "empty_prompt",
                &["What would you like to know, {user}?"],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        reply(
            &server,
            destination,
            &themed(
                "prompt_too_long",
                &["That question is too long, {user}; keep it under 1,000 characters."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if msg.user_id.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "identity_unavailable",
                &["I could not verify your stable profile, {user}; please try again shortly."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    let current = timestamp()?;
    let cooldown = setting_i64("cooldown_seconds", &server, channel, 30).clamp(0, 3_600);
    let key = cooldown_key(&server, &msg.user_id);
    let remaining = cooldown - current.saturating_sub(cooldown_get(&key)?);
    if current > 0 && remaining > 0 && remaining <= cooldown {
        let seconds = remaining.to_string();
        reply(
            &server,
            destination,
            &themed(
                "cooldown",
                &["Please wait {seconds}s before asking me again, {user}."],
                &[("seconds", &seconds), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    cooldown_set(&key, current)?;

    let temperature =
        setting_i64("temperature_percent", &server, channel, 70).clamp(0, 200) as f64 / 100.0;
    let max_tokens = setting_i64("max_tokens", &server, channel, 256).clamp(16, 1_024) as u32;
    let raw = unsafe {
        ai_chat(serde_json::to_string(&AiChatRequest {
            prompt: prompt.into(),
            temperature,
            max_tokens,
        })?)?
    };
    let response: AiChatResponse = serde_json::from_str(&raw)?;
    if let Some(text) = response.text {
        reply(
            &server,
            destination,
            &themed("response", &["{response}"], &[("response", &text)])?,
        )?;
        return Ok(());
    }
    let (key, default) = match response.error.as_deref() {
        Some("not_configured") => (
            "not_configured",
            "AI chat has not been configured by the operator yet.",
        ),
        Some("soul_unavailable") => (
            "soul_unavailable",
            "My SOUL.md is unavailable, so I cannot answer safely right now.",
        ),
        Some("busy") => ("busy", "I am already thinking about another question."),
        Some("authentication") => ("authentication", "The AI provider rejected its credentials."),
        Some("rate_limited") => ("rate_limited", "The AI provider is rate-limiting requests."),
        Some("invalid_request") => ("invalid_request", "The AI provider rejected that request."),
        _ => ("unavailable", "The AI provider is not answering right now."),
    };
    reply(&server, destination, &themed(key, &[default], &[])?)?;
    Ok(())
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let values = request
        .entries
        .iter()
        .filter(|entry| entry.key == key)
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if values.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({"cooldown_timestamps": values})
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let mutations = request
        .entries
        .iter()
        .filter(|entry| entry.key == key)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_explicit_channel_address_punctuation() {
        let names = names("jeevesbot", "jeeves, butler");
        assert_eq!(
            addressed_prompt("Jeeves, what time is it?", &names),
            Some("what time is it?")
        );
        assert_eq!(addressed_prompt("jeeves: hello", &names), Some("hello"));
        assert_eq!(addressed_prompt("I told jeeves hello", &names), None);
        assert_eq!(addressed_prompt("jeeves is useful", &names), None);
    }

    #[test]
    fn aliases_are_bounded_validated_and_deduplicated() {
        let parsed = names("JeevesBot", "jeeves, JEEVES, bad alias, helper");
        assert_eq!(parsed, vec!["jeevesbot", "jeeves", "helper"]);
    }

    #[test]
    fn cooldown_is_keyed_by_stable_profile_uuid() {
        assert!(cooldown_key("libera", "uuid-123").contains("757569642d313233"));
    }

    #[test]
    fn private_and_channel_enablement_are_isolated() {
        let names = names("jeeves", "");
        assert_eq!(
            select_prompt(true, true, false, "hello", &names),
            Some("hello")
        );
        assert_eq!(select_prompt(true, false, true, "hello", &names), None);
        assert_eq!(
            select_prompt(false, false, true, "jeeves: hello", &names),
            Some("hello")
        );
        assert_eq!(
            select_prompt(false, true, false, "jeeves: hello", &names),
            None
        );
    }

    #[test]
    fn private_commands_are_not_ai_prompts() {
        let prompt = select_prompt(true, true, false, "!mydata summary", &[]).unwrap();
        assert!(prompt.starts_with('!'));
    }
}
