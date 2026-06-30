//! Channel-local persistent memos delivered when their recipient next speaks.

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    Category, CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, Level, LogReq,
    MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    Profile, ProfileKey, Role, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, ThemeReq, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const MAX_MESSAGE_CHARS: usize = 300;
const MAX_NICK_CHARS: usize = 64;
const MAX_PENDING_PER_RECIPIENT: usize = 20;
const MAX_PENDING_PER_SENDER_RECIPIENT: usize = 5;
const MAX_PENDING_PER_SENDER_CHANNEL: usize = 20;
const MAX_PENDING_PER_CHANNEL: usize = 500;
const MAX_DELIVER_PER_MESSAGE: usize = 3;
const MEMO_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn log(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "retention_seconds".into(),
            description: "How long undelivered memos remain stored.".into(),
            default: MEMO_TTL_SECONDS.to_string(),
            kind: SettingKind::DurationSeconds {
                min: 24 * 60 * 60,
                max: 365 * 24 * 60 * 60,
            },
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
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "tell".into(),
                description: "Leave a channel-local message for another user.".into(),
                usage: "!tell <nick> <message>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "memos".into(),
                description: "Count or clear waiting messages; super-admins may inspect or clear a user's queue privately.".into(),
                usage: "!memos [clear | admin list <nick> | admin clear <nick>]".into(),
                ..Default::default()
            },
        ],
    })?)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Memo {
    id: u64,
    recipient_id: Option<String>,
    recipient_nick: String,
    recipient_label: String,
    sender_id: String,
    sender_display: String,
    message: String,
    created_at: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoBook {
    next_id: u64,
    memos: Vec<Memo>,
}

fn memo_matches(memo: &Memo, request: &ModuleDataRequest) -> bool {
    memo.sender_id == request.subject.profile_id
        || memo.recipient_id.as_deref() == Some(request.subject.profile_id.as_str())
        || request.aliases.iter().any(|alias| {
            let sender = memo
                .sender_id
                .strip_prefix("nick:")
                .unwrap_or(&memo.sender_id);
            normalize_nick(&request.subject.server, sender)
                == normalize_nick(&request.subject.server, alias)
                || normalize_nick(&request.subject.server, &memo.recipient_nick)
                    == normalize_nick(&request.subject.server, alias)
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let server_prefix = format!("book:{}:", encode(&request.subject.server));
    let mut books = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&server_prefix))
    {
        let book: MemoBook = serde_json::from_str(&entry.value)?;
        let memos = book
            .memos
            .into_iter()
            .filter(|memo| memo_matches(memo, &request))
            .collect::<Vec<_>>();
        if !memos.is_empty() {
            books.push(serde_json::json!({ "key": entry.key, "memos": memos }));
        }
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if books.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "channel_books": books })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let server_prefix = format!("book:{}:", encode(&request.subject.server));
    let mut mutations = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&server_prefix))
    {
        let mut book: MemoBook = serde_json::from_str(&entry.value)?;
        let before = book.memos.len();
        book.memos.retain(|memo| !memo_matches(memo, &request));
        if book.memos.len() != before {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: if book.memos.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&book)?)
                },
            });
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
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

fn admin_audit(server: &str, channel: &str, admin: &str, action: &str) -> Result<(), Error> {
    unsafe {
        log(serde_json::to_string(&LogReq {
            level: Level::Info,
            category: Category::Command,
            message: format!("[{server}] {admin} {action} in {channel}"),
        })?)?;
    }
    Ok(())
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn memo_ttl_seconds(server: &str, channel: &str) -> Result<i64, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "retention_seconds".into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    Ok(raw.parse().unwrap_or(MEMO_TTL_SECONDS))
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

fn book_key(server: &str, channel: &str) -> String {
    format!("book:{}:{}", encode(server), encode(channel))
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

fn load_book(server: &str, channel: &str) -> Result<MemoBook, Error> {
    let raw = kv_read(&book_key(server, channel))?;
    if raw.is_empty() {
        Ok(MemoBook {
            next_id: 1,
            memos: Vec::new(),
        })
    } else {
        let mut book: MemoBook = serde_json::from_str(&raw)?;
        if book.next_id == 0 {
            book.next_id = book.memos.iter().map(|memo| memo.id).max().unwrap_or(0) + 1;
        }
        Ok(book)
    }
}

fn save_book(server: &str, channel: &str, book: &MemoBook) -> Result<(), Error> {
    kv_write(&book_key(server, channel), &serde_json::to_string(book)?)
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
        if matches!(command.as_str(), "!tell" | "!memos") {
            reply(
                &server,
                &msg.nick,
                &themed(
                    "channel_only",
                    &["Memos belong to a channel. Please use that command where the message should be delivered."],
                    &[],
                )?,
            )?;
        }
        return Ok(());
    }

    let now = timestamp()?;
    if command == "!memos" {
        handle_memos(&server, &msg, text, now)?;
        return Ok(());
    }

    deliver_pending(&server, &msg, now)?;
    if command == "!tell" {
        handle_tell(&server, &msg, text, now)?;
    }
    Ok(())
}

fn handle_tell(server: &str, msg: &MessagePayload, text: &str, now: i64) -> Result<(), Error> {
    let mut parts = text.splitn(3, char::is_whitespace);
    let _command = parts.next();
    let target = parts.next().unwrap_or("").trim();
    let raw_message = parts.next().unwrap_or("").trim();
    if target.is_empty() || raw_message.is_empty() {
        return reply(
            server,
            &msg.target,
            &themed("tell_usage", &["Usage: !tell <nick> <message>"], &[])?,
        );
    }
    if !valid_nick(target) {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_invalid_nick",
                &["That does not look like a valid nickname."],
                &[],
            )?,
        );
    }

    let message = sanitize(raw_message);
    if message.is_empty() {
        return reply(
            server,
            &msg.target,
            &themed("tell_empty", &["The memo cannot be empty."], &[])?,
        );
    }
    let message_chars = message.chars().count();
    if message_chars > MAX_MESSAGE_CHARS {
        let max = MAX_MESSAGE_CHARS.to_string();
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_too_long",
                &["That memo is too long; please keep it to {max} characters."],
                &[("max", &max)],
            )?,
        );
    }

    let target_profile = profile(server, target)?;
    let recipient_id = target_profile.as_ref().map(|profile| profile.id.clone());
    let recipient_nick = normalize_nick(server, target);
    let sender_id = stable_id(server, &msg.user_id, &msg.nick);
    if recipient_id.as_deref() == Some(sender_id.as_str())
        || (recipient_id.is_none() && recipient_nick == normalize_nick(server, &msg.nick))
    {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_self",
                &["You are already here, {user}; there is no need to leave yourself a memo."],
                &[("user", display_name(msg))],
            )?,
        );
    }

    let mut book = load_book(server, &msg.target)?;
    expire_with_ttl(&mut book, now, memo_ttl_seconds(server, &msg.target)?);
    if book.memos.len() >= MAX_PENDING_PER_CHANNEL {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_channel_full",
                &["This channel already has too many messages waiting; please try again later."],
                &[],
            )?,
        );
    }
    let sender_channel_count = book
        .memos
        .iter()
        .filter(|memo| memo.sender_id == sender_id)
        .count();
    if sender_channel_count >= MAX_PENDING_PER_SENDER_CHANNEL {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_sender_channel_full",
                &["You already have too many messages waiting in this channel; please wait for some to be delivered."],
                &[],
            )?,
        );
    }
    let recipient_count = book
        .memos
        .iter()
        .filter(|memo| same_recipient(memo, server, recipient_id.as_deref(), &recipient_nick))
        .count();
    if recipient_count >= MAX_PENDING_PER_RECIPIENT {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_recipient_full",
                &["{target} already has too many messages waiting in this channel."],
                &[("target", target)],
            )?,
        );
    }
    let sender_count = book
        .memos
        .iter()
        .filter(|memo| {
            memo.sender_id == sender_id
                && same_recipient(memo, server, recipient_id.as_deref(), &recipient_nick)
        })
        .count();
    if sender_count >= MAX_PENDING_PER_SENDER_RECIPIENT {
        return reply(
            server,
            &msg.target,
            &themed(
                "tell_sender_full",
                &["You already have several messages waiting for {target}; please wait for them to speak."],
                &[("target", target)],
            )?,
        );
    }

    let id = book.next_id.max(1);
    book.next_id = id.saturating_add(1);
    let recipient_label = target_profile
        .as_ref()
        .map(|profile| profile.nick.clone())
        .unwrap_or_else(|| target.to_string());
    book.memos.push(Memo {
        id,
        recipient_id,
        recipient_nick,
        recipient_label: recipient_label.clone(),
        sender_id,
        sender_display: sanitize(display_name(msg)),
        message,
        created_at: now,
    });
    save_book(server, &msg.target, &book)?;
    reply(
        server,
        &msg.target,
        &themed(
            "tell_saved",
            &["Very good, {user}. I'll pass that on to {target} when they next speak here."],
            &[("user", display_name(msg)), ("target", &recipient_label)],
        )?,
    )
}

fn deliver_pending(server: &str, msg: &MessagePayload, now: i64) -> Result<(), Error> {
    let mut book = load_book(server, &msg.target)?;
    let expired = expire_with_ttl(&mut book, now, memo_ttl_seconds(server, &msg.target)?);
    let (deliveries, remaining) = take_deliveries(&mut book, server, msg, MAX_DELIVER_PER_MESSAGE);
    if expired || !deliveries.is_empty() {
        // Persist removal before posting so a send failure cannot cause repeated delivery.
        save_book(server, &msg.target, &book)?;
    }
    for memo in deliveries {
        let ago = relative_time(now.saturating_sub(memo.created_at));
        reply(
            server,
            &msg.target,
            &themed(
                "memo_delivery",
                &["Ah, a message for you, {user} — {sender} said {ago}: {message}"],
                &[
                    ("user", display_name(msg)),
                    ("sender", &memo.sender_display),
                    ("ago", &ago),
                    ("message", &memo.message),
                ],
            )?,
        )?;
    }
    if remaining > 0 {
        let count = remaining.to_string();
        reply(
            server,
            &msg.target,
            &themed(
                "memo_more",
                &["You have {count} more messages waiting, {user}; speak again when you are ready for them."],
                &[("count", &count), ("user", display_name(msg))],
            )?,
        )?;
    }
    Ok(())
}

fn handle_memos(server: &str, msg: &MessagePayload, text: &str, now: i64) -> Result<(), Error> {
    let arg = text
        .split_once(char::is_whitespace)
        .map(|(_, argument)| argument)
        .unwrap_or("")
        .trim();

    if arg
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("admin"))
    {
        if !msg.role.is_some_and(|r| r.satisfies(Role::SuperAdmin)) {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_denied",
                    &["This command is restricted to super-admins."],
                    &[],
                )?,
            );
        }
        let admin_rest = arg
            .split_once(char::is_whitespace)
            .map(|(_, rest)| rest.trim())
            .unwrap_or("");
        let mut book = load_book(server, &msg.target)?;
        let expired = expire_with_ttl(&mut book, now, memo_ttl_seconds(server, &msg.target)?);
        return handle_memos_admin(server, msg, admin_rest, &mut book, expired, now);
    }

    let mut book = load_book(server, &msg.target)?;
    let expired = expire_with_ttl(&mut book, now, memo_ttl_seconds(server, &msg.target)?);
    if arg.eq_ignore_ascii_case("clear") {
        let removed = remove_recipient_memos(&mut book, server, msg);
        if expired || removed > 0 {
            save_book(server, &msg.target, &book)?;
        }
        let count = removed.to_string();
        return reply(
            server,
            &msg.target,
            &themed(
                "memos_cleared",
                &["Cleared {count} waiting messages for you in this channel, {user}."],
                &[("count", &count), ("user", display_name(msg))],
            )?,
        );
    }
    if !arg.is_empty() {
        return reply(
            server,
            &msg.target,
            &themed("memos_usage", &["Usage: !memos or !memos clear"], &[])?,
        );
    }
    if expired {
        save_book(server, &msg.target, &book)?;
    }
    let count = count_for_recipient(&book, server, msg);
    let count_text = count.to_string();
    let key = if count == 0 {
        "memos_none"
    } else {
        "memos_waiting"
    };
    let defaults: &[&str] = if count == 0 {
        &["There are no messages waiting for you in this channel, {user}."]
    } else {
        &["You have {count} messages waiting in this channel, {user}. They will be delivered when you next speak."]
    };
    reply(
        server,
        &msg.target,
        &themed(
            key,
            defaults,
            &[("count", &count_text), ("user", display_name(msg))],
        )?,
    )
}

fn handle_memos_admin(
    server: &str,
    msg: &MessagePayload,
    admin_rest: &str,
    book: &mut MemoBook,
    expired: bool,
    now: i64,
) -> Result<(), Error> {
    let mut parts = admin_rest.splitn(2, char::is_whitespace);
    let subcmd = parts.next().unwrap_or("");
    let nick = parts.next().unwrap_or("").trim();

    if subcmd.eq_ignore_ascii_case("list") {
        if nick.is_empty() {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_usage",
                    &["Usage: !memos admin list <nick> | clear <nick>"],
                    &[],
                )?,
            );
        }
        let target_profile = profile(server, nick)?;
        let target_id = target_profile.as_ref().map(|p| p.id.clone());
        let target_nick = normalize_nick(server, nick);
        let pending: Vec<&Memo> = book
            .memos
            .iter()
            .filter(|memo| same_recipient(memo, server, target_id.as_deref(), &target_nick))
            .collect();
        admin_audit(
            server,
            &msg.target,
            &msg.nick,
            &format!("inspected {} pending memo(s) for {nick}", pending.len()),
        )?;
        if pending.is_empty() {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_none",
                    &["No pending memos for {target} in this channel."],
                    &[("target", nick)],
                )?,
            );
        }
        let count = pending.len().to_string();
        reply(
            server,
            &msg.nick,
            &themed(
                "memos_admin_list_header",
                &["Pending memos for {target} ({count}):"],
                &[("target", nick), ("count", &count)],
            )?,
        )?;
        for memo in pending.iter().take(10) {
            let ago = relative_time(now.saturating_sub(memo.created_at));
            let preview: String = memo.message.chars().take(60).collect();
            let ellipsis = if memo.message.chars().count() > 60 {
                "…"
            } else {
                ""
            };
            let id = memo.id.to_string();
            reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_list_item",
                    &["  #{id} from {sender} {ago}: {preview}{ellipsis}"],
                    &[
                        ("id", &id),
                        ("sender", &memo.sender_display),
                        ("ago", &ago),
                        ("preview", &preview),
                        ("ellipsis", ellipsis),
                    ],
                )?,
            )?;
        }
        if pending.len() > 10 {
            let extra = (pending.len() - 10).to_string();
            reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_list_more",
                    &["  … and {extra} more."],
                    &[("extra", &extra)],
                )?,
            )?;
        }
        return Ok(());
    }

    if subcmd.eq_ignore_ascii_case("clear") {
        if nick.is_empty() {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "memos_admin_usage",
                    &["Usage: !memos admin list <nick> | clear <nick>"],
                    &[],
                )?,
            );
        }
        let target_profile = profile(server, nick)?;
        let target_id = target_profile.as_ref().map(|p| p.id.clone());
        let target_nick = normalize_nick(server, nick);
        let before = book.memos.len();
        book.memos
            .retain(|memo| !same_recipient(memo, server, target_id.as_deref(), &target_nick));
        let removed = before - book.memos.len();
        if removed > 0 || expired {
            save_book(server, &msg.target, book)?;
        }
        admin_audit(
            server,
            &msg.target,
            &msg.nick,
            &format!("cleared {removed} pending memo(s) for {nick}"),
        )?;
        let count = removed.to_string();
        return reply(
            server,
            &msg.nick,
            &themed(
                "memos_admin_cleared",
                &["Cleared {count} pending memos for {target} in this channel."],
                &[("count", &count), ("target", nick)],
            )?,
        );
    }

    reply(
        server,
        &msg.nick,
        &themed(
            "memos_admin_usage",
            &["Usage: !memos admin list <nick> | clear <nick>"],
            &[],
        )?,
    )
}

fn take_deliveries(
    book: &mut MemoBook,
    server: &str,
    msg: &MessagePayload,
    limit: usize,
) -> (Vec<Memo>, usize) {
    let mut deliveries = Vec::new();
    let mut retained = Vec::with_capacity(book.memos.len());
    let mut remaining = 0;
    for memo in book.memos.drain(..) {
        if matches_message(&memo, server, msg) {
            if deliveries.len() < limit {
                deliveries.push(memo);
            } else {
                remaining += 1;
                retained.push(memo);
            }
        } else {
            retained.push(memo);
        }
    }
    book.memos = retained;
    (deliveries, remaining)
}

fn remove_recipient_memos(book: &mut MemoBook, server: &str, msg: &MessagePayload) -> usize {
    let before = book.memos.len();
    book.memos
        .retain(|memo| !matches_message(memo, server, msg));
    before - book.memos.len()
}

fn count_for_recipient(book: &MemoBook, server: &str, msg: &MessagePayload) -> usize {
    book.memos
        .iter()
        .filter(|memo| matches_message(memo, server, msg))
        .count()
}

fn matches_message(memo: &Memo, server: &str, msg: &MessagePayload) -> bool {
    match memo.recipient_id.as_deref() {
        Some(id) => !msg.user_id.is_empty() && id == msg.user_id,
        None => normalize_nick(server, &memo.recipient_nick) == normalize_nick(server, &msg.nick),
    }
}

fn same_recipient(memo: &Memo, server: &str, id: Option<&str>, nick: &str) -> bool {
    match (memo.recipient_id.as_deref(), id) {
        (Some(memo_id), Some(id)) => memo_id == id,
        (None, None) => normalize_nick(server, &memo.recipient_nick) == nick,
        _ => false,
    }
}

#[cfg(test)]
fn expire(book: &mut MemoBook, now: i64) -> bool {
    expire_with_ttl(book, now, MEMO_TTL_SECONDS)
}

fn expire_with_ttl(book: &mut MemoBook, now: i64, ttl_seconds: i64) -> bool {
    if now <= 0 {
        return false;
    }
    let cutoff = now.saturating_sub(ttl_seconds.max(1));
    let before = book.memos.len();
    book.memos.retain(|memo| memo.created_at >= cutoff);
    book.memos.len() != before
}

fn stable_id(server: &str, user_id: &str, nick: &str) -> String {
    if user_id.is_empty() {
        format!("nick:{}", normalize_nick(server, nick))
    } else {
        user_id.into()
    }
}

#[cfg(target_arch = "wasm32")]
fn normalize_nick(server: &str, nick: &str) -> String {
    unsafe {
        irc_casefold(
            serde_json::to_string(&IrcCasefold {
                server: server.into(),
                value: nick.into(),
            })
            .unwrap_or_default(),
        )
    }
    .unwrap_or_else(|_| nick.to_ascii_lowercase())
}

#[cfg(not(target_arch = "wasm32"))]
fn normalize_nick(_server: &str, nick: &str) -> String {
    default_irc_fold(nick)
}

#[cfg(not(target_arch = "wasm32"))]
fn default_irc_fold(nick: &str) -> String {
    nick.chars()
        .map(|character| match character {
            'A'..='Z' => character.to_ascii_lowercase(),
            '[' => '{',
            ']' => '}',
            '\\' => '|',
            '^' => '~',
            other => other,
        })
        .collect()
}

fn display_name(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn valid_nick(nick: &str) -> bool {
    !nick.is_empty()
        && nick.chars().count() <= MAX_NICK_CHARS
        && !nick.chars().any(char::is_control)
        && !nick.starts_with('!')
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn relative_time(seconds: i64) -> String {
    match seconds.max(0) {
        0..=4 => "just now".into(),
        5..=59 => format!("{seconds} seconds ago"),
        60..=3_599 => format!("{} minutes ago", seconds / 60),
        3_600..=86_399 => format!("{} hours ago", seconds / 3_600),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_recipient_matching_uses_irc_default_casemapping() {
        let stored = memo(1, None, "Target[One]", 1);
        assert!(matches_message(&stored, "net", &message("", "target{one}")));
    }

    fn message(user_id: &str, nick: &str) -> MessagePayload {
        MessagePayload {
            user_id: user_id.into(),
            nick: nick.into(),
            display: nick.into(),
            user: String::new(),
            host: String::new(),
            target: "#test".into(),
            text: "hello".into(),
            is_private: false,
            tags: Vec::new(),
            role: None,
        }
    }

    fn memo(id: u64, recipient_id: Option<&str>, nick: &str, created_at: i64) -> Memo {
        Memo {
            id,
            recipient_id: recipient_id.map(str::to_string),
            recipient_nick: normalize_nick("net", nick),
            recipient_label: nick.into(),
            sender_id: "sender-id".into(),
            sender_display: "Sender".into(),
            message: "hello".into(),
            created_at,
        }
    }

    #[test]
    fn stable_recipient_survives_nick_change() {
        let stored = memo(1, Some("user-1"), "OldNick", 100);
        assert!(matches_message(
            &stored,
            "net",
            &message("user-1", "NewNick")
        ));
        assert!(!matches_message(
            &stored,
            "net",
            &message("user-2", "OldNick")
        ));
    }

    #[test]
    fn unknown_recipient_matches_nick_case_insensitively() {
        let stored = memo(1, None, "SomeNick", 100);
        assert!(matches_message(
            &stored,
            "net",
            &message("new-id", "sOMEnICK")
        ));
        assert!(!matches_message(
            &stored,
            "net",
            &message("new-id", "OtherNick")
        ));
    }

    #[test]
    fn delivery_is_ordered_bounded_and_retains_overflow() {
        let mut book = MemoBook {
            next_id: 5,
            memos: vec![
                memo(1, Some("target"), "Target", 10),
                memo(2, Some("other"), "Other", 20),
                memo(3, Some("target"), "Target", 30),
                memo(4, Some("target"), "Target", 40),
            ],
        };
        let (delivered, remaining) =
            take_deliveries(&mut book, "net", &message("target", "Target"), 2);
        assert_eq!(
            delivered.iter().map(|memo| memo.id).collect::<Vec<_>>(),
            vec![1, 3]
        );
        assert_eq!(remaining, 1);
        assert_eq!(
            book.memos.iter().map(|memo| memo.id).collect::<Vec<_>>(),
            vec![2, 4]
        );
    }

    #[test]
    fn clearing_only_removes_requesters_memos() {
        let mut book = MemoBook {
            next_id: 3,
            memos: vec![
                memo(1, Some("target"), "Target", 10),
                memo(2, Some("other"), "Other", 20),
            ],
        };
        assert_eq!(
            remove_recipient_memos(&mut book, "net", &message("target", "Target")),
            1
        );
        assert_eq!(book.memos[0].id, 2);
    }

    #[test]
    fn old_memos_expire() {
        let now = MEMO_TTL_SECONDS + 100;
        let mut book = MemoBook {
            next_id: 3,
            memos: vec![
                memo(1, Some("target"), "Target", 99),
                memo(2, Some("target"), "Target", 100),
            ],
        };
        assert!(expire(&mut book, now));
        assert_eq!(
            book.memos.iter().map(|memo| memo.id).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn configured_retention_changes_expiry_cutoff() {
        let mut book = MemoBook {
            next_id: 2,
            memos: vec![memo(1, Some("target"), "Target", 100)],
        };
        assert!(!expire_with_ttl(&mut book, 200, 101));
        assert!(expire_with_ttl(&mut book, 200, 99));
    }

    #[test]
    fn sanitizes_control_characters_and_whitespace() {
        assert_eq!(sanitize(" hello\n\u{0003}04   there "), "hello04 there");
    }

    #[test]
    fn scoped_keys_do_not_collide() {
        assert_ne!(book_key("a:b", "c"), book_key("a", "b:c"));
    }
}
