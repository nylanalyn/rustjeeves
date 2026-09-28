//! Who is in each channel, as seen by the IRC actor.
//!
//! Filled from NAMES replies (on join) and kept current from JOIN, PART, KICK, QUIT, and NICK.
//! Keys are casefolded with the network's casemapping by the caller; the stored nick keeps its
//! display spelling. Exposed read-only to modules through the `channel_members` capability.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

/// Per network: folded channel → (folded nick → nick as displayed).
type Networks = HashMap<String, HashMap<String, BTreeMap<String, String>>>;

static MEMBERS: OnceLock<Mutex<Networks>> = OnceLock::new();

/// Channels larger than this are truncated rather than tracked in full.
const MAX_MEMBERS_PER_CHANNEL: usize = 5_000;

fn with<T>(f: impl FnOnce(&mut Networks) -> T) -> T {
    let mut networks = MEMBERS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    f(&mut networks)
}

/// Strip NAMES status prefixes (`@alice`, `+bob`, `@+carol`, `~dan`).
pub fn strip_prefixes(entry: &str) -> &str {
    entry.trim_start_matches(['~', '&', '@', '%', '+'])
}

pub fn add(server: &str, channel: &str, nick: &str, fold: impl Fn(&str) -> String) {
    if nick.is_empty() {
        return;
    }
    with(|networks| {
        let members = networks
            .entry(server.into())
            .or_default()
            .entry(fold(channel))
            .or_default();
        if members.len() < MAX_MEMBERS_PER_CHANNEL {
            members.insert(fold(nick), nick.into());
        }
    });
}

pub fn remove(server: &str, channel: &str, nick: &str, fold: impl Fn(&str) -> String) {
    with(|networks| {
        if let Some(members) = networks
            .get_mut(server)
            .and_then(|channels| channels.get_mut(&fold(channel)))
        {
            members.remove(&fold(nick));
        }
    });
}

/// A user quit: gone from every channel on the network.
pub fn quit(server: &str, nick: &str, fold: impl Fn(&str) -> String) {
    let folded = fold(nick);
    with(|networks| {
        for members in networks
            .get_mut(server)
            .into_iter()
            .flat_map(|c| c.values_mut())
        {
            members.remove(&folded);
        }
    });
}

pub fn rename(server: &str, old: &str, new: &str, fold: impl Fn(&str) -> String) {
    let (old, new_folded) = (fold(old), fold(new));
    with(|networks| {
        for members in networks
            .get_mut(server)
            .into_iter()
            .flat_map(|c| c.values_mut())
        {
            if members.remove(&old).is_some() {
                members.insert(new_folded.clone(), new.into());
            }
        }
    });
}

/// The bot left or was kicked: forget the channel entirely.
pub fn forget_channel(server: &str, channel: &str, fold: impl Fn(&str) -> String) {
    with(|networks| {
        if let Some(channels) = networks.get_mut(server) {
            channels.remove(&fold(channel));
        }
    });
}

/// A connection (re)started: membership is rebuilt from fresh NAMES replies.
pub fn forget_network(server: &str) {
    with(|networks| {
        networks.remove(server);
    });
}

/// Current members of a channel, as displayed, sorted by folded nick.
pub fn list(server: &str, folded_channel: &str) -> Vec<String> {
    with(|networks| {
        networks
            .get(server)
            .and_then(|channels| channels.get(folded_channel))
            .map(|members| members.values().cloned().collect())
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fold(value: &str) -> String {
        value.to_ascii_lowercase()
    }

    #[test]
    fn tracks_joins_parts_quits_and_renames() {
        let server = "members-test";
        forget_network(server);
        for entry in ["@Alice", "+bob", "@+Carol"] {
            add(server, "#Games", strip_prefixes(entry), fold);
        }
        assert_eq!(list(server, "#games"), ["Alice", "bob", "Carol"]);
        rename(server, "BOB", "Robert", fold);
        remove(server, "#games", "alice", fold);
        assert_eq!(list(server, "#games"), ["Carol", "Robert"]);
        add(server, "#other", "Carol", fold);
        quit(server, "carol", fold);
        assert_eq!(list(server, "#games"), ["Robert"]);
        assert!(list(server, "#other").is_empty());
        forget_channel(server, "#GAMES", fold);
        assert!(list(server, "#games").is_empty());
    }
}
