//! Channel-local karma for rustjeeves.
//!
//! `nick++` / `nick--` adjusts a per-channel score, as the last word of a line or as the first word
//! followed by a reason (`bob++ for fixing the build`). Votes are announced in the channel (the
//! `announce` setting, rate-limited). `!karma [nick]` shows a score; `!karma top` / `!karma bottom`
//! the leaderboard; `!karma reasons <nick>` recent reasons; `!karma given` whom you've upvoted. Scores are keyed on stable
//! profile UUIDs so nick changes preserve karma, and only nicks with an existing profile can be
//! karma'd — which doubles as a false-positive filter (`C++` won't karma a nick called "C" because
//! no one has that profile).
//!
//! State: one KV blob per `(server, channel)` holding the ledger, plus a small per-channel
//! cooldown map. Lifecycle hooks export and delete a subject's owned scores across all their
//! channels on a server.

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, SendMessage, SettingGet,
    SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

const DEFAULT_COOLDOWN_SECONDS: i64 = 60;
const LEADERBOARD_SIZE: usize = 5;
const MAX_LEDGER_ENTRIES: usize = 5_000;
const MAX_COOLDOWN_ENTRIES: usize = 10_000;
const MAX_REASONS: usize = 5;
const MAX_REASON_CHARS: usize = 80;
/// Distinct people one voter's `!karma given` tally remembers per channel.
const MAX_GIVEN_TARGETS: usize = 100;
/// At most this many announcements per channel per minute; the rest apply silently.
const ANNOUNCEMENTS_PER_MINUTE: usize = 6;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("kind_word", "A Kind Word", 1),
        ("patron_merit", "Patron of Merit", 25),
        ("rising_tide", "A Rising Tide", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Give {threshold} positive karma votes."),
        stat: "positive_given".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.extend(
        [
            ("well_regarded", "Well Regarded", "received_10"),
            ("local_institution", "Local Institution", "received_50"),
        ]
        .into_iter()
        .map(|(id, name, stat)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: if stat == "received_10" {
                "Reach a received karma score of +10.".into()
            } else {
                "Reach a received karma score of +50.".into()
            },
            stat: stat.into(),
            threshold: 1,
            optional: true,
            secret: false,
        }),
    );
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: ["positive_given", "received_10", "received_50"]
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

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("karma:{}:", encode(&request.server));
    let mut scores = HashMap::<String, i64>::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix) && !entry.value.is_empty())
    {
        for (id, value) in serde_json::from_str::<Ledger>(&entry.value)?.entries {
            *scores.entry(id).or_default() = scores
                .get(&id)
                .copied()
                .unwrap_or(0)
                .saturating_add(value.score);
        }
    }
    let values = scores
        .into_iter()
        .flat_map(|(profile_id, score)| {
            [("received_10", score >= 10), ("received_50", score >= 50)]
                .into_iter()
                .filter(|(_, earned)| *earned)
                .map(move |(stat, _)| AchievementSetMax {
                    profile_id: profile_id.clone(),
                    stat: stat.into(),
                    value: 1,
                })
        })
        .collect();
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values,
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display: &str,
    channel: &str,
    stats: Vec<&str>,
) -> Result<(), Error> {
    if profile_id.is_empty() || stats.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display.into(),
            target: channel.into(),
            increments: stats
                .into_iter()
                .map(|stat| StatIncrement {
                    stat: stat.into(),
                    amount: 1,
                })
                .collect(),
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

// ── exports ─────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "karma".into(),
            aliases: Vec::new(),
            description: "Karma scores, leaderboards, reasons, and whom you've upvoted. Vote with nick++ or nick-- (first word with a reason, or last word)."
                .into(),
            usage: "!karma [nick | top | bottom | reasons <nick> | given]".into(),
            ..Default::default()
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "cooldown_seconds".into(),
                description: "Minimum delay before a voter can karma the same nick again.".into(),
                default: DEFAULT_COOLDOWN_SECONDS.to_string(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "announce".into(),
                description:
                    "Confirm votes in the channel (\"bob → 12\"), a few per minute at most.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── data model ──────────────────────────────────────────────────────────────

#[derive(Clone, Default, Serialize, Deserialize)]
struct Entry {
    nick: String,
    score: i64,
    /// Newest last; shown without voters, kept with them so erasure can remove a voter's words.
    #[serde(default)]
    reasons: Vec<Reason>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Reason {
    voter: String,
    up: bool,
    text: String,
    at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    /// profile_id -> entry. Keyed on the stable UUID so nick changes preserve karma.
    entries: HashMap<String, Entry>,
    /// voter profile_id -> target profile_id -> positive votes given, for `!karma given`.
    #[serde(default)]
    given: HashMap<String, HashMap<String, u64>>,
}

#[derive(Default, Serialize, Deserialize)]
struct Cooldowns {
    /// (voter_id, target_id) -> last-vote timestamp. Pruned of expired entries on each write.
    votes: HashMap<String, i64>,
    /// Pairs already told about their cooldown, with the vote the notice was about.
    #[serde(default)]
    warned: HashMap<String, i64>,
}

/// A cooldown key pairs a voter and target so a voter can freely karma different people.
fn cooldown_pair(voter: &str, target: &str) -> String {
    format!("{voter}\x1f{target}")
}

// ── trigger parsing (pure, unit-tested) ─────────────────────────────────────

/// The karma operation extracted from a token suffix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Up,
    Down,
}

/// A vote in `text`: the first word (`bob++ for fixing the build`, with the rest as its reason)
/// or the last word (`nice one bob++`, no reason). Mid-sentence `C++` / `x++` in pasted code
/// never fires.
fn parse_karma_vote(text: &str) -> Option<(&str, Op, Option<String>)> {
    let first = text.split_whitespace().next()?;
    if let Some((nick, op)) = karma_token(first) {
        let reason = clean_reason(text.trim_start()[first.len()..].trim());
        return Some((nick, op, reason));
    }
    let (nick, op) = karma_token(text.split_whitespace().last()?)?;
    Some((nick, op, None))
}

fn karma_token(token: &str) -> Option<(&str, Op)> {
    let token = token.trim_end_matches([',', ':', '!', '.']);
    let (nick, op) = token
        .strip_suffix("++")
        .map(|n| (n, Op::Up))
        .or_else(|| token.strip_suffix("--").map(|n| (n, Op::Down)))?;
    looks_like_nick(nick).then_some((nick, op))
}

/// Printable, single-line, and short; nothing at all becomes no reason.
fn clean_reason(text: &str) -> Option<String> {
    let text = text
        .chars()
        .filter(|ch| !ch.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_REASON_CHARS {
        return Some(text);
    }
    let cut = text.chars().take(MAX_REASON_CHARS - 1).collect::<String>();
    Some(format!("{}…", cut.trim_end()))
}

/// A candidate nick must be non-empty and not start with punctuation, a channel prefix, or a URL
/// scheme. This is the first line of false-positive defence; the real-profile check at runtime is
/// the second.
fn looks_like_nick(candidate: &str) -> bool {
    if candidate.is_empty() {
        return false;
    }
    // Reject URL schemes (http://, https://) and path-like fragments outright.
    if candidate.starts_with("http") || candidate.contains("://") || candidate.contains('/') {
        return false;
    }
    candidate
        .chars()
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '\\')
}

// ── leaderboard (pure, unit-tested) ─────────────────────────────────────────

/// Sort entries for a top (highest-first) or bottom (lowest-first) leaderboard, truncated.
fn leaderboard(ledger: &Ledger, top: bool) -> Vec<(String, String, i64)> {
    let mut rows: Vec<(String, String, i64)> = ledger
        .entries
        .iter()
        .map(|(id, e)| (id.clone(), e.nick.clone(), e.score))
        .collect();
    if top {
        rows.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    } else {
        rows.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.1.cmp(&b.1)));
    }
    rows.truncate(LEADERBOARD_SIZE);
    rows
}

// ── host helpers ────────────────────────────────────────────────────────────

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|v| (*v).into()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
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

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse()?)
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

#[cfg(target_arch = "wasm32")]
fn fold_nick(server: &str, nick: &str) -> Result<String, Error> {
    Ok(unsafe {
        irc_casefold(serde_json::to_string(&IrcCasefold {
            server: server.into(),
            value: nick.into(),
        })?)
    }?)
}

#[cfg(not(target_arch = "wasm32"))]
fn fold_nick(_server: &str, nick: &str) -> Result<String, Error> {
    Ok(nick.to_ascii_lowercase())
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

fn ledger_key(server: &str, channel: &str) -> String {
    format!("karma:{}:{}", encode(server), encode(channel))
}

fn cooldown_key(server: &str, channel: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(channel))
}

fn load_ledger(server: &str, channel: &str) -> Result<Ledger, Error> {
    let raw = kv_read(&ledger_key(server, channel))?;
    if raw.is_empty() {
        Ok(Ledger::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn load_cooldowns(server: &str, channel: &str) -> Result<Cooldowns, Error> {
    let raw = kv_read(&cooldown_key(server, channel))?;
    if raw.is_empty() {
        Ok(Cooldowns::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
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

fn cooldown_seconds(server: &str, channel: &str) -> Result<i64, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "cooldown_seconds".into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    Ok(raw.parse().unwrap_or(DEFAULT_COOLDOWN_SECONDS))
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    if msg.is_private {
        return Ok(()); // karma is a channel-social feature
    }
    let server = env.server.as_str();
    let channel = msg.target.as_str();
    let text = msg.text.trim();

    // Never persist state under a missing or unstable identity.
    if msg.user_id.is_empty() {
        return Ok(());
    }

    // Command path.
    if text.starts_with('!') {
        let mut parts = text.splitn(2, char::is_whitespace);
        if parts.next() == Some("!karma") {
            let arg = parts.next().unwrap_or("").trim();
            return Ok(handle_command(server, channel, &msg, arg)?);
        }
        // Some other command; don't try to parse karma from it (e.g. "!tell aureate++ for that").
        return Ok(());
    }

    // Passive path: the first or last word may be a nick++/nick--.
    let Some((target_nick, op, reason)) = parse_karma_vote(text) else {
        return Ok(());
    };
    apply_karma(server, channel, &msg, target_nick, op, reason)?;
    Ok(())
}

fn handle_command(
    server: &str,
    channel: &str,
    msg: &jeeves_abi::MessagePayload,
    arg: &str,
) -> Result<(), Error> {
    let caller: &str = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    match arg.split_whitespace().next().unwrap_or("") {
        "" => {
            // Own score.
            let ledger = load_ledger(server, channel)?;
            match ledger.entries.get(&msg.user_id).map(|e| e.score) {
                Some(s) => reply(
                    server,
                    channel,
                    &themed(
                        "karma.own_score",
                        &["You have {score} karma in {channel}, {user}."],
                        &[
                            ("user", caller),
                            ("score", &s.to_string()),
                            ("channel", channel),
                        ],
                    )?,
                ),
                None => reply(
                    server,
                    channel,
                    &themed(
                        "karma.own_none",
                        &["You don't have any karma in {channel} yet, {user}."],
                        &[("user", caller), ("channel", channel)],
                    )?,
                ),
            }
        }
        "top" => show_leaderboard(server, channel, true),
        "bottom" => show_leaderboard(server, channel, false),
        "given" => show_given(server, channel, msg, caller),
        "reasons" | "why" => {
            let target = arg.split_whitespace().nth(1).unwrap_or("");
            show_reasons(server, channel, caller, target)
        }
        target => {
            // Someone else's score. Resolve to a profile so we read the stable-id-keyed entry.
            let Some(p) = profile(server, target)? else {
                return reply(
                    server,
                    channel,
                    &themed(
                        "karma.unknown",
                        &["{user}, I don't know who '{target}' is."],
                        &[("user", caller), ("target", target)],
                    )?,
                );
            };
            let ledger = load_ledger(server, channel)?;
            let display = display_nick(target, &p);
            match ledger.entries.get(&p.id).map(|e| e.score) {
                Some(s) => reply(
                    server,
                    channel,
                    &themed(
                        "karma.score",
                        &["{target} has {score} karma in {channel}."],
                        &[
                            ("target", &display),
                            ("score", &s.to_string()),
                            ("channel", channel),
                        ],
                    )?,
                ),
                None => reply(
                    server,
                    channel,
                    &themed(
                        "karma.none",
                        &["{target} has no karma in {channel} yet."],
                        &[("target", &display), ("channel", channel)],
                    )?,
                ),
            }
        }
    }
}

/// Break every word of a name with a zero-width space after its first character, so listing
/// someone on a leaderboard doesn't highlight (ping) them. Display names may carry a title
/// ("sir aureate"), so each word is broken rather than just the first.
fn no_highlight(name: &str) -> String {
    name.split(' ')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => format!("{first}\u{200B}{}", chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn show_leaderboard(server: &str, channel: &str, top: bool) -> Result<(), Error> {
    let ledger = load_ledger(server, channel)?;
    if ledger.entries.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "karma.empty",
                &["No karma recorded in {channel} yet."],
                &[("channel", channel)],
            )?,
        );
    }
    let rows = leaderboard(&ledger, top);
    let list = rows
        .iter()
        .map(|(_, nick, score)| format!("{} ({score})", no_highlight(nick)))
        .collect::<Vec<_>>()
        .join(", ");
    let key = if top { "karma.top" } else { "karma.bottom" };
    let default = if top {
        "Top karma in {channel}: {list}"
    } else {
        "Lowest karma in {channel}: {list}"
    };
    reply(
        server,
        channel,
        &themed(key, &[default], &[("channel", channel), ("list", &list)])?,
    )
}

/// Resolve a profile for the target, run the self-karma and cooldown checks, then apply the vote.
fn apply_karma(
    server: &str,
    channel: &str,
    msg: &jeeves_abi::MessagePayload,
    target_nick: &str,
    op: Op,
    reason: Option<String>,
) -> Result<(), Error> {
    let caller: &str = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    // Real-nicks-only: no profile means we silently ignore (avoid spamming "unknown" on every C++).
    let Some(target_profile) = profile(server, target_nick)? else {
        return Ok(());
    };
    // Self-karma check (case-insensitive via the network's casemapping).
    if same_user(server, &msg.nick, target_nick)? || msg.user_id == target_profile.id {
        return reply(
            server,
            channel,
            &themed(
                "karma.self",
                &["{user}, you can't karma yourself."],
                &[("user", caller)],
            )?,
        );
    }
    let mut ledger = load_ledger(server, channel)?;
    if !ledger.entries.contains_key(&target_profile.id)
        && ledger.entries.len() >= MAX_LEDGER_ENTRIES
    {
        return reply(
            server,
            channel,
            &themed(
                "karma.ledger_full",
                &["Karma storage for {channel} is full; ask an operator for help."],
                &[("channel", channel)],
            )?,
        );
    }
    let now = timestamp()?;
    let voter_id = &msg.user_id;
    // Cooldown check.
    let mut cooldowns = load_cooldowns(server, channel)?;
    let window = cooldown_seconds(server, channel)?;
    let pair = cooldown_pair(voter_id, &target_profile.id);
    if let Some(&last) = cooldowns.votes.get(&pair) {
        if now - last < window {
            // Say so once per throttled vote, then stay quiet so repeats can't spam.
            if announce_enabled(server, channel)? && cooldowns.warned.get(&pair) != Some(&last) {
                cooldowns.warned.insert(pair, last);
                kv_write(
                    &cooldown_key(server, channel),
                    &serde_json::to_string(&cooldowns)?,
                )?;
                let target = display_nick(target_nick, &target_profile);
                return reply(
                    server,
                    channel,
                    &themed(
                        "karma.cooldown",
                        &["You've karma'd {target} recently, {user}; that one didn't count."],
                        &[("user", caller), ("target", &no_highlight(&target))],
                    )?,
                );
            }
            return Ok(());
        }
    }
    // Prune expired cooldowns so the map stays small.
    cooldowns
        .votes
        .retain(|_, ts| now - *ts < window.max(DEFAULT_COOLDOWN_SECONDS));
    let votes = &cooldowns.votes;
    cooldowns
        .warned
        .retain(|pair, last| votes.get(pair) == Some(last));
    if !cooldowns.votes.contains_key(&pair) && cooldowns.votes.len() >= MAX_COOLDOWN_ENTRIES {
        if let Some(oldest) = cooldowns
            .votes
            .iter()
            .min_by_key(|(_, timestamp)| *timestamp)
            .map(|(key, _)| key.clone())
        {
            cooldowns.votes.remove(&oldest);
        }
    }
    cooldowns.votes.insert(pair, now);
    kv_write(
        &cooldown_key(server, channel),
        &serde_json::to_string(&cooldowns)?,
    )?;
    // Apply the vote.
    let entry = ledger.entries.entry(target_profile.id.clone()).or_default();
    entry.nick = display_nick(target_nick, &target_profile);
    entry.score = entry.score.saturating_add(match op {
        Op::Up => 1,
        Op::Down => -1,
    });
    if let Some(text) = &reason {
        entry.reasons.push(Reason {
            voter: voter_id.clone(),
            up: op == Op::Up,
            text: text.clone(),
            at: now,
        });
        let excess = entry.reasons.len().saturating_sub(MAX_REASONS);
        entry.reasons.drain(..excess);
    }
    let score = entry.score;
    let target_name = entry.nick.clone();
    if op == Op::Up {
        let tally = ledger.given.entry(voter_id.clone()).or_default();
        if tally.contains_key(&target_profile.id) || tally.len() < MAX_GIVEN_TARGETS {
            *tally.entry(target_profile.id.clone()).or_default() += 1;
        }
    }
    kv_write(
        &ledger_key(server, channel),
        &serde_json::to_string(&ledger)?,
    )?;
    if announce_enabled(server, channel)? && announcement_allowed(server, channel, now) {
        let target = no_highlight(&target_name);
        let score = score.to_string();
        let text = match &reason {
            Some(reason) => themed(
                "karma.announce_reason",
                &["{target} → {score} ({reason})"],
                &[("target", &target), ("score", &score), ("reason", reason)],
            )?,
            None => themed(
                "karma.announce",
                &["{target} → {score}"],
                &[("target", &target), ("score", &score)],
            )?,
        };
        reply(server, channel, &text)?;
    }
    // Award only after the vote is saved, using the new score so the vote that reaches +10 is
    // the one that unlocks the milestone.
    if op == Op::Up {
        award(server, voter_id, caller, channel, vec!["positive_given"])?;
        let mut received = Vec::new();
        if score >= 10 {
            received.push("received_10");
        }
        if score >= 50 {
            received.push("received_50");
        }
        award(
            server,
            &target_profile.id,
            &target_profile.nick,
            channel,
            received,
        )?;
    }
    Ok(())
}

fn announce_enabled(server: &str, channel: &str) -> Result<bool, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "announce".into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    Ok(raw != "false")
}

thread_local! {
    /// Recent announcement times per (server, channel); memory only, so a reload resets it.
    static ANNOUNCED: std::cell::RefCell<HashMap<(String, String), Vec<i64>>> =
        std::cell::RefCell::new(HashMap::new());
}

fn announcement_allowed(server: &str, channel: &str, now: i64) -> bool {
    ANNOUNCED.with(|announced| {
        let mut announced = announced.borrow_mut();
        let times = announced
            .entry((server.to_string(), channel.to_ascii_lowercase()))
            .or_default();
        times.retain(|at| now - at < 60);
        if times.len() >= ANNOUNCEMENTS_PER_MINUTE {
            return false;
        }
        times.push(now);
        true
    })
}

fn show_reasons(server: &str, channel: &str, caller: &str, target: &str) -> Result<(), Error> {
    if target.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "karma.reasons_usage",
                &["Whose reasons, {user}? !karma reasons <nick>"],
                &[("user", caller)],
            )?,
        );
    }
    let Some(p) = profile(server, target)? else {
        return reply(
            server,
            channel,
            &themed(
                "karma.unknown",
                &["{user}, I don't know who '{target}' is."],
                &[("user", caller), ("target", target)],
            )?,
        );
    };
    let ledger = load_ledger(server, channel)?;
    let display = no_highlight(&display_nick(target, &p));
    let reasons = ledger
        .entries
        .get(&p.id)
        .map(|entry| format_reasons(&entry.reasons))
        .unwrap_or_default();
    if reasons.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "karma.no_reasons",
                &["No reasons recorded for {target} in {channel} yet."],
                &[("target", &display), ("channel", channel)],
            )?,
        );
    }
    reply(
        server,
        channel,
        &themed(
            "karma.reasons",
            &["Why {target} has karma: {reasons}"],
            &[("target", &display), ("reasons", &reasons)],
        )?,
    )
}

/// Newest first: "+fixing the build · −breaking prod".
fn format_reasons(reasons: &[Reason]) -> String {
    reasons
        .iter()
        .rev()
        .map(|reason| format!("{}{}", if reason.up { "+" } else { "−" }, reason.text))
        .collect::<Vec<_>>()
        .join(" · ")
}

fn show_given(
    server: &str,
    channel: &str,
    msg: &jeeves_abi::MessagePayload,
    caller: &str,
) -> Result<(), Error> {
    let ledger = load_ledger(server, channel)?;
    let list = given_list(&ledger, &msg.user_id);
    if list.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "karma.given_none",
                &["You haven't given anyone karma in {channel} yet, {user}."],
                &[("user", caller), ("channel", channel)],
            )?,
        );
    }
    reply(
        server,
        channel,
        &themed(
            "karma.given",
            &["{user}, your karma in {channel} has gone most to: {list}"],
            &[("user", caller), ("channel", channel), ("list", &list)],
        )?,
    )
}

/// "b​ob (12), c​arol (5)" — the voter's most-upvoted people, without pinging them.
fn given_list(ledger: &Ledger, voter: &str) -> String {
    let Some(tally) = ledger.given.get(voter) else {
        return String::new();
    };
    let mut rows = tally
        .iter()
        .map(|(target, count)| {
            let nick = ledger
                .entries
                .get(target)
                .map_or("someone", |entry| entry.nick.as_str());
            (nick, *count)
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(right.0)));
    rows.iter()
        .take(LEADERBOARD_SIZE)
        .map(|(nick, count)| format!("{} ({count})", no_highlight(nick)))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether two nicks refer to the same user on a network, using its negotiated casemapping.
fn same_user(server: &str, a: &str, b: &str) -> Result<bool, Error> {
    Ok(fold_nick(server, a)? == fold_nick(server, b)?)
}

/// Prefer the profile's title-prefixed display when available; fall back to the typed nick.
fn display_nick(typed: &str, p: &Profile) -> String {
    if !p.nick.is_empty() {
        p.nick.clone()
    } else {
        typed.to_string()
    }
}

// ── lifecycle hooks ─────────────────────────────────────────────────────────

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let server = encode(&request.subject.server);
    let ledger_prefix = format!("karma:{server}:");
    let cooldown_prefix = format!("cooldown:{server}:");
    let subject_ids = lifecycle_subject_ids(&request)?;
    let mut channels = Vec::new();
    let mut cooldowns = Vec::new();
    for entry in &request.entries {
        if let Some(encoded_channel) = entry.key.strip_prefix(&ledger_prefix) {
            let ledger: Ledger = serde_json::from_str(&entry.value)?;
            let channel = decode_hex(encoded_channel)
                .ok_or_else(|| Error::msg("malformed karma channel key"))?;
            for (owner, e) in subject_ids
                .iter()
                .filter_map(|id| ledger.entries.get(id).map(|entry| (id, entry)))
            {
                channels.push(serde_json::json!({
                    "channel": channel,
                    "owner": owner,
                    "nick": e.nick,
                    "score": e.score,
                    "reasons_received": e.reasons.iter().map(|reason| &reason.text).collect::<Vec<_>>(),
                }));
            }
            let given_reasons = ledger
                .entries
                .values()
                .flat_map(|entry| entry.reasons.iter())
                .filter(|reason| subject_ids.contains(&reason.voter))
                .collect::<Vec<_>>();
            let tallies = subject_ids
                .iter()
                .filter_map(|id| ledger.given.get(id))
                .collect::<Vec<_>>();
            if !given_reasons.is_empty() || !tallies.is_empty() {
                channels.push(serde_json::json!({
                    "channel": channel,
                    "reasons_given": given_reasons,
                    "karma_given": tallies,
                }));
            }
        } else if let Some(encoded_channel) = entry.key.strip_prefix(&cooldown_prefix) {
            let state: Cooldowns = serde_json::from_str(&entry.value)?;
            let channel = decode_hex(encoded_channel)
                .ok_or_else(|| Error::msg("malformed karma cooldown channel key"))?;
            for (pair, last_vote) in state.votes {
                let Some((voter, target)) = pair.split_once('\x1f') else {
                    return Err(Error::msg("malformed karma cooldown identity").into());
                };
                if subject_ids.contains(voter) || subject_ids.contains(target) {
                    cooldowns.push(serde_json::json!({
                        "channel": channel,
                        "voter": voter,
                        "target": target,
                        "last_vote": last_vote,
                    }));
                }
            }
        }
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if channels.is_empty() && cooldowns.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "karma": channels, "cooldowns": cooldowns })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let server = encode(&request.subject.server);
    let ledger_prefix = format!("karma:{server}:");
    let cooldown_prefix = format!("cooldown:{server}:");
    let subject_ids = lifecycle_subject_ids(&request)?;
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if entry.key.starts_with(&ledger_prefix) {
            let mut ledger: Ledger = serde_json::from_str(&entry.value)?;
            if remove_subject_from_ledger(&mut ledger, &subject_ids) {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: if ledger.entries.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&ledger)?)
                    },
                });
            }
        } else if entry.key.starts_with(&cooldown_prefix) {
            let mut state: Cooldowns = serde_json::from_str(&entry.value)?;
            if remove_subject_from_cooldowns(&mut state, &subject_ids)? {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: if state.votes.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&state)?)
                    },
                });
            }
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

fn lifecycle_subject_ids(request: &ModuleDataRequest) -> Result<HashSet<String>, Error> {
    let mut ids = HashSet::from([request.subject.profile_id.clone()]);
    for alias in &request.aliases {
        ids.insert(format!(
            "nick:{}",
            fold_nick(&request.subject.server, alias)?
        ));
    }
    Ok(ids)
}

/// Remove the subject's own scores, the reasons they gave others, and their `given` tallies
/// (as voter and as target).
fn remove_subject_from_ledger(ledger: &mut Ledger, subject_ids: &HashSet<String>) -> bool {
    let mut changed = false;
    let before = ledger.entries.len();
    ledger.entries.retain(|id, _| !subject_ids.contains(id));
    changed |= ledger.entries.len() != before;
    for entry in ledger.entries.values_mut() {
        let before = entry.reasons.len();
        entry
            .reasons
            .retain(|reason| !subject_ids.contains(&reason.voter));
        changed |= entry.reasons.len() != before;
    }
    let before = ledger.given.len();
    ledger.given.retain(|voter, _| !subject_ids.contains(voter));
    changed |= ledger.given.len() != before;
    for tally in ledger.given.values_mut() {
        let before = tally.len();
        tally.retain(|target, _| !subject_ids.contains(target));
        changed |= tally.len() != before;
    }
    ledger.given.retain(|_, tally| !tally.is_empty());
    changed
}

fn remove_subject_from_cooldowns(
    cooldowns: &mut Cooldowns,
    subject_ids: &HashSet<String>,
) -> Result<bool, Error> {
    let mut remove = Vec::new();
    for pair in cooldowns.votes.keys() {
        let Some((voter, target)) = pair.split_once('\x1f') else {
            return Err(Error::msg("malformed karma cooldown identity"));
        };
        if subject_ids.contains(voter) || subject_ids.contains(target) {
            remove.push(pair.clone());
        }
    }
    let changed = !remove.is_empty();
    for pair in remove {
        cooldowns.votes.remove(&pair);
        cooldowns.warned.remove(&pair);
    }
    Ok(changed)
}

/// Best-effort hex decode for channel names recovered from KV keys.
fn decode_hex(hex: &str) -> Option<String> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaderboard_names_do_not_highlight() {
        assert_eq!(
            super::no_highlight("sir aureate"),
            "s\u{200B}ir a\u{200B}ureate"
        );
        assert_eq!(super::no_highlight(""), "");
    }

    // ── trigger parsing ─────────────────────────────────────────────────────

    fn vote(text: &str) -> Option<(&str, Op)> {
        parse_karma_vote(text).map(|(nick, op, _)| (nick, op))
    }

    #[test]
    fn first_word_votes_carry_reasons() {
        assert_eq!(
            parse_karma_vote("bob++ for fixing the build"),
            Some(("bob", Op::Up, Some("for fixing the build".into())))
        );
        assert_eq!(
            parse_karma_vote("bob--: broke prod"),
            Some(("bob", Op::Down, Some("broke prod".into())))
        );
        assert_eq!(parse_karma_vote("bob++"), Some(("bob", Op::Up, None)));
        assert_eq!(
            parse_karma_vote("nice work bob++"),
            Some(("bob", Op::Up, None)),
            "last-word votes keep no reason"
        );
        let long = format!("bob++ {}", "x".repeat(200));
        let reason = parse_karma_vote(&long).unwrap().2.unwrap();
        assert!(reason.ends_with('…') && reason.chars().count() == MAX_REASON_CHARS);
    }

    #[test]
    fn given_tallies_and_erasure_cover_voters() {
        let mut ledger = Ledger::default();
        for (id, nick) in [("b", "bob"), ("c", "carol")] {
            ledger.entries.insert(
                id.into(),
                Entry {
                    nick: nick.into(),
                    score: 1,
                    reasons: vec![Reason {
                        voter: "a".into(),
                        up: true,
                        text: "helped".into(),
                        at: 0,
                    }],
                },
            );
        }
        ledger.given.insert(
            "a".into(),
            HashMap::from([("b".to_string(), 3), ("c".to_string(), 7)]),
        );
        assert_eq!(
            given_list(&ledger, "a"),
            "c\u{200B}arol (7), b\u{200B}ob (3)"
        );
        assert_eq!(format_reasons(&ledger.entries["b"].reasons), "+helped");
        assert!(remove_subject_from_ledger(
            &mut ledger,
            &HashSet::from(["a".to_string()])
        ));
        assert!(ledger.given.is_empty());
        assert!(ledger
            .entries
            .values()
            .all(|entry| entry.reasons.is_empty()));
        assert_eq!(ledger.entries.len(), 2, "their targets keep their scores");
    }

    #[test]
    fn parses_plusplus_and_minusminus() {
        assert_eq!(vote("aureate++"), Some(("aureate", Op::Up)));
        assert_eq!(vote("buttbot--"), Some(("buttbot", Op::Down)));
    }

    #[test]
    fn karma_token_must_be_last() {
        assert_eq!(vote("thanks aureate++ nice work"), None);
        assert_eq!(vote("nice work aureate++"), Some(("aureate", Op::Up)));
    }

    #[test]
    fn mid_sentence_plusplus_is_ignored() {
        // Only the last token is checked, so "C++ is great" does not karma "C".
        assert_eq!(vote("I think C++ is great"), None);
    }

    #[test]
    fn rejects_urls_and_schemes() {
        assert_eq!(vote("http://foo++"), None);
        assert_eq!(vote("check https://bar++"), None);
        assert_eq!(vote("see /path++"), None);
    }

    #[test]
    fn rejects_empty_and_short_nicks() {
        assert_eq!(vote("++"), None);
        assert_eq!(vote("--"), None);
        assert_eq!(vote("hello"), None);
        assert_eq!(vote(""), None);
    }

    // ── leaderboard ─────────────────────────────────────────────────────────

    fn ledger_from(rows: &[(&str, &str, i64)]) -> Ledger {
        Ledger {
            entries: rows
                .iter()
                .map(|(id, nick, score)| {
                    (
                        (*id).into(),
                        Entry {
                            nick: (*nick).into(),
                            score: *score,
                            ..Entry::default()
                        },
                    )
                })
                .collect(),
            ..Ledger::default()
        }
    }

    #[test]
    fn top_leaderboard_sorts_descending_and_truncates() {
        let rows = [
            ("a", "alice", 3),
            ("b", "bob", 10),
            ("c", "carol", 7),
            ("d", "dave", 1),
            ("e", "eve", 10),
            ("f", "frank", 5),
            ("g", "grace", 0),
        ];
        let ledger = ledger_from(&rows);
        let top = leaderboard(&ledger, true);
        assert_eq!(top.len(), LEADERBOARD_SIZE);
        assert_eq!(top[0].2, 10);
        // Tie at 10 broken alphabetically by nick.
        assert_eq!(top[0].1, "bob");
        assert_eq!(top[1].1, "eve");
    }

    #[test]
    fn bottom_leaderboard_sorts_ascending() {
        let ledger = ledger_from(&[("a", "alice", 3), ("b", "bob", -5), ("c", "carol", 7)]);
        let bottom = leaderboard(&ledger, false);
        assert_eq!(bottom[0].1, "bob");
        assert_eq!(bottom[0].2, -5);
    }

    // ── ledger application ──────────────────────────────────────────────────

    #[test]
    fn applying_votes_updates_scores() {
        let mut ledger = Ledger::default();
        let entry = ledger.entries.entry("uuid-1".into()).or_default();
        entry.nick = "alice".into();
        entry.score += 1; // up
        entry.score += 1; // up
        entry.score -= 1; // down
        assert_eq!(ledger.entries["uuid-1"].score, 1);
    }

    #[test]
    fn cooldown_pair_distinguishes_voter_target() {
        let ab = cooldown_pair("v1", "t1");
        let ac = cooldown_pair("v1", "t2");
        assert_ne!(ab, ac);
    }

    // ── lifecycle ───────────────────────────────────────────────────────────

    #[test]
    fn delete_removes_only_the_subject() {
        let mut ledger = ledger_from(&[("subject", "alice", 10), ("other", "bob", 5)]);
        ledger.entries.remove("subject");
        assert!(!ledger.entries.contains_key("subject"));
        assert!(ledger.entries.contains_key("other"));
        assert_eq!(ledger.entries["other"].score, 5);
    }

    #[test]
    fn lifecycle_delete_removes_subject_from_ledger_and_cooldowns() {
        let subject_ids = HashSet::from(["subject".to_string(), "nick:alice".to_string()]);
        let mut ledger = ledger_from(&[
            ("subject", "alice", 10),
            ("nick:alice", "Alice", 2),
            ("other", "bob", 5),
        ]);
        assert!(remove_subject_from_ledger(&mut ledger, &subject_ids));
        assert_eq!(ledger.entries.len(), 1);
        assert!(ledger.entries.contains_key("other"));

        let mut cooldowns = Cooldowns {
            votes: HashMap::from([
                (cooldown_pair("subject", "other"), 10),
                (cooldown_pair("other", "subject"), 11),
                (cooldown_pair("other", "third"), 12),
            ]),
            ..Cooldowns::default()
        };
        assert!(remove_subject_from_cooldowns(&mut cooldowns, &subject_ids).unwrap());
        assert_eq!(cooldowns.votes.len(), 1);
        assert!(cooldowns
            .votes
            .contains_key(&cooldown_pair("other", "third")));
    }

    #[test]
    fn malformed_cooldown_identity_fails_lifecycle_delete() {
        let mut cooldowns = Cooldowns {
            votes: HashMap::from([("not-a-pair".to_string(), 10)]),
            ..Cooldowns::default()
        };
        assert!(remove_subject_from_cooldowns(&mut cooldowns, &HashSet::new()).is_err());
    }

    #[test]
    fn decode_hex_round_trips() {
        assert_eq!(decode_hex(&encode("#chan")), Some("#chan".into()));
        assert_eq!(decode_hex(&encode("libera")), Some("libera".into()));
        assert!(decode_hex("zz").is_none());
    }
}
