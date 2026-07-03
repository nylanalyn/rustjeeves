//! Spontaneous animal hunt game for rustjeeves.
//!
//! At random intervals the bot releases a wild animal into an enabled channel.
//! The first person to !hunt or !hug it claims it. Scores are tracked per channel.
//!
//! IMPORTANT: must be explicitly enabled per channel via the `enabled` setting.
//!
//! Commands: !hunt  !hug  !hunt score [nick]  !hunt top  !hunt status  !hunt cancel (admin)
//!
//! Theme keys (all under "hunt.*"):
//!   animals (list — the pool of creatures that appear; change to theme the whole game),
//!   release, caught, hugged, escaped, nothing,
//!   score, no_score, top, top_empty,
//!   status_active, status_next, status_idle, status_disabled,
//!   admin_cancel, admin_cancel_none, cancel_denied

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, RandomBytesRequest, RandomBytesResponse, Role,
    ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind,
    SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

// Default animal pool — operators override "hunt.animals" in theme.toml to change the whole game.
const DEFAULT_ANIMALS: &[&str] = &[
    "cat", "kitten", "puppy", "duck", "rabbit", "squirrel", "hedgehog",
];
const MAX_BOARD_ENTRIES: usize = 500;

// ── host function imports ─────────────────────────────────────────────────────

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = Vec::new();
    for (stat, values) in [
        (
            "hunts",
            [
                ("call_wild", "Call of the Wild", 1),
                ("seasoned_tracker", "Seasoned Tracker", 25),
                ("apex_naturalist", "Apex Naturalist", 100),
            ],
        ),
        (
            "hugs",
            [
                ("soft_touch", "A Soft Touch", 1),
                ("friend_beasts", "Friend to Beasts", 25),
                ("peaceable_kingdom", "The Peaceable Kingdom", 100),
            ],
        ),
    ] {
        achievements.extend(
            values
                .into_iter()
                .map(|(id, name, threshold)| AchievementSpec {
                    id: id.into(),
                    name: name.into(),
                    description: format!("Complete {threshold} {stat}."),
                    stat: stat.into(),
                    threshold,
                    optional: false,
                    secret: false,
                }),
        );
    }
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: ["hunts", "hugs", "claims"]
            .into_iter()
            .map(|id| AchievementStat {
                id: id.into(),
                description: id.into(),
            })
            .collect(),
        achievements,
        prestige: vec![jeeves_abi::PrestigeSpec {
            id: "master_beasts".into(),
            name: "Master of Beasts".into(),
            stat: "claims".into(),
            first_threshold: 200,
            every: 100,
        }],
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("board:{}:", request.server);
    let mut totals = std::collections::BTreeMap::<String, (u64, u64)>::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix) && !entry.value.is_empty())
    {
        for score in serde_json::from_str::<Vec<BoardEntry>>(&entry.value)? {
            if score.user_id.is_empty() {
                continue;
            }
            let total = totals.entry(score.user_id).or_default();
            total.0 += score.hunted as u64;
            total.1 += score.hugged as u64;
        }
    }
    let values = totals
        .into_iter()
        .flat_map(|(profile_id, (hunts, hugs))| {
            [("hunts", hunts), ("hugs", hugs), ("claims", hunts + hugs)]
                .into_iter()
                .map(move |(stat, value)| AchievementSetMax {
                    profile_id: profile_id.clone(),
                    stat: stat.into(),
                    value,
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
    kind: ClaimType,
) -> Result<(), Error> {
    let stat = if matches!(kind, ClaimType::Hunt) {
        "hunts"
    } else {
        "hugs"
    };
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display.into(),
            target: channel.into(),
            increments: vec![
                StatIncrement {
                    stat: stat.into(),
                    amount: 1,
                },
                StatIncrement {
                    stat: "claims".into(),
                    amount: 1,
                },
            ],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn fold_nick(server: &str, nick: &str) -> String {
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
fn fold_nick(_server: &str, nick: &str) -> String {
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

// ── command manifest ──────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let c = |name: &str, desc: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: desc.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            c(
                "hunt",
                "Catch or check scores in the channel animal hunt.",
                "!hunt [score [nick] | top | status | cancel]",
            ),
            c("hug", "Hug the animal instead of catching it.", "!hug"),
        ],
    })?)
}

// ── settings manifest ─────────────────────────────────────────────────────────

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether to release animals spontaneously in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_interval_mins".into(),
                description: "Minimum minutes between animal appearances.".into(),
                default: "60".into(),
                kind: SettingKind::Integer { min: 5, max: 1440 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_interval_mins".into(),
                description: "Maximum minutes between animal appearances.".into(),
                default: "180".into(),
                kind: SettingKind::Integer { min: 5, max: 2880 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "expire_mins".into(),
                description: "Minutes before an unclaimed animal wanders away.".into(),
                default: "10".into(),
                kind: SettingKind::Integer { min: 1, max: 60 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── state structs ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct ActiveEvent {
    animal: String,
    released_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct BoardEntry {
    /// Stable profile UUID. Empty values are legacy display-only entries and are never claimable.
    user_id: String,
    nick: String,
    hunted: u32,
    hugged: u32,
}

// ── job ID helpers (encoded per server+channel to avoid cross-channel cancel) ─

fn next_job_id(server: &str, channel: &str) -> String {
    format!("next:{server}:{channel}")
}

fn expire_job_id(server: &str, channel: &str) -> String {
    format!("expire:{server}:{channel}")
}

// ── KV helpers ────────────────────────────────────────────────────────────────

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn active_key(server: &str, channel: &str) -> String {
    format!("active:{server}:{channel}")
}

fn board_key(server: &str, channel: &str) -> String {
    format!("board:{server}:{channel}")
}

fn load_active(server: &str, channel: &str) -> Result<Option<ActiveEvent>, Error> {
    let raw = kv_load(&active_key(server, channel))?;
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str(&raw).ok())
}

fn clear_active(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&active_key(server, channel), "")
}

fn load_board(server: &str, channel: &str) -> Result<Vec<BoardEntry>, Error> {
    let raw = kv_load(&board_key(server, channel))?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_board(server: &str, channel: &str, board: &[BoardEntry]) -> Result<(), Error> {
    kv_save(&board_key(server, channel), &serde_json::to_string(board)?)
}

fn lifecycle_score_matches(score: &BoardEntry, request: &ModuleDataRequest) -> bool {
    score.user_id == request.subject.profile_id
        || request.aliases.iter().any(|alias| {
            score.user_id.eq_ignore_ascii_case(alias)
                || fold_nick(&request.subject.server, &score.nick)
                    == fold_nick(&request.subject.server, alias)
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let prefix = format!("board:{}:", request.subject.server);
    let mut scores = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let board: Vec<BoardEntry> = serde_json::from_str(&entry.value)?;
        if let Some(score) = board
            .into_iter()
            .find(|score| lifecycle_score_matches(score, &request))
        {
            scores.push(serde_json::json!({ "key": entry.key, "score": score }));
        }
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if scores.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "channel_scores": scores })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let prefix = format!("board:{}:", request.subject.server);
    let mut mutations = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let mut board: Vec<BoardEntry> = serde_json::from_str(&entry.value)?;
        let before = board.len();
        board.retain(|score| !lifecycle_score_matches(score, &request));
        if board.len() != before {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: if board.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&board)?)
                },
            });
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

fn board_index_by_id(board: &[BoardEntry], user_id: &str) -> Option<usize> {
    (!user_id.is_empty())
        .then(|| board.iter().position(|entry| entry.user_id == user_id))
        .flatten()
}

// ── host helpers ──────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    unsafe {
        now(String::new())
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?;
    }
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|s| s.to_string()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn read_setting_raw(key: &str, server: &str, channel: &str) -> Option<String> {
    let raw = unsafe {
        setting_get(
            serde_json::to_string(&SettingGet {
                key: key.into(),
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .ok()?,
        )
        .ok()?
    };
    Some(raw)
}

fn read_setting_bool(key: &str, server: &str, channel: &str, default: bool) -> bool {
    read_setting_raw(key, server, channel)
        .and_then(|s| match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn read_setting_i64(key: &str, server: &str, channel: &str, default: i64) -> i64 {
    read_setting_raw(key, server, channel)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn get_random_bytes(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

fn has_pending_job(server: &str, channel: &str, id: &str) -> bool {
    let raw = unsafe {
        schedule_list(
            serde_json::to_string(&ScheduleList {
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .unwrap_or_default(),
        )
        .unwrap_or_default()
    };
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
    jobs.iter().any(|j| j.id == id)
}

// ── scheduling ────────────────────────────────────────────────────────────────

fn schedule_next(server: &str, channel: &str) -> Result<(), Error> {
    let min_mins = read_setting_i64("min_interval_mins", server, channel, 60);
    let max_mins = read_setting_i64("max_interval_mins", server, channel, 180).max(min_mins + 1);

    let bytes = get_random_bytes(4)?;
    let range = ((max_mins - min_mins) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min_mins * 60 + (r % range) as i64;

    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: next_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

/// Ensure a "next" job is queued for this channel if none is pending and no animal is active.
/// Called lazily on every message in enabled channels so the module bootstraps itself.
fn ensure_scheduled(server: &str, channel: &str) -> Result<(), Error> {
    let nid = next_job_id(server, channel);
    let eid = expire_job_id(server, channel);
    if has_pending_job(server, channel, &nid) || has_pending_job(server, channel, &eid) {
        return Ok(());
    }
    if load_active(server, channel)?.is_some() {
        return Ok(());
    }
    schedule_next(server, channel)
}

fn cancel_expire(server: &str, channel: &str) {
    let _ = unsafe {
        schedule_cancel(
            serde_json::to_string(&ScheduleCancel {
                id: expire_job_id(server, channel),
            })
            .unwrap_or_default(),
        )
    };
}

// ── timer handlers ────────────────────────────────────────────────────────────

fn handle_next(server: &str, channel: &str) -> Result<(), Error> {
    if !read_setting_bool("enabled", server, channel, false) {
        return Ok(());
    }

    // Theme system picks a random entry from the list — operators swap the whole animal pool here.
    let animal = themed("hunt.animals", DEFAULT_ANIMALS, &[])?;

    let active = ActiveEvent {
        animal: animal.to_string(),
        released_at: now_secs(),
    };
    kv_save(
        &active_key(server, channel),
        &serde_json::to_string(&active)?,
    )?;

    let expire_mins = read_setting_i64("expire_mins", server, channel, 10);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: expire_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + expire_mins * 60,
            payload: animal.to_string(),
        })?)?;
    }

    reply(
        server,
        channel,
        &themed(
            "hunt.release",
            &["A wild {animal} appears! Type !hunt to catch it or !hug to befriend it."],
            &[("animal", &animal)],
        )?,
    )?;
    Ok(())
}

fn handle_expire(server: &str, channel: &str) -> Result<(), Error> {
    if let Some(event) = load_active(server, channel)? {
        clear_active(server, channel)?;
        if read_setting_bool("enabled", server, channel, false) {
            reply(
                server,
                channel,
                &themed(
                    "hunt.escaped",
                    &["The {animal} wandered away..."],
                    &[("animal", &event.animal)],
                )?,
            )?;
        }
    }

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next(server, channel)?;
    }
    Ok(())
}

// ── command handlers ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum ClaimType {
    Hunt,
    Hug,
}

fn cmd_claim(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
    claim_type: ClaimType,
) -> Result<(), Error> {
    if user_id.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "hunt.identity_unavailable",
                &["I couldn't verify a stable profile for {nick}; the animal remains unclaimed."],
                &[("nick", display)],
            )?,
        );
    }
    let Some(event) = load_active(server, channel)? else {
        reply(
            server,
            channel,
            &themed(
                "hunt.nothing",
                &["There's nothing here right now. Wait for an animal to appear."],
                &[],
            )?,
        )?;
        return Ok(());
    };

    let mut board = load_board(server, channel)?;
    let idx = board_index_by_id(&board, user_id);
    if idx.is_none() && board.len() >= MAX_BOARD_ENTRIES {
        return reply(
            server,
            channel,
            &themed(
                "hunt.board_full",
                &["The hunt board is full; the animal remains unclaimed."],
                &[],
            )?,
        );
    }

    let animal = event.animal.clone();
    cancel_expire(server, channel);
    clear_active(server, channel)?;

    match idx {
        Some(i) => {
            board[i].nick = nick.to_string();
            match &claim_type {
                ClaimType::Hunt => board[i].hunted += 1,
                ClaimType::Hug => board[i].hugged += 1,
            }
        }
        None => {
            board.push(BoardEntry {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                hunted: matches!(claim_type, ClaimType::Hunt) as u32,
                hugged: matches!(claim_type, ClaimType::Hug) as u32,
            });
        }
    }
    save_board(server, channel, &board)?;

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next(server, channel)?;
    }

    match claim_type {
        ClaimType::Hunt => reply(
            server,
            channel,
            &themed(
                "hunt.caught",
                &["{nick} caught the {animal}!"],
                &[("nick", display), ("animal", &animal)],
            )?,
        )?,
        ClaimType::Hug => reply(
            server,
            channel,
            &themed(
                "hunt.hugged",
                &["{nick} hugged the {animal}!"],
                &[("nick", display), ("animal", &animal)],
            )?,
        )?,
    }
    award(server, user_id, display, channel, claim_type)?;
    Ok(())
}

fn cmd_score(
    server: &str,
    channel: &str,
    target_nick: &str,
    target_display: &str,
    target_user_id: Option<&str>,
) -> Result<(), Error> {
    let board = load_board(server, channel)?;
    let found = match target_user_id {
        Some(user_id) => board
            .iter()
            .find(|entry| !user_id.is_empty() && entry.user_id == user_id),
        None => {
            let target = fold_nick(server, target_nick);
            board
                .iter()
                .find(|entry| fold_nick(server, &entry.nick) == target)
        }
    };
    match found {
        Some(e) => reply(
            server,
            channel,
            &themed(
                "hunt.score",
                &["{nick}: {hunted} caught, {hugged} hugged"],
                &[
                    ("nick", target_display),
                    ("hunted", &e.hunted.to_string()),
                    ("hugged", &e.hugged.to_string()),
                ],
            )?,
        )?,
        None => reply(
            server,
            channel,
            &themed(
                "hunt.no_score",
                &["{nick} hasn't caught or hugged anything yet."],
                &[("nick", target_display)],
            )?,
        )?,
    }
    Ok(())
}

fn cmd_top(server: &str, channel: &str) -> Result<(), Error> {
    let mut board = load_board(server, channel)?;

    if board.is_empty() {
        reply(
            server,
            channel,
            &themed(
                "hunt.top_empty",
                &["Nobody has caught or hugged anything yet. Watch for animals!"],
                &[],
            )?,
        )?;
        return Ok(());
    }

    board.sort_by(|a, b| {
        (b.hunted + b.hugged)
            .cmp(&(a.hunted + a.hugged))
            .then(b.hunted.cmp(&a.hunted))
    });

    let entries: Vec<String> = board
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, e)| {
            format!(
                "{}. {} ({} caught, {} hugged)",
                i + 1,
                e.nick,
                e.hunted,
                e.hugged
            )
        })
        .collect();

    reply(
        server,
        channel,
        &themed(
            "hunt.top",
            &["Hunt board: {board}"],
            &[("board", &entries.join(" | "))],
        )?,
    )?;
    Ok(())
}

fn cmd_status(server: &str, channel: &str) -> Result<(), Error> {
    if let Some(event) = load_active(server, channel)? {
        return reply(
            server,
            channel,
            &themed(
                "hunt.status_active",
                &["A {animal} is loose! Use !hunt to catch it or !hug to befriend it."],
                &[("animal", &event.animal)],
            )?,
        );
    }
    // Read the pending next-announce job to show time until next appearance.
    let raw = unsafe {
        schedule_list(serde_json::to_string(&ScheduleList {
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
    let nid = next_job_id(server, channel);
    if let Some(job) = jobs.iter().find(|j| j.id == nid) {
        let mins = ((job.due_at - now_secs()).max(0) / 60).to_string();
        return reply(
            server,
            channel,
            &themed(
                "hunt.status_next",
                &["No animal right now. The next appearance is in about {mins} minutes."],
                &[("mins", &mins)],
            )?,
        );
    }
    let enabled = read_setting_bool("enabled", server, channel, false);
    if enabled {
        reply(
            server,
            channel,
            &themed(
                "hunt.status_idle",
                &["No animal active and none scheduled yet — one will appear shortly."],
                &[],
            )?,
        )
    } else {
        reply(
            server,
            channel,
            &themed(
                "hunt.status_disabled",
                &["No animal active. Spontaneous appearances are disabled in this channel."],
                &[],
            )?,
        )
    }
}

fn cmd_admin_cancel(server: &str, channel: &str, display: &str) -> Result<(), Error> {
    let active = load_active(server, channel)?;
    cancel_expire(server, channel);
    clear_active(server, channel)?;
    match active {
        Some(event) => reply(
            server,
            channel,
            &themed(
                "hunt.admin_cancel",
                &["Jeeves discreetly ushers the {animal} away at {nick}'s request."],
                &[("animal", &event.animal), ("nick", display)],
            )?,
        )?,
        None => reply(
            server,
            channel,
            &themed(
                "hunt.admin_cancel_none",
                &["There is no animal to dismiss right now, {nick}."],
                &[("nick", display)],
            )?,
        )?,
    }
    let nid = next_job_id(server, channel);
    if read_setting_bool("enabled", server, channel, false)
        && !has_pending_job(server, channel, &nid)
    {
        schedule_next(server, channel)?;
    }
    Ok(())
}

// ── exports ───────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer { id, channel, .. } = env.event else {
        return Ok(());
    };

    if id.starts_with("next:") {
        handle_next(&server, &channel)?;
    } else if id.starts_with("expire:") {
        handle_expire(&server, &channel)?;
    }

    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    if msg.is_private {
        return Ok(());
    }

    let channel = &msg.target;
    let enabled = read_setting_bool("enabled", &server, channel, false);

    if enabled {
        ensure_scheduled(&server, channel)?;
    }

    let text = msg.text.trim();
    let lower = text.to_ascii_lowercase();

    if !lower.starts_with("!hunt") && !lower.starts_with("!hug") {
        return Ok(());
    }

    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };
    let user_id = &msg.user_id;

    if lower == "!hug" || lower.starts_with("!hug ") {
        cmd_claim(&server, channel, nick, display, user_id, ClaimType::Hug)?;
        return Ok(());
    }

    // !hunt [score [nick] | top]
    let rest = text[5..].trim(); // after "!hunt"
    let sub = rest.split_whitespace().next().unwrap_or("");

    match sub {
        "" => cmd_claim(&server, channel, nick, display, user_id, ClaimType::Hunt)?,
        "score" => {
            let target = rest["score".len()..].trim();
            let (tnick, tdisp, target_id) = if target.is_empty() {
                (nick.as_str(), display, Some(user_id.as_str()))
            } else {
                (target, target, None)
            };
            cmd_score(&server, channel, tnick, tdisp, target_id)?;
        }
        "top" => cmd_top(&server, channel)?,
        "status" => cmd_status(&server, channel)?,
        "cancel" => {
            if msg.role.is_some_and(|r| r.satisfies(Role::Admin)) {
                cmd_admin_cancel(&server, channel, display)?;
            } else {
                reply(
                    &server,
                    channel,
                    &themed(
                        "hunt.cancel_denied",
                        &["Only administrators may cancel a hunt event, {nick}."],
                        &[("nick", display)],
                    )?,
                )?;
            }
        }
        _ => {}
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nickname_score_lookup_uses_irc_default_casemapping() {
        assert_eq!(fold_nick("net", "Hunter[One]^"), "hunter{one}~");
    }

    #[test]
    fn job_ids_are_channel_scoped() {
        assert_ne!(
            next_job_id("libera", "#general"),
            next_job_id("libera", "#other"),
        );
        assert_ne!(
            expire_job_id("net1", "#chan"),
            expire_job_id("net2", "#chan"),
        );
    }

    #[test]
    fn default_animals_nonempty() {
        assert!(!DEFAULT_ANIMALS.is_empty());
        assert!(DEFAULT_ANIMALS.iter().all(|a| !a.is_empty()));
    }

    #[test]
    fn random_delay_stays_in_range() {
        let min_mins: i64 = 60;
        let max_mins: i64 = 180;
        let range = ((max_mins - min_mins) * 60).max(1) as u64;
        // Test a few representative byte patterns
        for bytes in [[0u8, 0, 0, 0], [255, 255, 255, 255], [1, 2, 3, 4]] {
            let r = u32::from_le_bytes(bytes) as u64;
            let delay = min_mins * 60 + (r % range) as i64;
            assert!(delay >= min_mins * 60);
            assert!(delay < max_mins * 60);
        }
    }

    #[test]
    fn board_sort_order() {
        let mut board = [
            BoardEntry {
                user_id: String::new(),
                nick: "alice".into(),
                hunted: 1,
                hugged: 0,
            },
            BoardEntry {
                user_id: String::new(),
                nick: "bob".into(),
                hunted: 5,
                hugged: 3,
            },
            BoardEntry {
                user_id: String::new(),
                nick: "carol".into(),
                hunted: 2,
                hugged: 2,
            },
        ];
        board.sort_by(|a, b| {
            (b.hunted + b.hugged)
                .cmp(&(a.hunted + a.hugged))
                .then(b.hunted.cmp(&a.hunted))
        });
        assert_eq!(board[0].nick, "bob"); // 8 total
        assert_eq!(board[1].nick, "carol"); // 4 total
        assert_eq!(board[2].nick, "alice"); // 1 total
    }

    #[test]
    fn stable_id_never_falls_back_to_matching_nick() {
        let board = vec![BoardEntry {
            user_id: "old-profile".into(),
            nick: "alice".into(),
            hunted: 10,
            hugged: 2,
        }];
        assert_eq!(board_index_by_id(&board, "old-profile"), Some(0));
        assert_eq!(board_index_by_id(&board, "new-profile"), None);
        assert_eq!(board_index_by_id(&board, ""), None);
    }
}
