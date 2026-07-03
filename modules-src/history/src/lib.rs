//! Channel-local `!seen` and `!quote` commands. Private messages are never recorded.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, Role, SendMessage, SettingGet,
    SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};

const MAX_TEXT_CHARS: usize = 350;
const MAX_PATTERN_CHARS: usize = 100;
const MAX_REPLACEMENT_CHARS: usize = 200;
const SED_COOLDOWN_SECONDS: i64 = 5;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let achievements = [
        ("saw_earlier", "I Saw Them Earlier", "seen", 1),
        ("quotable_material", "Quotable Material", "quotes", 1),
        (
            "small_correction",
            "Just a Small Correction",
            "corrections",
            1,
        ),
        ("archivist", "Archivist", "actions", 25),
        ("keeper_record", "Keeper of the Record", "actions", 100),
    ]
    .into_iter()
    .map(|(id, name, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: match stat {
            "seen" => "Complete a successful seen lookup.".into(),
            "quotes" => "Submit a quote successfully.".into(),
            "corrections" => "Apply a successful sed correction.".into(),
            _ => format!("Complete {threshold} successful history actions."),
        },
        stat: stat.into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect();
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: ["seen", "quotes", "corrections", "actions"]
            .into_iter()
            .map(|id| AchievementStat {
                id: id.into(),
                description: id.into(),
            })
            .collect(),
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(server: &str, msg: &jeeves_abi::MessagePayload, stat: &str) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Ok(());
    }
    let display = if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    };
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display.clone(),
            target: msg.target.clone(),
            increments: vec![
                StatIncrement {
                    stat: stat.into(),
                    amount: 1,
                },
                StatIncrement {
                    stat: "actions".into(),
                    amount: 1,
                },
            ],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "sed_corrections".into(),
            description:
                "Whether s/pattern/replacement/ corrections are processed in this channel.".into(),
            default: "true".into(),
            kind: SettingKind::Boolean,
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }],
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            command("seen", "Show when a user last spoke here.", "!seen <nick>"),
            command("quote", "Manage channel quotes.", "!quote [nick|text|#id]"),
        ],
    })?)
}

#[derive(Clone, Serialize, Deserialize)]
struct SeenRecord {
    user_id: String,
    nick: String,
    display: String,
    text: String,
    timestamp: i64,
}

#[derive(Clone, Serialize, Deserialize)]
struct Quote {
    id: u64,
    author_id: String,
    author: String,
    text: String,
    timestamp: i64,
    submitted_by: String,
}

#[derive(Default, Serialize, Deserialize)]
struct QuoteBook {
    next_id: u64,
    quotes: Vec<Quote>,
}

fn lifecycle_identities(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.clone())
        .chain(request.aliases.iter().cloned())
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let identities = lifecycle_identities(&request);
    let server_hex = encode(&request.subject.server);
    let seen_prefix = format!("seen:{server_hex}:");
    let last_prefix = format!("last:{server_hex}:");
    let mut records = Vec::new();
    let mut quotes = Vec::new();
    for entry in &request.entries {
        if entry.key.starts_with(&seen_prefix) || entry.key.starts_with(&last_prefix) {
            let record: SeenRecord = serde_json::from_str(&entry.value)?;
            if identities
                .iter()
                .any(|identity| identity.eq_ignore_ascii_case(&record.user_id))
            {
                records.push(serde_json::json!({ "key": entry.key, "record": record }));
            }
        } else if entry.key.starts_with(&format!("quotes:{server_hex}:")) {
            let book: QuoteBook = serde_json::from_str(&entry.value)?;
            let matches = book
                .quotes
                .into_iter()
                .filter(|quote| {
                    identities.iter().any(|identity| {
                        identity.eq_ignore_ascii_case(&quote.author_id)
                            || identity.eq_ignore_ascii_case(&quote.submitted_by)
                    })
                })
                .collect::<Vec<_>>();
            if !matches.is_empty() {
                quotes.push(serde_json::json!({ "key": entry.key, "quotes": matches }));
            }
        }
    }
    let data = if records.is_empty() && quotes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "seen_records": records, "quotes": quotes })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let identities = lifecycle_identities(&request);
    let server_hex = encode(&request.subject.server);
    let seen_prefix = format!("seen:{server_hex}:");
    let last_prefix = format!("last:{server_hex}:");
    let encoded_ids = identities
        .iter()
        .map(|identity| encode(identity))
        .collect::<Vec<_>>();
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if entry.key.starts_with(&seen_prefix) || entry.key.starts_with(&last_prefix) {
            let record: SeenRecord = serde_json::from_str(&entry.value)?;
            let matches = identities
                .iter()
                .any(|identity| identity.eq_ignore_ascii_case(&record.user_id));
            if matches {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: None,
                });
            }
        } else if entry
            .key
            .starts_with(&format!("sed-cooldown:{server_hex}:"))
            && encoded_ids
                .iter()
                .any(|identity| entry.key.ends_with(&format!(":{identity}")))
        {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            });
        } else if entry.key.starts_with(&format!("quotes:{server_hex}:")) {
            let mut book: QuoteBook = serde_json::from_str(&entry.value)?;
            let before = book.quotes.len();
            book.quotes.retain(|quote| {
                !identities.iter().any(|identity| {
                    identity.eq_ignore_ascii_case(&quote.author_id)
                        || identity.eq_ignore_ascii_case(&quote.submitted_by)
                })
            });
            if book.quotes.len() != before {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: Some(serde_json::to_string(&book)?),
                });
            }
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

#[derive(Debug, PartialEq, Eq)]
struct Correction {
    pattern: String,
    replacement: String,
    global: bool,
    case_insensitive: bool,
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

fn sed_corrections_enabled(server: &str, channel: &str) -> Result<bool, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "sed_corrections".into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    Ok(raw != "false")
}

fn kv_read(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_write(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?
    };
    Ok(())
}

fn scoped_key(kind: &str, server: &str, channel: &str, id: &str) -> String {
    format!(
        "{kind}:{}:{}:{}",
        encode(server),
        encode(channel),
        encode(id)
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

fn profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn load_seen(
    kind: &str,
    server: &str,
    channel: &str,
    user_id: &str,
) -> Result<Option<SeenRecord>, Error> {
    let raw = kv_read(&scoped_key(kind, server, channel, user_id))?;
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn save_seen(kind: &str, server: &str, channel: &str, record: &SeenRecord) -> Result<(), Error> {
    kv_write(
        &scoped_key(kind, server, channel, &record.user_id),
        &serde_json::to_string(record)?,
    )
}

fn quote_key(server: &str, channel: &str) -> String {
    scoped_key("quotes", server, channel, "book")
}

fn load_quotes(server: &str, channel: &str) -> Result<QuoteBook, Error> {
    let raw = kv_read(&quote_key(server, channel))?;
    if raw.is_empty() {
        Ok(QuoteBook {
            next_id: 1,
            quotes: Vec::new(),
        })
    } else {
        let mut book: QuoteBook = serde_json::from_str(&raw)?;
        if book.next_id == 0 {
            book.next_id = book.quotes.iter().map(|quote| quote.id).max().unwrap_or(0) + 1;
        }
        Ok(book)
    }
}

fn save_quotes(server: &str, channel: &str, book: &QuoteBook) -> Result<(), Error> {
    kv_write(&quote_key(server, channel), &serde_json::to_string(book)?)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let command = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if msg.is_private {
        if matches!(command.as_str(), "!seen" | "!quote") || text.starts_with("s/") {
            reply(
                &server,
                &msg.nick,
                &themed("channel_only", &["Seen, quotes, and corrections only work in channels; private messages are never recorded."], &[])?,
            )?;
        }
        return Ok(());
    }

    let now = timestamp()?;
    if text.starts_with("s/") {
        if sed_corrections_enabled(&server, &msg.target)? {
            handle_correction(&server, &msg, text, now)?;
        }
        let record = SeenRecord {
            user_id: stable_id(&msg.user_id, &msg.nick),
            nick: msg.nick.clone(),
            display: if msg.display.is_empty() {
                msg.nick.clone()
            } else {
                msg.display.clone()
            },
            text: sanitize(text),
            timestamp: now,
        };
        save_seen("seen", &server, &msg.target, &record)?;
        return Ok(());
    }
    match command.as_str() {
        "!seen" => handle_seen(&server, &msg, text, now)?,
        "!quote" => handle_quote(&server, &msg, text, now)?,
        _ => {}
    }

    let record = SeenRecord {
        user_id: if msg.user_id.is_empty() {
            format!("nick:{}", msg.nick.to_ascii_lowercase())
        } else {
            msg.user_id.clone()
        },
        nick: msg.nick.clone(),
        display: if msg.display.is_empty() {
            msg.nick.clone()
        } else {
            msg.display.clone()
        },
        text: sanitize(text),
        timestamp: now,
    };
    save_seen("seen", &server, &msg.target, &record)?;
    if !text.starts_with('!') && !record.text.is_empty() {
        save_seen("last", &server, &msg.target, &record)?;
    }
    Ok(())
}

fn handle_seen(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    text: &str,
    now: i64,
) -> Result<(), Error> {
    let channel = msg.target.as_str();
    let nick = text
        .split_once(char::is_whitespace)
        .map(|(_, argument)| argument)
        .unwrap_or("")
        .trim();
    if nick.is_empty() {
        return reply(
            server,
            channel,
            &themed("seen_usage", &["Usage: !seen <nick>"], &[])?,
        );
    }
    let Some(target) = profile(server, nick)? else {
        return reply(
            server,
            channel,
            &themed(
                "seen_unknown",
                &["I haven't seen {target} in this channel."],
                &[("target", nick)],
            )?,
        );
    };
    let Some(record) = load_seen("seen", server, channel, &target.id)? else {
        return reply(
            server,
            channel,
            &themed(
                "seen_unknown",
                &["I haven't seen {target} in this channel."],
                &[("target", nick)],
            )?,
        );
    };
    let ago = relative_time(now.saturating_sub(record.timestamp));
    reply(
        server,
        channel,
        &themed(
            "seen_result",
            &["{target} was last seen {ago}, saying: {text}"],
            &[
                ("target", &record.display),
                ("ago", &ago),
                ("text", &record.text),
            ],
        )?,
    )?;
    award(server, msg, "seen")
}

fn handle_quote(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    text: &str,
    now: i64,
) -> Result<(), Error> {
    let channel = msg.target.as_str();
    let arg = text
        .split_once(char::is_whitespace)
        .map(|(_, argument)| argument)
        .unwrap_or("")
        .trim();
    if arg.is_empty() {
        let book = load_quotes(server, channel)?;
        if book.quotes.is_empty() {
            return reply(
                server,
                channel,
                &themed(
                    "quote_empty",
                    &["There are no quotes in this channel yet."],
                    &[],
                )?,
            );
        }
        let index = (now.max(0) as usize) % book.quotes.len();
        return show_quote(server, channel, &book.quotes[index]);
    }
    if let Some(id) = parse_quote_id(arg) {
        let book = load_quotes(server, channel)?;
        return match book.quotes.iter().find(|quote| quote.id == id) {
            Some(quote) => show_quote(server, channel, quote),
            None => quote_not_found(server, channel, id),
        };
    }
    if let Some(rest) = arg
        .strip_prefix("del ")
        .or_else(|| arg.strip_prefix("delete "))
    {
        let Some(id) = parse_quote_id(rest.trim()) else {
            return reply(
                server,
                channel,
                &themed("quote_delete_usage", &["Usage: !quote del #<id>"], &[])?,
            );
        };
        let mut book = load_quotes(server, channel)?;
        let Some(index) = book.quotes.iter().position(|quote| quote.id == id) else {
            return quote_not_found(server, channel, id);
        };
        let quote = &book.quotes[index];
        let requester = stable_id(&msg.user_id, &msg.nick);
        let admin = msg.role.is_some_and(|role| role.satisfies(Role::Admin));
        if !admin && quote.submitted_by != requester && quote.author_id != requester {
            return reply(
                server,
                channel,
                &themed(
                    "quote_delete_denied",
                    &["Only the quoted person, submitter, or an admin may delete that quote."],
                    &[],
                )?,
            );
        }
        book.quotes.remove(index);
        save_quotes(server, channel, &book)?;
        let id_text = id.to_string();
        return reply(
            server,
            channel,
            &themed(
                "quote_deleted",
                &["Deleted quote #{id}."],
                &[("id", &id_text)],
            )?,
        );
    }

    let (author_id, author, quoted_text) = if let Some(manual) = parse_manual_quote(arg) {
        let id = stable_id(&msg.user_id, &msg.nick);
        let author = if msg.display.is_empty() {
            msg.nick.clone()
        } else {
            msg.display.clone()
        };
        (id, author, sanitize(manual))
    } else {
        let Some(target) = profile(server, arg)? else {
            return reply(
                server,
                channel,
                &themed(
                    "quote_unknown",
                    &["I don't know anyone named {target}."],
                    &[("target", arg)],
                )?,
            );
        };
        let Some(last) = load_seen("last", server, channel, &target.id)? else {
            return reply(
                server,
                channel,
                &themed(
                    "quote_no_line",
                    &["I don't have a quotable line from {target} in this channel."],
                    &[("target", arg)],
                )?,
            );
        };
        (last.user_id, last.display, last.text)
    };
    if quoted_text.is_empty() {
        return reply(
            server,
            channel,
            &themed("quote_empty_text", &["That quote is empty."], &[])?,
        );
    }

    let mut book = load_quotes(server, channel)?;
    let id = book.next_id.max(1);
    book.next_id = id.saturating_add(1);
    book.quotes.push(Quote {
        id,
        author_id,
        author: author.clone(),
        text: quoted_text.clone(),
        timestamp: now,
        submitted_by: stable_id(&msg.user_id, &msg.nick),
    });
    save_quotes(server, channel, &book)?;
    let id_text = id.to_string();
    reply(
        server,
        channel,
        &themed(
            "quote_saved",
            &["Saved quote #{id} from {author}: {text}"],
            &[
                ("id", &id_text),
                ("author", &author),
                ("text", &quoted_text),
            ],
        )?,
    )?;
    award(server, msg, "quotes")
}

fn show_quote(server: &str, channel: &str, quote: &Quote) -> Result<(), Error> {
    let id = quote.id.to_string();
    reply(
        server,
        channel,
        &themed(
            "quote_result",
            &["#{id} <{author}> {text}"],
            &[
                ("id", &id),
                ("author", &quote.author),
                ("text", &quote.text),
            ],
        )?,
    )
}

fn quote_not_found(server: &str, channel: &str, id: u64) -> Result<(), Error> {
    let id = id.to_string();
    reply(
        server,
        channel,
        &themed(
            "quote_not_found",
            &["There is no quote #{id} in this channel."],
            &[("id", &id)],
        )?,
    )
}

fn handle_correction(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    text: &str,
    now: i64,
) -> Result<(), Error> {
    let correction = match parse_correction(text) {
        Ok(correction) => correction,
        Err(reason) => {
            return reply(
                server,
                &msg.target,
                &themed(
                    "sed_invalid",
                    &["I couldn't parse that correction ({reason}). Use s/pattern/replacement/ with optional g or i flags."],
                    &[("reason", reason)],
                )?,
            )
        }
    };
    let user_id = stable_id(&msg.user_id, &msg.nick);
    let Some(mut previous) = load_seen("last", server, &msg.target, &user_id)? else {
        return reply(
            server,
            &msg.target,
            &themed(
                "sed_no_history",
                &["I don't have an earlier line from you to correct, {user}."],
                &[("user", display_name(msg))],
            )?,
        );
    };

    let cooldown_key = scoped_key("sed-cooldown", server, &msg.target, &user_id);
    let last_used = kv_read(&cooldown_key)?.parse::<i64>().unwrap_or(0);
    let elapsed = now.saturating_sub(last_used);
    if last_used > 0 && elapsed < SED_COOLDOWN_SECONDS {
        let wait = (SED_COOLDOWN_SECONDS - elapsed).to_string();
        return reply(
            server,
            &msg.target,
            &themed(
                "sed_cooldown",
                &["Please wait {seconds} seconds before correcting another line, {user}."],
                &[("seconds", &wait), ("user", display_name(msg))],
            )?,
        );
    }
    kv_write(&cooldown_key, &now.to_string())?;

    let corrected = match apply_correction(&previous.text, &correction) {
        Ok(Some(corrected)) => corrected,
        Ok(None) => {
            return reply(
                server,
                &msg.target,
                &themed(
                    "sed_no_match",
                    &["I couldn't find that text in your previous line, {user}."],
                    &[("user", display_name(msg))],
                )?,
            )
        }
        Err(_) => {
            return reply(
                server,
                &msg.target,
                &themed(
                    "sed_bad_regex",
                    &["That correction contains an invalid regular expression."],
                    &[],
                )?,
            )
        }
    };
    if corrected == previous.text {
        return reply(
            server,
            &msg.target,
            &themed(
                "sed_no_change",
                &["That correction would not change your previous line, {user}."],
                &[("user", display_name(msg))],
            )?,
        );
    }
    if corrected.chars().count() > MAX_TEXT_CHARS {
        return reply(
            server,
            &msg.target,
            &themed(
                "sed_too_long",
                &["That correction would make the line too long."],
                &[],
            )?,
        );
    }
    previous.nick = msg.nick.clone();
    previous.display = display_name(msg).to_string();
    previous.text = sanitize(&corrected);
    previous.timestamp = now;
    save_seen("last", server, &msg.target, &previous)?;
    reply(
        server,
        &msg.target,
        &themed(
            "sed_result",
            &["What {user} meant to say is: {text}"],
            &[("user", display_name(msg)), ("text", &previous.text)],
        )?,
    )?;
    award(server, msg, "corrections")
}

fn apply_correction(source: &str, correction: &Correction) -> Result<Option<String>, regex::Error> {
    let regex = RegexBuilder::new(&correction.pattern)
        .case_insensitive(correction.case_insensitive)
        .size_limit(1_000_000)
        .dfa_size_limit(1_000_000)
        .build()?;
    if !regex.is_match(source) {
        return Ok(None);
    }
    let corrected = if correction.global {
        regex.replace_all(source, correction.replacement.as_str())
    } else {
        regex.replace(source, correction.replacement.as_str())
    };
    Ok(Some(corrected.into_owned()))
}

fn display_name(msg: &jeeves_abi::MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn stable_id(user_id: &str, nick: &str) -> String {
    if user_id.is_empty() {
        format!("nick:{}", nick.to_ascii_lowercase())
    } else {
        user_id.into()
    }
}

fn parse_correction(value: &str) -> Result<Correction, &'static str> {
    let Some(body) = value.strip_prefix("s/") else {
        return Err("missing s/ prefix");
    };
    let (pattern, replacement_start) = parse_segment(body, 0)?;
    let (replacement, flags_start) = parse_segment(body, replacement_start)?;
    if pattern.is_empty() {
        return Err("the pattern is empty");
    }
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err("the pattern is too long");
    }
    if replacement.chars().count() > MAX_REPLACEMENT_CHARS {
        return Err("the replacement is too long");
    }
    if pattern.chars().any(char::is_control) || replacement.chars().any(char::is_control) {
        return Err("control characters are not allowed");
    }

    let mut global = false;
    let mut case_insensitive = false;
    for flag in body[flags_start..].chars() {
        match flag {
            'g' if !global => global = true,
            'i' if !case_insensitive => case_insensitive = true,
            'g' | 'i' => return Err("a flag was repeated"),
            _ => return Err("only g and i flags are supported"),
        }
    }
    Ok(Correction {
        pattern,
        replacement,
        global,
        case_insensitive,
    })
}

fn parse_segment(value: &str, start: usize) -> Result<(String, usize), &'static str> {
    let mut out = String::new();
    let mut chars = value[start..].char_indices();
    while let Some((offset, character)) = chars.next() {
        match character {
            '/' => return Ok((out, start + offset + 1)),
            '\\' => {
                let Some((_, escaped)) = chars.next() else {
                    return Err("the expression ends with an escape");
                };
                if escaped == '/' {
                    out.push('/');
                } else {
                    out.push('\\');
                    out.push(escaped);
                }
            }
            _ => out.push(character),
        }
    }
    Err("a closing / is missing")
}

fn parse_quote_id(value: &str) -> Option<u64> {
    value.strip_prefix('#')?.parse().ok()
}

fn parse_manual_quote(value: &str) -> Option<&str> {
    let value = value.trim();
    (value.len() >= 2 && value.starts_with('"') && value.ends_with('"'))
        .then(|| &value[1..value.len() - 1])
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_TEXT_CHARS)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn relative_time(seconds: i64) -> String {
    match seconds.max(0) {
        0..=4 => "just now".into(),
        5..=59 => format!("{seconds} seconds ago"),
        60..=3599 => format!("{} minutes ago", seconds / 60),
        3600..=86_399 => format!("{} hours ago", seconds / 3600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_explicit_quote_ids() {
        assert_eq!(parse_quote_id("#42"), Some(42));
        assert_eq!(parse_quote_id("42"), None);
    }

    #[test]
    fn parses_manual_quotes() {
        assert_eq!(parse_manual_quote("\"hello there\""), Some("hello there"));
        assert_eq!(parse_manual_quote("alice"), None);
    }

    #[test]
    fn sanitizes_irc_control_text() {
        assert_eq!(sanitize("hello\n\u{0003}04 world"), "hello04 world");
    }

    #[test]
    fn formats_relative_time() {
        assert_eq!(relative_time(2), "just now");
        assert_eq!(relative_time(125), "2 minutes ago");
    }

    #[test]
    fn scoped_keys_do_not_collide() {
        assert_ne!(
            scoped_key("seen", "a:b", "c", "d"),
            scoped_key("seen", "a", "b:c", "d")
        );
    }

    #[test]
    fn parses_sed_corrections_and_flags() {
        assert_eq!(
            parse_correction("s/thing/thing2/").unwrap(),
            Correction {
                pattern: "thing".into(),
                replacement: "thing2".into(),
                global: false,
                case_insensitive: false,
            }
        );
        assert_eq!(
            parse_correction(r"s/one\/two/three\/four/gi").unwrap(),
            Correction {
                pattern: "one/two".into(),
                replacement: "three/four".into(),
                global: true,
                case_insensitive: true,
            }
        );
    }

    #[test]
    fn preserves_regex_escapes() {
        assert_eq!(parse_correction(r"s/\d+/number/g").unwrap().pattern, r"\d+");
    }

    #[test]
    fn rejects_malformed_sed_corrections() {
        assert!(parse_correction("s/a/b").is_err());
        assert!(parse_correction("s//b/").is_err());
        assert!(parse_correction("s/a/b/x").is_err());
        assert!(parse_correction("s/a/b/gg").is_err());
    }

    #[test]
    fn applies_first_global_case_insensitive_and_capture_replacements() {
        let first = parse_correction("s/cat/dog/").unwrap();
        assert_eq!(
            apply_correction("cat cat", &first).unwrap().as_deref(),
            Some("dog cat")
        );
        let global = parse_correction("s/cat/dog/gi").unwrap();
        assert_eq!(
            apply_correction("Cat cat", &global).unwrap().as_deref(),
            Some("dog dog")
        );
        let captures = parse_correction(r"s/(hello) (world)/$2, $1/").unwrap();
        assert_eq!(
            apply_correction("hello world", &captures)
                .unwrap()
                .as_deref(),
            Some("world, hello")
        );
    }
}
