//! DeepL-backed `!tr` / `!translate` commands. HTTP and credentials stay in the host.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, SendMessage, StatIncrement, ThemeReq, TranslateQuery,
    TranslateResponse, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION,
};

const COOLDOWN_SECS: i64 = 10;
const MAX_TEXT_CHARS: usize = 350;

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
            usage: "!translate <target> <text>".into(),
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
    if arguments.eq_ignore_ascii_case("help") || arguments.is_empty() {
        reply(
            &server,
            destination,
            &themed(
                "help",
                &["Usage: !tr <target> <text> or !tr <source>:<target> <text>. Example: !tr fr Hello."],
                &[],
            )?,
        )?;
        return Ok(());
    }
    if arguments.eq_ignore_ascii_case("languages") {
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

    let Some((specification, source_text)) = arguments.split_once(char::is_whitespace) else {
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
    };
    let source_text = sanitize(source_text);
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

    let (source_lang, target_lang) = match parse_language_specification(specification) {
        Some(languages) => languages,
        None => {
            reply(
                &server,
                destination,
                &themed(
                    "invalid_language",
                    &["I don't recognize that language. Try !tr languages."],
                    &[],
                )?,
            )?;
            return Ok(());
        }
    };

    let current_time = timestamp()?;
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
        reply(
            &server,
            destination,
            &themed(
                "result",
                &["{source} → {target}: {translation}"],
                &[
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
        "norwegian" => "nb",
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
    if !(2..=10).contains(&code.len())
        || !code
            .bytes()
            .all(|byte| byte.is_ascii_alphabetic() || byte == b'-')
    {
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
    fn parses_auto_detect_and_explicit_source() {
        assert_eq!(
            parse_language_specification("fr"),
            Some((None, "FR".into()))
        );
        assert_eq!(
            parse_language_specification("de:en"),
            Some((Some("DE".into()), "EN-US".into()))
        );
    }

    #[test]
    fn accepts_language_names_and_rejects_bad_values() {
        assert_eq!(language_code("French", true).as_deref(), Some("FR"));
        assert_eq!(language_code("English", true).as_deref(), Some("EN-US"));
        assert!(language_code("not a language", true).is_none());
    }

    #[test]
    fn sanitizes_and_limits_text() {
        assert_eq!(sanitize("hello\n\u{0003}04 world"), "hello04 world");
        assert_eq!(sanitize(&"a".repeat(400)).chars().count(), MAX_TEXT_CHARS);
    }
}
