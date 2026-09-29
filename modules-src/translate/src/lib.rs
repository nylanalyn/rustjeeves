//! DeepL-backed `!tr` / `!translate` commands. HTTP and credentials stay in the host.
//!
//! With the channel's `enabled` setting on, it also auto-translates lines that are confidently
//! not in the channel's target language, within an hourly cap and a daily character budget, and
//! never for people who opted out with `!tr auto off`.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, MessagePayload, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, RecentLine, RecentLinesRequest,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement,
    TranslateQuery, TranslateResponse, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{encode, no_highlight, reply, themed, timestamp};
use serde::{Deserialize, Serialize};
use whatlang::{detect_lang, Lang};

const DEFAULT_COOLDOWN_SECS: i64 = 10;
const DEFAULT_TARGET: &str = "EN-US";
const DEFAULT_AUTO_MIN_WORDS: usize = 4;
const DEFAULT_AUTO_HOURLY_LIMIT: u64 = 60;
const DEFAULT_AUTO_DAILY_CHARS: u64 = 20_000;
/// whatlang confidence a line needs before it is worth spending DeepL characters on.
const AUTO_MIN_CONFIDENCE: f64 = 0.85;
/// Targets offered in the `target_language` setting (DeepL target codes).
const TARGETS: &[&str] = &[
    "EN-US", "EN-GB", "AR", "BG", "CS", "DA", "DE", "EL", "ES", "ET", "FI", "FR", "HU", "ID", "IT",
    "JA", "KO", "LT", "LV", "NB", "NL", "PL", "PT-BR", "PT-PT", "RO", "RU", "SK", "SL", "SV", "TH",
    "TR", "UK", "VI", "ZH",
];
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
    /// `!tr auto` (status), `!tr auto on|off` (personal opt-in/out).
    Auto(Option<bool>),
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
    fn translate(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn recent_lines(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn setting_get(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all = vec![
        SettingScope::Global,
        SettingScope::Network,
        SettingScope::Channel,
    ];
    let spec = |key: &str, description: &str, default: String, kind: SettingKind| SettingSpec {
        key: key.into(),
        description: description.into(),
        default,
        kind,
        scopes: all.clone(),
        applies_immediately: true,
    };
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            spec(
                "enabled",
                "Auto-translate channel lines that are confidently in another language. Commands work either way.",
                "false".into(),
                SettingKind::Boolean,
            ),
            spec(
                "target_language",
                "Default language for !tr and for auto-translation.",
                DEFAULT_TARGET.into(),
                SettingKind::Choice {
                    options: TARGETS.iter().map(|code| code.to_string()).collect(),
                },
            ),
            spec(
                "cooldown_seconds",
                "Minimum delay between one person's !tr commands.",
                DEFAULT_COOLDOWN_SECS.to_string(),
                SettingKind::DurationSeconds { min: 0, max: 300 },
            ),
            spec(
                "auto_min_words",
                "Shortest line, in words, that auto-translation considers.",
                DEFAULT_AUTO_MIN_WORDS.to_string(),
                SettingKind::Integer { min: 2, max: 30 },
            ),
            spec(
                "auto_hourly_limit",
                "Most auto-translations posted per channel per hour.",
                DEFAULT_AUTO_HOURLY_LIMIT.to_string(),
                SettingKind::Integer { min: 1, max: 1_000 },
            ),
            spec(
                "auto_daily_chars",
                "DeepL characters auto-translation may spend per channel per UTC day (0 stops it).",
                DEFAULT_AUTO_DAILY_CHARS.to_string(),
                SettingKind::Integer {
                    min: 0,
                    max: 500_000,
                },
            ),
            spec(
                "auto_skip_languages",
                "Languages never auto-translated here, comma-separated codes or names (e.g. \"fr, spanish\").",
                String::new(),
                SettingKind::String { max_len: 200 },
            ),
        ],
    })?)
}

fn setting(server: &str, channel: Option<&str>, key: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    })
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
            description: "Translate text with DeepL; bare !tr translates a recent line.".into(),
            usage: "!translate [>target|to target|source:target] [text] | !translate auto [on|off]"
                .into(),
            ..Default::default()
        }],
    })?)
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

/// Recent eligible chat in this channel, from the host's in-memory line buffer.
fn fetch_recent(server: &str, channel: &str) -> Result<RecentHistory, Error> {
    let raw = unsafe {
        recent_lines(serde_json::to_string(&RecentLinesRequest {
            server: server.into(),
            channel: channel.into(),
            limit: MAX_RECENT_MESSAGES,
            max_age_seconds: RECENT_MESSAGE_MAX_AGE_SECS,
            user_id: None,
            exclude_commands: true,
        })?)?
    };
    Ok(history_from_recent(serde_json::from_str(&raw)?))
}

fn history_from_recent(lines: Vec<RecentLine>) -> RecentHistory {
    RecentHistory {
        messages: lines
            .into_iter()
            .filter(|line| !line.text.trim().starts_with('!'))
            .filter_map(|line| {
                let text = sanitize(&line.text);
                (!text.is_empty()).then(|| RecentMessage {
                    speaker: if line.display.is_empty() {
                        line.nick.clone()
                    } else {
                        line.display.clone()
                    },
                    user_id: line.user_id,
                    nick: line.nick,
                    text,
                    timestamp: line.timestamp,
                })
            })
            .collect(),
    }
}

thread_local! {
    /// Channels whose legacy KV history this plugin instance has already cleared.
    static LEGACY_CLEARED: std::cell::RefCell<std::collections::BTreeSet<(String, String)>> =
        const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Recent lines used to be copied into module KV on every chat line. Now that the host buffers
/// them, empty each channel's leftover copy once per plugin instance.
fn clear_legacy_history(server: &str, channel: &str) -> Result<(), Error> {
    let key = (server.to_string(), channel.to_string());
    if LEGACY_CLEARED.with(|cleared| cleared.borrow().contains(&key)) {
        return Ok(());
    }
    let stored = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: history_key(server, channel),
        })?)?
    };
    let empty = serde_json::to_string(&RecentHistory::default())?;
    if !stored.is_empty() && stored != empty {
        unsafe {
            kv_set(serde_json::to_string(&KvSet {
                key: history_key(server, channel),
                value: empty,
            })?)?
        };
    }
    LEGACY_CLEARED.with(|cleared| cleared.borrow_mut().insert(key));
    Ok(())
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

fn optout_keys(request: &ModuleDataRequest) -> Vec<String> {
    lifecycle_identities(request)
        .into_iter()
        .map(|identity| optout_key(&request.subject.server, identity))
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
    let optouts = optout_keys(&request);
    let auto_opted_out = request
        .entries
        .iter()
        .any(|entry| optouts.contains(&entry.key) && entry.value == "1");
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
        data: if cooldown_timestamps.is_empty() && recent_messages.is_empty() && !auto_opted_out {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "cooldown_timestamps": cooldown_timestamps,
                "recent_messages": recent_messages,
                "auto_translate_opted_out": auto_opted_out,
            })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let mut keys = lifecycle_keys(&request);
    keys.extend(optout_keys(&request));
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
    if command != "!translate" {
        if !msg.is_private {
            clear_legacy_history(&server, &msg.target)?;
            // Ambient lines only arrive when the channel's `enabled` setting is on.
            auto_translate(&server, &msg)?;
        }
        return Ok(());
    }
    let channel = (!msg.is_private).then_some(msg.target.as_str());
    let default_target = target_language(&server, channel)?;

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
        match parse_command_intent(arguments, &default_target) {
            CommandIntent::Help => {
                reply(
                    &server,
                    destination,
                    &themed(
                        "help",
                        &["Usage: !tr <text>, !tr >fr <text> (or !tr to fr <text>, !tr french <text>), or !tr <source>:<target> <text>. Bare !tr translates a recent message; !tr auto shows auto-translation and !tr auto off opts you out."],
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
            CommandIntent::Auto(choice) => {
                return Ok(handle_auto(&server, &msg, destination, user, choice)?);
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
                let history = fetch_recent(&server, &msg.target)?;
                let selected = select_recent_message(&history).cloned();
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
                    default_target.clone(),
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
    if msg.user_id.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "identity_unavailable",
                &["I can't verify your profile right now, {user}; please try again shortly."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    let key = cooldown_key(&server, &msg.user_id, &msg.nick);
    let (last_used, warned) = get_cooldown(&key)?;
    let window = setting(&server, channel, "cooldown_seconds")?
        .parse()
        .unwrap_or(DEFAULT_COOLDOWN_SECS);
    let remaining = window - current_time.saturating_sub(last_used);
    if current_time > 0 && remaining > 0 && remaining <= window {
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

fn target_language(server: &str, channel: Option<&str>) -> Result<String, Error> {
    let value = setting(server, channel, "target_language")?;
    Ok(if TARGETS.contains(&value.as_str()) {
        value
    } else {
        DEFAULT_TARGET.into()
    })
}

fn optout_key(server: &str, profile_id: &str) -> String {
    format!("optout:{}:{}", encode(server), encode(profile_id))
}

/// Channel counters are not personal data: `{period}:{amount}` per server and channel.
fn counter_key(kind: &str, server: &str, channel: &str) -> String {
    format!(
        "{kind}:{}:{}",
        encode(server),
        encode(&channel.to_lowercase())
    )
}

fn read_counter(key: &str, period: i64) -> Result<u64, Error> {
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    Ok(raw
        .split_once(':')
        .filter(|(stored, _)| stored.parse::<i64>().ok() == Some(period))
        .and_then(|(_, amount)| amount.parse().ok())
        .unwrap_or(0))
}

fn write_counter(key: &str, period: i64, amount: u64) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: format!("{period}:{amount}"),
        })?)?
    };
    Ok(())
}

fn opted_out(server: &str, profile_id: &str) -> Result<bool, Error> {
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: optout_key(server, profile_id),
        })?)?
    };
    Ok(raw == "1")
}

fn handle_auto(
    server: &str,
    msg: &MessagePayload,
    destination: &str,
    user: &str,
    choice: Option<bool>,
) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return reply(
            server,
            destination,
            &themed(
                "identity_unavailable",
                &["I can't verify your profile right now, {user}; please try again shortly."],
                &[("user", user)],
            )?,
        );
    }
    if let Some(included) = choice {
        unsafe {
            kv_set(serde_json::to_string(&KvSet {
                key: optout_key(server, &msg.user_id),
                value: if included { String::new() } else { "1".into() },
            })?)?
        };
        let (key, default) = if included {
            (
                "translate.auto_opted_in",
                "Very good, {user}; your lines may be auto-translated where a channel allows it.",
            )
        } else {
            (
                "translate.auto_opted_out",
                "Very good, {user}; I won't auto-translate your lines.",
            )
        };
        return reply(
            server,
            destination,
            &themed(key, &[default], &[("user", user)])?,
        );
    }
    let personal = if opted_out(server, &msg.user_id)? {
        "opted out"
    } else {
        "included"
    };
    if msg.is_private {
        return reply(
            server,
            destination,
            &themed(
                "translate.auto_status_private",
                &["Your lines are {personal} in auto-translation, {user}. !tr auto on|off changes that."],
                &[("personal", personal), ("user", user)],
            )?,
        );
    }
    let channel = Some(msg.target.as_str());
    let enabled = setting(server, channel, "enabled")? == "true";
    let budget = setting(server, channel, "auto_daily_chars")?
        .parse()
        .unwrap_or(DEFAULT_AUTO_DAILY_CHARS);
    let day = timestamp()?.div_euclid(86_400);
    let used = read_counter(&counter_key("chars", server, &msg.target), day)?;
    reply(
        server,
        destination,
        &themed(
            "translate.auto_status",
            &["Auto-translation is {state} here (to {target}, {used}/{budget} characters used today); your lines are {personal}, {user}. !tr auto on|off changes that."],
            &[
                ("state", if enabled { "on" } else { "off" }),
                ("target", &target_language(server, channel)?),
                ("used", &used.to_string()),
                ("budget", &budget.to_string()),
                ("personal", personal),
                ("user", user),
            ],
        )?,
    )
}

/// Post a translation of a channel line when it is confidently in another language and every
/// safeguard allows it. Every failure is silent: nobody asked for this reply.
fn auto_translate(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Ok(());
    }
    let Some(text) = auto_candidate(&msg.text) else {
        return Ok(());
    };
    let channel = Some(msg.target.as_str());
    let min_words = setting(server, channel, "auto_min_words")?
        .parse()
        .unwrap_or(DEFAULT_AUTO_MIN_WORDS);
    let target = target_language(server, channel)?;
    let Some(source) = confident_source(&text, min_words, &target) else {
        return Ok(());
    };
    let skipped = setting(server, channel, "auto_skip_languages")?;
    if skipped
        .split(',')
        .filter_map(|name| language_code(name, false))
        .any(|code| language_base(&code) == source)
    {
        return Ok(());
    }
    if opted_out(server, &msg.user_id)? {
        return Ok(());
    }
    let now = timestamp()?;
    let (hour, day) = (now.div_euclid(3_600), now.div_euclid(86_400));
    let hourly_key = counter_key("hourly", server, &msg.target);
    let chars_key = counter_key("chars", server, &msg.target);
    let hourly_limit = setting(server, channel, "auto_hourly_limit")?
        .parse()
        .unwrap_or(DEFAULT_AUTO_HOURLY_LIMIT);
    let budget = setting(server, channel, "auto_daily_chars")?
        .parse()
        .unwrap_or(DEFAULT_AUTO_DAILY_CHARS);
    let posted = read_counter(&hourly_key, hour)?;
    let spent = read_counter(&chars_key, day)?;
    let cost = text.chars().count() as u64;
    if posted >= hourly_limit || spent + cost > budget {
        return Ok(());
    }
    // DeepL bills the characters sent, so charge the budget before asking.
    write_counter(&chars_key, day, spent + cost)?;
    let raw = unsafe {
        translate(serde_json::to_string(&TranslateQuery {
            text: text.clone(),
            target_lang: target.clone(),
            source_lang: None,
        })?)?
    };
    let response: TranslateResponse = serde_json::from_str(&raw)?;
    let Some(translated) = response.text.map(|text| sanitize(&text)) else {
        return Ok(());
    };
    let detected = language_base(
        response
            .detected_source_language
            .as_deref()
            .unwrap_or(source),
    );
    // DeepL decided it was the target language after all, or there was nothing to change.
    if detected == language_base(&target) || translated.eq_ignore_ascii_case(&text) {
        return Ok(());
    }
    write_counter(&hourly_key, hour, posted + 1)?;
    let speaker = if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    };
    reply(
        server,
        &msg.target,
        &themed(
            "translate.auto",
            &["↪ {speaker} ({source}): {translation}"],
            &[
                ("speaker", &no_highlight(speaker)),
                ("source", &detected),
                ("target", &target),
                ("translation", &translated),
            ],
        )?,
    )
}

/// The translatable part of a channel line: no commands or CTCP, `/me` unwrapped, URLs and a
/// leading "nick:" address removed.
fn auto_candidate(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = match raw.strip_prefix("\u{1}ACTION ") {
        Some(action) => action.trim_end_matches('\u{1}'),
        None if raw.starts_with('\u{1}') => return None,
        None => raw,
    };
    if raw.starts_with('!') {
        return None;
    }
    let mut words = raw
        .split_whitespace()
        .filter(|word| {
            let lower = word.to_ascii_lowercase();
            !(lower.starts_with("http://")
                || lower.starts_with("https://")
                || lower.starts_with("www."))
        })
        .collect::<Vec<_>>();
    if words.first().is_some_and(|first| {
        first.len() > 1
            && (first.ends_with(':') || first.ends_with(','))
            && first.chars().filter(|ch| ch.is_alphabetic()).count() + 1 >= first.chars().count()
    }) && words.len() > 1
    {
        words.remove(0);
    }
    let text = sanitize(&words.join(" "));
    (!text.is_empty()).then_some(text)
}

/// Unspaced scripts (Chinese, Japanese, Thai) count roughly two characters per word.
fn word_count(text: &str) -> usize {
    let unspaced = text
        .chars()
        .filter(|ch| {
            matches!(*ch as u32,
                0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0x0E00..=0x0E7F | 0xF900..=0xFAFF)
        })
        .count();
    let spaced = text
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphabetic))
        .count();
    spaced.max(unspaced / 2)
}

/// The DeepL source code for a line whatlang is sure about, unless it's the target language.
fn confident_source(text: &str, min_words: usize, target: &str) -> Option<&'static str> {
    if word_count(text) < min_words {
        return None;
    }
    let info = whatlang::detect(text)?;
    if !info.is_reliable() || info.confidence() < AUTO_MIN_CONFIDENCE {
        return None;
    }
    let code = deepl_source(info.lang())?;
    (language_base(target) != code).then_some(code)
}

fn deepl_source(lang: Lang) -> Option<&'static str> {
    Some(match lang {
        Lang::Eng => "en",
        Lang::Fra => "fr",
        Lang::Deu => "de",
        Lang::Spa => "es",
        Lang::Ita => "it",
        Lang::Por => "pt",
        Lang::Nld => "nl",
        Lang::Pol => "pl",
        Lang::Rus => "ru",
        Lang::Jpn => "ja",
        Lang::Kor => "ko",
        Lang::Cmn => "zh",
        Lang::Ukr => "uk",
        Lang::Tur => "tr",
        Lang::Swe => "sv",
        Lang::Dan => "da",
        Lang::Fin => "fi",
        Lang::Ces => "cs",
        Lang::Slk => "sk",
        Lang::Slv => "sl",
        Lang::Hun => "hu",
        Lang::Ron => "ro",
        Lang::Bul => "bg",
        Lang::Ell => "el",
        Lang::Est => "et",
        Lang::Lav => "lv",
        Lang::Lit => "lt",
        Lang::Ind => "id",
        Lang::Nob => "nb",
        Lang::Ara => "ar",
        Lang::Tha => "th",
        Lang::Vie => "vi",
        _ => return None,
    })
}

/// "EN-US" → "en", "pt-br" → "pt".
fn language_base(code: &str) -> String {
    code.split('-')
        .next()
        .unwrap_or(code)
        .trim()
        .to_ascii_lowercase()
}

fn parse_command_intent(arguments: &str, default_target: &str) -> CommandIntent {
    let arguments = arguments.trim();
    if arguments.is_empty() {
        return CommandIntent::Recent;
    }
    match arguments.to_ascii_lowercase().as_str() {
        "auto" => return CommandIntent::Auto(None),
        "auto on" => return CommandIntent::Auto(Some(true)),
        "auto off" => return CommandIntent::Auto(Some(false)),
        _ => {}
    }
    if arguments.eq_ignore_ascii_case("help") {
        return CommandIntent::Help;
    }
    if arguments.eq_ignore_ascii_case("languages") {
        return CommandIntent::Languages;
    }
    if let Some((first, rest)) = arguments.split_once(char::is_whitespace) {
        // Explicit target forms always win: `>it text` and `to it text`.
        let explicit = if let Some(code) = first.strip_prefix('>') {
            Some((code, rest))
        } else if first.eq_ignore_ascii_case("to") {
            rest.trim_start().split_once(char::is_whitespace)
        } else {
            None
        };
        if let Some((specification, text)) = explicit {
            if let Some((source_lang, target_lang)) = parse_language_specification(specification) {
                return CommandIntent::Translate {
                    source_lang,
                    target_lang,
                    text: text.trim().into(),
                };
            }
        }
        // A bare code that is also an everyday word (`it is raining`, `de nada`) is text, not a
        // target language; the explicit forms above cover those languages.
        if !is_ambiguous_bare_code(first) {
            if let Some((source_lang, target_lang)) = parse_language_specification(first) {
                return CommandIntent::Translate {
                    source_lang,
                    target_lang,
                    text: rest.trim().into(),
                };
            }
        }
    }
    CommandIntent::Translate {
        source_lang: None,
        target_lang: default_target.into(),
        text: arguments.into(),
    }
}

/// Two-letter codes that are also common words in some language. Bare, they are treated as the
/// start of the text; `>it`, `to it`, `italian`, or `src:it` still select them explicitly.
fn is_ambiguous_bare_code(value: &str) -> bool {
    !value.contains(':')
        && matches!(
            value.to_ascii_lowercase().as_str(),
            "it" | "no" | "de" | "es" | "en" | "el" | "da" | "et" | "id" | "ja" | "vi" | "uk"
        )
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
            parse_command_intent("bonjour tout le monde", "EN-US"),
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
            parse_command_intent("fr hello", "EN-US"),
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
            parse_command_intent("de:en Guten Morgen", "EN-US"),
            CommandIntent::Translate {
                source_lang: Some("DE".into()),
                target_lang: "EN-US".into(),
                text: "Guten Morgen".into(),
            }
        );
    }

    #[test]
    fn everyday_words_are_not_eaten_as_language_codes() {
        for (input, text) in [
            ("it is raining", "it is raining"),
            ("de nada", "de nada"),
            ("no quiero", "no quiero"),
            ("es verdad", "es verdad"),
        ] {
            assert_eq!(
                parse_command_intent(input, "EN-US"),
                CommandIntent::Translate {
                    source_lang: None,
                    target_lang: "EN-US".into(),
                    text: text.into(),
                }
            );
        }
    }

    #[test]
    fn ambiguous_languages_remain_reachable_explicitly() {
        let italian = CommandIntent::Translate {
            source_lang: None,
            target_lang: "IT".into(),
            text: "good morning".into(),
        };
        assert_eq!(parse_command_intent(">it good morning", "EN-US"), italian);
        assert_eq!(parse_command_intent("to it good morning", "EN-US"), italian);
        assert_eq!(
            parse_command_intent("italian good morning", "EN-US"),
            italian
        );
        assert_eq!(
            parse_command_intent("en:de good morning", "EN-US"),
            CommandIntent::Translate {
                source_lang: Some("EN".into()),
                target_lang: "DE".into(),
                text: "good morning".into(),
            }
        );
    }

    #[test]
    fn help_and_languages_remain_subcommands() {
        assert_eq!(parse_command_intent("help", "EN-US"), CommandIntent::Help);
        assert_eq!(
            parse_command_intent("LANGUAGES", "EN-US"),
            CommandIntent::Languages
        );
    }

    #[test]
    fn accepts_language_names_and_rejects_unrecognized_codes() {
        assert_eq!(language_code("French", true).as_deref(), Some("FR"));
        assert_eq!(language_code("English", true).as_deref(), Some("EN-US"));
        assert!(language_code("bonjour", true).is_none());
    }

    #[test]
    fn buffered_commands_and_blank_lines_are_not_translatable() {
        let line = |text: &str, timestamp| RecentLine {
            user_id: "id".into(),
            nick: "nick".into(),
            display: "Sir Nick".into(),
            text: text.into(),
            timestamp,
            is_command: false,
        };
        let history = history_from_recent(vec![
            line("!unknowncommand bonjour", 1),
            line("   ", 2),
            line("bonjour tout le monde", 3),
        ]);
        assert_eq!(history.messages.len(), 1);
        assert_eq!(history.messages[0].speaker, "Sir Nick");
        assert_eq!(history.messages[0].text, "bonjour tout le monde");
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
        assert_eq!(parse_command_intent("", "EN-US"), CommandIntent::Recent);
        assert!(select_recent_message(&RecentHistory::default()).is_none());
    }

    #[test]
    fn sanitizes_and_limits_text() {
        assert_eq!(sanitize("hello\n\u{0003}04 world"), "hello04 world");
        assert_eq!(sanitize(&"a".repeat(400)).chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn auto_subcommands_parse() {
        assert_eq!(
            parse_command_intent("auto", "EN-US"),
            CommandIntent::Auto(None)
        );
        assert_eq!(
            parse_command_intent("AUTO off", "EN-US"),
            CommandIntent::Auto(Some(false))
        );
        assert_eq!(
            parse_command_intent("auto on", "EN-US"),
            CommandIntent::Auto(Some(true))
        );
        // A configured default target applies to plain text.
        assert!(matches!(
            parse_command_intent("bonjour tout le monde", "EN-GB"),
            CommandIntent::Translate { target_lang, .. } if target_lang == "EN-GB"
        ));
    }

    #[test]
    fn auto_candidates_drop_commands_urls_and_addresses() {
        assert_eq!(auto_candidate("!weather paris"), None);
        assert_eq!(auto_candidate("\u{1}VERSION\u{1}"), None);
        assert_eq!(
            auto_candidate("\u{1}ACTION salue tout le monde\u{1}").as_deref(),
            Some("salue tout le monde")
        );
        assert_eq!(
            auto_candidate("alice: regarde https://example.com/x ça").as_deref(),
            Some("regarde ça")
        );
        assert_eq!(auto_candidate("https://example.com"), None);
    }

    #[test]
    fn only_confident_foreign_lines_qualify() {
        let french = "Je pense que nous devrions partir demain matin avant la pluie.";
        let german = "Ich glaube, wir sollten morgen früh losfahren, bevor es regnet.";
        let english = "I think we should leave tomorrow morning before the rain starts.";
        let japanese = "明日の朝、雨が降る前に出発したほうがいいと思います。";
        assert_eq!(confident_source(french, 4, "EN-US"), Some("fr"));
        assert_eq!(confident_source(german, 4, "EN-US"), Some("de"));
        assert_eq!(confident_source(japanese, 4, "EN-US"), Some("ja"));
        assert_eq!(confident_source(english, 4, "EN-US"), None);
        assert_eq!(confident_source(english, 4, "DE"), Some("en"));
        assert_eq!(confident_source(german, 4, "DE"), None);
        assert_eq!(confident_source("oui merci", 4, "EN-US"), None, "too short");
        for chatter in [
            "lol ok brb",
            "haha nice one mate",
            "gg wp everyone",
            "ok so what now",
        ] {
            assert_eq!(confident_source(chatter, 4, "EN-US"), None, "{chatter}");
        }
    }

    #[test]
    fn helpers_normalise_codes_and_avoid_highlights() {
        assert_eq!(language_base("EN-US"), "en");
        assert_eq!(language_base("pt-br"), "pt");
        assert_eq!(no_highlight("Alice"), "A\u{200B}lice");
        assert_eq!(word_count("明日の朝雨が降る前に"), 5);
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
