//! In-memory buffer of recent channel lines, shared by all modules through the `recent_lines`
//! host function.
//!
//! Several modules need "what was said here in the last few minutes" (bare `!tr`, `s///`
//! corrections). Keeping a copy in each module's KV cost a SQLite write per module per chat line;
//! the host already sees every line, so it keeps one bounded buffer instead. It is deliberately
//! volatile: nothing is persisted, lines older than [`MAX_AGE_SECS`] are dropped, and data erasure
//! purges a subject's lines immediately.

use jeeves_abi::{RecentLine, RecentLinesRequest};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

/// Lines kept per channel.
pub const MAX_LINES_PER_CHANNEL: usize = 100;
/// Oldest line kept, in seconds.
pub const MAX_AGE_SECS: i64 = 3 * 60 * 60;
/// Channels tracked at once; the least recently active is dropped beyond this.
const MAX_CHANNELS: usize = 1_000;
/// Longest text stored for one line.
const MAX_TEXT_CHARS: usize = 500;

pub type SharedRecentLines = Arc<RecentLines>;

#[derive(Default)]
pub struct RecentLines {
    channels: Mutex<HashMap<(String, String), VecDeque<RecentLine>>>,
}

impl RecentLines {
    pub fn shared() -> SharedRecentLines {
        Arc::new(Self::default())
    }

    /// Record one channel line. `channel` should already be the canonical spelling.
    pub fn record(&self, server: &str, channel: &str, mut line: RecentLine) {
        line.text = line
            .text
            .chars()
            .filter(|character| !character.is_control())
            .take(MAX_TEXT_CHARS)
            .collect();
        let mut channels = self.channels.lock().unwrap();
        let key = (server.to_string(), channel.to_string());
        if !channels.contains_key(&key) && channels.len() >= MAX_CHANNELS {
            if let Some(stalest) = channels
                .iter()
                .min_by_key(|(_, lines)| lines.back().map_or(i64::MIN, |line| line.timestamp))
                .map(|(key, _)| key.clone())
            {
                channels.remove(&stalest);
            }
        }
        let lines = channels.entry(key).or_default();
        let cutoff = line.timestamp.saturating_sub(MAX_AGE_SECS);
        while lines.front().is_some_and(|old| old.timestamp < cutoff) {
            lines.pop_front();
        }
        if lines.len() >= MAX_LINES_PER_CHANNEL {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// The newest matching lines, oldest first.
    pub fn query(&self, request: &RecentLinesRequest, now: i64) -> Vec<RecentLine> {
        let max_age = request.max_age_seconds.clamp(0, MAX_AGE_SECS);
        let limit = request.limit.min(MAX_LINES_PER_CHANNEL);
        let channels = self.channels.lock().unwrap();
        let Some(lines) = channels.get(&(request.server.clone(), request.channel.clone())) else {
            return Vec::new();
        };
        let mut selected = lines
            .iter()
            .rev()
            .filter(|line| now.saturating_sub(line.timestamp) <= max_age)
            .filter(|line| !request.exclude_commands || !line.is_command)
            .filter(|line| {
                request
                    .user_id
                    .as_deref()
                    .is_none_or(|user_id| line.user_id == user_id)
            })
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();
        selected.reverse();
        selected
    }

    /// Erasure: drop every buffered line from this profile (by id, or by a legacy nick alias).
    pub fn purge(&self, server: &str, profile_id: &str, nicks: &[String]) {
        let mut channels = self.channels.lock().unwrap();
        for ((line_server, _), lines) in channels.iter_mut() {
            if line_server != server {
                continue;
            }
            lines.retain(|line| {
                line.user_id != profile_id
                    && !nicks
                        .iter()
                        .any(|nick| nick.eq_ignore_ascii_case(&line.nick))
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(user_id: &str, text: &str, timestamp: i64, is_command: bool) -> RecentLine {
        RecentLine {
            user_id: user_id.into(),
            nick: user_id.into(),
            display: user_id.into(),
            text: text.into(),
            timestamp,
            is_command,
        }
    }

    fn request(user_id: Option<&str>, limit: usize) -> RecentLinesRequest {
        RecentLinesRequest {
            server: "net".into(),
            channel: "#c".into(),
            limit,
            max_age_seconds: MAX_AGE_SECS,
            user_id: user_id.map(str::to_string),
            exclude_commands: true,
        }
    }

    #[test]
    fn keeps_recent_lines_bounded_and_filterable() {
        let recent = RecentLines::default();
        for index in 0..(MAX_LINES_PER_CHANNEL as i64 + 5) {
            recent.record(
                "net",
                "#c",
                line("a", &format!("line {index}"), 1_000 + index, false),
            );
        }
        recent.record("net", "#c", line("b", "!tr", 2_000, true));
        recent.record("net", "#c", line("b", "bonjour", 2_001, false));

        let all = recent.query(&request(None, 3), 2_001);
        assert_eq!(
            all.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["line 103", "line 104", "bonjour"]
        );
        let mine = recent.query(&request(Some("b"), 10), 2_001);
        assert_eq!(mine.len(), 1, "commands are excluded");
        assert!(recent
            .query(&request(None, 10), 2_001 + MAX_AGE_SECS + 1)
            .is_empty());
    }

    #[test]
    fn purge_removes_a_subject_everywhere_on_that_network() {
        let recent = RecentLines::default();
        recent.record("net", "#c", line("a", "mine", 1, false));
        recent.record("net", "#d", line("legacy", "old nick", 1, false));
        recent.record("other", "#c", line("a", "other network", 1, false));
        recent.purge("net", "a", &["LEGACY".into()]);
        assert!(recent.query(&request(None, 10), 1).is_empty());
        let legacy_channel = RecentLinesRequest {
            channel: "#d".into(),
            ..request(None, 10)
        };
        assert!(recent.query(&legacy_channel, 1).is_empty());
        let other = RecentLinesRequest {
            server: "other".into(),
            ..request(None, 10)
        };
        assert_eq!(recent.query(&other, 1).len(), 1);
    }
}
