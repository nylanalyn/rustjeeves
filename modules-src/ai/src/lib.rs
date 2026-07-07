//! Addressed AI chat responder. HTTP, endpoint selection, SOUL.md, and credentials remain in the
//! host; this module handles IRC addressing, policy settings, cooldowns, and themed replies.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AiChatContextLine, AiChatRequest,
    AiChatResponse, AwardStatsRequest, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, SendMessage, ServerQuery, SettingGet,
    SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const MAX_PROMPT_CHARS: usize = 1_000;
const MAX_STORED_CONTEXT_LINES: usize = 30;
const MAX_CONTEXT_TEXT_CHARS: usize = 400;
const MAX_PROVIDER_CONTEXT_CHARS: usize = 8_000;
const DEFAULT_RESPONSE_LINE_BYTES: usize = 400;
const DEFAULT_RESPONSE_MAX_LINES: usize = 3;

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
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "responses".into(),
            description: "Successful AI responses".into(),
        }],
        achievements: [
            ("word_with_jeeves", "A Word with Jeeves", 1),
            ("regular_consultation", "A Regular Consultation", 25),
            ("considerable_length", "At Considerable Length", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Receive {threshold} successful AI responses."),
            stat: "responses".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

fn award(server: &str, profile_id: &str, display_name: &str, target: &str) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "responses".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
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
                kind: SettingKind::Integer {
                    min: 16,
                    max: 1_024,
                },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "response_line_bytes".into(),
                description: "Preferred maximum UTF-8 bytes per IRC line.".into(),
                default: DEFAULT_RESPONSE_LINE_BYTES.to_string(),
                kind: SettingKind::Integer { min: 100, max: 450 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "response_max_lines".into(),
                description: "Maximum IRC lines sent for one AI response.".into(),
                default: DEFAULT_RESPONSE_MAX_LINES.to_string(),
                kind: SettingKind::Integer { min: 1, max: 3 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "context_lines".into(),
                description: "Recent room or PM lines supplied as conversational context.".into(),
                default: "25".into(),
                kind: SettingKind::Integer { min: 0, max: 30 },
                scopes: all_scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "context_max_age_minutes".into(),
                description: "Maximum age of AI conversation context.".into(),
                default: "180".into(),
                kind: SettingKind::Integer { min: 1, max: 1_440 },
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

fn response_lines(text: &str, max_bytes: usize, max_lines: usize) -> Vec<String> {
    let max_bytes = max_bytes.max(4);
    let mut remaining = text.trim();
    let mut lines = Vec::new();
    while !remaining.is_empty() && lines.len() < max_lines {
        if remaining.len() <= max_bytes {
            lines.push(remaining.to_string());
            break;
        }
        let mut sentence_end = None;
        let mut word_end = None;
        for (byte, ch) in remaining.char_indices() {
            let end = byte + ch.len_utf8();
            if end > max_bytes {
                break;
            }
            if matches!(ch, '.' | '!' | '?') {
                sentence_end = Some(end);
            }
            if ch.is_whitespace() {
                word_end = Some(byte);
            }
        }
        let split = sentence_end.or(word_end).unwrap_or_else(|| {
            let mut end = max_bytes.min(remaining.len());
            while !remaining.is_char_boundary(end) {
                end -= 1;
            }
            end
        });
        let line = remaining[..split].trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
        remaining = remaining[split..].trim_start();
    }
    lines
}

fn reply_response(
    server: &str,
    target: &str,
    text: &str,
    max_bytes: usize,
    max_lines: usize,
) -> Result<(), Error> {
    for line in response_lines(text, max_bytes, max_lines) {
        reply(server, target, &line)?;
    }
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

fn context_key(server: &str, conversation: &str) -> String {
    format!("context:{}:{}", encode(server), encode(conversation))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct ContextLine {
    profile_id: String,
    speaker: String,
    text: String,
    timestamp: i64,
}

fn context_get(key: &str) -> Result<Vec<ContextLine>, Error> {
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    if raw.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn context_set(key: &str, lines: &[ContextLine]) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: serde_json::to_string(lines)?,
        })?)?
    };
    Ok(())
}

fn bounded_text(text: &str) -> String {
    text.trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_CONTEXT_TEXT_CHARS)
        .collect()
}

fn bounded_speaker(speaker: &str) -> String {
    let speaker: String = speaker
        .trim()
        .chars()
        .filter(|character| !character.is_control())
        .take(64)
        .collect();
    if speaker.is_empty() {
        "user".into()
    } else {
        speaker
    }
}

fn prune_context(lines: &mut Vec<ContextLine>, now: i64, max_age_seconds: i64, limit: usize) {
    let cutoff = now.saturating_sub(max_age_seconds);
    lines.retain(|line| line.timestamp >= cutoff);
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
}

fn provider_context(lines: &[ContextLine], limit: usize) -> Vec<AiChatContextLine> {
    let mut chars = 0;
    let mut selected = lines
        .iter()
        .rev()
        .take(limit)
        .take_while(|line| {
            let line_chars = line.speaker.chars().count() + line.text.chars().count();
            if chars + line_chars > MAX_PROVIDER_CONTEXT_CHARS {
                false
            } else {
                chars += line_chars;
                true
            }
        })
        .map(|line| AiChatContextLine {
            speaker: line.speaker.clone(),
            text: line.text.clone(),
        })
        .collect::<Vec<_>>();
    selected.reverse();
    selected
}

fn cooldown_get(key: &str) -> Result<i64, Error> {
    Ok(
        unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? }
            .parse()
            .unwrap_or(0),
    )
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
    let prompt = select_prompt(
        msg.is_private,
        pm_enabled,
        channel_enabled,
        &msg.text,
        &names,
    )
    .map(str::to_string);
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
    let current = timestamp()?;
    let context_limit = setting_i64("context_lines", &server, channel, 25)
        .clamp(0, MAX_STORED_CONTEXT_LINES as i64) as usize;
    let context_max_age =
        setting_i64("context_max_age_minutes", &server, channel, 180).clamp(1, 1_440) * 60;
    let conversation = if msg.is_private {
        format!("pm:{}", msg.user_id)
    } else {
        format!("channel:{}", msg.target)
    };
    let context_key = context_key(&server, &conversation);
    let mut context = if context_limit > 0 {
        context_get(&context_key)?
    } else {
        Vec::new()
    };
    prune_context(&mut context, current, context_max_age, context_limit);
    let request_context = provider_context(&context, context_limit);

    // Commands are not conversation, and lines without stable ownership cannot participate in
    // lifecycle export/deletion. All other enabled-room messages become bounded local context.
    let message_text = bounded_text(&msg.text);
    let retain_message = context_limit > 0
        && !msg.user_id.is_empty()
        && !message_text.is_empty()
        && !message_text.starts_with('!');
    if retain_message {
        context.push(ContextLine {
            profile_id: msg.user_id.clone(),
            speaker: bounded_speaker(user),
            text: message_text,
            timestamp: current,
        });
        prune_context(&mut context, current, context_max_age, context_limit);
        context_set(&context_key, &context)?;
    }

    let Some(prompt) = prompt.as_deref() else {
        return Ok(());
    };
    if msg.is_private && prompt.starts_with('!') {
        return Ok(());
    }
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
            context: request_context,
            temperature,
            max_tokens,
        })?)?
    };
    let response: AiChatResponse = serde_json::from_str(&raw)?;
    if let Some(text) = response.text {
        let rendered = themed("response", &["{response}"], &[("response", &text)])?;
        let response_line_bytes = setting_i64(
            "response_line_bytes",
            &server,
            channel,
            DEFAULT_RESPONSE_LINE_BYTES as i64,
        )
        .clamp(100, 450) as usize;
        let response_max_lines = setting_i64(
            "response_max_lines",
            &server,
            channel,
            DEFAULT_RESPONSE_MAX_LINES as i64,
        )
        .clamp(1, 3) as usize;
        reply_response(
            &server,
            destination,
            &rendered,
            response_line_bytes,
            response_max_lines,
        )?;
        if context_limit > 0 {
            context.push(ContextLine {
                profile_id: msg.user_id.clone(),
                speaker: if configured_bot_nick.is_empty() {
                    "bot".into()
                } else {
                    bounded_speaker(&configured_bot_nick)
                },
                text: bounded_text(&rendered),
                timestamp: current,
            });
            prune_context(&mut context, current, context_max_age, context_limit);
            context_set(&context_key, &context)?;
        }
        award(&server, &msg.user_id, user, destination)?;
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
        Some("authentication") => (
            "authentication",
            "The AI provider rejected its credentials.",
        ),
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
    let cooldown_key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let cooldown_timestamps = request
        .entries
        .iter()
        .filter(|entry| entry.key == cooldown_key)
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    let context_prefix = format!("context:{}:", encode(&request.subject.server));
    let mut context_lines = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&context_prefix))
    {
        let lines: Vec<ContextLine> = serde_json::from_str(&entry.value)?;
        context_lines.extend(
            lines
                .into_iter()
                .filter(|line| line.profile_id == request.subject.profile_id)
                .map(|line| {
                    serde_json::json!({
                        "conversation": entry.key,
                        "speaker": line.speaker,
                        "text": line.text,
                        "timestamp": line.timestamp,
                    })
                }),
        );
    }
    let empty = cooldown_timestamps.is_empty() && context_lines.is_empty();
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if empty {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "cooldown_timestamps": cooldown_timestamps,
                "recent_context": context_lines,
            })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    Ok(data_delete_impl(input)?)
}

fn data_delete_impl(input: String) -> Result<String, Error> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let cooldown_key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let context_prefix = format!("context:{}:", encode(&request.subject.server));
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if entry.key == cooldown_key {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            });
        } else if entry.key.starts_with(&context_prefix) {
            let mut lines: Vec<ContextLine> = serde_json::from_str(&entry.value)?;
            let original_len = lines.len();
            lines.retain(|line| line.profile_id != request.subject.profile_id);
            if lines.len() != original_len {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: if lines.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&lines)?)
                    },
                });
            }
        }
    }
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

    #[test]
    fn context_is_pruned_by_age_and_line_count() {
        let mut lines = vec![
            ContextLine {
                profile_id: "old".into(),
                speaker: "old".into(),
                text: "expired".into(),
                timestamp: 10,
            },
            ContextLine {
                profile_id: "a".into(),
                speaker: "alice".into(),
                text: "one".into(),
                timestamp: 90,
            },
            ContextLine {
                profile_id: "b".into(),
                speaker: "bob".into(),
                text: "two".into(),
                timestamp: 100,
            },
        ];
        prune_context(&mut lines, 100, 20, 1);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].speaker, "bob");
    }

    #[test]
    fn provider_context_preserves_recent_order() {
        let lines = (0..4)
            .map(|index| ContextLine {
                profile_id: index.to_string(),
                speaker: format!("user{index}"),
                text: format!("line{index}"),
                timestamp: index,
            })
            .collect::<Vec<_>>();
        let context = provider_context(&lines, 2);
        assert_eq!(context[0].speaker, "user2");
        assert_eq!(context[1].speaker, "user3");
    }

    #[test]
    fn responses_split_at_sentences_and_respect_line_limit() {
        let text = "First sentence. Second sentence is longer. Third sentence. Fourth sentence.";
        assert_eq!(
            response_lines(text, 32, 3),
            vec![
                "First sentence.",
                "Second sentence is longer.",
                "Third sentence. Fourth sentence."
            ]
        );
    }

    #[test]
    fn responses_fall_back_to_unicode_safe_boundaries() {
        assert_eq!(
            response_lines("café-example", 8, 2),
            vec!["café-ex", "ample"]
        );
    }

    #[test]
    fn lifecycle_delete_removes_only_the_subjects_shared_context_lines() {
        let key = context_key("net", "channel:#room");
        let lines = vec![
            ContextLine {
                profile_id: "subject".into(),
                speaker: "alice".into(),
                text: "remove me".into(),
                timestamp: 1,
            },
            ContextLine {
                profile_id: "other".into(),
                speaker: "bob".into(),
                text: "keep me".into(),
                timestamp: 2,
            },
        ];
        let request = serde_json::json!({
            "version": DATA_LIFECYCLE_VERSION,
            "subject": {"server": "net", "profile_id": "subject"},
            "aliases": [],
            "entries": [{"key": key, "value": serde_json::to_string(&lines).unwrap()}],
        });
        let plan: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete_impl(request.to_string()).unwrap()).unwrap();
        let remaining: Vec<ContextLine> =
            serde_json::from_str(plan.mutations[0].value.as_deref().unwrap()).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].profile_id, "other");
    }
}
