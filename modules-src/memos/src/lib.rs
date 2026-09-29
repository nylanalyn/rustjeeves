//! Persistent memos. `!tell` in a channel leaves a channel memo, delivered there when the recipient
//! next speaks, or by NOTICE when they join; `!tell` by private message leaves a private memo,
//! delivered by PM when they next speak or join anywhere on the network. `!memos sent` and
//! `!memos unsend <id>` let senders see and withdraw what's still waiting.

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, Category,
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, Level, LogReq,
    MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    Profile, ProfileKey, Role, SendMessage, SendNotice, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
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
/// The network-wide book of private memos (not a valid channel name, so it can't collide).
const PRIVATE_BOOK: &str = "@private";
const MAX_SENT_LISTED: usize = 5;

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
    fn award_stats(input: String) -> String;
    fn send_notice(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("message_bottle", "Message in a Bottle", 1),
        ("correspondent", "Correspondent", 25),
        ("postmaster_general", "Postmaster General", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Have {threshold} memos successfully delivered."),
        stat: "sent_delivered".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "you_have_mail".into(),
        name: "You Have Mail".into(),
        description: "Receive 25 delivered memos.".into(),
        stat: "received".into(),
        threshold: 25,
        optional: true,
        secret: false,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "sent_delivered".into(),
                description: "Sent memos successfully delivered".into(),
            },
            AchievementStat {
                id: "received".into(),
                description: "Memos received".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display: &str,
    target: &str,
    stat: &str,
    event_id: String,
) -> Result<(), Error> {
    if profile_id.is_empty() || profile_id.starts_with("nick:") {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: Some(event_id),
        })?)?;
    }
    Ok(())
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
                description: "Leave a message for someone: in a channel it waits there; sent to me privately, it's delivered privately wherever they turn up.".into(),
                usage: "!tell <nick> <message>".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "memos".into(),
                description: "Count or clear messages waiting for you, list or withdraw ones you sent; super-admins may inspect or clear a user's queue privately.".into(),
                usage: "!memos [clear | sent | unsend <id> | admin list <nick> | admin clear <nick>]".into(),
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
    kv_write(&book_key(server, channel), &serde_json::to_string(book)?)?;
    kv_write(
        &pending_key(server, channel),
        if book.memos.is_empty() { "0" } else { "1" },
    )
}

/// "0" once a book is known empty, so ordinary chat skips loading it; anything else (including
/// books saved before this flag existed) means "look".
fn pending_key(server: &str, channel: &str) -> String {
    format!("pending:{}:{}", encode(server), encode(channel))
}

fn may_have_memos(server: &str, channel: &str) -> Result<bool, Error> {
    Ok(kv_read(&pending_key(server, channel))? != "0")
}

fn send_private_notice(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_notice(serde_json::to_string(&SendNotice {
            server: server.into(),
            target: target.into(),
            text: text.into(),
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
    let command = text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let now = timestamp()?;
    if command == "!memos" {
        return Ok(handle_memos(&server, &msg, text, now)?);
    }
    if msg.is_private {
        deliver(&server, PRIVATE_BOOK, &msg, Delivery::Private, now)?;
    } else {
        deliver(&server, &msg.target, &msg, Delivery::Channel, now)?;
        deliver(&server, PRIVATE_BOOK, &msg, Delivery::Private, now)?;
    }
    if command == "!tell" {
        handle_tell(&server, &msg, text, now)?;
    }
    Ok(())
}

/// A join delivers waiting memos without waiting for the person to speak: channel memos by
/// NOTICE (so the channel isn't interrupted), private ones by PM.
#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::UserJoined { channel, nick, .. } = env.event else {
        return Ok(());
    };
    let server = env.server;
    let joined = MessagePayload {
        user_id: profile(&server, &nick)?
            .map(|profile| profile.id)
            .unwrap_or_default(),
        display: nick.clone(),
        nick,
        target: channel.clone(),
        ..MessagePayload::default()
    };
    let now = timestamp()?;
    deliver(&server, &channel, &joined, Delivery::Notice, now)?;
    deliver(&server, PRIVATE_BOOK, &joined, Delivery::Private, now)?;
    Ok(())
}

fn handle_tell(server: &str, msg: &MessagePayload, text: &str, now: i64) -> Result<(), Error> {
    // Private memos live in one network-wide book and are answered privately.
    let (book_name, dest) = if msg.is_private {
        (PRIVATE_BOOK, msg.nick.as_str())
    } else {
        (msg.target.as_str(), msg.target.as_str())
    };
    let mut parts = text.splitn(3, char::is_whitespace);
    let _command = parts.next();
    let target = parts.next().unwrap_or("").trim();
    let raw_message = parts.next().unwrap_or("").trim();
    if target.is_empty() || raw_message.is_empty() {
        return reply(
            server,
            dest,
            &themed("tell_usage", &["Usage: !tell <nick> <message>"], &[])?,
        );
    }
    if !valid_nick(target) {
        return reply(
            server,
            dest,
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
            dest,
            &themed("tell_empty", &["The memo cannot be empty."], &[])?,
        );
    }
    let message_chars = message.chars().count();
    if message_chars > MAX_MESSAGE_CHARS {
        let max = MAX_MESSAGE_CHARS.to_string();
        return reply(
            server,
            dest,
            &themed(
                "tell_too_long",
                &["That memo is too long; please keep it to {max} characters."],
                &[("max", &max)],
            )?,
        );
    }

    let target_profile = profile(server, target)?;
    let known = target_profile.is_some();
    let recipient_id = target_profile.as_ref().map(|profile| profile.id.clone());
    let recipient_nick = normalize_nick(server, target);
    let sender_id = stable_id(server, &msg.user_id, &msg.nick);
    if recipient_id.as_deref() == Some(sender_id.as_str())
        || (recipient_id.is_none() && recipient_nick == normalize_nick(server, &msg.nick))
    {
        return reply(
            server,
            dest,
            &themed(
                "tell_self",
                &["You are already here, {user}; there is no need to leave yourself a memo."],
                &[("user", display_name(msg))],
            )?,
        );
    }

    let mut book = load_book(server, book_name)?;
    expire_with_ttl(&mut book, now, memo_ttl_seconds(server, book_name)?);
    if book.memos.len() >= MAX_PENDING_PER_CHANNEL {
        return reply(
            server,
            dest,
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
            dest,
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
            dest,
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
            dest,
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
    save_book(server, book_name, &book)?;
    let id_text = id.to_string();
    let vars = [
        ("user", display_name(msg)),
        ("target", recipient_label.as_str()),
        ("id", id_text.as_str()),
    ];
    let (key, default) = match (msg.is_private, known) {
        (false, true) => (
            "tell_saved",
            "Very good, {user}. I'll pass that on to {target} when they next speak here.",
        ),
        (false, false) => (
            "memos.tell_saved_unknown",
            "I've never seen {target}, {user}; I'll pass it on if they turn up here, but do check the spelling. (!memos unsend {id} takes it back.)",
        ),
        (true, true) => (
            "memos.tell_saved_private",
            "Very good, {user}. I'll pass that on to {target} privately when they next turn up.",
        ),
        (true, false) => (
            "memos.tell_saved_private_unknown",
            "I've never seen {target}, {user}; I'll pass it on privately if they turn up, but do check the spelling. (!memos unsend {id} takes it back.)",
        ),
    };
    reply(server, dest, &themed(key, &[default], &vars)?)
}

#[derive(Clone, Copy, PartialEq)]
enum Delivery {
    /// In the channel where the memo was left.
    Channel,
    /// By NOTICE to the person who just joined.
    Notice,
    /// A private memo, by PM.
    Private,
}

fn deliver(
    server: &str,
    book_name: &str,
    msg: &MessagePayload,
    how: Delivery,
    now: i64,
) -> Result<(), Error> {
    if !may_have_memos(server, book_name)? {
        return Ok(());
    }
    let mut book = load_book(server, book_name)?;
    let expired = expire_with_ttl(&mut book, now, memo_ttl_seconds(server, book_name)?);
    let (deliveries, remaining) = take_deliveries(&mut book, server, msg, MAX_DELIVER_PER_MESSAGE);
    if expired || !deliveries.is_empty() || kv_read(&pending_key(server, book_name))?.is_empty() {
        // Persist removal before posting so a send failure cannot cause repeated delivery.
        save_book(server, book_name, &book)?;
    }
    let send = |text: &str| -> Result<(), Error> {
        match how {
            Delivery::Channel => reply(server, &msg.target, text),
            Delivery::Notice => send_private_notice(server, &msg.nick, text),
            Delivery::Private => reply(server, &msg.nick, text),
        }
    };
    for memo in deliveries {
        let ago = relative_time(now.saturating_sub(memo.created_at));
        let vars = [
            ("user", display_name(msg)),
            ("sender", memo.sender_display.as_str()),
            ("ago", ago.as_str()),
            ("message", memo.message.as_str()),
            ("channel", msg.target.as_str()),
        ];
        let (key, default) = match how {
            Delivery::Channel => (
                "memo_delivery",
                "Ah, a message for you, {user} — {sender} said {ago}: {message}",
            ),
            Delivery::Notice => (
                "memos.delivery_on_join",
                "Welcome back, {user}. A message left for you in {channel} — {sender} said {ago}: {message}",
            ),
            Delivery::Private => (
                "memos.delivery_private",
                "A private message for you, {user} — {sender} said {ago}: {message}",
            ),
        };
        send(&themed(key, &[default], &vars)?)?;
        let event = format!("{}:{}:{}", server, book_name, memo.id);
        // Unlocks from a private delivery are announced privately too.
        let announce_to = if how == Delivery::Private {
            msg.nick.as_str()
        } else {
            msg.target.as_str()
        };
        award(
            server,
            &memo.sender_id,
            &memo.sender_display,
            announce_to,
            "sent_delivered",
            format!("sent:{event}"),
        )?;
        award(
            server,
            &msg.user_id,
            display_name(msg),
            announce_to,
            "received",
            format!("received:{event}"),
        )?;
    }
    if remaining > 0 {
        let count = remaining.to_string();
        send(&themed(
            "memo_more",
            &["You have {count} more messages waiting, {user}; speak again when you are ready for them."],
            &[("count", &count), ("user", display_name(msg))],
        )?)?;
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
        && !msg.is_private
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

    let (book_name, dest) = if msg.is_private {
        (PRIVATE_BOOK, msg.nick.as_str())
    } else {
        (msg.target.as_str(), msg.target.as_str())
    };
    let mut book = load_book(server, book_name)?;
    let expired = expire_with_ttl(&mut book, now, memo_ttl_seconds(server, book_name)?);
    let first = arg
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if first == "sent" || first == "unsend" {
        if expired {
            save_book(server, book_name, &book)?;
        }
        return handle_sent(server, msg, dest, book_name, &mut book, arg, now);
    }
    if arg.eq_ignore_ascii_case("clear") {
        let removed = remove_recipient_memos(&mut book, server, msg);
        if expired || removed > 0 {
            save_book(server, book_name, &book)?;
        }
        let count = removed.to_string();
        return reply(
            server,
            dest,
            &themed(
                "memos_cleared",
                &["Cleared {count} waiting messages for you here, {user}."],
                &[("count", &count), ("user", display_name(msg))],
            )?,
        );
    }
    if !arg.is_empty() {
        return reply(
            server,
            dest,
            &themed(
                "memos_usage",
                &["Usage: !memos, !memos clear, !memos sent, or !memos unsend <id>"],
                &[],
            )?,
        );
    }
    if expired {
        save_book(server, book_name, &book)?;
    }
    let count = count_for_recipient(&book, server, msg);
    let count_text = count.to_string();
    let (key, defaults): (&str, &[&str]) = match (msg.is_private, count) {
        (false, 0) => (
            "memos_none",
            &["There are no messages waiting for you in this channel, {user}."],
        ),
        (false, _) => (
            "memos_waiting",
            &["You have {count} messages waiting in this channel, {user}. They will be delivered when you next speak."],
        ),
        (true, 0) => (
            "memos.private_none",
            &["There are no private messages waiting for you, {user}."],
        ),
        (true, _) => (
            "memos.private_waiting",
            &["You have {count} private messages waiting, {user}; say anything to me to receive them."],
        ),
    };
    reply(
        server,
        dest,
        &themed(
            key,
            defaults,
            &[("count", &count_text), ("user", display_name(msg))],
        )?,
    )
}

/// `!memos sent` lists what the caller left in this book that's still waiting; `!memos unsend
/// <id>` withdraws one of them.
fn handle_sent(
    server: &str,
    msg: &MessagePayload,
    dest: &str,
    book_name: &str,
    book: &mut MemoBook,
    arg: &str,
    now: i64,
) -> Result<(), Error> {
    let sender_id = stable_id(server, &msg.user_id, &msg.nick);
    let user = display_name(msg);
    let mut words = arg.split_whitespace();
    let first = words.next().unwrap_or("").to_ascii_lowercase();
    if first == "unsend" {
        let Some(id) = words
            .next()
            .and_then(|word| word.trim_start_matches('#').parse::<u64>().ok())
        else {
            return reply(
                server,
                dest,
                &themed(
                    "memos.unsend_usage",
                    &["Which one, {user}? !memos sent shows the numbers; then !memos unsend <id>."],
                    &[("user", user)],
                )?,
            );
        };
        let Some(index) = book
            .memos
            .iter()
            .position(|memo| memo.id == id && memo.sender_id == sender_id)
        else {
            return reply(
                server,
                dest,
                &themed(
                    "memos.unsend_unknown",
                    &["You have no waiting memo #{id} here, {user}."],
                    &[("id", &id.to_string()), ("user", user)],
                )?,
            );
        };
        let memo = book.memos.remove(index);
        save_book(server, book_name, book)?;
        return reply(
            server,
            dest,
            &themed(
                "memos.unsent",
                &["Withdrawn, {user}: memo #{id} to {target} won't be delivered."],
                &[
                    ("id", &id.to_string()),
                    ("target", &memo.recipient_label),
                    ("user", user),
                ],
            )?,
        );
    }
    let sent = book
        .memos
        .iter()
        .filter(|memo| memo.sender_id == sender_id)
        .collect::<Vec<_>>();
    if sent.is_empty() {
        return reply(
            server,
            dest,
            &themed(
                "memos.sent_none",
                &["Nothing you've sent is still waiting here, {user}."],
                &[("user", user)],
            )?,
        );
    }
    let list = sent_list(&sent, now);
    reply(
        server,
        dest,
        &themed(
            "memos.sent",
            &["Still waiting, {user}: {list}. !memos unsend <id> withdraws one."],
            &[("list", &list), ("user", user)],
        )?,
    )
}

/// "#3 to alice 2h ago: first words… · #5 to bob just now: … · +2 more"
fn sent_list(sent: &[&Memo], now: i64) -> String {
    let mut parts = sent
        .iter()
        .rev()
        .take(MAX_SENT_LISTED)
        .map(|memo| {
            let preview = memo.message.chars().take(30).collect::<String>();
            let ellipsis = if memo.message.chars().count() > 30 {
                "…"
            } else {
                ""
            };
            format!(
                "#{} to {} {}: {preview}{ellipsis}",
                memo.id,
                memo.recipient_label,
                relative_time(now.saturating_sub(memo.created_at))
            )
        })
        .collect::<Vec<_>>();
    if sent.len() > MAX_SENT_LISTED {
        parts.push(format!("+{} more", sent.len() - MAX_SENT_LISTED));
    }
    parts.join(" · ")
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
    let ago = |count: i64, unit: &str| {
        if count == 1 {
            format!("1 {unit} ago")
        } else {
            format!("{count} {unit}s ago")
        }
    };
    match seconds.max(0) {
        0..=4 => "just now".into(),
        5..=59 => ago(seconds, "second"),
        60..=3_599 => ago(seconds / 60, "minute"),
        3_600..=86_399 => ago(seconds / 3_600, "hour"),
        _ => ago(seconds / 86_400, "day"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sent_list_previews_newest_first_and_counts_the_rest() {
        let memo = |id, message: &str| Memo {
            id,
            recipient_id: None,
            recipient_nick: "alice".into(),
            recipient_label: "alice".into(),
            sender_id: "me".into(),
            sender_display: "me".into(),
            message: message.into(),
            created_at: 0,
        };
        let memos = (1..=7)
            .map(|id| {
                memo(
                    id,
                    if id == 7 {
                        "a rather long message that keeps going"
                    } else {
                        "hi"
                    },
                )
            })
            .collect::<Vec<_>>();
        let refs = memos.iter().collect::<Vec<_>>();
        let list = sent_list(&refs, 3_600);
        assert!(
            list.starts_with("#7 to alice 1 hour ago: a rather long message that kee…"),
            "{list}"
        );
        assert!(list.ends_with("+2 more"), "{list}");
    }

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
            honorific: String::new(),
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
