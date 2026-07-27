//! DeepL-backed `!tr` / `!translate` commands. HTTP and credentials stay in the host.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, SendMessage, StatIncrement, ThemeReq, TranslateQuery,
    TranslateResponse, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION,
};
use serde::{Deserialize, Serialize};
use whatlang::{detect_lang, Lang};

const COOLDOWN_SECS: i64 = 10;
const MAX_TEXT_CHARS: usize = 350;
const MAX_RECENT_MESSAGES: usize = 10;
const RECENT_MESSAGE_MAX_AGE_SECS: i64 = 15 * 60;
const HISTORY_KEY_PREFIX: &str = "recent:";

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct RecentMessage {
    #[serde(default)]
    user_id: String,
    #[serde(default)]
    nick: String,
    speaker: String,
    text: String,
    timestamp: i64,
}

#[derive(Default, Deserialize, Serialize)]
struct RecentHistory {
    messages: Vec<RecentMessage>,
}

#[derive(Debug, PartialEq, Eq)]
enum CommandIntent {
    Recent,
    Help,
    Languages,
    Translate {
        source_lang: Option<String>,
        target_lang: String,
        text: String,
    },
}

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn translate(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "translations".into(),
            description: "Successful translations".into(),
        }],
        achievements: [
            ("parlez_vous", "Parlez-vous?", 1),
            ("phrasebook_worn", "Phrasebook Worn", 25),
            ("babels_butler", "Babel’s Butler", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Complete {threshold} successful translations."),
            stat: "translations".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

fn award(server: &str, profile_id: &str, display_name: &str, target: &str) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "translations".into(),
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
            name: "translate".into(),
            aliases: vec!["tr".into()],
            description: "Translate text with DeepL.".into(),
            usage: "!translate [target|source:target] [text]".into(),
        }],
    })?)
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|value| (*value).into()).collect(),
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

fn cooldown_key(server: &str, user_id: &str, nick: &str) -> String {
    let identity = if user_id.is_empty() { nick } else { user_id };
    format!("cooldown:{}:{}", encode(server), encode(identity))
}

fn history_key(server: &str, channel: &str) -> String {
    format!("{HISTORY_KEY_PREFIX}{}:{}", encode(server), encode(channel))
}

fn history_key_prefix(server: &str) -> String {
    format!("{HISTORY_KEY_PREFIX}{}:", encode(server))
}

fn load_history(server: &str, channel: &str) -> Result<RecentHistory, Error> {
    let value = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: history_key(server, channel),
        })?)?
    };
    if value.is_empty() {
        Ok(RecentHistory::default())
    } else {
        Ok(serde_json::from_str(&value)?)
    }
}

fn save_history(server: &str, channel: &str, history: &RecentHistory) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: history_key(server, channel),
            value: serde_json::to_string(history)?,
        })?)?
    };
    Ok(())
}

fn prune_history(history: &mut RecentHistory, current_time: i64) {
    if current_time > 0 {
        history.messages.retain(|message| {
            message.timestamp <= 0
                || current_time.saturating_sub(message.timestamp) <= RECENT_MESSAGE_MAX_AGE_SECS
        });
    }
    let excess = history.messages.len().saturating_sub(MAX_RECENT_MESSAGES);
    history.messages.drain(..excess);
}

fn retain_message(
    history: &mut RecentHistory,
    is_private: bool,
    user_id: &str,
    nick: &str,
    speaker: &str,
    text: &str,
    current_time: i64,
) {
    let text = text.trim();
    if is_private || text.is_empty() || text.starts_with('!') {
        return;
    }
    let text = sanitize(text);
    if text.is_empty() {
        return;
    }
    history.messages.push(RecentMessage {
        user_id: user_id.into(),
        nick: nick.into(),
        speaker: speaker.into(),
        text,
        timestamp: current_time,
    });
    prune_history(history, current_time);
}

fn select_recent_message(history: &RecentHistory) -> Option<&RecentMessage> {
    history
        .messages
        .iter()
        .rev()
        .find(|message| detect_lang(&message.text).is_some_and(|lang| lang != Lang::Eng))
        .or_else(|| history.messages.last())
}

fn lifecycle_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| cooldown_key(&request.subject.server, identity, identity))
        .collect()
}

fn lifecycle_identities(request: &ModuleDataRequest) -> Vec<&str> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .collect()
}

fn message_belongs_to(message: &RecentMessage, identities: &[&str]) -> bool {
    identities
        .iter()
        .any(|identity| *identity == message.user_id || *identity == message.nick)
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let cooldown_timestamps = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    let identities = lifecycle_identities(&request);
    let history_prefix = history_key_prefix(&request.subject.server);
    let mut recent_messages = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&history_prefix))
    {
        let history: RecentHistory = serde_json::from_str(&entry.value)?;
        recent_messages.extend(
            history
                .messages
                .into_iter()
                .filter(|message| message_belongs_to(message, &identities)),
        );
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if cooldown_timestamps.is_empty() && recent_messages.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "cooldown_timestamps": cooldown_timestamps,
                "recent_messages": recent_messages,
            })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let identities = lifecycle_identities(&request);
    let history_prefix = history_key_prefix(&request.subject.server);
    let mut mutations = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect::<Vec<_>>();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&history_prefix))
    {
        let mut history: RecentHistory = serde_json::from_str(&entry.value)?;
        let original_len = history.messages.len();
        history
            .messages
            .retain(|message| !message_belongs_to(message, &identities));
        if history.messages.len() != original_len {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: if history.messages.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&history)?)
                },
            });
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

/// A negative timestamp means this cooldown has already displayed its one warning.
fn get_cooldown(key: &str) -> Result<(i64, bool), Error> {
    let value = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    let timestamp = value.parse::<i64>().unwrap_or(0);
    Ok((timestamp.saturating_abs(), timestamp < 0))
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
    let mut command_parts = text.splitn(2, char::is_whitespace);
    let command = command_parts.next().unwrap_or("").to_ascii_lowercase();
    if !matches!(command.as_str(), "!tr" | "!translate") {
        if !msg.is_private && !text.is_empty() && !text.starts_with('!') {
            let current_time = timestamp()?;
            let mut history = load_history(&server, &msg.target)?;
            let speaker = if msg.display.is_empty() {
                &msg.nick
            } else {
                &msg.display
            };
            retain_message(
                &mut history,
                false,
                &msg.user_id,
                &msg.nick,
                speaker,
                text,
                current_time,
            );
            save_history(&server, &msg.target, &history)?;
        }
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
    let arguments = command_parts.next().unwrap_or("").trim();
    let (source_lang, target_lang, source_text, recent_speaker, current_time) =
        match parse_command_intent(arguments) {
            CommandIntent::Help => {
                reply(
                    &server,
                    destination,
                    &themed(
                        "help",
                        &["Usage: !tr <text>, !tr <target> <text>, or !tr <source>:<target> <text>. Bare !tr translates a recent message."],
                        &[],
                    )?,
                )?;
                return Ok(());
            }
            CommandIntent::Languages => {
                reply(
                    &server,
                    destination,
                    &themed(
                        "languages",
                        &["Use language codes such as en, fr, de, es, it, nl, pl, pt-br, ja, ko, zh, uk, or a language name."],
                        &[],
                    )?,
                )?;
                return Ok(());
            }
            CommandIntent::Recent => {
                if msg.is_private {
                    reply(
                        &server,
                        destination,
                        &themed(
                            "translate.no_recent",
                            &["I haven't heard anything recent to translate."],
                            &[],
                        )?,
                    )?;
                    return Ok(());
                }
                let current_time = timestamp()?;
                let mut history = load_history(&server, &msg.target)?;
                prune_history(&mut history, current_time);
                let selected = select_recent_message(&history).cloned();
                save_history(&server, &msg.target, &history)?;
                let Some(selected) = selected else {
                    reply(
                        &server,
                        destination,
                        &themed(
                            "translate.no_recent",
                            &["I haven't heard anything recent to translate."],
                            &[],
                        )?,
                    )?;
                    return Ok(());
                };
                (
                    None,
                    "EN-US".into(),
                    selected.text,
                    Some(selected.speaker),
                    current_time,
                )
            }
            CommandIntent::Translate {
                source_lang,
                target_lang,
                text,
            } => (source_lang, target_lang, text, None, timestamp()?),
        };
    let source_text = sanitize(&source_text);
    if source_text.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "missing_text",
                &["What should I translate, {user}?"],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    let key = cooldown_key(&server, &msg.user_id, &msg.nick);
    let (last_used, warned) = get_cooldown(&key)?;
    let remaining = COOLDOWN_SECS - current_time.saturating_sub(last_used);
    if current_time > 0 && remaining > 0 && remaining <= COOLDOWN_SECS {
        if warned {
            return Ok(());
        }
        set_cooldown(&key, -last_used)?;
        let seconds = remaining.to_string();
        reply(
            &server,
            destination,
            &themed(
                "cooldown",
                &["Please wait {seconds}s before translating again, {user}."],
                &[("seconds", &seconds), ("user", user)],
            )?,
        )?;
        return Ok(());
    }
    set_cooldown(&key, current_time)?;

    let request = TranslateQuery {
        text: source_text,
        target_lang: target_lang.clone(),
        source_lang: source_lang.clone(),
    };
    let raw = unsafe { translate(serde_json::to_string(&request)?)? };
    let response: TranslateResponse = serde_json::from_str(&raw)?;
    if let Some(translated) = response.text {
        let translated = sanitize(&translated);
        let source = response
            .detected_source_language
            .or(source_lang)
            .unwrap_or_else(|| "AUTO".into());
        let (theme_key, defaults) = if recent_speaker.is_some() {
            (
                "translate.recent_result",
                &["{speaker} said, {source} → {target}: {translation}"][..],
            )
        } else {
            ("result", &["{source} → {target}: {translation}"][..])
        };
        let speaker = recent_speaker.as_deref().unwrap_or("");
        reply(
            &server,
            destination,
            &themed(
                theme_key,
                defaults,
                &[
                    ("speaker", speaker),
                    ("source", &source),
                    ("target", &target_lang),
                    ("translation", &translated),
                ],
            )?,
        )?;
        award(&server, &msg.user_id, user, destination)?;
    } else {
        let (key, default) = match response.error.as_deref() {
            Some("not_configured") => (
                "not_configured",
                "Translation needs a DeepL API key in F3 Integrations.",
            ),
            Some("authentication") => ("authentication", "DeepL rejected the configured API key."),
            Some("quota_exceeded") => (
                "quota_exceeded",
                "The DeepL translation quota has been exhausted.",
            ),
            Some("rate_limited") => (
                "rate_limited",
                "DeepL is receiving too many requests; please try again shortly.",
            ),
            Some("same_language") => (
                "same_language",
                "Source and target languages must be different.",
            ),
            Some("invalid_request") => (
                "invalid_request",
                "DeepL could not translate that language or text.",
            ),
            _ => ("unavailable", "DeepL isn't answering right now."),
        };
        reply(
            &server,
            destination,
            &themed(key, &[default], &[("user", user)])?,
        )?;
    }
    Ok(())
}

fn parse_command_intent(arguments: &str) -> CommandIntent {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return CommandIntent::Recent;
    }
    if arguments.eq_ignore_ascii_case("help") {
        return CommandIntent::Help;
    }
    if arguments.eq_ignore_ascii_case("languages") {
        return CommandIntent::Languages;
    }
    if let Some((specification, text)) = arguments.split_once(char::is_whitespace) {
        if let Some((source_lang, target_lang)) = parse_language_specification(specification) {
            return CommandIntent::Translate {
                source_lang,
                target_lang,
                text: text.trim().into(),
            };
        }
    }
    CommandIntent::Translate {
        source_lang: None,
        target_lang: "EN-US".into(),
        text: arguments.into(),
    }
}

fn parse_language_specification(value: &str) -> Option<(Option<String>, String)> {
    match value.split_once(':') {
        Some((source, target)) => Some((
            Some(language_code(source, false)?),
            language_code(target, true)?,
        )),
        None => Some((None, language_code(value, true)?)),
    }
}

fn language_code(value: &str, target: bool) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    let code = match value.as_str() {
        "arabic" => "ar",
        "bulgarian" => "bg",
        "chinese" => "zh",
        "czech" => "cs",
        "danish" => "da",
        "dutch" => "nl",
        "english" => "en",
        "estonian" => "et",
        "finnish" => "fi",
        "french" => "fr",
        "german" => "de",
        "greek" => "el",
        "hungarian" => "hu",
        "indonesian" => "id",
        "italian" => "it",
        "japanese" => "ja",
        "korean" => "ko",
        "latvian" => "lv",
        "lithuanian" => "lt",
        "norwegian" | "no" => "nb",
        "polish" => "pl",
        "portuguese" => "pt",
        "romanian" => "ro",
        "russian" => "ru",
        "slovak" => "sk",
        "slovenian" => "sl",
        "spanish" => "es",
        "swedish" => "sv",
        "thai" => "th",
        "turkish" => "tr",
        "ukrainian" => "uk",
        "vietnamese" => "vi",
        _ => value.as_str(),
    };
    const SUPPORTED: &[&str] = &[
        "ar", "bg", "cs", "da", "de", "el", "en", "en-gb", "en-us", "es", "et", "fi", "fr", "hu",
        "id", "it", "ja", "ko", "lt", "lv", "nb", "nl", "pl", "pt", "pt-br", "pt-pt", "ro", "ru",
        "sk", "sl", "sv", "th", "tr", "uk", "vi", "zh", "zh-hans", "zh-hant",
    ];
    if !SUPPORTED.contains(&code) {
        return None;
    }
    if target && code == "en" {
        Some("EN-US".into())
    } else {
        Some(code.to_ascii_uppercase())
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_defaults_to_english() {
        assert_eq!(
            parse_command_intent("bonjour tout le monde"),
            CommandIntent::Translate {
                source_lang: None,
                target_lang: "EN-US".into(),
                text: "bonjour tout le monde".into(),
            }
        );
    }

    #[test]
    fn recognized_target_language_still_works() {
        assert_eq!(
            parse_command_intent("fr hello"),
            CommandIntent::Translate {
                source_lang: None,
                target_lang: "FR".into(),
                text: "hello".into(),
            }
        );
    }

    #[test]
    fn explicit_source_and_target_still_work() {
        assert_eq!(
            parse_command_intent("de:en Guten Morgen"),
            CommandIntent::Translate {
                source_lang: Some("DE".into()),
                target_lang: "EN-US".into(),
                text: "Guten Morgen".into(),
            }
        );
    }

    #[test]
    fn help_and_languages_remain_subcommands() {
        assert_eq!(parse_command_intent("help"), CommandIntent::Help);
        assert_eq!(parse_command_intent("LANGUAGES"), CommandIntent::Languages);
    }

    #[test]
    fn accepts_language_names_and_rejects_unrecognized_codes() {
        assert_eq!(language_code("French", true).as_deref(), Some("FR"));
        assert_eq!(language_code("English", true).as_deref(), Some("EN-US"));
        assert!(language_code("bonjour", true).is_none());
    }

    #[test]
    fn recent_history_retains_only_ten_messages() {
        let mut history = RecentHistory::default();
        for index in 0..12 {
            retain_message(
                &mut history,
                false,
                "user-id",
                "nick",
                "Nick",
                &format!("message {index}"),
                100 + index,
            );
        }
        assert_eq!(history.messages.len(), 10);
        assert_eq!(history.messages[0].text, "message 2");
        assert_eq!(history.messages[9].text, "message 11");
    }

    #[test]
    fn commands_and_private_messages_are_not_retained() {
        let mut history = RecentHistory::default();
        retain_message(
            &mut history,
            false,
            "user-id",
            "nick",
            "Nick",
            "!tr bonjour",
            100,
        );
        retain_message(
            &mut history,
            true,
            "user-id",
            "nick",
            "Nick",
            "a private message",
            100,
        );
        assert!(history.messages.is_empty());
    }

    #[test]
    fn bare_translation_chooses_newest_detected_non_english_message() {
        let history = RecentHistory {
            messages: vec![
                recent(
                    "Alice",
                    "Este mensaje está escrito completamente en español.",
                    100,
                ),
                recent(
                    "Bob",
                    "Das ist eine längere Nachricht in deutscher Sprache.",
                    101,
                ),
                recent(
                    "Carol",
                    "This is the newest message and it is clearly written in English.",
                    102,
                ),
            ],
        };
        assert_eq!(
            select_recent_message(&history).map(|message| message.speaker.as_str()),
            Some("Bob")
        );
    }

    #[test]
    fn bare_translation_falls_back_to_newest_eligible_message() {
        let history = RecentHistory {
            messages: vec![
                recent(
                    "Alice",
                    "This sentence is clearly and entirely written in English.",
                    100,
                ),
                recent(
                    "Bob",
                    "The newest eligible sentence is also written in plain English.",
                    101,
                ),
            ],
        };
        assert_eq!(
            select_recent_message(&history).map(|message| message.speaker.as_str()),
            Some("Bob")
        );
    }

    #[test]
    fn bare_translation_reports_when_no_recent_message_exists() {
        assert_eq!(parse_command_intent(""), CommandIntent::Recent);
        assert!(select_recent_message(&RecentHistory::default()).is_none());
    }

    #[test]
    fn sanitizes_and_limits_text() {
        assert_eq!(sanitize("hello\n\u{0003}04 world"), "hello04 world");
        assert_eq!(sanitize(&"a".repeat(400)).chars().count(), MAX_TEXT_CHARS);
    }

    fn recent(speaker: &str, text: &str, timestamp: i64) -> RecentMessage {
        RecentMessage {
            user_id: format!("{speaker}-id"),
            nick: speaker.into(),
            speaker: speaker.into(),
            text: text.into(),
            timestamp,
        }
    }
}
