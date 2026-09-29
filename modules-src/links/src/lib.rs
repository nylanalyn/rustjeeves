//! Page titles for links posted in chat.
//!
//! Where the `enabled` setting is on (off by default, per channel), links posted in the channel
//! get a short "↳ Title — site" line. YouTube links are left to the youtube module, which shows
//! richer details, and operators can ignore more domains. `!link <url>` looks one up on request
//! anywhere. Fetching happens in the host (`link_title`), which only ever connects to public
//! addresses; a failed passive lookup stays silent.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, LinkTitleRequest, LinkTitleResponse,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, COMMAND_MANIFEST_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{reply, themed, timestamp};
use std::cell::RefCell;
use std::collections::HashMap;

const DEFAULT_IGNORE: &str = "youtube.com, youtu.be";
const DEFAULT_MAX_PER_MESSAGE: i64 = 2;
const DEFAULT_REPEAT_SECONDS: i64 = 30 * 60;
const MAX_REMEMBERED: usize = 500;

#[host_fn]
extern "ExtismHost" {
    fn setting_get(input: String) -> String;
    fn link_title(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "link".into(),
            aliases: Vec::new(),
            description: "Show a web page's title. Channels can also have titles shown for every link posted.".into(),
            usage: "!link <url>".into(),
            ..Default::default()
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all = vec![
        SettingScope::Global,
        SettingScope::Network,
        SettingScope::Channel,
    ];
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Show titles for links posted in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "ignore_domains".into(),
                description: "Domains (and their subdomains) never titled, comma-separated.".into(),
                default: DEFAULT_IGNORE.into(),
                kind: SettingKind::String { max_len: 500 },
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_per_message".into(),
                description: "Most titles shown for one message.".into(),
                default: DEFAULT_MAX_PER_MESSAGE.to_string(),
                kind: SettingKind::Integer { min: 1, max: 3 },
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "repeat_seconds".into(),
                description: "How long before the same link is titled again in a channel.".into(),
                default: DEFAULT_REPEAT_SECONDS.to_string(),
                kind: SettingKind::DurationSeconds {
                    min: 0,
                    max: 86_400,
                },
                scopes: all,
                applies_immediately: true,
            },
        ],
    })?)
}

fn setting(server: &str, channel: &str, key: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    })
}

fn lookup(url: &str) -> Result<LinkTitleResponse, Error> {
    Ok(serde_json::from_str(&unsafe {
        link_title(serde_json::to_string(&LinkTitleRequest {
            url: url.into(),
        })?)?
    })?)
}

thread_local! {
    /// (server, channel, url) → when it was last titled. Memory only; a reload just forgets.
    static TITLED: RefCell<HashMap<(String, String, String), i64>> = RefCell::new(HashMap::new());
}

/// http(s) links in a line, trailing punctuation and wrapping brackets removed, in order.
fn links_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for word in text.split_whitespace() {
        let word = word.trim_start_matches(['<', '(', '[', '"', '\'']);
        let lower = word.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            continue;
        }
        let mut url = word.trim_end_matches(['>', ']', '"', '\'', ',', '.', '!', '?', ';', ':']);
        // Keep a closing parenthesis only when the link itself opened one (wikipedia-style).
        while url.ends_with(')') && url.matches('(').count() < url.matches(')').count() {
            url = &url[..url.len() - 1];
        }
        if url.len() > "https://".len() && !found.iter().any(|seen: &String| seen == url) {
            found.push(url.to_string());
        }
    }
    found
}

fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if host.starts_with('[') {
        host
    } else {
        host.split(':').next().unwrap_or(host)
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// `youtube.com` ignores `youtube.com`, `www.youtube.com`, and `music.youtube.com`.
fn ignored(host: &str, list: &str) -> bool {
    list.split(',')
        .map(|domain| domain.trim().trim_start_matches("*.").to_ascii_lowercase())
        .filter(|domain| !domain.is_empty())
        .any(|domain| host == domain || host.ends_with(&format!(".{domain}")))
}

/// "↳ Title — Site", leaving the site off when the title already names it.
fn render(response: &LinkTitleResponse) -> Result<Option<String>, Error> {
    let Some(title) = response.title.as_deref() else {
        return Ok(None);
    };
    let site = response
        .site
        .clone()
        .or_else(|| response.host.clone())
        .unwrap_or_default();
    let names_site = !site.is_empty() && title.to_lowercase().contains(&site.to_lowercase());
    Ok(Some(if site.is_empty() || names_site {
        themed("links.title_only", &["↳ {title}"], &[("title", title)])?
    } else {
        themed(
            "links.title",
            &["↳ {title} — {site}"],
            &[("title", title), ("site", &site)],
        )?
    }))
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let server = env.server.as_str();
    let text = msg.text.trim();
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    if let Some(rest) = text.strip_prefix("!link") {
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return Ok(());
        }
        let Some(url) = links_in(rest).into_iter().next() else {
            reply(
                server,
                dest,
                &themed(
                    "links.usage",
                    &["Give me a link, {user}: !link https://…"],
                    &[("user", &msg.nick)],
                )?,
            )?;
            return Ok(());
        };
        let response = lookup(&url)?;
        let text = match render(&response)? {
            Some(text) => text,
            None => themed(
                "links.no_title",
                &["I couldn't find a title for that page, {user}."],
                &[("user", &msg.nick)],
            )?,
        };
        reply(server, dest, &text)?;
        return Ok(());
    }
    // Passive titles: channels only, never for commands (another module may be handling them).
    // The host only delivers these lines where the `enabled` setting is on.
    if msg.is_private || text.starts_with('!') {
        return Ok(());
    }
    let links = links_in(text);
    if links.is_empty() {
        return Ok(());
    }
    let channel = msg.target.as_str();
    let ignore = setting(server, channel, "ignore_domains")?;
    let max = setting(server, channel, "max_per_message")?
        .parse()
        .unwrap_or(DEFAULT_MAX_PER_MESSAGE)
        .clamp(1, 3) as usize;
    let repeat = setting(server, channel, "repeat_seconds")?
        .parse()
        .unwrap_or(DEFAULT_REPEAT_SECONDS)
        .max(0);
    let now = timestamp()?;
    for url in links
        .into_iter()
        .filter(|url| !ignored(&host_of(url), &ignore))
        .take(max)
    {
        let key = (
            server.to_string(),
            channel.to_ascii_lowercase(),
            url.clone(),
        );
        let recent = TITLED.with(|titled| {
            titled
                .borrow()
                .get(&key)
                .is_some_and(|at| now - at < repeat)
        });
        if recent {
            continue;
        }
        TITLED.with(|titled| {
            let mut titled = titled.borrow_mut();
            if titled.len() >= MAX_REMEMBERED {
                titled.retain(|_, at| now - *at < repeat);
                if titled.len() >= MAX_REMEMBERED {
                    titled.clear();
                }
            }
            titled.insert(key, now);
        });
        if let Some(text) = render(&lookup(&url)?)? {
            reply(server, channel, &text)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_and_trims_punctuation() {
        assert_eq!(
            links_in("see https://example.com/a, and (https://en.wikipedia.org/wiki/Rust_(programming_language)) ok"),
            [
                "https://example.com/a",
                "https://en.wikipedia.org/wiki/Rust_(programming_language)"
            ]
        );
        assert_eq!(links_in("<http://x.org/p?q=1>."), ["http://x.org/p?q=1"]);
        assert!(links_in("ftp://nope and https:// and plain text").is_empty());
        assert_eq!(links_in("https://a.io https://a.io").len(), 1);
    }

    #[test]
    fn ignores_domains_and_their_subdomains() {
        assert_eq!(
            host_of("https://User@Music.YouTube.com:443/watch?v=1"),
            "music.youtube.com"
        );
        assert!(ignored("music.youtube.com", DEFAULT_IGNORE));
        assert!(ignored("youtu.be", DEFAULT_IGNORE));
        assert!(!ignored("notyoutube.com", DEFAULT_IGNORE));
        assert!(!ignored("example.com", ""));
    }
}
