//! Channel-local `!seen` and `!quote` commands. Private messages are never recorded.

use extism_pdk::*;
use jeeves_abi::{
    Event, EventEnvelope, KvGet, KvSet, Profile, ProfileKey, Role, SendMessage, ThemeReq,
};
use serde::{Deserialize, Serialize};

const MAX_TEXT_CHARS: usize = 350;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn now(input: String) -> String;
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
    format!("{kind}:{}:{}:{}", encode(server), encode(channel), encode(id))
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

fn load_seen(kind: &str, server: &str, channel: &str, user_id: &str) -> Result<Option<SeenRecord>, Error> {
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
        Ok(QuoteBook { next_id: 1, quotes: Vec::new() })
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
    let command = text.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    if msg.is_private {
        if matches!(command.as_str(), "!seen" | "!quote") {
            reply(
                &server,
                &msg.nick,
                &themed("channel_only", &["Seen and quotes only work in channels; private messages are never recorded."], &[])?,
            )?;
        }
        return Ok(());
    }

    let now = timestamp()?;
    match command.as_str() {
        "!seen" => handle_seen(&server, &msg.target, text, now)?,
        "!quote" => handle_quote(&server, &msg, text, now)?,
        _ => {}
    }

    let record = SeenRecord {
        user_id: if msg.user_id.is_empty() { format!("nick:{}", msg.nick.to_ascii_lowercase()) } else { msg.user_id.clone() },
        nick: msg.nick.clone(),
        display: if msg.display.is_empty() { msg.nick.clone() } else { msg.display.clone() },
        text: sanitize(text),
        timestamp: now,
    };
    save_seen("seen", &server, &msg.target, &record)?;
    if !text.starts_with('!') && !record.text.is_empty() {
        save_seen("last", &server, &msg.target, &record)?;
    }
    Ok(())
}

fn handle_seen(server: &str, channel: &str, text: &str, now: i64) -> Result<(), Error> {
    let nick = text.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
    if nick.is_empty() {
        return reply(server, channel, &themed("seen_usage", &["Usage: !seen <nick>"], &[])?);
    }
    let Some(target) = profile(server, nick)? else {
        return reply(server, channel, &themed("seen_unknown", &["I haven't seen {target} in this channel."], &[("target", nick)])?);
    };
    let Some(record) = load_seen("seen", server, channel, &target.id)? else {
        return reply(server, channel, &themed("seen_unknown", &["I haven't seen {target} in this channel."], &[("target", nick)])?);
    };
    let ago = relative_time(now.saturating_sub(record.timestamp));
    reply(
        server,
        channel,
        &themed(
            "seen_result",
            &["{target} was last seen {ago}, saying: {text}"],
            &[("target", &record.display), ("ago", &ago), ("text", &record.text)],
        )?,
    )
}

fn handle_quote(server: &str, msg: &jeeves_abi::MessagePayload, text: &str, now: i64) -> Result<(), Error> {
    let channel = msg.target.as_str();
    let arg = text.splitn(2, char::is_whitespace).nth(1).unwrap_or("").trim();
    if arg.is_empty() {
        let book = load_quotes(server, channel)?;
        if book.quotes.is_empty() {
            return reply(server, channel, &themed("quote_empty", &["There are no quotes in this channel yet."], &[])?);
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
    if let Some(rest) = arg.strip_prefix("del ").or_else(|| arg.strip_prefix("delete ")) {
        let Some(id) = parse_quote_id(rest.trim()) else {
            return reply(server, channel, &themed("quote_delete_usage", &["Usage: !quote del #<id>"], &[])?);
        };
        let mut book = load_quotes(server, channel)?;
        let Some(index) = book.quotes.iter().position(|quote| quote.id == id) else {
            return quote_not_found(server, channel, id);
        };
        let quote = &book.quotes[index];
        let requester = stable_id(&msg.user_id, &msg.nick);
        let admin = msg.role.is_some_and(|role| role.satisfies(Role::Admin));
        if !admin && quote.submitted_by != requester && quote.author_id != requester {
            return reply(server, channel, &themed("quote_delete_denied", &["Only the quoted person, submitter, or an admin may delete that quote."], &[])?);
        }
        book.quotes.remove(index);
        save_quotes(server, channel, &book)?;
        let id_text = id.to_string();
        return reply(server, channel, &themed("quote_deleted", &["Deleted quote #{id}."], &[("id", &id_text)])?);
    }

    let (author_id, author, quoted_text) = if let Some(manual) = parse_manual_quote(arg) {
        let id = stable_id(&msg.user_id, &msg.nick);
        let author = if msg.display.is_empty() { msg.nick.clone() } else { msg.display.clone() };
        (id, author, sanitize(manual))
    } else {
        let Some(target) = profile(server, arg)? else {
            return reply(server, channel, &themed("quote_unknown", &["I don't know anyone named {target}."], &[("target", arg)])?);
        };
        let Some(last) = load_seen("last", server, channel, &target.id)? else {
            return reply(server, channel, &themed("quote_no_line", &["I don't have a quotable line from {target} in this channel."], &[("target", arg)])?);
        };
        (last.user_id, last.display, last.text)
    };
    if quoted_text.is_empty() {
        return reply(server, channel, &themed("quote_empty_text", &["That quote is empty."], &[])?);
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
        &themed("quote_saved", &["Saved quote #{id} from {author}: {text}"], &[("id", &id_text), ("author", &author), ("text", &quoted_text)])?,
    )
}

fn show_quote(server: &str, channel: &str, quote: &Quote) -> Result<(), Error> {
    let id = quote.id.to_string();
    reply(
        server,
        channel,
        &themed("quote_result", &["#{id} <{author}> {text}"], &[("id", &id), ("author", &quote.author), ("text", &quote.text)])?,
    )
}

fn quote_not_found(server: &str, channel: &str, id: u64) -> Result<(), Error> {
    let id = id.to_string();
    reply(server, channel, &themed("quote_not_found", &["There is no quote #{id} in this channel."], &[("id", &id)])?)
}

fn stable_id(user_id: &str, nick: &str) -> String {
    if user_id.is_empty() { format!("nick:{}", nick.to_ascii_lowercase()) } else { user_id.into() }
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
        assert_ne!(scoped_key("seen", "a:b", "c", "d"), scoped_key("seen", "a", "b:c", "d"));
    }
}
