//! Bounded dictionary definitions through the host-owned `dictionary_lookup` capability.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, DictionaryQuery, DictionaryResponse, Event, EventEnvelope, KvGet, KvSet,
    ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, SendMessage,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};

const DEFAULT_COOLDOWN_SECONDS: i64 = 20;
const MAX_WORD_CHARS: usize = 64;
const MAX_SENSES: usize = 3;
const MAX_DEFINITION_CHARS: usize = 110;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn dictionary_lookup(input: String) -> String;
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
            id: "definitions".into(),
            description: "Successful definitions".into(),
        }],
        achievements: [
            ("a_word_sir", "A Word, Sir?", 1),
            ("lexically_inclined", "Lexically Inclined", 25),
            ("walking_dictionary", "Walking Dictionary", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Look up {threshold} successful definitions."),
            stat: "definitions".into(),
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
                stat: "definitions".into(),
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
        commands: vec![CommandSpec {
            name: "define".into(),
            aliases: vec!["def".into()],
            description: "Look up a short, safe dictionary definition.".into(),
            usage: "!define <word>".into(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "cooldown_seconds".into(),
            description: "Minimum delay between dictionary lookups by one user.".into(),
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

fn get_cooldown(key: &str) -> Result<i64, Error> {
    let value = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    if value.is_empty() {
        Ok(0)
    } else {
        Ok(value.parse()?)
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
    if !matches!(command.as_str(), "!define" | "!def") {
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
    let word = parts.next().unwrap_or("").trim();
    if word.is_empty() {
        reply(
            &env.server,
            destination,
            &themed(
                "define.usage",
                &["What word should I define, {user}? Try !define <word>."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if !valid_word(word) {
        reply(
            &env.server,
            destination,
            &themed(
                "define.invalid",
                &["{user}, enter one word of at most 64 letters; hyphens and apostrophes are allowed."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if msg.user_id.is_empty() {
        reply(
            &env.server,
            destination,
            &themed(
                "define.identity_unavailable",
                &["I can't verify your profile for a dictionary lookup right now, {user}."],
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
    let remaining = window.saturating_sub(now.saturating_sub(get_cooldown(&key)?));
    if window > 0 && remaining > 0 && remaining <= window {
        reply(
            &env.server,
            destination,
            &themed(
                "define.cooldown",
                &["Please wait {seconds}s before another definition, {user}."],
                &[("seconds", &remaining.to_string()), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    set_cooldown(&key, now)?;

    let raw = unsafe {
        dictionary_lookup(serde_json::to_string(&DictionaryQuery {
            word: word.into(),
        })?)?
    };
    let response: DictionaryResponse = serde_json::from_str(&raw)?;
    if response.senses.is_empty() {
        let (key, default) = match response.error.as_deref() {
            Some("not_found" | "invalid_word") | None => (
                "define.not_found",
                "I couldn't find a definition for '{word}', {user}.",
            ),
            Some(_) => (
                "define.unavailable",
                "The dictionary isn't answering right now, {user}.",
            ),
        };
        reply(
            &env.server,
            destination,
            &themed(key, &[default], &[("word", word), ("user", user)])?,
        )?;
        return Ok(());
    }
    let display_word = clean(response.word.as_deref().unwrap_or(word), MAX_WORD_CHARS);
    let phonetic = clean(response.phonetic.as_deref().unwrap_or(""), 80);
    let definitions = format_senses(&response);
    reply(
        &env.server,
        destination,
        &themed(
            "define.result",
            &["{word} {phonetic} — {definitions}"],
            &[
                ("word", &display_word),
                ("phonetic", &phonetic),
                ("definitions", &definitions),
                ("user", user),
            ],
        )?,
    )?;
    award(&env.server, &msg.user_id, user, destination)?;
    Ok(())
}

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().count() <= MAX_WORD_CHARS
        && word
            .chars()
            .all(|c| c.is_alphabetic() || c == '-' || c == '\'')
}

fn format_senses(response: &DictionaryResponse) -> String {
    response
        .senses
        .iter()
        .take(MAX_SENSES)
        .enumerate()
        .map(|(index, sense)| {
            let part = clean(&sense.part_of_speech, 24);
            let definition = clean(&sense.definition, MAX_DEFINITION_CHARS);
            if part.is_empty() {
                format!("{}. {definition}", index + 1)
            } else {
                format!("{}. ({part}) {definition}", index + 1)
            }
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn clean(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(max_chars)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::DictionarySense;

    #[test]
    fn validates_single_words() {
        assert!(valid_word("dictionary"));
        assert!(valid_word("mother-in-law"));
        assert!(valid_word("don't"));
        assert!(!valid_word("two words"));
        assert!(!valid_word("word/../../path"));
    }

    #[test]
    fn formats_and_bounds_senses() {
        let response = DictionaryResponse {
            senses: (1..=4)
                .map(|number| DictionarySense {
                    part_of_speech: "noun".into(),
                    definition: format!("definition {number}"),
                })
                .collect(),
            ..DictionaryResponse::default()
        };
        let output = format_senses(&response);
        assert!(output.contains("1. (noun) definition 1"));
        assert!(output.contains("3. (noun) definition 3"));
        assert!(!output.contains("definition 4"));
    }

    #[test]
    fn cooldown_keys_are_unambiguous() {
        assert_ne!(cooldown_key("a:b", "c"), cooldown_key("a", "b:c"));
    }
}
