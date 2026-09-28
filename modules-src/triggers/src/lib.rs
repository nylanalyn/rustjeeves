//! Operator-defined call-and-response triggers.
//!
//! Admins teach Jeeves per-channel responses to words or short phrases:
//! `!trigger add caw|kaw = The murder hears you, {user}.` Each trigger holds a pool of responses
//! (one is picked at random), may be limited to one nick, and has its own cooldown. The old
//! banter rituals ship as presets (`!trigger preset crows`, `!trigger preset sailing <nick>`).
//!
//! Spontaneous output, so channels are off until the `enabled` setting is turned on. Responses
//! are operator-authored configuration, stored per channel; no personal data is kept.

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, MessagePayload, RandomBytesRequest,
    RandomBytesResponse, Role, SendMessage, ServerQuery, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

mod presets;

const MAX_TRIGGERS_PER_CHANNEL: usize = 50;
const MAX_RESPONSES_PER_TRIGGER: usize = 25;
const MAX_RESPONSE_CHARS: usize = 300;
const MAX_WORD_CHARS: usize = 40;
const MAX_WORDS_PER_PHRASE: usize = 4;
const MAX_PHRASES_PER_TRIGGER: usize = 5;
const DEFAULT_COOLDOWN_SECONDS: i64 = 10;
const MAX_COOLDOWN_SECONDS: i64 = 3_600;
const SHOW_PAGE_SIZE: usize = 10;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn bot_nick(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn award_stats(input: String) -> String;
}

// ── state ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Trigger {
    /// Normalized phrases (lowercase words joined by one space); any of them fires the trigger.
    phrases: Vec<String>,
    responses: Vec<String>,
    /// Casefolded nick this trigger answers exclusively, if limited.
    #[serde(default)]
    nick: Option<String>,
    /// The nick as the admin typed it, for display.
    #[serde(default)]
    nick_display: Option<String>,
    cooldown_seconds: i64,
    #[serde(default)]
    last_fired: i64,
}

impl Trigger {
    fn label(&self) -> String {
        self.phrases.join("|")
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    triggers: Vec<Trigger>,
    /// Last response of any trigger in this channel, for the channel-wide spacing setting.
    #[serde(default)]
    last_any: i64,
}

// ── manifests ───────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "trigger".into(),
            aliases: vec!["triggers".into()],
            description: "List or (admins) manage this channel's call-and-response triggers."
                .into(),
            usage: "!trigger [list | show <word> [page] | add <word[|word]> = <response> | \
                    del <word> [n] | nick <word> <nick|any> | cooldown <word> <seconds> | \
                    preset crows | preset sailing <nick>]"
                .into(),
            ..Default::default()
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let scopes = || {
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
                description: "Whether this channel's triggers respond to chat.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "channel_cooldown_seconds".into(),
                description: "Minimum delay between any two trigger responses in one channel."
                    .into(),
                default: "3".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: scopes(),
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "responses".into(),
            description: "Trigger responses prompted".into(),
        }],
        achievements: [
            ("familiar_refrain", "A Familiar Refrain", 1),
            ("call_and_response", "Call and Response", 25),
            ("chorus_master", "Chorus Master", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Prompt {threshold} trigger responses."),
            stat: "responses".into(),
            threshold,
            // Triggers are configured per channel and off by default.
            optional: true,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

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

fn say(
    server: &str,
    target: &str,
    key: &str,
    default: &str,
    vars: &[(&str, &str)],
) -> Result<(), Error> {
    reply(server, target, &themed(key, &[default], vars)?)
}

fn setting(key: &str, server: &str, channel: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    })
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.trim().parse().unwrap_or(0))
}

fn random_index(len: usize) -> Result<usize, Error> {
    if len <= 1 {
        return Ok(0);
    }
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let bytes: [u8; 8] = serde_json::from_str::<RandomBytesResponse>(&raw)?
        .bytes
        .try_into()
        .map_err(|_| Error::msg("random_bytes returned the wrong byte count"))?;
    Ok((u64::from_le_bytes(bytes) % len as u64) as usize)
}

#[cfg(target_arch = "wasm32")]
fn fold(server: &str, nick: &str) -> Result<String, Error> {
    Ok(unsafe {
        irc_casefold(serde_json::to_string(&IrcCasefold {
            server: server.into(),
            value: nick.into(),
        })?)?
    })
}

#[cfg(not(target_arch = "wasm32"))]
fn fold(_server: &str, nick: &str) -> Result<String, Error> {
    Ok(nick.to_ascii_lowercase())
}

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn book_key(server: &str, channel: &str) -> String {
    format!("book:{}:{}", encode(server), encode(channel))
}

fn load_book(server: &str, channel: &str) -> Result<Book, Error> {
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: book_key(server, channel),
        })?)?
    };
    if raw.trim().is_empty() {
        Ok(Book::default())
    } else {
        // Never treat an unreadable book as empty: the next edit would erase it.
        Ok(serde_json::from_str(&raw)?)
    }
}

fn save_book(server: &str, channel: &str, book: &Book) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: book_key(server, channel),
            value: serde_json::to_string(book)?,
        })?)?
    };
    Ok(())
}

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn honorific(msg: &MessagePayload) -> &str {
    if msg.honorific.is_empty() {
        display(msg)
    } else {
        &msg.honorific
    }
}

// ── pure helpers (unit-tested) ──────────────────────────────────────────────

/// Lowercase alphanumeric words of `text`, e.g. "Well... CAW!" → ["well", "caw"].
fn words(text: &str) -> Vec<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Normalize a `word|other phrase` spec into phrases, or explain why it can't be one.
fn parse_phrases(spec: &str) -> Result<Vec<String>, &'static str> {
    let mut phrases = Vec::new();
    for part in spec.split('|') {
        let phrase = words(part).join(" ");
        if phrase.is_empty() {
            return Err("a trigger needs at least one letter or digit");
        }
        if phrase.chars().count() > MAX_WORD_CHARS
            || phrase.split(' ').count() > MAX_WORDS_PER_PHRASE
        {
            return Err("keep each trigger phrase to four short words");
        }
        if !phrases.contains(&phrase) {
            phrases.push(phrase);
        }
    }
    if phrases.len() > MAX_PHRASES_PER_TRIGGER {
        return Err("a trigger may have at most five alternative phrases");
    }
    Ok(phrases)
}

/// Whether `phrase` occurs in `text` on word boundaries.
fn phrase_matches(text_words: &[String], phrase: &str) -> bool {
    let wanted = phrase.split(' ').collect::<Vec<_>>();
    !wanted.is_empty()
        && text_words
            .windows(wanted.len())
            .any(|window| window.iter().zip(&wanted).all(|(have, want)| have == want))
}

/// Bot commands are not conversation.
fn looks_like_command(text: &str) -> bool {
    let mut chars = text.trim_start().chars();
    matches!(chars.next(), Some('!' | '.' | '~' | '@' | '$'))
        && chars.next().is_some_and(char::is_alphanumeric)
}

/// Substitute `{user}`, `{nick}`, `{honorific}`, `{channel}` in one pass; values are never
/// rescanned and unknown placeholders are kept.
fn render(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').and_then(|close| {
            vars.iter()
                .find(|(key, _)| *key == &after[..close])
                .map(|(_, value)| (*value, close))
        }) {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn clean_response(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn find_trigger<'a>(book: &'a mut Book, spec: &str) -> Option<&'a mut Trigger> {
    let phrases = parse_phrases(spec).ok()?;
    book.triggers.iter_mut().find(|trigger| {
        phrases
            .iter()
            .any(|phrase| trigger.phrases.contains(phrase))
    })
}

// ── passive responses ───────────────────────────────────────────────────────

fn respond(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if msg.is_private || looks_like_command(&msg.text) {
        return Ok(());
    }
    if setting("enabled", server, &msg.target)?.trim() != "true" {
        return Ok(());
    }
    let own_nick = unsafe {
        bot_nick(serde_json::to_string(&ServerQuery {
            server: server.into(),
        })?)?
    };
    if msg.nick.eq_ignore_ascii_case(own_nick.trim()) {
        return Ok(());
    }
    let text_words = words(&msg.text);
    if text_words.is_empty() {
        return Ok(());
    }
    let mut book = load_book(server, &msg.target)?;
    let speaker = fold(server, &msg.nick)?;
    // The first trigger that matches this speaker decides; a cooling-down trigger stays quiet
    // rather than letting a less specific one answer instead.
    let Some(index) = book.triggers.iter().position(|trigger| {
        trigger.nick.as_ref().is_none_or(|nick| *nick == speaker)
            && trigger
                .phrases
                .iter()
                .any(|phrase| phrase_matches(&text_words, phrase))
    }) else {
        return Ok(());
    };
    let current = timestamp()?;
    let channel_gap = setting("channel_cooldown_seconds", server, &msg.target)?
        .trim()
        .parse::<i64>()
        .unwrap_or(3)
        .clamp(0, MAX_COOLDOWN_SECONDS);
    let trigger = &book.triggers[index];
    if trigger.responses.is_empty()
        || current.saturating_sub(trigger.last_fired) < trigger.cooldown_seconds
        || current.saturating_sub(book.last_any) < channel_gap
    {
        return Ok(());
    }
    let template = trigger.responses[random_index(trigger.responses.len())?].clone();
    book.triggers[index].last_fired = current;
    book.last_any = current;
    save_book(server, &msg.target, &book)?;
    let text = render(
        &template,
        &[
            ("user", display(msg)),
            ("nick", &msg.nick),
            ("honorific", honorific(msg)),
            ("channel", &msg.target),
        ],
    );
    reply(
        server,
        &msg.target,
        &themed("triggers.response", &["{response}"], &[("response", &text)])?,
    )?;
    if !msg.user_id.is_empty() {
        unsafe {
            award_stats(serde_json::to_string(&AwardStatsRequest {
                server: server.into(),
                profile_id: msg.user_id.clone(),
                display_name: display(msg).into(),
                target: msg.target.clone(),
                increments: vec![StatIncrement {
                    stat: "responses".into(),
                    amount: 1,
                }],
                deduplication_id: None,
            })?)?;
        }
    }
    Ok(())
}

// ── commands ────────────────────────────────────────────────────────────────

fn handle_command(server: &str, msg: &MessagePayload, args: &str) -> Result<(), Error> {
    let user = display(msg);
    if msg.is_private {
        return say(
            server,
            &msg.nick,
            "triggers.channel_only",
            "Triggers belong to a channel; manage them there, {user}.",
            &[("user", user)],
        );
    }
    let channel = msg.target.as_str();
    let (sub, rest) = args
        .split_once(char::is_whitespace)
        .map(|(sub, rest)| (sub, rest.trim()))
        .unwrap_or((args, ""));
    let sub = sub.to_ascii_lowercase();
    match sub.as_str() {
        "" | "list" => return list(server, channel),
        "show" => return show(server, msg, rest),
        "add" | "del" | "delete" | "remove" | "nick" | "cooldown" | "preset" => {}
        _ => {
            return say(
                server,
                channel,
                "triggers.usage",
                "Usage: !trigger list | show <word> | add <word> = <response> | del <word> [n] | nick <word> <nick|any> | cooldown <word> <seconds> | preset crows | preset sailing <nick>",
                &[("user", user)],
            )
        }
    }
    if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
        return say(
            server,
            channel,
            "triggers.denied",
            "Only administrators may teach me new refrains, {user}.",
            &[("user", user)],
        );
    }
    let mut book = load_book(server, channel)?;
    let outcome = match sub.as_str() {
        "add" => add(&mut book, rest),
        "del" | "delete" | "remove" => delete(&mut book, rest),
        "nick" => set_nick(server, &mut book, rest),
        "cooldown" => set_cooldown(&mut book, rest),
        _ => preset(server, &mut book, rest),
    };
    match outcome? {
        Ok((key, default, vars)) => {
            save_book(server, channel, &book)?;
            let mut vars = vars
                .iter()
                .map(|(key, value)| (*key, value.as_str()))
                .collect::<Vec<_>>();
            vars.push(("user", user));
            say(server, channel, key, default, &vars)
        }
        Err(problem) => say(
            server,
            channel,
            "triggers.invalid",
            "I can't do that, {user}: {problem}.",
            &[("user", user), ("problem", problem)],
        ),
    }
}

/// A successful edit: theme key, default text, and variables.
type Edit = (&'static str, &'static str, Vec<(&'static str, String)>);
type EditResult = Result<Result<Edit, &'static str>, Error>;

fn add(book: &mut Book, rest: &str) -> EditResult {
    let Some((spec, response)) = rest.split_once('=') else {
        return Ok(Err("use !trigger add <word> = <response>"));
    };
    let phrases = match parse_phrases(spec) {
        Ok(phrases) => phrases,
        Err(problem) => return Ok(Err(problem)),
    };
    let response = clean_response(response);
    if response.is_empty() {
        return Ok(Err("the response is empty"));
    }
    if response.chars().count() > MAX_RESPONSE_CHARS {
        return Ok(Err("responses are limited to 300 characters"));
    }
    let label = phrases.join("|");
    if let Some(existing) = book.triggers.iter_mut().find(|trigger| {
        phrases
            .iter()
            .any(|phrase| trigger.phrases.contains(phrase))
    }) {
        if existing.responses.len() >= MAX_RESPONSES_PER_TRIGGER {
            return Ok(Err("that trigger already has 25 responses"));
        }
        for phrase in phrases {
            if !existing.phrases.contains(&phrase)
                && existing.phrases.len() < MAX_PHRASES_PER_TRIGGER
            {
                existing.phrases.push(phrase);
            }
        }
        existing.responses.push(response);
        let count = existing.responses.len().to_string();
        return Ok(Ok((
            "triggers.added_response",
            "Response #{count} added to {trigger}, {user}.",
            vec![("trigger", existing.label()), ("count", count)],
        )));
    }
    if book.triggers.len() >= MAX_TRIGGERS_PER_CHANNEL {
        return Ok(Err("this channel already has 50 triggers"));
    }
    book.triggers.push(Trigger {
        phrases,
        responses: vec![response],
        cooldown_seconds: DEFAULT_COOLDOWN_SECONDS,
        ..Default::default()
    });
    Ok(Ok((
        "triggers.added",
        "Very good, {user}. I shall answer {trigger}.",
        vec![("trigger", label)],
    )))
}

fn delete(book: &mut Book, rest: &str) -> EditResult {
    let mut parts = rest.rsplitn(2, char::is_whitespace);
    let last = parts.next().unwrap_or("");
    let (spec, number) = match (last.trim_start_matches('#').parse::<usize>(), parts.next()) {
        (Ok(number), Some(spec)) => (spec, Some(number)),
        _ => (rest, None),
    };
    let Some(position) = parse_phrases(spec).ok().and_then(|phrases| {
        book.triggers.iter().position(|trigger| {
            phrases
                .iter()
                .any(|phrase| trigger.phrases.contains(phrase))
        })
    }) else {
        return Ok(Err("there is no such trigger here"));
    };
    let label = book.triggers[position].label();
    match number {
        None => {
            book.triggers.remove(position);
            Ok(Ok((
                "triggers.deleted",
                "I shall no longer answer {trigger}, {user}.",
                vec![("trigger", label)],
            )))
        }
        Some(number) => {
            let trigger = &mut book.triggers[position];
            if number == 0 || number > trigger.responses.len() {
                return Ok(Err("that response number doesn't exist"));
            }
            trigger.responses.remove(number - 1);
            if trigger.responses.is_empty() {
                book.triggers.remove(position);
            }
            Ok(Ok((
                "triggers.deleted_response",
                "Removed response #{number} from {trigger}, {user}.",
                vec![("trigger", label), ("number", number.to_string())],
            )))
        }
    }
}

fn set_nick(server: &str, book: &mut Book, rest: &str) -> EditResult {
    let Some((spec, nick)) = rest.rsplit_once(char::is_whitespace) else {
        return Ok(Err("use !trigger nick <word> <nick|any>"));
    };
    let folded = if nick.eq_ignore_ascii_case("any") {
        None
    } else {
        Some(fold(server, nick)?)
    };
    let Some(trigger) = find_trigger(book, spec) else {
        return Ok(Err("there is no such trigger here"));
    };
    trigger.nick = folded;
    trigger.nick_display = trigger.nick.as_ref().map(|_| nick.to_string());
    let who = trigger
        .nick_display
        .clone()
        .unwrap_or_else(|| "anyone".into());
    Ok(Ok((
        "triggers.nick_set",
        "{trigger} now answers {who}, {user}.",
        vec![("trigger", trigger.label()), ("who", who)],
    )))
}

fn set_cooldown(book: &mut Book, rest: &str) -> EditResult {
    let Some((spec, seconds)) = rest.rsplit_once(char::is_whitespace) else {
        return Ok(Err("use !trigger cooldown <word> <seconds>"));
    };
    let Ok(seconds) = seconds.trim_end_matches('s').parse::<i64>() else {
        return Ok(Err("the cooldown must be a number of seconds"));
    };
    if !(0..=MAX_COOLDOWN_SECONDS).contains(&seconds) {
        return Ok(Err("cooldowns run from 0 to 3600 seconds"));
    }
    let Some(trigger) = find_trigger(book, spec) else {
        return Ok(Err("there is no such trigger here"));
    };
    trigger.cooldown_seconds = seconds;
    Ok(Ok((
        "triggers.cooldown_set",
        "{trigger} now rests {seconds}s between answers, {user}.",
        vec![
            ("trigger", trigger.label()),
            ("seconds", seconds.to_string()),
        ],
    )))
}

fn preset(server: &str, book: &mut Book, rest: &str) -> EditResult {
    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or("").to_ascii_lowercase();
    let Some(mut trigger) = presets::trigger(&name) else {
        return Ok(Err("the presets are 'crows' and 'sailing <nick>'"));
    };
    if trigger.phrases.iter().any(|phrase| phrase == "sail") {
        let Some(nick) = parts.next() else {
            return Ok(Err(
                "the sailing preset answers one sailor: preset sailing <nick>",
            ));
        };
        trigger.nick = Some(fold(server, nick)?);
        trigger.nick_display = Some(nick.to_string());
    }
    let label = trigger.label();
    // Re-applying a preset refreshes it in place.
    book.triggers
        .retain(|existing| !existing.phrases.iter().any(|p| trigger.phrases.contains(p)));
    if book.triggers.len() >= MAX_TRIGGERS_PER_CHANNEL {
        return Ok(Err("this channel already has 50 triggers"));
    }
    book.triggers.push(trigger);
    Ok(Ok((
        "triggers.preset_installed",
        "The {trigger} refrain is installed, {user}. Remember to enable triggers for this channel.",
        vec![("trigger", label)],
    )))
}

fn list(server: &str, channel: &str) -> Result<(), Error> {
    let book = load_book(server, channel)?;
    let state = if setting("enabled", server, channel)?.trim() == "true" {
        "on"
    } else {
        "off"
    };
    if book.triggers.is_empty() {
        return say(
            server,
            channel,
            "triggers.list_empty",
            "No triggers are set in {channel} (triggers are {state}).",
            &[("channel", channel), ("state", state)],
        );
    }
    let entries = book
        .triggers
        .iter()
        .map(|trigger| match &trigger.nick_display {
            Some(nick) => format!(
                "{} ({}, only {nick})",
                trigger.label(),
                trigger.responses.len()
            ),
            None => format!("{} ({})", trigger.label(), trigger.responses.len()),
        })
        .collect::<Vec<_>>()
        .join(", ");
    say(
        server,
        channel,
        "triggers.list",
        "Triggers in {channel} ({state}): {triggers}",
        &[
            ("channel", channel),
            ("state", state),
            ("triggers", &entries),
        ],
    )
}

/// Send a trigger's numbered responses privately, a page at a time.
fn show(server: &str, msg: &MessagePayload, rest: &str) -> Result<(), Error> {
    let (spec, page) = match rest.rsplit_once(char::is_whitespace) {
        Some((spec, page)) if page.parse::<usize>().is_ok() => {
            (spec, page.parse::<usize>().unwrap_or(1).max(1))
        }
        _ => (rest, 1),
    };
    let mut book = load_book(server, &msg.target)?;
    let Some(trigger) = find_trigger(&mut book, spec) else {
        return say(
            server,
            &msg.target,
            "triggers.unknown",
            "There is no trigger for that in {channel}, {user}.",
            &[("channel", &msg.target), ("user", display(msg))],
        );
    };
    let pages = trigger.responses.len().div_ceil(SHOW_PAGE_SIZE).max(1);
    let page = page.min(pages);
    say(
        server,
        &msg.nick,
        "triggers.show_header",
        "{trigger} in {channel}: {count} response(s), {cooldown}s cooldown, answers {who}. Page {page}/{pages}:",
        &[
            ("trigger", &trigger.label()),
            ("channel", &msg.target),
            ("count", &trigger.responses.len().to_string()),
            ("cooldown", &trigger.cooldown_seconds.to_string()),
            (
                "who",
                trigger.nick_display.as_deref().unwrap_or("anyone"),
            ),
            ("page", &page.to_string()),
            ("pages", &pages.to_string()),
        ],
    )?;
    for (index, response) in trigger
        .responses
        .iter()
        .enumerate()
        .skip((page - 1) * SHOW_PAGE_SIZE)
        .take(SHOW_PAGE_SIZE)
    {
        say(
            server,
            &msg.nick,
            "triggers.show_item",
            "#{number} {response}",
            &[("number", &(index + 1).to_string()), ("response", response)],
        )?;
    }
    Ok(())
}

// ── entry points ────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, args) = text
        .split_once(char::is_whitespace)
        .map(|(command, args)| (command, args.trim()))
        .unwrap_or((text, ""));
    // The host rewrites the `!triggers` alias to the canonical `!trigger`.
    if command.eq_ignore_ascii_case("!trigger") {
        return Ok(handle_command(&env.server, &msg, args)?);
    }
    Ok(respond(&env.server, &msg)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrases_normalize_and_bound() {
        assert_eq!(parse_phrases("Caw|KAW!").unwrap(), ["caw", "kaw"]);
        assert_eq!(parse_phrases("good  Morning").unwrap(), ["good morning"]);
        assert!(parse_phrases("???").is_err());
        assert!(parse_phrases("one two three four five").is_err());
    }

    #[test]
    fn phrases_match_whole_words_only() {
        let text = words("well... a most definite CAW, then");
        assert!(phrase_matches(&text, "caw"));
        assert!(phrase_matches(&text, "definite caw"));
        assert!(!phrase_matches(&words("awkward sailing"), "sail"));
        assert!(!phrase_matches(&words("because"), "caw"));
    }

    #[test]
    fn responses_render_known_placeholders_once() {
        assert_eq!(
            render(
                "Ahoy, {user}! {missing} {nick}",
                &[("user", "{nick}"), ("nick", "rae")]
            ),
            "Ahoy, {nick}! {missing} rae"
        );
    }

    #[test]
    fn commands_are_not_conversation() {
        assert!(looks_like_command("!sail 5"));
        assert!(!looks_like_command("we sail at dawn"));
    }

    #[test]
    fn adding_extends_an_existing_trigger() {
        let mut book = Book::default();
        assert!(add(&mut book, "caw = one").unwrap().is_ok());
        assert!(add(&mut book, "kaw|caw = two").unwrap().is_ok());
        assert_eq!(book.triggers.len(), 1);
        assert_eq!(book.triggers[0].phrases, ["caw", "kaw"]);
        assert_eq!(book.triggers[0].responses, ["one", "two"]);
        assert!(delete(&mut book, "kaw 1").unwrap().is_ok());
        assert_eq!(book.triggers[0].responses, ["two"]);
        assert!(delete(&mut book, "caw").unwrap().is_ok());
        assert!(book.triggers.is_empty());
    }

    #[test]
    fn presets_install_banter_rituals() {
        let mut book = Book::default();
        assert!(preset("net", &mut book, "crows").unwrap().is_ok());
        assert!(preset("net", &mut book, "sailing Witeshark2")
            .unwrap()
            .is_ok());
        assert!(preset("net", &mut book, "sailing").unwrap().is_err());
        assert_eq!(book.triggers.len(), 2);
        let sailing = &book.triggers[1];
        assert_eq!(sailing.nick.as_deref(), Some("witeshark2"));
        assert_eq!(sailing.responses.len(), 20);
        // Re-applying refreshes instead of duplicating.
        assert!(preset("net", &mut book, "crows").unwrap().is_ok());
        assert_eq!(book.triggers.len(), 2);
    }
}
