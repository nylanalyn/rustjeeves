//! Bounded dictionary definitions (`!define`) and Wiktionary etymologies (`!etym`) through the
//! host-owned `dictionary_lookup` capability.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, DictionaryQuery, DictionaryResponse, EtymologyResponse, Event, EventEnvelope,
    KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{encode, reply, themed, timestamp};

const DEFAULT_COOLDOWN_SECONDS: i64 = 20;
const MAX_WORD_CHARS: usize = 64;
const MAX_SENSES: usize = 3;
const MAX_DEFINITION_CHARS: usize = 110;
const MAX_WORDS: usize = 3;
const MAX_SYNONYMS: usize = 5;
/// Total length of an etymology reply; a second etymology is shown only if it fits.
const MAX_ETYMOLOGY_CHARS: usize = 600;

#[host_fn]
extern "ExtismHost" {
    fn dictionary_lookup(input: String) -> String;
    fn etymology_lookup(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 2,
        stats: vec![
            AchievementStat {
                id: "definitions".into(),
                description: "Successful definitions".into(),
            },
            AchievementStat {
                id: "etymologies".into(),
                description: "Word histories looked up".into(),
            },
        ],
        achievements: [
            ("a_word_sir", "A Word, Sir?", "definitions", 1),
            (
                "lexically_inclined",
                "Lexically Inclined",
                "definitions",
                25,
            ),
            (
                "walking_dictionary",
                "Walking Dictionary",
                "definitions",
                100,
            ),
            ("whence_it_came", "Whence It Came", "etymologies", 1),
            ("philologist", "Philologist", "etymologies", 25),
        ]
        .into_iter()
        .map(|(id, name, stat, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: match (stat, threshold) {
                ("definitions", 1) => "Look up a successful definition.".into(),
                ("definitions", _) => format!("Look up {threshold} successful definitions."),
                (_, 1) => "Trace the history of a word with !etym.".into(),
                _ => format!("Trace the history of {threshold} words with !etym."),
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
        commands: vec![
            CommandSpec {
                name: "define".into(),
                aliases: vec!["def".into()],
                description: "Look up a short, safe dictionary definition.".into(),
                usage: "!define <word or short phrase>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "etym".into(),
                aliases: vec!["etymology".into()],
                description: "Where an English word comes from, according to Wiktionary.".into(),
                usage: "!etym <word or short phrase>".into(),
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
    if command != "!define" && command != "!etym" {
        return Ok(());
    }
    let etymology = command == "!etym";
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
    let word = parts
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let word = word.as_str();
    if word.is_empty() {
        let (key, default) = if etymology {
            (
                "define.etym_usage",
                "Which word's history would you like, {user}? Try !etym <word>.",
            )
        } else {
            (
                "define.usage",
                "What word should I define, {user}? Try !define <word>.",
            )
        };
        reply(
            &env.server,
            destination,
            &themed(key, &[default], &[("user", user)])?,
        )?;
        return Ok(());
    }
    if !valid_word(word) {
        reply(
            &env.server,
            destination,
            &themed(
                "define.invalid",
                &["{user}, enter a word or a phrase of up to three words; hyphens and apostrophes are allowed."],
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
                "define.cooldown",
                &["Please wait {seconds}s before another definition, {user}."],
                &[("seconds", &remaining.to_string()), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    set_cooldown(&key, now)?;
    if etymology {
        return Ok(reply_etymology(
            &env.server,
            destination,
            user,
            &msg.user_id,
            word,
        )?);
    }

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
    let synonyms = response
        .synonyms
        .iter()
        .take(MAX_SYNONYMS)
        .map(|synonym| clean(synonym, 40))
        .collect::<Vec<_>>()
        .join(", ");
    let vars = [
        ("word", display_word.as_str()),
        ("phonetic", phonetic.as_str()),
        ("definitions", definitions.as_str()),
        ("synonyms", synonyms.as_str()),
        ("user", user),
    ];
    let text = if synonyms.is_empty() {
        themed(
            "define.result",
            &["{word} {phonetic} — {definitions}"],
            &vars,
        )?
    } else {
        themed(
            "define.result_synonyms",
            &["{word} {phonetic} — {definitions} · Synonyms: {synonyms}"],
            &vars,
        )?
    };
    // A missing phonetic would leave a double space in the default layout.
    let text = text
        .split(' ')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    reply(&env.server, destination, &text)?;
    award(&env.server, &msg.user_id, user, destination)?;
    Ok(())
}

fn reply_etymology(
    server: &str,
    destination: &str,
    user: &str,
    user_id: &str,
    word: &str,
) -> Result<(), Error> {
    let raw = unsafe {
        etymology_lookup(serde_json::to_string(&DictionaryQuery {
            word: word.into(),
        })?)?
    };
    let response: EtymologyResponse = serde_json::from_str(&raw)?;
    if response.etymologies.is_empty() {
        let (key, default) = match response.error.as_deref() {
            Some("not_found" | "invalid_word") | None => (
                "define.etym_not_found",
                "Wiktionary has no English etymology for '{word}', {user}.",
            ),
            Some(_) => (
                "define.etym_unavailable",
                "Wiktionary isn't answering right now, {user}.",
            ),
        };
        return reply(
            server,
            destination,
            &themed(key, &[default], &[("word", word), ("user", user)])?,
        );
    }
    let display_word = clean(response.word.as_deref().unwrap_or(word), MAX_WORD_CHARS);
    let url = response.url.unwrap_or_default();
    let text = themed(
        "define.etym_result",
        &["{word}: {etymology}"],
        &[
            ("word", &display_word),
            ("etymology", &format_etymologies(&response.etymologies)),
            ("url", &url),
            ("user", user),
        ],
    )?;
    reply(server, destination, &text)?;
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: user_id.into(),
            display_name: user.into(),
            target: destination.into(),
            increments: vec![StatIncrement {
                stat: "etymologies".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

/// One etymology as-is; several numbered, keeping later ones only while the reply stays short.
fn format_etymologies(etymologies: &[String]) -> String {
    let first = clean(&etymologies[0], MAX_ETYMOLOGY_CHARS);
    let rest = etymologies[1..]
        .iter()
        .map(|text| clean(text, MAX_ETYMOLOGY_CHARS))
        .collect::<Vec<_>>();
    if rest.is_empty() {
        return first;
    }
    let mut out = format!("1. {first}");
    for (index, text) in rest.iter().enumerate() {
        let next = format!(" {}. {text}", index + 2);
        if out.chars().count() + next.chars().count() > MAX_ETYMOLOGY_CHARS {
            break;
        }
        out.push_str(&next);
    }
    if out == format!("1. {first}") {
        first
    } else {
        out
    }
}

fn valid_word(word: &str) -> bool {
    !word.is_empty()
        && word.chars().count() <= MAX_WORD_CHARS
        && word.split(' ').count() <= MAX_WORDS
        && word
            .chars()
            .all(|c| c.is_alphabetic() || matches!(c, '-' | '\'' | ' '))
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

/// Collapse whitespace and fit `max_chars`, marking a cut with `…` so a truncated definition
/// doesn't read as complete.
fn clean(value: &str, max_chars: usize) -> String {
    let text = value
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.chars().count() <= max_chars {
        return text;
    }
    let mut cut = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    if let Some(space) = cut.rfind(' ').filter(|space| *space > cut.len() / 2) {
        cut.truncate(space);
    }
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_text_is_marked() {
        assert_eq!(clean("  a   b ", 10), "a b");
        let cut = clean("a frozen dessert made from cream and sugar", 20);
        assert!(cut.ends_with('…') && cut.chars().count() <= 20, "{cut}");
    }
    use jeeves_abi::DictionarySense;

    #[test]
    fn validates_single_words() {
        assert!(valid_word("dictionary"));
        assert!(valid_word("mother-in-law"));
        assert!(valid_word("don't"));
        assert!(valid_word("ice cream"));
        assert!(!valid_word("far too many words"));
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
    fn etymologies_are_numbered_only_when_several_fit() {
        assert_eq!(format_etymologies(&["From Latin.".into()]), "From Latin.");
        assert_eq!(
            format_etymologies(&["From Latin.".into(), "From Greek.".into()]),
            "1. From Latin. 2. From Greek."
        );
        let long = "word ".repeat(100);
        assert_eq!(
            format_etymologies(&[long.clone(), long]),
            clean(&"word ".repeat(100), MAX_ETYMOLOGY_CHARS)
        );
    }

    #[test]
    fn cooldown_keys_are_unambiguous() {
        assert_ne!(cooldown_key("a:b", "c"), cooldown_key("a", "b:c"));
    }
}
