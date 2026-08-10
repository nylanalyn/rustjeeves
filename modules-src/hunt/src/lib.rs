//! Spontaneous animal hunt game for rustjeeves.
//!
//! At random intervals the bot releases a wild animal into an enabled channel.
//! The first person to !hunt or bare !hug it claims it. `!hug <nick>` starts a
//! short, rejectable social hug incident. Scores are tracked per channel.
//!
//! IMPORTANT: must be explicitly enabled per channel via the `enabled` setting.
//!
//! Commands: !hunt  !hug [nick]  !reject  !hunt score [nick]  !hunt top
//!           !hunt status  !hunt cancel (admin)
//!
//! Theme keys (all under "hunt.*"):
//!   animals (list — the pool of creatures that appear; change to theme the whole game),
//!   release, caught, hugged, escaped, nothing,
//!   score ({nick}, {hunted}, {hugged}, {hunted_total}, {hugged_total}), no_score, top, top_empty,
//!   status_active, status_next, status_idle, status_disabled,
//!   admin_cancel, admin_cancel_none, cancel_denied,
//!   social_disabled, social_channel_only, social_identity_unavailable,
//!   social_unknown_target, social_usage, social_self, social_miss,
//!   social_attempt, social_busy_target, social_busy_initiator, social_capacity,
//!   social_cooldown, social_reject_usage, social_reject_none, social_rejected,
//!   social_completed

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, RandomBytesRequest,
    RandomBytesResponse, Role, ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob,
    SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// Default animal pool — operators override "hunt.animals" in theme.toml to change the whole game.
const DEFAULT_ANIMALS: &[&str] = &[
    "cat", "kitten", "puppy", "duck", "rabbit", "squirrel", "hedgehog",
];
const MAX_BOARD_ENTRIES: usize = 500;
const MAX_SOCIAL_PENDING: usize = 100;
const MAX_SOCIAL_COOLDOWNS: usize = 500;

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
    fn profile_get(input: String) -> String;
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
    let [stat, combined] = claim_stats(kind);
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
                    stat: combined.into(),
                    amount: 1,
                },
            ],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

fn claim_stats(kind: ClaimType) -> [&'static str; 2] {
    match kind {
        ClaimType::Hunt => ["hunts", "claims"],
        ClaimType::Hug => ["hugs", "claims"],
    }
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
            c(
                "hug",
                "Hug the loose animal, or begin a rejectable hug attempt toward someone.",
                "!hug [nick]",
            ),
            c(
                "reject",
                "Counter the pending hug attempt aimed at you.",
                "!reject",
            ),
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
                key: "reminder_mins".into(),
                description: "Minutes between reminders while an animal remains loose.".into(),
                default: "300".into(),
                kind: SettingKind::Integer { min: 30, max: 2880 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "social_hugs_enabled".into(),
                description: "Whether people may use !hug <nick> in this channel.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "hug_reject_seconds".into(),
                description: "Seconds a target has to reject a social hug attempt.".into(),
                default: "30".into(),
                kind: SettingKind::Integer { min: 5, max: 120 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "hug_cooldown_seconds".into(),
                description: "Seconds between social hug attempts by one person.".into(),
                default: "20".into(),
                kind: SettingKind::Integer { min: 5, max: 300 },
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
    /// Per-animal history added after the original aggregate-only score format.
    #[serde(default)]
    hunted_animals: BTreeMap<String, u32>,
    #[serde(default)]
    hugged_animals: BTreeMap<String, u32>,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct SocialHugState {
    #[serde(default)]
    pending: Vec<PendingHug>,
    #[serde(default)]
    cooldowns: Vec<HugCooldown>,
}

#[derive(Serialize, Deserialize, Clone)]
struct PendingHug {
    id: String,
    initiator_id: String,
    initiator_display: String,
    target_id: String,
    target_display: String,
    created_at: i64,
    expires_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct HugCooldown {
    profile_id: String,
    until: i64,
}

// ── job ID helpers (encoded per server+channel to avoid cross-channel cancel) ─

fn next_job_id(server: &str, channel: &str) -> String {
    format!("next:{server}:{channel}")
}

fn reminder_job_id(server: &str, channel: &str) -> String {
    format!("reminder:{server}:{channel}")
}

fn legacy_expire_job_id(server: &str, channel: &str) -> String {
    format!("expire:{server}:{channel}")
}

fn social_hug_job_id(id: &str) -> String {
    format!("social-hug:{id}")
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

fn social_hug_key(server: &str, channel: &str) -> String {
    format!("social-hugs:{server}:{channel}")
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

fn load_social_hugs(server: &str, channel: &str) -> Result<SocialHugState, Error> {
    let raw = kv_load(&social_hug_key(server, channel))?;
    if raw.is_empty() {
        Ok(SocialHugState::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn save_social_hugs(server: &str, channel: &str, state: &SocialHugState) -> Result<(), Error> {
    kv_save(
        &social_hug_key(server, channel),
        &serde_json::to_string(state)?,
    )
}

fn lifecycle_score_matches(score: &BoardEntry, request: &ModuleDataRequest) -> bool {
    score.user_id == request.subject.profile_id
        || request.aliases.iter().any(|alias| {
            score.user_id.eq_ignore_ascii_case(alias)
                || fold_nick(&request.subject.server, &score.nick)
                    == fold_nick(&request.subject.server, alias)
        })
}

fn lifecycle_pending_matches(pending: &PendingHug, request: &ModuleDataRequest) -> bool {
    pending.initiator_id == request.subject.profile_id
        || pending.target_id == request.subject.profile_id
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let board_prefix = format!("board:{}:", request.subject.server);
    let social_prefix = format!("social-hugs:{}:", request.subject.server);
    let mut scores = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&board_prefix))
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

    let mut social_hugs = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&social_prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let state: SocialHugState = serde_json::from_str(&entry.value)?;
        let pending = state
            .pending
            .into_iter()
            .filter(|pending| lifecycle_pending_matches(pending, &request))
            .collect::<Vec<_>>();
        let cooldown_until = state
            .cooldowns
            .into_iter()
            .find(|cooldown| cooldown.profile_id == request.subject.profile_id)
            .map(|cooldown| cooldown.until);
        if !pending.is_empty() || cooldown_until.is_some() {
            social_hugs.push(serde_json::json!({
                "key": entry.key,
                "pending": pending,
                "cooldown_until": cooldown_until,
            }));
        }
    }

    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if scores.is_empty() && social_hugs.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "channel_scores": scores,
                "social_hugs": social_hugs,
            })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let board_prefix = format!("board:{}:", request.subject.server);
    let social_prefix = format!("social-hugs:{}:", request.subject.server);
    let mut mutations = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&board_prefix))
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

    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&social_prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let mut state: SocialHugState = serde_json::from_str(&entry.value)?;
        let pending_before = state.pending.len();
        let cooldowns_before = state.cooldowns.len();
        state
            .pending
            .retain(|pending| !lifecycle_pending_matches(pending, &request));
        state
            .cooldowns
            .retain(|cooldown| cooldown.profile_id != request.subject.profile_id);
        if state.pending.len() != pending_before || state.cooldowns.len() != cooldowns_before {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: if state.pending.is_empty() && state.cooldowns.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&state)?)
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

fn record_animal(counts: &mut BTreeMap<String, u32>, animal: &str) {
    if animal.is_empty() {
        return;
    }
    let count = counts.entry(animal.to_string()).or_default();
    *count = count.saturating_add(1);
}

fn pluralize_animal(animal: &str) -> String {
    let lower = animal.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "deer" | "elk" | "fish" | "geese" | "moose" | "sheep"
    ) {
        return animal.to_string();
    }
    if lower.ends_with("mouse") {
        return format!("{}mice", &animal[..animal.len() - 5]);
    }
    if lower.ends_with('y')
        && lower
            .as_bytes()
            .get(lower.len().saturating_sub(2))
            .is_some_and(|byte| !matches!(byte, b'a' | b'e' | b'i' | b'o' | b'u'))
    {
        return format!("{}ies", &animal[..animal.len() - 1]);
    }
    if lower.ends_with('s') {
        return animal.to_string();
    }
    if lower.ends_with('x')
        || lower.ends_with('z')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        return format!("{animal}es");
    }
    format!("{animal}s")
}

fn format_animal_counts(
    counts: &BTreeMap<String, u32>,
    total: u32,
    animal_names: &BTreeSet<String>,
) -> String {
    if animal_names.is_empty() {
        return if total == 0 {
            "none".into()
        } else {
            format!("{total} untracked animals")
        };
    }

    let tracked = animal_names
        .iter()
        .map(|animal| u64::from(counts.get(animal).copied().unwrap_or_default()))
        .sum::<u64>();
    let mut parts = animal_names
        .iter()
        .map(|animal| {
            format!(
                "{} {}",
                counts.get(animal).copied().unwrap_or_default(),
                pluralize_animal(animal)
            )
        })
        .collect::<Vec<_>>();
    let untracked = u64::from(total).saturating_sub(tracked);
    if untracked > 0 {
        parts.push(format!("{untracked} untracked animals"));
    }
    parts.join(", ")
}

fn animal_breakdowns(entry: &BoardEntry) -> (String, String) {
    let animal_names = entry
        .hunted_animals
        .keys()
        .chain(entry.hugged_animals.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    (
        format_animal_counts(&entry.hunted_animals, entry.hunted, &animal_names),
        format_animal_counts(&entry.hugged_animals, entry.hugged, &animal_names),
    )
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
    if resp.bytes.len() != count {
        return Err(Error::msg("random_bytes returned the wrong byte count"));
    }
    Ok(resp.bytes)
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

fn social_hug_token(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_immediate_social_miss(random: u8) -> bool {
    random.is_multiple_of(4)
}

fn prune_social_cooldowns(state: &mut SocialHugState, current: i64) {
    state.cooldowns.retain(|cooldown| cooldown.until > current);
}

fn social_cooldown_remaining(state: &SocialHugState, profile_id: &str, current: i64) -> i64 {
    state
        .cooldowns
        .iter()
        .find(|cooldown| cooldown.profile_id == profile_id)
        .map_or(0, |cooldown| (cooldown.until - current).max(0))
}

fn set_social_cooldown(state: &mut SocialHugState, profile_id: &str, until: i64) {
    if let Some(cooldown) = state
        .cooldowns
        .iter_mut()
        .find(|cooldown| cooldown.profile_id == profile_id)
    {
        cooldown.until = until;
        return;
    }
    if state.cooldowns.len() >= MAX_SOCIAL_COOLDOWNS {
        if let Some((oldest, _)) = state
            .cooldowns
            .iter()
            .enumerate()
            .min_by_key(|(_, cooldown)| cooldown.until)
        {
            state.cooldowns.remove(oldest);
        }
    }
    state.cooldowns.push(HugCooldown {
        profile_id: profile_id.into(),
        until,
    });
}

fn active_social_pending(pending: &PendingHug, current: i64) -> bool {
    pending.expires_at > current
}

fn cancel_social_hug(id: &str) {
    let _ = unsafe {
        schedule_cancel(
            serde_json::to_string(&ScheduleCancel {
                id: social_hug_job_id(id),
            })
            .unwrap_or_default(),
        )
    };
}

fn schedule_social_hug(server: &str, channel: &str, pending: &PendingHug) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: social_hug_job_id(&pending.id),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: Some(pending.initiator_id.clone()),
            due_at: pending.expires_at,
            payload: String::new(),
        })?)?;
    }
    Ok(())
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
    let rid = reminder_job_id(server, channel);
    let legacy_id = legacy_expire_job_id(server, channel);
    if load_active(server, channel)?.is_some() {
        if !has_pending_job(server, channel, &rid) && !has_pending_job(server, channel, &legacy_id)
        {
            schedule_reminder(server, channel)?;
        }
        return Ok(());
    }
    if has_pending_job(server, channel, &nid) {
        return Ok(());
    }
    schedule_next(server, channel)
}

fn schedule_reminder(server: &str, channel: &str) -> Result<(), Error> {
    let reminder_mins = read_setting_i64("reminder_mins", server, channel, 300).clamp(30, 2880);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: reminder_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + reminder_mins * 60,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn cancel_reminder(server: &str, channel: &str) {
    for id in [
        reminder_job_id(server, channel),
        legacy_expire_job_id(server, channel),
    ] {
        let _ = unsafe {
            schedule_cancel(serde_json::to_string(&ScheduleCancel { id }).unwrap_or_default())
        };
    }
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

    schedule_reminder(server, channel)?;

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

fn handle_reminder(server: &str, channel: &str) -> Result<(), Error> {
    if let Some(event) = load_active(server, channel)? {
        if read_setting_bool("enabled", server, channel, false) {
            reply(
                server,
                channel,
                &themed(
                    "hunt.reminder",
                    &["A small reminder from Jeeves: the {animal} is still loose. Type !hunt to catch it or !hug to befriend it."],
                    &[("animal", &event.animal)],
                )?,
            )?;
            schedule_reminder(server, channel)?;
        }
    } else if read_setting_bool("enabled", server, channel, false) {
        schedule_next(server, channel)?;
    }
    Ok(())
}

// ── command handlers ──────────────────────────────────────────────────────────

fn cmd_social_hug(
    server: &str,
    channel: &str,
    initiator_display: &str,
    initiator_id: &str,
    raw_target: &str,
) -> Result<(), Error> {
    if !read_setting_bool("social_hugs_enabled", server, channel, true) {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_disabled",
                &["Social hugs are resting in this channel."],
                &[],
            )?,
        );
    }
    if initiator_id.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_identity_unavailable",
                &[
                    "I couldn't verify a stable profile for {nick}, so the hug attempt was not filed.",
                ],
                &[("nick", initiator_display)],
            )?,
        );
    }

    let target_nick = raw_target.strip_prefix('@').unwrap_or(raw_target);
    if target_nick.is_empty() || target_nick.len() > 64 {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_usage",
                &["Try !hug <nick>; hug trajectories require exactly one destination."],
                &[],
            )?,
        );
    }
    let Some(target) = profile(server, target_nick)? else {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_unknown_target",
                &["I don't know {target} well enough to calculate a hug trajectory."],
                &[("target", target_nick)],
            )?,
        );
    };
    if target.id.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_unknown_target",
                &["I don't know {target} well enough to calculate a hug trajectory."],
                &[("target", target_nick)],
            )?,
        );
    }

    let current = now_secs();
    let mut state = load_social_hugs(server, channel)?;
    prune_social_cooldowns(&mut state, current);
    let remaining = social_cooldown_remaining(&state, initiator_id, current);
    if remaining > 0 {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_cooldown",
                &["{nick}'s hug apparatus needs {seconds} more seconds to reset."],
                &[
                    ("nick", initiator_display),
                    ("seconds", &remaining.to_string()),
                ],
            )?,
        );
    }

    let cooldown_seconds =
        read_setting_i64("hug_cooldown_seconds", server, channel, 20).clamp(5, 300);
    if target.id == initiator_id {
        set_social_cooldown(&mut state, initiator_id, current + cooldown_seconds);
        save_social_hugs(server, channel, &state)?;
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_self",
                &[
                    "{initiator} hugs themselves. It is awkwardly executed but administratively valid.",
                    "{initiator} attempts self-comfort and accidentally performs a wrestling hold.",
                    "{initiator} gives themselves a reassuring squeeze. Jeeves records no witnesses.",
                    "{initiator} folds their arms and declares the hug successfully delivered.",
                ],
                &[("initiator", initiator_display)],
            )?,
        );
    }

    if state.pending.iter().any(|pending| {
        active_social_pending(pending, current) && pending.initiator_id == initiator_id
    }) {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_busy_initiator",
                &[
                    "{initiator} already has one hug in flight. Air-traffic control refuses another.",
                ],
                &[("initiator", initiator_display)],
            )?,
        );
    }
    if state
        .pending
        .iter()
        .any(|pending| active_social_pending(pending, current) && pending.target_id == target.id)
    {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_busy_target",
                &["{target} already has an incoming hug to resolve."],
                &[("target", &target.nick)],
            )?,
        );
    }
    if state.pending.len() >= MAX_SOCIAL_PENDING {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_capacity",
                &["The channel's hug airspace is temporarily full."],
                &[],
            )?,
        );
    }

    let mut random = get_random_bytes(9)?;
    if is_immediate_social_miss(random[0]) {
        set_social_cooldown(&mut state, initiator_id, current + cooldown_seconds);
        save_social_hugs(server, channel, &state)?;
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_miss",
                &[
                    "{initiator} lunges for {target}, misses entirely, and hugs the atmosphere beside them.",
                    "{initiator} advances on {target}, encounters a ficus, and quietly revises the mission report.",
                    "{initiator} attempts to glomp {target}, misjudges the approach velocity, and captures one sleeve-shaped patch of air.",
                    "{initiator} opens their arms toward {target}, trips over the concept of distance, and aborts.",
                ],
                &[
                    ("initiator", initiator_display),
                    ("target", &target.nick),
                ],
            )?,
        );
    }

    let mut id = social_hug_token(&random[1..]);
    while state.pending.iter().any(|pending| pending.id == id) {
        random = get_random_bytes(8)?;
        id = social_hug_token(&random);
    }
    let reject_seconds = read_setting_i64("hug_reject_seconds", server, channel, 30).clamp(5, 120);
    let pending = PendingHug {
        id,
        initiator_id: initiator_id.into(),
        initiator_display: initiator_display.into(),
        target_id: target.id,
        target_display: target.nick,
        created_at: current,
        expires_at: current + reject_seconds,
    };
    let attempt_target = pending.target_display.clone();
    schedule_social_hug(server, channel, &pending)?;
    set_social_cooldown(&mut state, initiator_id, current + cooldown_seconds);
    state.pending.push(pending);
    save_social_hugs(server, channel, &state)?;
    reply(
        server,
        channel,
        &themed(
            "hunt.social_attempt",
            &[
                "{initiator} advances on {target} with arms spread and no discernible plan. {target} may !reject.",
                "{initiator} begins a structurally questionable hug approach toward {target}. {target} may !reject.",
                "{initiator} has declared {target} to be within hugging distance. {target} may !reject.",
                "{initiator} winds up an alarmingly sincere embrace aimed at {target}. {target} may !reject.",
            ],
            &[
                ("initiator", initiator_display),
                ("target", &attempt_target),
            ],
        )?,
    )
}

fn cmd_reject_social_hug(
    server: &str,
    channel: &str,
    target_display: &str,
    target_id: &str,
) -> Result<(), Error> {
    if target_id.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_identity_unavailable",
                &[
                    "I couldn't verify a stable profile for {nick}, so I cannot assign that counter-move.",
                ],
                &[("nick", target_display)],
            )?,
        );
    }
    let current = now_secs();
    let mut state = load_social_hugs(server, channel)?;
    let Some(index) = state.pending.iter().position(|pending| {
        active_social_pending(pending, current) && pending.target_id == target_id
    }) else {
        return reply(
            server,
            channel,
            &themed(
                "hunt.social_reject_none",
                &["No unresolved hug is currently aimed at {target}."],
                &[("target", target_display)],
            )?,
        );
    };
    let pending = state.pending.remove(index);
    save_social_hugs(server, channel, &state)?;
    cancel_social_hug(&pending.id);
    reply(
        server,
        channel,
        &themed(
            "hunt.social_rejected",
            &[
                "{target} deploys a perfectly timed office chair. {initiator} hugs the backrest and pretends this was intentional.",
                "{target} raises a single finger. {initiator}'s hug request is returned unopened.",
                "{target} counters with a handshake, converting the encounter into a minor business meeting.",
                "{target} ducks beneath the hug. {initiator} performs an elegant half-spin and denies everything.",
                "{target} produces a cushion from nowhere. {initiator} hugs that instead and finds it surprisingly supportive.",
                "{target} shouts PARRY! and somehow it works.",
            ],
            &[
                ("target", &pending.target_display),
                ("initiator", &pending.initiator_display),
            ],
        )?,
    )
}

fn handle_social_hug_expiry(server: &str, channel: &str, id: &str) -> Result<(), Error> {
    let mut state = load_social_hugs(server, channel)?;
    let Some(index) = state.pending.iter().position(|pending| pending.id == id) else {
        return Ok(());
    };
    let pending = state.pending.remove(index);
    prune_social_cooldowns(&mut state, now_secs());
    save_social_hugs(server, channel, &state)?;
    reply(
        server,
        channel,
        &themed(
            "hunt.social_completed",
            &[
                "{initiator} catches {target} in a brief but structurally sound embrace.",
                "{initiator} wraps {target} in an embrace best described as well-intentioned containment.",
                "{initiator} hugs {target}. Somewhere, a chiropractor senses a disturbance.",
                "{initiator}'s hug reaches {target} with the solemn determination of someone testing a watermelon.",
                "{initiator} and {target} complete the hug without violating any known building codes.",
            ],
            &[
                ("initiator", &pending.initiator_display),
                ("target", &pending.target_display),
            ],
        )?,
    )
}

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
    cancel_reminder(server, channel);
    clear_active(server, channel)?;

    match idx {
        Some(i) => {
            board[i].nick = nick.to_string();
            match &claim_type {
                ClaimType::Hunt => {
                    board[i].hunted += 1;
                    record_animal(&mut board[i].hunted_animals, &animal);
                }
                ClaimType::Hug => {
                    board[i].hugged += 1;
                    record_animal(&mut board[i].hugged_animals, &animal);
                }
            }
        }
        None => {
            let mut hunted_animals = BTreeMap::new();
            let mut hugged_animals = BTreeMap::new();
            match claim_type {
                ClaimType::Hunt => record_animal(&mut hunted_animals, &animal),
                ClaimType::Hug => record_animal(&mut hugged_animals, &animal),
            }
            board.push(BoardEntry {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                hunted: matches!(claim_type, ClaimType::Hunt) as u32,
                hugged: matches!(claim_type, ClaimType::Hug) as u32,
                hunted_animals,
                hugged_animals,
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
        Some(e) => {
            let (hunted, hugged) = animal_breakdowns(e);
            reply(
                server,
                channel,
                &themed(
                    "hunt.score",
                    &["[Hunt] {nick}: hugged: {hugged}. Hunted: {hunted}."],
                    &[
                        ("nick", target_display),
                        ("hunted", &hunted),
                        ("hugged", &hugged),
                        ("hunted_total", &e.hunted.to_string()),
                        ("hugged_total", &e.hugged.to_string()),
                    ],
                )?,
            )?
        }
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
    cancel_reminder(server, channel);
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

    if let Some(social_id) = id.strip_prefix("social-hug:") {
        handle_social_hug_expiry(&server, &channel, social_id)?;
    } else if id.starts_with("next:") {
        handle_next(&server, &channel)?;
    } else if id.starts_with("reminder:") || id.starts_with("expire:") {
        handle_reminder(&server, &channel)?;
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

    let text = msg.text.trim();
    let lower = text.to_ascii_lowercase();
    let command = lower.split_whitespace().next().unwrap_or("");
    if command != "!hunt" && command != "!hug" && command != "!reject" {
        return Ok(());
    }

    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };

    if msg.is_private {
        if command == "!hug" || command == "!reject" {
            reply(
                &server,
                nick,
                &themed(
                    "hunt.social_channel_only",
                    &["Social hugs only work in a channel, {nick}."],
                    &[("nick", display)],
                )?,
            )?;
        }
        return Ok(());
    }

    let channel = &msg.target;
    let enabled = read_setting_bool("enabled", &server, channel, false);

    if enabled {
        ensure_scheduled(&server, channel)?;
    }

    let user_id = &msg.user_id;

    if command == "!hug" {
        let arguments = text.split_whitespace().skip(1).collect::<Vec<_>>();
        match arguments.as_slice() {
            [] => cmd_claim(&server, channel, nick, display, user_id, ClaimType::Hug)?,
            [target] => cmd_social_hug(&server, channel, display, user_id, target)?,
            _ => reply(
                &server,
                channel,
                &themed(
                    "hunt.social_usage",
                    &["Try !hug <nick>; hug trajectories require exactly one destination."],
                    &[],
                )?,
            )?,
        }
        return Ok(());
    }

    if command == "!reject" {
        if text.split_whitespace().nth(1).is_some() {
            reply(
                &server,
                channel,
                &themed(
                    "hunt.social_reject_usage",
                    &["Use !reject by itself to counter the hug aimed at you."],
                    &[],
                )?,
            )?;
        } else {
            cmd_reject_social_hug(&server, channel, display, user_id)?;
        }
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
            reminder_job_id("net1", "#chan"),
            reminder_job_id("net2", "#chan"),
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
                hunted_animals: BTreeMap::new(),
                hugged_animals: BTreeMap::new(),
            },
            BoardEntry {
                user_id: String::new(),
                nick: "bob".into(),
                hunted: 5,
                hugged: 3,
                hunted_animals: BTreeMap::new(),
                hugged_animals: BTreeMap::new(),
            },
            BoardEntry {
                user_id: String::new(),
                nick: "carol".into(),
                hunted: 2,
                hugged: 2,
                hunted_animals: BTreeMap::new(),
                hugged_animals: BTreeMap::new(),
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
            hunted_animals: BTreeMap::new(),
            hugged_animals: BTreeMap::new(),
        }];
        assert_eq!(board_index_by_id(&board, "old-profile"), Some(0));
        assert_eq!(board_index_by_id(&board, "new-profile"), None);
        assert_eq!(board_index_by_id(&board, ""), None);
    }

    #[test]
    fn legacy_board_entries_deserialize_without_animal_breakdowns() {
        let entry: BoardEntry = serde_json::from_str(
            r#"{"user_id":"profile","nick":"alice","hunted":10,"hugged":2}"#,
        )
        .expect("legacy board entry should remain readable");
        assert!(entry.hunted_animals.is_empty());
        assert!(entry.hugged_animals.is_empty());
        assert_eq!(
            animal_breakdowns(&entry),
            (
                "10 untracked animals".to_string(),
                "2 untracked animals".to_string()
            )
        );
    }

    #[test]
    fn animal_breakdowns_include_zeroes_and_pluralize_names() {
        let hugged_animals = BTreeMap::from([
            ("capybara".to_string(), 24),
            ("hedgehog".to_string(), 20),
            ("kitten".to_string(), 3),
            ("puppy".to_string(), 2),
        ]);
        let entry = BoardEntry {
            user_id: "profile".into(),
            nick: "alice".into(),
            hunted: 0,
            hugged: 49,
            hunted_animals: BTreeMap::new(),
            hugged_animals,
        };

        assert_eq!(
            animal_breakdowns(&entry),
            (
                "0 capybaras, 0 hedgehogs, 0 kittens, 0 puppies".to_string(),
                "24 capybaras, 20 hedgehogs, 3 kittens, 2 puppies".to_string()
            )
        );
    }

    #[test]
    fn achievement_claim_stats_distinguish_hunts_from_hugs() {
        assert_eq!(claim_stats(ClaimType::Hunt), ["hunts", "claims"]);
        assert_eq!(claim_stats(ClaimType::Hug), ["hugs", "claims"]);
    }

    #[test]
    fn social_misses_are_exactly_one_quarter_of_branch_values() {
        assert_eq!(
            (u8::MIN..=u8::MAX)
                .filter(|value| is_immediate_social_miss(*value))
                .count(),
            64,
        );
    }

    #[test]
    fn social_cooldowns_are_stable_id_owned_and_pruned() {
        let mut state = SocialHugState::default();
        set_social_cooldown(&mut state, "profile-a", 120);
        set_social_cooldown(&mut state, "profile-a", 140);
        set_social_cooldown(&mut state, "profile-b", 90);

        assert_eq!(state.cooldowns.len(), 2);
        assert_eq!(social_cooldown_remaining(&state, "profile-a", 100), 40);
        prune_social_cooldowns(&mut state, 100);
        assert_eq!(state.cooldowns.len(), 1);
        assert_eq!(state.cooldowns[0].profile_id, "profile-a");
    }

    #[test]
    fn pending_hug_lifecycle_belongs_to_both_people() {
        let pending = PendingHug {
            id: "incident".into(),
            initiator_id: "profile-a".into(),
            initiator_display: "Alice".into(),
            target_id: "profile-b".into(),
            target_display: "Bob".into(),
            created_at: 100,
            expires_at: 130,
        };
        let request_for = |profile_id: &str| ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: jeeves_abi::DataSubject {
                server: "net".into(),
                profile_id: profile_id.into(),
            },
            aliases: Vec::new(),
            entries: Vec::new(),
        };

        assert!(lifecycle_pending_matches(
            &pending,
            &request_for("profile-a")
        ));
        assert!(lifecycle_pending_matches(
            &pending,
            &request_for("profile-b")
        ));
        assert!(!lifecycle_pending_matches(
            &pending,
            &request_for("profile-c")
        ));
    }

    #[test]
    fn social_job_ids_do_not_expose_nicks() {
        let id = social_hug_job_id(&social_hug_token(&[0x12, 0xab, 0x00, 0xff]));
        assert_eq!(id, "social-hug:12ab00ff");
        assert!(!id.contains("Alice"));
        assert!(!id.contains("Bob"));
    }
}
