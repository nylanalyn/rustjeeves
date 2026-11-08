//! Fishing mini-game for rustjeeves — a port of jeeves/modules/fishing.py.
//!
//! The cast/reel loop over locations (Puddle -> The Void), with levelling, weighted catches by
//! wait time, junk, line breaks, XP and bonuses, random events, artifacts, lures, chum, seasonal
//! champions, endless levelling with catch epithets past the old cap, rare wormhole quests, and
//! the risk toys (`!dynamite`, danger mode).
//!
//! State lives in one JSON blob in the module's namespaced kv store (`data`). The fish database is
//! the real `fish_database.json`, bundled at compile time.
//!
//! Where things live:
//!
//! - `lib.rs` — the extism ABI exports, host-function wrappers, persistence, and shared game
//!   mechanics such as RNG, levelling, rod wear, and identity migration.
//! - [`commands`] — the `dispatch` table and most command handlers. Start here to trace a command.
//! - [`cast`] / [`reel`] / [`danger`] / [`wormhole`] — feature areas big enough to own a file,
//!   including their own commands.
//! - [`catalog`] — the static fish database and the pure roll tables over it.
//! - [`model`] — the persisted `State` tree.
//! - [`seasons`] — quarter boundaries, lazy resets, seasonal counters, and champions.

mod cast;
mod catalog;
mod commands;
mod danger;
mod model;
mod reel;
mod seasons;
mod tips;
mod wormhole;

use catalog::{
    calc_weight, round1, round2, select_fish, select_rarity, Artifact, EventDef, Fish, Location,
    VoidExpansion, VoidNests,
};
use danger::{CONFIRM_SECS, RECOVERY_SECS};
use model::{
    ActiveEvent, Cast, CatchMilestones, Champions, Chum, Player, RareCatch, SpeciesCareer, State,
    Wormhole, WormholeKind,
};
use seasons::{champion_titles, maybe_seasonal_reset, season_stats_mut};
use wormhole::{
    cmd_wormhole_cast, new_quest, quest_task_text, resolve_wormhole_reel, WORMHOLE_TRIGGER_CHANCE,
};

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandShortcut, CommandSpec, Event, EventEnvelope, KvList, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvEntry, ModuleKvMutation, Profile, ProfileKey,
    RandomBytesRequest, RandomBytesResponse, Role, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{reply, themed};
use std::collections::HashMap;
use std::sync::OnceLock;

#[host_fn]
extern "ExtismHost" {
    fn kv_list(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn cosmetics_worn(input: String) -> String;
    fn economy_award(input: String) -> String;
}

/// Brass for a catch (paid through the host economy, the same purse gacha eggs spend from):
/// uncommon 2, rare 5, legendary 15, a new record 10, and 5 per level gained, scaled by the
/// `brass_percent` setting.
pub(crate) fn catch_brass(rarity: &str, new_record: bool, levels: u64, percent: i64) -> u64 {
    let base = match rarity {
        "uncommon" => 2,
        "rare" => 5,
        "legendary" => 15,
        _ => 0,
    } + if new_record { 10 } else { 0 }
        + levels * 5;
    base * percent.max(0) as u64 / 100
}

/// Pay brass once per event id; retries of the same event pay nothing.
pub(crate) fn award_brass(
    server: &str,
    profile_id: &str,
    amount: u64,
    event_id: &str,
) -> Result<(), Error> {
    if profile_id.is_empty() || amount == 0 {
        return Ok(());
    }
    unsafe {
        economy_award(serde_json::to_string(
            &jeeves_abi::EconomyTransactionRequest {
                server: server.into(),
                profile_id: profile_id.into(),
                amount,
                event_id: event_id.into(),
                reason: "fishing".into(),
            },
        )?)?;
    }
    Ok(())
}

/// " 🎺 fanfare!" when the winner wears a flourish (cosmetics found in gacha eggs), else "".
fn flourish(server: &str, profile_id: &str) -> Result<String, Error> {
    if profile_id.is_empty() {
        return Ok(String::new());
    }
    let raw = unsafe {
        cosmetics_worn(serde_json::to_string(&jeeves_abi::CosmeticsWornRequest {
            server: server.into(),
            profile_ids: vec![profile_id.into()],
        })?)?
    };
    let worn: Vec<jeeves_abi::WornCosmetics> = serde_json::from_str(&raw)?;
    Ok(worn
        .into_iter()
        .next()
        .and_then(|worn| worn.flourish)
        .map(|flourish| format!(" {flourish}"))
        .unwrap_or_default())
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("no_longer_tiddling", "No Longer Tiddling", "level", 5),
        ("old_salt", "Old Salt", "level", 10),
        ("reinforced_resolve", "Reinforced Resolve", "level", 15),
        // 19 stays the classic milestone; leveling continues past it with no cap.
        (
            "compleat_angler",
            "The Compleat Angler",
            "level",
            EXPANSION_MAX_LEVEL as u64,
        ),
        (
            "into_the_first_void",
            "Through the Looking Puddle",
            "level",
            (EXPANSION_MAX_LEVEL + 1) as u64,
        ),
        ("something_biting", "Something’s Biting", "catches", 1),
        ("fine_kettle", "A Fine Kettle of Fish", "catches", 100),
        ("more_in_sea", "Plenty More in the Sea", "catches", 500),
        ("one_records", "One for the Records", "rare_catches", 1),
        ("aquarium", "It Belongs in an Aquarium", "artifacts", 1),
        (
            "through_wormhole",
            "Through the Wormhole",
            "wormhole_quests",
            1,
        ),
    ]
    .into_iter()
    .map(|(id, name, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: match stat {
            "level" => format!("Reach fishing level {threshold}."),
            "catches" => format!("Land {threshold} fish."),
            "rare_catches" => "Land a rare or legendary fish.".into(),
            "wormhole_quests" => "Complete a wormhole quest.".into(),
            _ => "Find a fishing artifact.".into(),
        },
        stat: stat.into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "got_away".into(),
        name: "The One That Got Away".into(),
        description: "Break a fishing line.".into(),
        stat: "line_breaks".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    achievements.push(AchievementSpec {
        id: "vampire_shark".into(),
        name: "What We Do in the Water".into(),
        description: "Reel in the impossible catch during hour 666.".into(),
        stat: "vampire_sharks".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    for (id, name, description, stat, secret) in [
        (
            "wise_move",
            "Wise Move.",
            "Decline Jeeves's invitation to declare war on fishdom.",
            "danger_backouts",
            false,
        ),
        (
            "war_were_declared",
            "War Were Declared",
            "Confirm DANGER MODE despite receiving excellent advice.",
            "danger_enlistments",
            false,
        ),
        (
            "insufficiently_limbed",
            "Insufficiently Limbed",
            "Lose all four limbs in DANGER MODE.",
            "danger_full_injuries",
            true,
        ),
    ] {
        achievements.push(AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            stat: stat.into(),
            threshold: 1,
            optional: true,
            secret,
        });
    }
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        // Bumped to 2 when compleat_angler's threshold was corrected 20 → 19, then to 3 for the
        // optional DANGER MODE catalog additions, then to 4 for the hour-666 secret, then to 5
        // for the wormhole quest achievement, then to 6 for entering the First Void (level 20).
        catalog_version: 6,
        stats: [
            "level",
            "catches",
            "rare_catches",
            "artifacts",
            "line_breaks",
            "vampire_sharks",
            "danger_backouts",
            "danger_enlistments",
            "danger_full_injuries",
            "wormhole_quests",
        ]
        .into_iter()
        .map(|id| AchievementStat {
            id: id.into(),
            description: id.into(),
        })
        .collect(),
        achievements,
        prestige: vec![jeeves_abi::PrestigeSpec {
            id: "fishmonger".into(),
            name: "Fishmonger".into(),
            stat: "catches".into(),
            first_threshold: 1000,
            every: 500,
        }],
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&achievement_backfill_response(
        request,
    )?)?)
}

fn achievement_backfill_response(
    request: AchievementBackfillRequest,
) -> Result<AchievementBackfillResponse, Error> {
    let (state, _) = state_from_entries(&request.entries)?;
    let prefix = format!("{}/", request.server);
    let values = state
        .players
        .into_iter()
        .filter_map(|(key, player)| {
            key.strip_prefix(&prefix)
                .filter(|id| !id.is_empty())
                .map(|id| (id.to_string(), player))
        })
        .flat_map(|(profile_id, player)| {
            [
                ("level", player.level.max(0) as u64),
                ("catches", player.total_fish.max(0) as u64),
                ("rare_catches", player.rare_total().max(0) as u64),
                ("artifacts", player.artifact.is_some() as u64),
                ("line_breaks", player.lines_broken.max(0) as u64),
            ]
            .into_iter()
            .map(move |(stat, value)| AchievementSetMax {
                profile_id: profile_id.clone(),
                stat: stat.into(),
                value,
            })
        })
        .collect();
    Ok(AchievementBackfillResponse { values })
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: format!("!{name}"),
        ..Default::default()
    };
    let mut cast = command(
        "cast",
        "Cast a fishing line, optionally spending XP on bait.",
    );
    cast.usage =
        "!cast [location] [bait <100-1700 XP>] | !cast <nick> (for a dynamite-banned angler)"
            .into();
    let mut fish = command(
        "fish",
        "Show fishing stats and subcommands; leveling never stops, past level 19 the world recurses inside the Void, and some catches wear unlocked epithets.",
    );
    fish.aliases = vec!["fishing".into(), "fishstats".into()];
    fish.usage = "!fish [nick | recast [loc] [bait N] [lure] [chum] | top | location | champions | help | info [loc] | aquarium | mastery [nick] | records [nick] | rod | fix | heal | lure | chum | discard | dynamite | hands | danger | yes | no | safety | limbs | tips on|off]".into();
    fish.shortcuts = commands::FISH_SHORTCUTS
        .iter()
        .map(|name| {
            let (description, usage) = shortcut_help(name);
            CommandShortcut::new(name, name).described(description, usage)
        })
        .collect();
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![cast, command("reel", "Reel in a fishing line."), fish],
    })?)
}

/// Help text for each `!fish` subcommand shortcut (formerly standalone commands).
fn shortcut_help(name: &str) -> (&'static str, &'static str) {
    match name {
        "recast" => (
            "Reel in and cast straight back out: same water and no bait unless you name them. Add lure or chum to rig a lure or chum the water just before the reel.",
            "!recast [location] [bait <XP>] [lure] [chum]",
        ),
        "fishinfo" => (
            "List locations, or show what lives in one.",
            "!fishinfo [location]",
        ),
        "aquarium" => ("Show your aquarium.", "!aquarium"),
        "mastery" => ("Show lifetime species mastery.", "!mastery [nick]"),
        "records" => ("Show personal specimen records.", "!records [nick]"),
        "rod" => (
            "Inspect your fishing rod's strength (level 15+; strengthens with your level).",
            "!rod",
        ),
        "fix" => (
            "Retired: rods now reinforce themselves as you level — see !rod.",
            "!fix",
        ),
        "heal" => (
            "Spend XP to restore limbs lost to fishing explosives.",
            "!heal",
        ),
        "lure" => ("Rig a lure for your next catch (costs XP).", "!lure"),
        "chum" => (
            "Chum the water for everyone fishing here (costs XP).",
            "!chum",
        ),
        "discard" => ("Discard your aquarium artifact.", "!discard"),
        "dynamite" => ("Use dynamite while fishing. Ill-advised.", "!dynamite"),
        "hands" => (
            "Check dynamite damage, or your injuries and recovery while in DANGER MODE.",
            "!hands",
        ),
        "danger" => ("Request enlistment in DANGER MODE.", "!danger"),
        "yes" => ("Confirm a pending DANGER MODE warning.", "!yes"),
        "no" => ("Decline a pending DANGER MODE warning.", "!no"),
        "safety" => ("Leave DANGER MODE.", "!safety"),
        "limbs" => ("Inspect your DANGER MODE limbs and equipment.", "!limbs"),
        _ => ("", ""),
    }
}

/// One operator-tunable knob: the single source of truth for both its manifest entry and its
/// runtime clamp.
///
/// [`settings`] renders these into the manifest an operator sees, and [`fishing_settings`] clamps
/// host-provided overrides to the same bounds. Declare a knob here and the two cannot disagree —
/// never hardcode a bound at a call site.
struct SettingDef {
    key: &'static str,
    description: &'static str,
    default: i64,
    min: i64,
    max: i64,
    /// Advertised as `DurationSeconds` rather than a plain `Integer`.
    duration: bool,
}

const SETTING_DEFS: &[SettingDef] = &[
    SettingDef {
        key: "champion_brass",
        description: "Brass paid to each season champion at the crowning (titles carry no other advantage).",
        default: 100,
        min: 0,
        max: 1_000,
        duration: false,
    },
    SettingDef {
        key: "brass_percent",
        description: "Brass paid for good catches, records, and level-ups, as a percentage of the standard amounts (0 turns it off).",
        default: 100,
        min: 0,
        max: 500,
        duration: false,
    },
    SettingDef {
        key: "danger_confirm_seconds",
        description: "Seconds allowed to confirm DANGER MODE.",
        default: CONFIRM_SECS,
        min: 5,
        max: 300,
        duration: true,
    },
    SettingDef {
        key: "danger_recovery_seconds",
        description: "Seconds before each lost DANGER MODE limb recovers.",
        default: RECOVERY_SECS,
        min: 3_600,
        max: 2_592_000,
        duration: true,
    },
    SettingDef {
        key: "danger_minor_injury_seconds",
        description: "Seconds that a cosmetic DANGER MODE injury remains visible.",
        default: MINOR_INJURY_SECS,
        min: 86_400,
        max: 172_800,
        duration: true,
    },
    SettingDef {
        key: "danger_weapon_drop_percent",
        description: "Chance that a successful DANGER MODE reel changes your weapon.",
        default: DANGER_WEAPON_DROP_PERCENT,
        min: 0,
        max: 100,
        duration: false,
    },
    SettingDef {
        key: "dynamite_hand_regrow_seconds",
        description: "Seconds for a hand lost to dynamite to regrow.",
        default: HAND_REGROW_SECS,
        min: 3_600,
        max: 2_592_000,
        duration: true,
    },
    SettingDef {
        key: "chum_active_seconds",
        description: "Seconds that chum increases catch sizes.",
        default: CHUM_ACTIVE_SECS,
        min: 60,
        max: 86_400,
        duration: true,
    },
    SettingDef {
        key: "chum_cooldown_seconds",
        description: "Seconds before chum can be used again.",
        default: CHUM_COOLDOWN_SECS,
        min: 60,
        max: 172_800,
        duration: true,
    },
    SettingDef {
        key: "lure_xp_cost",
        description: "XP cost to rig a mystery lure.",
        default: LURE_XP_COST,
        min: 1,
        max: 10_000,
        duration: false,
    },
    SettingDef {
        key: "chum_xp_cost",
        description: "XP cost to throw fishing chum.",
        default: CHUM_XP_COST,
        min: 1,
        max: 10_000,
        duration: false,
    },
    SettingDef {
        key: "limb_heal_xp_cost",
        description: "XP cost to restore each limb lost to dynamite or DANGER MODE.",
        default: LIMB_HEAL_XP_COST,
        min: 1_000,
        max: 1_000_000,
        duration: false,
    },
    SettingDef {
        key: "rod_max_strength",
        description: "Maximum reinforced-rod strength.",
        default: ROD_MAX_STRENGTH as i64,
        min: 1,
        max: 100,
        duration: false,
    },
    SettingDef {
        key: "rod_strength_per_level",
        description: "Rod strength each level past 15 adds passively (no !fix needed); wear regenerates one point per hour.",
        default: ROD_STRENGTH_PER_LEVEL,
        min: 0,
        max: 20,
        duration: false,
    },
];

/// Look up a knob by key. Panics on an unknown key: every key is a literal in this file, and
/// `setting_keys_are_unique` plus the manifest test pin the table's shape.
fn setting_def(key: &str) -> &'static SettingDef {
    SETTING_DEFS
        .iter()
        .find(|def| def.key == key)
        .unwrap_or_else(|| panic!("unknown fishing setting key: {key}"))
}

/// Build the settings manifest from [`SETTING_DEFS`]. Split out from the plugin export so tests
/// can inspect it without crossing the wasm boundary.
fn settings_manifest() -> SettingsManifest {
    SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: SETTING_DEFS
            .iter()
            .map(|def| SettingSpec {
                key: def.key.into(),
                description: def.description.into(),
                default: def.default.to_string(),
                kind: if def.duration {
                    SettingKind::DurationSeconds {
                        min: def.min,
                        max: def.max,
                    }
                } else {
                    SettingKind::Integer {
                        min: def.min,
                        max: def.max,
                    }
                },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            })
            .collect(),
    }
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&settings_manifest())?)
}

// ── host helpers ────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    unsafe { now(String::new()) }
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
pub(crate) struct FishingSettings {
    pub(crate) danger_confirm_seconds: i64,
    pub(crate) danger_recovery_seconds: i64,
    pub(crate) danger_minor_injury_seconds: i64,
    pub(crate) danger_weapon_drop_percent: i64,
    pub(crate) dynamite_hand_regrow_seconds: i64,
    pub(crate) chum_active_seconds: i64,
    pub(crate) chum_cooldown_seconds: i64,
    pub(crate) lure_xp_cost: i64,
    pub(crate) chum_xp_cost: i64,
    pub(crate) limb_heal_xp_cost: i64,
    pub(crate) rod_max_strength: u8,
    pub(crate) rod_strength_per_level: i64,
    pub(crate) brass_percent: i64,
}

/// Clamp a raw host setting value into the knob's range, falling back to its default when the
/// setting is unset or unparseable. The pure half of [`setting_i64`], so it can be tested off-wasm.
fn clamp_setting(raw: Option<&str>, def: &SettingDef) -> i64 {
    raw.and_then(|value| value.trim().parse::<i64>().ok())
        .map(|value| value.clamp(def.min, def.max))
        .unwrap_or(def.default)
}

fn setting_i64(key: &str, server: &str) -> i64 {
    let def = setting_def(key);
    let raw = unsafe {
        setting_get(
            serde_json::to_string(&SettingGet {
                key: key.into(),
                server: Some(server.into()),
                channel: None,
            })
            .unwrap_or_default(),
        )
    };
    clamp_setting(raw.as_deref().ok(), def)
}

pub(crate) fn fishing_settings(server: &str) -> FishingSettings {
    let get = |key| setting_i64(key, server);
    FishingSettings {
        danger_confirm_seconds: get("danger_confirm_seconds"),
        danger_recovery_seconds: get("danger_recovery_seconds"),
        danger_minor_injury_seconds: get("danger_minor_injury_seconds"),
        danger_weapon_drop_percent: get("danger_weapon_drop_percent"),
        dynamite_hand_regrow_seconds: get("dynamite_hand_regrow_seconds"),
        chum_active_seconds: get("chum_active_seconds"),
        chum_cooldown_seconds: get("chum_cooldown_seconds"),
        lure_xp_cost: get("lure_xp_cost"),
        chum_xp_cost: get("chum_xp_cost"),
        limb_heal_xp_cost: get("limb_heal_xp_cost"),
        rod_max_strength: get("rod_max_strength") as u8,
        rod_strength_per_level: get("rod_strength_per_level"),
        brass_percent: get("brass_percent"),
    }
}

/// Render a duration as `1h 2m 3s`, dropping empty leading units. Negative inputs clamp to zero.
fn format_elapsed(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}h {minutes}m {seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
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

/// Lifetime XP a player has banked: every completed level's worth plus the unspent remainder.
fn lifetime_xp(p: &Player) -> i64 {
    (0..p.level)
        .map(xp_for_level)
        .sum::<i64>()
        .saturating_add(p.xp)
}

/// One-time merge of the retired expedition system: fold every stashed universe's lifetime XP
/// into the active (Prime) save, re-derive the level on the endless curve, and clear the stash.
/// Runs on every load; once merged, no stashes remain, so later loads are no-ops. Merged XP is
/// spendable — it always was, in its own universe.
fn merge_stashed_universes(state: &mut State) {
    for (key, worlds) in state.stash.drain() {
        let merged = worlds.iter().map(lifetime_xp).sum::<i64>();
        if let Some(p) = state.players.get_mut(&key) {
            p.xp = lifetime_xp(p).saturating_add(merged);
            p.level = 0;
            p.universe_index = 0;
            p.universe_name.clear();
            p.universe_theme.clear();
            check_level_up(p);
        }
    }
}

// Storage layout: each player lives in its own `player:{key}` entry and everything shared (casts,
// chum, events, champions) in `data`, so a reel rewrites one player rather than every angler on
// every network. Older saves kept players inside `data`; they load as before and move out on the
// next save. An empty `player:` value is a deleted player.

const PLAYER_PREFIX: &str = "player:";

/// Serialized form of what was last loaded or saved, so a save writes only what changed.
#[derive(Default)]
struct Snapshot {
    data: Option<String>,
    players: HashMap<String, String>,
}

thread_local! {
    static SNAPSHOT: std::cell::RefCell<Snapshot> = std::cell::RefCell::new(Snapshot::default());
}

/// The whole game state from this module's KV entries, plus the snapshot of per-player entries.
/// Persistent state must never be discarded just because one field is malformed: returning the
/// parse error keeps a later command from saving an empty State over the original.
fn state_from_entries(entries: &[ModuleKvEntry]) -> Result<(State, Snapshot), Error> {
    let mut snapshot = Snapshot::default();
    let mut state = match entries.iter().find(|entry| entry.key == "data") {
        Some(entry) if !entry.value.is_empty() => {
            let state: State = serde_json::from_str(&entry.value)?;
            // A legacy `data` still holding players must be rewritten without them.
            if state.players.is_empty() {
                snapshot.data = Some(entry.value.clone());
            }
            state
        }
        _ => State::default(),
    };
    for entry in entries {
        let Some(key) = entry.key.strip_prefix(PLAYER_PREFIX) else {
            continue;
        };
        snapshot
            .players
            .insert(key.to_string(), entry.value.clone());
        if entry.value.is_empty() {
            state.players.remove(key);
        } else {
            state
                .players
                .insert(key.to_string(), serde_json::from_str(&entry.value)?);
        }
    }
    Ok((state, snapshot))
}

/// Everything but the players, as stored under `data`.
fn shared_json(state: &State) -> Result<String, Error> {
    let mut value = serde_json::to_value(state)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("players");
    }
    Ok(serde_json::to_string(&value)?)
}

fn load_state() -> Result<State, Error> {
    let raw = unsafe { kv_list(serde_json::to_string(&KvList::default())?)? };
    let entries: Vec<ModuleKvEntry> = serde_json::from_str(&raw)?;
    let (mut state, snapshot) = state_from_entries(&entries)?;
    merge_stashed_universes(&mut state);
    SNAPSHOT.with(|stored| *stored.borrow_mut() = snapshot);
    Ok(state)
}

fn kv_write(key: &str, value: &str) -> Result<(), Error> {
    let req = KvSet {
        key: key.into(),
        value: value.into(),
    };
    unsafe { kv_set(serde_json::to_string(&req)?)? };
    Ok(())
}

fn save_state(state: &State) -> Result<(), Error> {
    let mut snapshot = SNAPSHOT.with(|stored| std::mem::take(&mut *stored.borrow_mut()));
    // Players first: if a save stops part-way, a legacy `data` still has them.
    for (key, player) in &state.players {
        let value = serde_json::to_string(player)?;
        if snapshot.players.get(key) != Some(&value) {
            kv_write(&format!("{PLAYER_PREFIX}{key}"), &value)?;
            snapshot.players.insert(key.clone(), value);
        }
    }
    for (key, value) in snapshot.players.iter_mut() {
        if !state.players.contains_key(key) && !value.is_empty() {
            kv_write(&format!("{PLAYER_PREFIX}{key}"), "")?;
            value.clear();
        }
    }
    let data = shared_json(state)?;
    if snapshot.data.as_ref() != Some(&data) {
        kv_write("data", &data)?;
        snapshot.data = Some(data);
    }
    SNAPSHOT.with(|stored| *stored.borrow_mut() = snapshot);
    Ok(())
}

// ── bundled fish database ───────────────────────────────────────────────────

const FISH_DB_JSON: &str = include_str!("../fish_database.json");

struct Data {
    locations: Vec<Location>,
    fish_by_location: HashMap<String, Vec<Fish>>,
    junk_items: HashMap<String, Vec<String>>,
    rarity_weights: Vec<(String, i64)>,
    rarity_xp_multiplier: HashMap<String, i64>,
    cast_messages: Vec<String>,
    too_early_messages: Vec<String>,
    danger_zone_messages: HashMap<String, Vec<String>>,
    events: HashMap<String, EventDef>,
    artifacts: Vec<Artifact>,
    void_nests: VoidNests,
}

fn data() -> &'static Data {
    static DATA: OnceLock<Data> = OnceLock::new();
    DATA.get_or_init(|| {
        let v: serde_json::Value =
            serde_json::from_str(FISH_DB_JSON).expect("valid fish_database.json");
        let mut locations: Vec<Location> =
            serde_json::from_value(v["locations"].clone()).unwrap_or_default();
        let mut fish_by_location = HashMap::new();
        for loc in &locations {
            let fish: Vec<Fish> = serde_json::from_value(v[&loc.name].clone()).unwrap_or_default();
            fish_by_location.insert(loc.name.clone(), fish);
        }
        let expansion: VoidExpansion = serde_json::from_value(v["void_expansion"].clone())
            .expect("valid void expansion in fish_database.json");
        for tier in expansion.tiers {
            let fish = expansion
                .fish
                .iter()
                .cloned()
                .map(|mut fish| {
                    fish.name = fish.name.replace("{color}", &tier.color);
                    fish.min_weight *= tier.weight_multiplier;
                    fish.max_weight *= tier.weight_multiplier;
                    fish
                })
                .collect();
            fish_by_location.insert(tier.name.clone(), fish);
            locations.push(Location {
                name: tier.name,
                level: tier.level,
                max_distance: tier.max_distance,
                kind: "space".into(),
            });
        }
        // Preserve the configured rarity order (common..legendary) for weighted selection.
        let rarity_weights = ["common", "uncommon", "rare", "legendary"]
            .iter()
            .filter_map(|r| v["rarity_weights"][r].as_i64().map(|w| (r.to_string(), w)))
            .collect();
        Data {
            locations,
            fish_by_location,
            junk_items: serde_json::from_value(v["junk_items"].clone()).unwrap_or_default(),
            rarity_weights,
            rarity_xp_multiplier: serde_json::from_value(v["rarity_xp_multiplier"].clone())
                .unwrap_or_default(),
            cast_messages: serde_json::from_value(v["cast_messages"].clone()).unwrap_or_default(),
            too_early_messages: serde_json::from_value(v["too_early_messages"].clone())
                .unwrap_or_default(),
            danger_zone_messages: serde_json::from_value(v["danger_zone_messages"].clone())
                .unwrap_or_default(),
            events: serde_json::from_value(v["events"].clone()).unwrap_or_default(),
            artifacts: serde_json::from_value(v["artifacts"].clone()).unwrap_or_default(),
            void_nests: serde_json::from_value(v["void_nests"].clone())
                .expect("valid void_nests in fish_database.json"),
        }
    })
}

// ── persistent state ────────────────────────────────────────────────────────

fn lifecycle_player_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.clone())
        .chain(
            request
                .aliases
                .iter()
                .map(|alias| fold_nick(&request.subject.server, alias)),
        )
        .map(|identity| format!("{}/{}", request.subject.server, identity))
        .collect()
}

fn lifecycle_chum_matches(chum: &Chum, request: &ModuleDataRequest, keys: &[String]) -> bool {
    keys.contains(&chum.by_id)
        || request.aliases.iter().any(|alias| {
            fold_nick(&request.subject.server, &chum.by_name)
                == fold_nick(&request.subject.server, alias)
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let (state, _) = state_from_entries(&request.entries)?;
    let keys = lifecycle_player_keys(&request);
    let players = keys
        .iter()
        .filter_map(|key| state.players.get(key).map(|player| (key, player)))
        .map(|(key, player)| serde_json::json!({ "key": key, "player": player }))
        .collect::<Vec<_>>();
    let active_casts = keys
        .iter()
        .filter_map(|key| state.active_casts.get(key).map(|cast| (key, cast)))
        .map(|(key, cast)| serde_json::json!({ "key": key, "cast": cast }))
        .collect::<Vec<_>>();
    let chum = state
        .chum
        .get(&request.subject.server)
        .filter(|chum| lifecycle_chum_matches(chum, &request, &keys));
    let stash = keys
        .iter()
        .filter_map(|key| state.stash.get(key).map(|worlds| (key, worlds)))
        .map(|(key, worlds)| serde_json::json!({ "key": key, "worlds": worlds }))
        .collect::<Vec<_>>();
    let prestige = keys
        .iter()
        .filter_map(|key| state.prestige.get(key).map(|stars| (key, stars)))
        .map(|(key, stars)| serde_json::json!({ "key": key, "stars": stars }))
        .collect::<Vec<_>>();
    let data = if players.is_empty()
        && active_casts.is_empty()
        && chum.is_none()
        && stash.is_empty()
        && prestige.is_empty()
    {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "players": players, "active_casts": active_casts, "chum": chum, "stashed_universes": stash, "deep_stars": prestige })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&data_delete_plan(&request)?)?)
}

fn data_delete_plan(request: &ModuleDataRequest) -> Result<ModuleDataDeletePlan, Error> {
    let keys = lifecycle_player_keys(request);
    // Players stored on their own are deleted outright.
    let mut mutations = request
        .entries
        .iter()
        .filter(|entry| {
            entry
                .key
                .strip_prefix(PLAYER_PREFIX)
                .is_some_and(|key| keys.iter().any(|wanted| wanted == key))
        })
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect::<Vec<_>>();
    let Some(entry) = request.entries.iter().find(|entry| entry.key == "data") else {
        return Ok(ModuleDataDeletePlan {
            version: DATA_LIFECYCLE_VERSION,
            mutations,
        });
    };
    // `data` is edited as stored, including any players a legacy save still keeps there.
    let mut state: State = serde_json::from_str(&entry.value)?;
    let mut changed = false;
    for key in &keys {
        changed |= state.players.remove(key).is_some();
        changed |= state.active_casts.remove(key).is_some();
        changed |= state.stash.remove(key).is_some();
        changed |= state.prestige.remove(key).is_some();
    }
    for chum in state.chum.values_mut() {
        let before = chum.cooldown_notices.len();
        chum.cooldown_notices
            .retain(|profile_id, _| !keys.contains(profile_id));
        changed |= chum.cooldown_notices.len() != before;
    }
    if state
        .chum
        .get(&request.subject.server)
        .is_some_and(|chum| lifecycle_chum_matches(chum, request, &keys))
    {
        state.chum.remove(&request.subject.server);
        changed = true;
    }
    if let Some(champions) = state.champions.get_mut(&request.subject.server) {
        for (id, name) in [
            (&mut champions.traveler, &mut champions.traveler_name),
            (&mut champions.caster, &mut champions.caster_name),
            (&mut champions.collector, &mut champions.collector_name),
        ] {
            if id.as_ref().is_some_and(|id| keys.contains(id)) {
                *id = None;
                name.clear();
                changed = true;
            }
        }
    }
    if changed {
        mutations.push(ModuleKvMutation {
            key: entry.key.clone(),
            value: Some(serde_json::to_string(&state)?),
        });
    }
    Ok(ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })
}

fn species_key(location: &str, name: &str) -> String {
    // Unit Separator avoids collisions without depending on user-visible punctuation.
    format!("{location}\u{1f}{name}")
}

fn mastery_for(catches: i64) -> Option<&'static str> {
    match catches {
        250.. => Some("Iridescent"),
        100.. => Some("Gold"),
        25.. => Some("Silver"),
        5.. => Some("Bronze"),
        _ => None,
    }
}

fn migrate_species_careers(player: &mut Player) -> bool {
    if player.species_careers_migrated {
        return false;
    }
    for (name, catches) in &player.catches {
        let matches: Vec<(&str, &Fish)> = data()
            .fish_by_location
            .iter()
            .flat_map(|(location, fish)| {
                fish.iter()
                    .filter(move |candidate| candidate.name == *name)
                    .map(move |candidate| (location.as_str(), candidate))
            })
            .collect();
        let (location, key) = if matches.len() == 1 {
            let location = matches[0].0;
            (location.to_string(), species_key(location, name))
        } else {
            // Retain otherwise-unmappable history instead of silently assigning it incorrectly.
            ("Legacy".to_string(), species_key("Legacy", name))
        };
        player.species_careers.entry(key).or_insert(SpeciesCareer {
            name: name.clone(),
            location,
            catches: *catches,
            ..Default::default()
        });
    }
    player.species_careers_migrated = true;
    true
}

/// Keep the per-species maps bounded: nests add species forever, so only the top
/// [`MAX_SPECIES_CAREERS`] by catches are retained.
fn cap_species_maps(player: &mut Player) {
    if player.species_careers.len() > MAX_SPECIES_CAREERS {
        let mut entries: Vec<(String, SpeciesCareer)> = player.species_careers.drain().collect();
        entries.sort_by(|a, b| b.1.catches.cmp(&a.1.catches).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(MAX_SPECIES_CAREERS);
        player.species_careers = entries.into_iter().collect();
    }
    if player.catches.len() > MAX_SPECIES_CAREERS {
        let mut entries: Vec<(String, i64)> = player.catches.drain().collect();
        entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        entries.truncate(MAX_SPECIES_CAREERS);
        player.catches = entries.into_iter().collect();
    }
}

fn record_species_catch(
    player: &mut Player,
    location: &str,
    fish: &Fish,
    landed_weight: f64,
    natural_weight: f64,
) -> CatchMilestones {
    migrate_species_careers(player);
    *player.catches.entry(fish.name.clone()).or_insert(0) += 1;
    let career = player
        .species_careers
        .entry(species_key(location, &fish.name))
        .or_insert_with(|| SpeciesCareer {
            name: fish.name.clone(),
            location: location.to_string(),
            ..Default::default()
        });
    let previous_mastery = mastery_for(career.catches);
    career.catches += 1;
    let mastery = mastery_for(career.catches);
    let quality = if fish.max_weight > 0.0 {
        natural_weight / fish.max_weight
    } else {
        0.0
    };
    let previous_record = career.best_weight;
    let new_record = landed_weight > previous_record;
    if new_record {
        career.best_weight = landed_weight;
        career.best_record_quality = quality;
    }
    career.best_quality = career.best_quality.max(quality);
    cap_species_maps(player);
    CatchMilestones {
        previous_mastery,
        mastery,
        previous_record,
        new_record,
        trophy: quality >= 0.95,
    }
}

/// A stable numeric id for a location name: bundled locations are their index in
/// `data().locations`; a nest location encodes as `N + (nest-1)*levels_per_nest + base`.
fn location_id(name: &str) -> Option<i64> {
    if let Some(i) = data().locations.iter().position(|l| l.name == name) {
        return Some(i as i64);
    }
    if let Some((nest, base)) = parse_nest_location(name) {
        let n = data().locations.len() as i64;
        return Some(n + (nest - 1) * data().void_nests.levels_per_nest.max(1) + base as i64);
    }
    None
}

/// One-time migration of `locations_fished` names into `location_ids`.
fn migrate_locations_fished(player: &mut Player) -> bool {
    if player.locations_migrated {
        return false;
    }
    for name in &player.locations_fished {
        if let Some(id) = location_id(name) {
            if !player.location_ids.contains(&id) {
                player.location_ids.push(id);
            }
        }
    }
    player.locations_fished.clear();
    player.locations_migrated = true;
    true
}

/// Record that `location_name` was fished, as a stable id.
fn record_location(player: &mut Player, location_name: &str) {
    migrate_locations_fished(player);
    if let Some(id) = location_id(location_name) {
        if !player.location_ids.contains(&id) {
            player.location_ids.push(id);
        }
    }
}

// ── small deterministic generator, seeded from host-provided OS randomness ───

struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform float in [0, 1).
    fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    fn choice<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len())])
        }
    }
}

// ── game math (pure, unit-tested) ───────────────────────────────────────────

const MIN_WAIT_HOURS: f64 = 1.0;
const OPTIMAL_WAIT_HOURS: f64 = 24.0;
const DANGER_THRESHOLD_HOURS: f64 = 24.0;
const VAMPIRE_HOUR: i64 = 666;

fn is_vampire_hour(elapsed_seconds: i64) -> bool {
    (VAMPIRE_HOUR * 3600..(VAMPIRE_HOUR + 1) * 3600).contains(&elapsed_seconds)
}

fn vampire_shark(elapsed_seconds: i64) -> Option<Fish> {
    is_vampire_hour(elapsed_seconds).then(|| Fish {
        name: "Vampire Shark".into(),
        min_weight: 666.0,
        max_weight: 666.0,
        rarity: "legendary".into(),
    })
}
const LEGACY_MAX_LEVEL: i64 = 9;
const EXPANSION_MAX_LEVEL: i64 = 19;
const VOID_EXPANSION_START: i64 = 1_782_864_000; // 2026-07-01 00:00:00 UTC
const NESTED_VOID_START: i64 = 1_798_761_600; // 2027-01-01 00:00:00 UTC (the Q1 2027 season boundary)
const BAIT_XP_PER_HOUR: i64 = 100;
const MAX_BAIT_XP: i64 = 1_700;

// Reinforced rod: a permanent strength sink for level 15+ players that lowers line-break chance.
// Each point reduces break chance by 1%, floored at ROD_BREAK_FLOOR of the capped natural risk, so
// megafauna stay scary but every catch remains possible. Built with `!fix` (time), worn only by
// big fish.
const ROD_UNLOCK_LEVEL: i64 = 15;
const ROD_MAX_STRENGTH: u8 = 50;
/// Passive rod reinforcement: each level past [`ROD_UNLOCK_LEVEL`] adds this many strength
/// points, so the rod keeps mattering without the retired `!fix` workshop.
const ROD_STRENGTH_PER_LEVEL: i64 = 5;
/// One point of rod wear regenerates per this many seconds — no command needed.
const ROD_WEAR_REGEN_SECS: i64 = 3_600;
const ROD_BIG_FISH_THRESHOLD: f64 = 2000.0;
const ROD_DECAY_EVERY: u8 = 10;
/// Caps on per-player maps so KV stays bounded across infinitely many nest species.
const MAX_SPECIES_CAREERS: usize = 200;
/// Location names listed before collapsing to "current, next, …and N more".
const MAX_LOCATION_LIST: usize = 20;
const HAND_REGROW_SECS: i64 = 7 * 86_400;
const LURE_XP_COST: i64 = 30;
const CHUM_XP_COST: i64 = 250;
const LIMB_HEAL_XP_COST: i64 = 10_000;
const CHUM_ACTIVE_SECS: i64 = 20 * 60;
const CHUM_COOLDOWN_SECS: i64 = 50 * 60;
const MINOR_INJURY_SECS: i64 = 2 * 86_400;
const DANGER_WEAPON_DROP_PERCENT: i64 = 25;
/// Even an unreinforced line retains this much landing chance. This also bounds future fish and
/// size multipliers instead of relying on one-off exceptions for today's heaviest species.
const MAX_NATURAL_BREAK_CHANCE: f64 = 0.95;
/// Break chance never drops below this fraction of its natural value (0.5 = 50% of natural risk).
const ROD_BREAK_FLOOR: f64 = 0.5;

fn expansion_active(at: i64) -> bool {
    at >= VOID_EXPANSION_START
}

/// The nested voids open at the Q1 2027 boundary; before that the coloured tiers are the end.
fn nested_active(at: i64) -> bool {
    at >= NESTED_VOID_START
}

fn max_level(at: i64) -> i64 {
    if nested_active(at) {
        // Nested voids are infinite: every level has a home, so nothing is dormant.
        i64::MAX
    } else if expansion_active(at) {
        EXPANSION_MAX_LEVEL
    } else {
        LEGACY_MAX_LEVEL
    }
}

fn xp_for_level(level: i64) -> i64 {
    (100.0 * ((level + 1) as f64).powf(1.5)) as i64
}

// ── nested voids: the world inside the Void ──────────────────────────────────
//
// Past the coloured tiers, nest k (First Void, Second Void, …) replays the ten base locations in
// world order: nest k spans levels start + (k-1)*10 .. start + k*10 - 1. Locations and fish are
// computed by formula from the base locations, so there is no cap and no final frontier.

/// The `(nest, base)` a level maps to once nested voids are active; `None` below the start level.
fn nest_for_level(level: i64) -> Option<(i64, usize)> {
    let n = &data().void_nests;
    let per = n.levels_per_nest.max(1);
    if level < n.start_level {
        return None;
    }
    let off = level - n.start_level;
    Some((off / per + 1, (off % per).min(per - 1) as usize))
}

/// The `base`-th bundled base location (the first `levels_per_nest` static locations).
fn base_location(base: usize) -> &'static Location {
    let d = data();
    let count = (d.void_nests.levels_per_nest.max(1) as usize).min(d.locations.len());
    &d.locations[base.min(count - 1)]
}

const ORDINAL_WORDS: &[&str] = &[
    "First",
    "Second",
    "Third",
    "Fourth",
    "Fifth",
    "Sixth",
    "Seventh",
    "Eighth",
    "Ninth",
    "Tenth",
    "Eleventh",
    "Twelfth",
    "Thirteenth",
    "Fourteenth",
    "Fifteenth",
    "Sixteenth",
    "Seventeenth",
    "Eighteenth",
    "Nineteenth",
    "Twentieth",
];

fn ordinal_suffix(n: i64) -> &'static str {
    if (11..=13).contains(&(n % 100)) {
        "th"
    } else {
        match n % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        }
    }
}

/// "First", "Second", … "Twentieth", then "21st", "22nd", … — display names for nests.
fn ordinal(n: i64) -> String {
    if (1..=ORDINAL_WORDS.len() as i64).contains(&n) {
        ORDINAL_WORDS[(n - 1) as usize].to_string()
    } else {
        format!("{n}{}", ordinal_suffix(n))
    }
}

/// Parse an ordinal back: exact words, or a full number with its suffix ("21st"). Prefix
/// matching is allowed only for the word forms, and only when unambiguous.
fn parse_ordinal(text: &str) -> Option<i64> {
    let t = text.trim().to_lowercase();
    if let Some(i) = ORDINAL_WORDS.iter().position(|w| w.to_lowercase() == t) {
        return Some(i as i64 + 1);
    }
    let matches: Vec<i64> = ORDINAL_WORDS
        .iter()
        .enumerate()
        .filter(|(_, w)| w.to_lowercase().starts_with(&t))
        .map(|(i, _)| i as i64 + 1)
        .collect();
    if matches.len() == 1 {
        return Some(matches[0]);
    }
    let digits = t
        .trim_end_matches("st")
        .trim_end_matches("nd")
        .trim_end_matches("rd")
        .trim_end_matches("th");
    if digits != t && digits.chars().all(|c| c.is_ascii_digit()) && !digits.is_empty() {
        return digits.parse().ok();
    }
    None
}

/// Match the base-location half of a nest query: exact name, else a unique prefix.
fn find_base_location(query: &str) -> Option<usize> {
    let d = data();
    let count = (d.void_nests.levels_per_nest.max(1) as usize).min(d.locations.len());
    if let Some(i) = (0..count).find(|&i| d.locations[i].name.to_lowercase() == query) {
        return Some(i);
    }
    let prefix: Vec<usize> = (0..count)
        .filter(|&i| d.locations[i].name.to_lowercase().starts_with(query))
        .collect();
    (prefix.len() == 1).then_some(prefix[0])
}

/// Parse a nested-void location name ("puddle in the first void") into `(nest, base)`.
fn parse_nest_location(query: &str) -> Option<(i64, usize)> {
    let q = query.trim().to_lowercase();
    let (base_part, rest) = q.rsplit_once(" in the ")?;
    let nest_part = rest.strip_suffix(" void")?;
    let nest = parse_ordinal(nest_part.trim())?;
    let base = find_base_location(base_part.trim())?;
    Some((nest, base))
}

/// The location for a nest slot, generated from its base location and the data templates.
fn nest_location(nest: i64, base: usize) -> Location {
    let n = &data().void_nests;
    let base_loc = base_location(base);
    let name = fill(
        &n.location_name_template,
        &[("base", &base_loc.name), ("ordinal", &ordinal(nest))],
    );
    let level = n.start_level + (nest - 1) * n.levels_per_nest.max(1) + base as i64;
    Location {
        name,
        level,
        max_distance: base_loc.max_distance
            * n.distance_multiplier_per_nest
                .powi((nest - 1).max(0) as i32),
        // Nested waters are void waters: space junk, void events.
        kind: "space".into(),
    }
}

/// The fish living in a nest location: the base location's fish, renamed and rescaled per nest.
fn nest_fish(nest: i64, base: usize) -> Vec<Fish> {
    let n = &data().void_nests;
    let loc = nest_location(nest, base);
    let mult = n.weight_multiplier_per_nest.powi((nest - 1).max(0) as i32);
    data().fish_by_location[&base_location(base).name]
        .iter()
        .map(|f| Fish {
            name: fill(
                &n.fish_name_template,
                &[("fish", &f.name), ("location", &loc.name)],
            ),
            min_weight: f.min_weight * mult,
            max_weight: f.max_weight * mult,
            rarity: f.rarity.clone(),
        })
        .collect()
}

/// All fish that live at `location_name` — bundled locations come straight from the database,
/// nested-void locations are generated from their base location's fish.
fn fish_table(location_name: &str) -> Vec<Fish> {
    if let Some(fish) = data().fish_by_location.get(location_name) {
        return fish.clone();
    }
    if let Some((nest, base)) = parse_nest_location(location_name) {
        return nest_fish(nest, base);
    }
    Vec::new()
}

/// Any location by its exact name — bundled or generated.
fn location_by_name(name: &str) -> Location {
    if let Some(l) = data().locations.iter().find(|l| l.name == name) {
        return l.clone();
    }
    if let Some((nest, base)) = parse_nest_location(name) {
        return nest_location(nest, base);
    }
    data().locations[0].clone()
}

/// Best location for a level: a generated nest location once nested voids are active and the
/// level is past the coloured tiers, else the highest bundled location at or below the level.
fn location_for_level(level: i64, now: i64) -> Location {
    if nested_active(now) {
        if let Some((nest, base)) = nest_for_level(level) {
            return nest_location(nest, base);
        }
    }
    let d = data();
    d.locations
        .iter()
        .rev()
        .find(|l| l.level <= level)
        .cloned()
        .unwrap_or_else(|| d.locations[0].clone())
}

// ── catch epithets (endless-level flavour) ───────────────────────────────────

/// Cosmetic adjectives some catches wear once an angler outlevels the old expansion cap. The
/// first unlocks at DESCRIPTOR_FIRST_LEVEL, then one more every DESCRIPTOR_EVERY levels until
/// the list runs out; after that, levels alone mark progress.
const FISH_DESCRIPTORS: &[&str] = &[
    "Verdant",
    "Ashen",
    "Cerulean",
    "Crimson",
    "Obsidian",
    "Gilded",
    "Frostbound",
    "Umbral",
    "Radiant",
    "Duskwater",
];
const DESCRIPTOR_FIRST_LEVEL: i64 = EXPANSION_MAX_LEVEL + 1;
const DESCRIPTOR_EVERY: i64 = 10;
/// Chance that an eligible catch wears one of the unlocked epithets.
const DESCRIPTOR_CHANCE: f64 = 0.25;

/// How many epithets `level` has unlocked (0 until DESCRIPTOR_FIRST_LEVEL).
fn descriptors_unlocked(level: i64) -> usize {
    if level < DESCRIPTOR_FIRST_LEVEL {
        return 0;
    }
    let tiers = ((level - DESCRIPTOR_FIRST_LEVEL) / DESCRIPTOR_EVERY + 1) as usize;
    tiers.min(FISH_DESCRIPTORS.len())
}

/// Roll for a catch epithet: past the old cap, some fish wear a random unlocked adjective.
fn descriptor_for(rng: &mut Rng, level: i64) -> Option<&'static str> {
    let tiers = descriptors_unlocked(level);
    if tiers == 0 || rng.f64() >= DESCRIPTOR_CHANCE {
        return None;
    }
    Some(FISH_DESCRIPTORS[rng.below(tiers)])
}

/// Star count for an identity. No new stars are minted — the expedition system is retired —
/// but the count stays as a permanent badge for worlds mastered back when portals existed.
fn star_count(state: &State, key: &str) -> i64 {
    state.prestige.get(key).copied().unwrap_or(0)
}

/// Find a location by name: exact match first, then a unique prefix among the bundled
/// locations, then a nested-void name ("puddle in the first void", with a unique base prefix
/// allowed). The old loose substring fallback is gone — with nested names it matched the wrong
/// water ("void" would have caught several).
fn find_location(query: &str) -> Option<Location> {
    let q = query.trim().to_lowercase();
    let d = data();
    if let Some(l) = d.locations.iter().find(|l| l.name.to_lowercase() == q) {
        return Some(l.clone());
    }
    let prefix: Vec<&Location> = d
        .locations
        .iter()
        .filter(|l| l.name.to_lowercase().starts_with(&q))
        .collect();
    if prefix.len() == 1 {
        return Some(prefix[0].clone());
    }
    if prefix.is_empty() {
        if let Some((nest, base)) = parse_nest_location(&q) {
            return Some(nest_location(nest, base));
        }
    }
    None
}

/// Names of every location a level can fish: bundled locations within the level and the gate,
/// plus the generated nest locations up to that level once nested voids are active.
fn available_location_names(level: i64, now: i64) -> Vec<String> {
    let cap = max_level(now);
    let mut names: Vec<String> = data()
        .locations
        .iter()
        .filter(|l| l.level <= level && l.level <= cap)
        .map(|l| l.name.clone())
        .collect();
    if nested_active(now) {
        if let Some((nest, _)) = nest_for_level(level) {
            let per = data().void_nests.levels_per_nest.max(1) as usize;
            for k in 1..=nest {
                for base in 0..per {
                    let loc = nest_location(k, base);
                    if loc.level <= level {
                        names.push(loc.name);
                    }
                }
            }
        }
    }
    names
}

/// Collapse a long location list to "current, next, …and N more" so endgame lines stay short.
fn summarize_locations(avail: &[String], level: i64, now: i64) -> String {
    if avail.len() <= MAX_LOCATION_LIST {
        return avail.join(", ");
    }
    let current = location_for_level(level, now).name;
    let next = location_for_level(level + 1, now).name;
    if next == current {
        format!("{current}, …and {} more", avail.len() - 1)
    } else {
        format!("{current}, {next}, …and {} more", avail.len() - 2)
    }
}

/// Whether a location counts as Void water for location-restricted events (solar flare,
/// meteor shower): The Void itself, the coloured tiers, and every nested-void location.
fn is_void_location(name: &str) -> bool {
    name == "The Void" || name.ends_with(" Void") || parse_nest_location(name).is_some()
}

fn location_prep(loc: &Location) -> String {
    if parse_nest_location(&loc.name).is_some() {
        // Nested voids are bodies of water inside the void, not directions toward it.
        format!("into the {}", loc.name)
    } else if loc.kind == "space" {
        match loc.name.as_str() {
            "The Void" => "into The Void".into(),
            "Moon" => "toward the Moon".into(),
            other => format!("toward {other}"),
        }
    } else {
        format!("into the {}", loc.name)
    }
}

fn cast_distance(rng: &mut Rng, level: i64, loc: &Location) -> f64 {
    let max = loc.max_distance;
    let min = max * 0.3;
    // Preserve the original curve through level 9, then cap it. Higher Void tiers already
    // increase max_distance; allowing this bonus to grow too would exceed the location maximum.
    let level_bonus = (level as f64 / LEGACY_MAX_LEVEL as f64).min(1.0) * 0.3;
    let base_max = max * (0.7 + level_bonus);
    round1(rng.range(min, base_max))
}

fn event_allows_location(locations: &[String], location: &str) -> bool {
    locations.iter().any(|candidate| candidate == location)
        || (is_void_location(location) && locations.iter().any(|candidate| candidate == "The Void"))
}

/// Weighted rarity selection adjusted by wait time, an event rare-boost multiplier, and a combined
/// artifact/lure rarity boost (fraction of common weight shifted up to rare/legendary).
fn artifact_bonus(player: &Player, kind: &str) -> f64 {
    player
        .artifact
        .as_ref()
        .filter(|a| a.bonus_type == kind)
        .map(|a| a.bonus_value)
        .unwrap_or(0.0)
}

// ── reinforced rod ──────────────────────────────────────────────────────────

/// Bound the raw weight-derived chance, then reduce it by the player's rod strength. Capping before
/// applying strength guarantees every fish can be landed and lets reinforcement help consistently,
/// even when the raw formula exceeds 100% for Krakens or boosted Leviathans.
fn effective_break_chance(natural: f64, strength: u8) -> f64 {
    let natural = natural.clamp(0.0, MAX_NATURAL_BREAK_CHANCE);
    let reduction = (strength as f64) / 100.0;
    (natural * (1.0 - reduction)).max(natural * ROD_BREAK_FLOOR)
}

/// The passive strength cap a level earns: `rod_strength_per_level` per level past
/// [`ROD_UNLOCK_LEVEL`], capped at `max_strength`.
fn rod_level_cap(level: i64, per_level: i64, max_strength: u8) -> u8 {
    if level < ROD_UNLOCK_LEVEL {
        return 0;
    }
    ((level - ROD_UNLOCK_LEVEL) * per_level).clamp(0, max_strength as i64) as u8
}

/// Wear still outstanding at `now`, after automatic regeneration. Does not mutate.
fn rod_wear_now(player: &Player, now: i64) -> u8 {
    if player.rod_wear == 0 || player.rod_wear_at <= 0 {
        return player.rod_wear;
    }
    let regen =
        ((now - player.rod_wear_at) / ROD_WEAR_REGEN_SECS).clamp(0, player.rod_wear as i64) as u8;
    player.rod_wear - regen
}

/// The player's current effective rod strength: the passive level-derived cap plus any legacy
/// `!fix` bonus, minus outstanding wear. Does not mutate; callers that go on to write rod state
/// should use [`settle_rod`] first.
fn current_rod_strength(player: &Player, now: i64, settings: &FishingSettings) -> u8 {
    let base = rod_level_cap(
        player.level,
        settings.rod_strength_per_level,
        settings.rod_max_strength,
    )
    .saturating_add(player.rod_strength)
    .min(settings.rod_max_strength);
    base.saturating_sub(rod_wear_now(player, now))
}

/// Retire any pending `!fix` fields (the workshop is gone) and fold wear regeneration into
/// `rod_wear`. Call this before any mutation of rod state so regen is never lost or double-counted.
fn settle_rod(player: &mut Player, now: i64, max_strength: u8) -> bool {
    let mut changed = false;
    if player.fixing_until.is_some() || player.fixing_hours.is_some() {
        player.fixing_until = None;
        player.fixing_hours = None;
        changed = true;
    }
    if player.rod_strength > max_strength {
        player.rod_strength = max_strength;
        changed = true;
    }
    if player.rod_wear > 0 && player.rod_wear_at > 0 {
        let regen = ((now - player.rod_wear_at) / ROD_WEAR_REGEN_SECS)
            .clamp(0, player.rod_wear as i64) as u8;
        if regen > 0 {
            player.rod_wear -= regen;
            player.rod_wear_at = now;
            changed = true;
        }
    }
    changed
}

/// Apply wear from one landed catch. Returns true when a wear point was added; the wear
/// regenerates on its own over time (see [`rod_wear_now`]).
fn apply_rod_wear(player: &mut Player, weight: f64, now: i64, settings: &FishingSettings) -> bool {
    if weight <= ROD_BIG_FISH_THRESHOLD || current_rod_strength(player, now, settings) == 0 {
        return false;
    }
    player.big_catch_counter = player.big_catch_counter.saturating_add(1);
    if player.big_catch_counter < ROD_DECAY_EVERY {
        return false;
    }
    player.big_catch_counter = 0;
    player.rod_wear = player.rod_wear.saturating_add(1);
    player.rod_wear_at = now;
    true
}

/// The active event for `server`, if present, unexpired, and valid for `location`. Clears expired.
fn active_event_for(
    state: &mut State,
    server: &str,
    location: &str,
    now: i64,
) -> Option<ActiveEvent> {
    let ev = state.active_events.get(server)?.clone();
    if now >= ev.expires {
        state.active_events.remove(server);
        return None;
    }
    if let Some(def) = data().events.get(&ev.type_id) {
        if let Some(locs) = &def.locations {
            if !event_allows_location(locs, location) {
                return None;
            }
        }
    }
    Some(ev)
}

/// 5% chance to start a random (location-valid) event on cast. Returns its name and description.
fn maybe_trigger_event(
    rng: &mut Rng,
    state: &mut State,
    server: &str,
    location: &str,
    now: i64,
) -> Option<(String, String)> {
    if rng.f64() > 0.05 {
        return None;
    }
    let candidates: Vec<(&String, &EventDef)> = data()
        .events
        .iter()
        .filter(|(_, e)| {
            e.locations
                .as_ref()
                .is_none_or(|locations| event_allows_location(locations, location))
        })
        .collect();
    let (id, def) = rng.choice(&candidates)?;
    let ev = ActiveEvent {
        name: def.name.clone(),
        description: def.description.clone(),
        effect: def.effect.clone(),
        multiplier: def.multiplier,
        expires: now + def.duration_minutes * 60,
        type_id: (*id).clone(),
    };
    let announce = (def.name.clone(), def.description.clone());
    state.active_events.insert(server.to_string(), ev);
    Some(announce)
}

// ── dynamite recovery ───────────────────────────────────────────────────────

/// Settle a completed `!dynamite` recovery. Legacy two-hand bans used their ban expiry as the
/// recovery time, so preserve that behavior while adding recovery for one-hand injuries.
fn settle_dynamite_hands(player: &mut Player, now: i64) -> bool {
    let regrow_at = player
        .dynamite_hands_regrow_at
        .or(player.dynamite_banned_until);
    let Some(regrow_at) = regrow_at else {
        // Older saves recorded a lost hand without its recovery deadline. Restore it rather than
        // leaving anyone permanently injured just because the old format cannot prove when the
        // seven-day window began.
        if player.dynamite_hands_lost > 0 {
            player.dynamite_hands_lost = 0;
            return true;
        }
        return false;
    };
    if now < regrow_at {
        if player.dynamite_hands_regrow_at.is_none() {
            player.dynamite_hands_regrow_at = Some(regrow_at);
            return true;
        }
        return false;
    }
    player.dynamite_hands_lost = 0;
    player.dynamite_banned_until = None;
    player.dynamite_hands_regrow_at = None;
    true
}

/// `!dynamite` ban gate: returns the future expiry if banned; clears an expired recovery and
/// returns `None`.
fn active_dynamite_ban(player: &mut Player, now: i64) -> Option<i64> {
    settle_dynamite_hands(player, now);
    player.dynamite_banned_until.filter(|&expiry| now < expiry)
}

// ── entry point ─────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let nick = msg.nick.as_str();
    let addr = if msg.display.is_empty() {
        nick
    } else {
        msg.display.as_str()
    };
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    // Other modules' commands must not pay for loading (and migrating) the whole fishing save.
    if !commands::is_fishing_command(cmd) {
        return Ok(());
    }

    let ctx = Ctx {
        server: &server,
        dest,
        nick,
        addr,
        user_id: &msg.user_id,
        role: msg.role,
    };

    // One-time migration from the legacy server/nick key to the host's stable profile UUID.
    if !msg.user_id.is_empty() {
        let mut state = load_state()?;
        if migrate_identity(&mut state, &server, nick, &msg.user_id) {
            save_state(&state)?;
        }
    }

    // Lazy seasonal reset (no scheduler in wasm): may crown champions + wipe before the command.
    {
        let mut state = load_state()?;
        let (crowned, state_changed) = maybe_seasonal_reset(&server, &mut state, now_secs());
        if state_changed {
            save_state(&state)?;
        }
        for champions in &crowned {
            announce_season(&ctx, champions)?;
        }
    }

    commands::dispatch(&ctx, cmd, arg)?;
    Ok(())
}

/// A champion's profile, from their player key, if it is a stable profile ID (older saves keyed
/// some players by nick, which can't be paid).
fn champion_profile(key: &str) -> Option<&str> {
    let (_, id) = key.split_once('/')?;
    let uuid = id.len() == 36 && id.chars().filter(|ch| *ch == '-').count() == 4;
    uuid.then_some(id)
}

/// The new season's crowning: who won each title (each paid `champion_brass`, once), then that
/// careers carry forward. Titles are honours only; they give no advantage next season.
fn announce_season(ctx: &Ctx, c: &Champions) -> Result<(), Error> {
    let prize = setting_i64("champion_brass", ctx.server).max(0) as u64;
    if prize > 0 {
        for (title, holder) in [
            ("traveler", &c.traveler),
            ("caster", &c.caster),
            ("collector", &c.collector),
        ] {
            if let Some(id) = holder.as_deref().and_then(champion_profile) {
                award_brass(
                    ctx.server,
                    id,
                    prize,
                    // Event ids can't contain spaces; "Q2 2026" becomes "Q2-2026".
                    &format!(
                        "fishing:champion:{}:{title}:{id}",
                        c.season.replace(' ', "-")
                    ),
                )?;
            }
        }
    }
    let traveler = match c.traveler {
        Some(_) => themed(
            "season_traveler",
            &["the Traveler: {name} (climbed {levels} levels)"],
            &[
                ("name", &commands::no_highlight(&c.traveler_name)),
                ("levels", &c.traveler_levels.to_string()),
                ("xp", &c.traveler_xp.to_string()),
            ],
        )?,
        None => themed(
            "season_traveler_none",
            &["the Traveler: unclaimed (nobody climbed a level this season)"],
            &[],
        )?,
    };
    let caster = match c.caster {
        Some(_) => themed(
            "season_caster",
            &["the Caster: {name} (cast {distance}m)"],
            &[
                ("name", &commands::no_highlight(&c.caster_name)),
                ("distance", &format!("{:.1}", c.caster_distance)),
            ],
        )?,
        None => themed(
            "season_caster_none",
            &["the Caster: unclaimed (no casts recorded this season)"],
            &[],
        )?,
    };
    let collector = match c.collector {
        Some(_) => themed(
            "season_collector",
            &["the Collector: {name} ({count} rare/legendary catches)"],
            &[
                ("name", &commands::no_highlight(&c.collector_name)),
                ("count", &c.collector_count.to_string()),
            ],
        )?,
        None => themed(
            "season_collector_none",
            &["the Collector: unclaimed (no rare catches this season)"],
            &[],
        )?,
    };
    let prize_text = if prize > 0 {
        themed(
            "season_prize",
            &[" · each title pays {brass} brass"],
            &[("brass", &prize.to_string())],
        )?
    } else {
        String::new()
    };
    ctx.say(
        "season_announcement",
        &["** NEW FISHING SEASON ** Career progress is safe! {season} champions: {champions}{prize}"],
        &[
            ("season", &c.season),
            ("champions", &[traveler, caster, collector].join(" | ")),
            ("prize", &prize_text),
        ],
    )?;
    ctx.say(
        "season_carry_forward",
        &["A new season begins; levels, catches, records, artifacts, XP, and active casts all carry forward."],
        &[],
    )
}

/// `{name}` placeholders in `template` replaced by `vars`, in one pass.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').map(|close| (&after[..close], close)) {
            Some((name, close)) if vars.iter().any(|(var, _)| *var == name) => {
                let value = vars.iter().find(|(var, _)| *var == name).map(|(_, v)| *v);
                out.push_str(value.unwrap_or_default());
                rest = &after[close + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

struct Ctx<'a> {
    server: &'a str,
    dest: &'a str,
    nick: &'a str,
    addr: &'a str,
    user_id: &'a str,
    role: Option<Role>,
}

impl Ctx<'_> {
    fn key(&self) -> String {
        let identity = if self.user_id.is_empty() {
            fold_nick(self.server, self.nick)
        } else {
            self.user_id.to_string()
        };
        format!("{}/{}", self.server, identity)
    }
    fn rng(&self, _state: &mut State) -> Result<Rng, Error> {
        let raw =
            unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
        let bytes = serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes;
        let seed = u64::from_le_bytes(
            bytes
                .try_into()
                .map_err(|_| Error::msg("random_bytes returned the wrong byte count"))?,
        );
        Ok(Rng(seed | 1))
    }
    /// Replies with a themed line. Many keys used to be a bare `"{text}"` fed a finished
    /// sentence; `text` is still supplied (the default, filled in) so an operator's wrapper
    /// around it, like `"🎣 {text}"`, keeps working.
    fn say(&self, key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<(), Error> {
        let text = defaults
            .first()
            .filter(|_| !vars.iter().any(|(name, _)| *name == "text"))
            .map(|default| fill(default, vars));
        let mut all = vars.to_vec();
        if let Some(text) = &text {
            all.push(("text", text));
        }
        reply(self.server, self.dest, &themed(key, defaults, &all)?)
    }
    fn award(&self, increments: Vec<(&str, u64)>) -> Result<(), Error> {
        let increments = increments
            .into_iter()
            .filter(|(_, amount)| *amount > 0)
            .map(|(stat, amount)| StatIncrement {
                stat: stat.into(),
                amount,
            })
            .collect::<Vec<_>>();
        if self.user_id.is_empty() || increments.is_empty() {
            return Ok(());
        }
        unsafe {
            award_stats(serde_json::to_string(&AwardStatsRequest {
                server: self.server.into(),
                profile_id: self.user_id.into(),
                display_name: self.addr.into(),
                target: self.dest.into(),
                increments,
                deduplication_id: None,
            })?)?;
        }
        Ok(())
    }
}

fn migrate_identity(state: &mut State, server: &str, nick: &str, user_id: &str) -> bool {
    let prefix = format!("{server}/");
    let folded_nick = fold_nick(server, nick);
    let legacy_match = |key: &str| {
        key.strip_prefix(&prefix)
            .is_some_and(|identity| fold_nick(server, identity) == folded_nick)
    };
    let old = state
        .players
        .keys()
        .chain(state.active_casts.keys())
        .find(|key| legacy_match(key))
        .cloned()
        .unwrap_or_else(|| format!("{server}/{folded_nick}"));
    let new = format!("{server}/{user_id}");
    if old == new {
        return false;
    }
    let mut changed = false;
    if !state.players.contains_key(&new) {
        if let Some(player) = state.players.remove(&old) {
            state.players.insert(new.clone(), player);
            changed = true;
        }
    }
    if !state.active_casts.contains_key(&new) {
        if let Some(cast) = state.active_casts.remove(&old) {
            state.active_casts.insert(new.clone(), cast);
            changed = true;
        }
    }
    if !state.stash.contains_key(&new) {
        if let Some(stash) = state.stash.remove(&old) {
            state.stash.insert(new.clone(), stash);
            changed = true;
        }
    }
    if !state.prestige.contains_key(&new) {
        if let Some(stars) = state.prestige.remove(&old) {
            state.prestige.insert(new.clone(), stars);
            changed = true;
        }
    }
    for champions in state.champions.values_mut() {
        for winner in [
            &mut champions.traveler,
            &mut champions.caster,
            &mut champions.collector,
        ] {
            if winner.as_deref() == Some(old.as_str()) {
                *winner = Some(new.clone());
                changed = true;
            }
        }
    }
    changed
}

// ── commands: core loop ─────────────────────────────────────────────────────

/// Fold raw XP into levels. There is no cap: `xp_for_level` keeps growing, so each level takes
/// longer than the last while catch XP stays level-independent — progress slows, never stops.
fn check_level_up(player: &mut Player) -> Option<i64> {
    let start = player.level;
    let mut level = player.level;
    let mut xp = player.xp;
    while xp >= xp_for_level(level) {
        xp -= xp_for_level(level);
        level += 1;
    }
    player.xp = xp;
    if level > start {
        player.level = level;
        Some(level)
    } else {
        None
    }
}

/// XP that lifts a player from `(level, xp)` exactly `count` levels up, ignoring bonuses. Used
/// by the wormhole quest reward and the dynamite haul so their payouts stay level-relative as
/// the curve grows.
fn xp_for_next_levels(level: i64, xp: i64, count: i64) -> i64 {
    let (mut tl, mut tx, mut grant, mut gained) = (level, xp, 0i64, 0i64);
    while gained < count {
        grant += (xp_for_level(tl) - tx).max(0);
        tx = 0;
        tl += 1;
        gained += 1;
    }
    grant
}

/// Shared LEVEL UP narration for catches and quest rewards: which location opened, the rod
/// unlock hint at 15, and any catch-epithet milestone. Empty when no level was gained.
fn level_up_suffix(level_before: i64, new_level: Option<i64>, now: i64) -> Result<String, Error> {
    let Some(lvl) = new_level else {
        return Ok(String::new());
    };
    // Past the coloured tiers (nested voids active) every new level opens a named spot; before
    // the old cap, the level itself is the reward.
    let mut text =
        if location_for_level(lvl, now).name != location_for_level(level_before, now).name {
            format!(
                " LEVEL UP! You're now level {lvl} and can fish at {}!",
                location_for_level(lvl, now).name
            )
        } else {
            format!(" LEVEL UP! You're now level {lvl}.")
        };
    // Crossing into level 15 unlocks the reinforced rod. Announce it once so the player
    // discovers the feature naturally rather than having to guess !rod exists.
    if level_before < ROD_UNLOCK_LEVEL && lvl >= ROD_UNLOCK_LEVEL {
        text.push_str(&themed(
            "rod_unlocked",
            &[" You can now reinforce your fishing rod! Use !rod to inspect it and !fix [1-24h] to add strength — a stronger line lands bigger fish."],
            &[],
        )?);
    }
    // Epithet milestone: name the colour that just entered this angler's pool.
    let now_tiers = descriptors_unlocked(lvl);
    if now_tiers > descriptors_unlocked(level_before) {
        text.push_str(&themed(
            "descriptor_unlocked",
            &[" New catch epithet unlocked: {descriptor} — some of your fish will now wear it."],
            &[("descriptor", FISH_DESCRIPTORS[now_tiers - 1])],
        )?);
    }
    Ok(text)
}

fn junk_item(rng: &mut Rng, location_kind: &str) -> String {
    let d = data();
    let items = d
        .junk_items
        .get(location_kind)
        .or_else(|| d.junk_items.get("terrestrial"));
    items
        .and_then(|v| rng.choice(v))
        .cloned()
        .unwrap_or_else(|| "an old boot".into())
}

fn profile_for_nick(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.to_string(),
            nick: nick.to_string(),
        })?)?
    };
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_nick_keys_use_irc_default_casemapping() {
        assert_eq!(fold_nick("net", "Sailor[One]^"), "sailor{one}~");
    }

    #[test]
    fn legacy_player_state_defaults_new_features_to_disabled() {
        let player: Player = serde_json::from_str("{}").unwrap();
        assert!(!player.dlc_enabled);
        assert!(!player.danger.enabled);
        assert!(player.danger.missing_limbs().is_empty());
    }

    #[test]
    fn legacy_special_character_nick_migrates_to_stable_uuid() {
        let mut state = State::default();
        state.players.insert(
            "net/sailor[one]".into(),
            Player {
                nick: "Sailor[One]".into(),
                ..Player::default()
            },
        );
        assert!(migrate_identity(
            &mut state,
            "net",
            "sailor{one}",
            "stable-profile"
        ));
        assert!(state.players.contains_key("net/stable-profile"));
        assert!(!state.players.contains_key("net/sailor[one]"));
    }

    #[test]
    fn good_catches_pay_brass() {
        assert_eq!(catch_brass("common", false, 0, 100), 0);
        assert_eq!(catch_brass("rare", false, 0, 100), 5);
        assert_eq!(catch_brass("legendary", true, 1, 100), 30);
        assert_eq!(catch_brass("legendary", true, 1, 0), 0, "0% turns it off");
        assert_eq!(catch_brass("uncommon", false, 2, 200), 24);
    }

    #[test]
    fn fill_supplies_the_legacy_text_var() {
        assert_eq!(
            fill(
                "{user} reels in {fish}{extras}",
                &[("user", "ann"), ("fish", "a {carp}"), ("extras", "")]
            ),
            "ann reels in a {carp}"
        );
        assert_eq!(fill("{unknown} {", &[]), "{unknown} {");
    }

    #[test]
    fn xp_curve() {
        assert_eq!(xp_for_level(0), 100);
        assert!(xp_for_level(1) > xp_for_level(0));
        assert!(xp_for_level(8) > xp_for_level(4));
    }

    #[test]
    fn leveling_consumes_xp() {
        let mut p = Player {
            xp: 100,
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut p), Some(1));
        assert_eq!(p.level, 1);
        assert_eq!(p.xp, 0);
        // Not enough for the next level.
        assert_eq!(check_level_up(&mut p), None);
    }

    #[test]
    fn setting_values_clamp_into_range_and_fall_back_to_the_default() {
        let def = SettingDef {
            key: "test_knob",
            description: "",
            default: 60,
            min: 5,
            max: 300,
            duration: true,
        };
        assert_eq!(clamp_setting(Some("120"), &def), 120);
        assert_eq!(
            clamp_setting(Some("  120  "), &def),
            120,
            "whitespace trimmed"
        );
        assert_eq!(
            clamp_setting(Some("1"), &def),
            5,
            "below range clamps up to min"
        );
        assert_eq!(
            clamp_setting(Some("99999"), &def),
            300,
            "above range clamps down to max"
        );
        assert_eq!(clamp_setting(Some("-7"), &def), 5, "negative clamps to min");
        assert_eq!(
            clamp_setting(Some("lots"), &def),
            60,
            "unparseable falls back to the default, not to min"
        );
        assert_eq!(clamp_setting(Some(""), &def), 60, "empty falls back");
        assert_eq!(clamp_setting(None, &def), 60, "unset falls back");
    }

    #[test]
    fn every_setting_default_lies_within_its_own_range() {
        for def in SETTING_DEFS {
            assert!(def.min <= def.max, "{}: min exceeds max", def.key);
            assert!(
                (def.min..=def.max).contains(&def.default),
                "{}: default {} is outside {}..={}, so an operator who resets to the advertised \
                 default would get a different value than the module ships with",
                def.key,
                def.default,
                def.min,
                def.max
            );
        }
    }

    #[test]
    fn setting_keys_are_unique() {
        let mut keys: Vec<&str> = SETTING_DEFS.iter().map(|def| def.key).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "duplicate setting key in SETTING_DEFS");
    }

    #[test]
    fn manifest_advertises_exactly_the_ranges_the_module_clamps_to() {
        let manifest = settings_manifest();
        assert_eq!(manifest.settings.len(), SETTING_DEFS.len());
        for (spec, def) in manifest.settings.iter().zip(SETTING_DEFS) {
            assert_eq!(spec.key, def.key);
            let (min, max) = match spec.kind {
                SettingKind::DurationSeconds { min, max } | SettingKind::Integer { min, max } => {
                    (min, max)
                }
                _ => panic!("{}: unexpected setting kind", def.key),
            };
            assert_eq!(
                (min, max),
                (def.min, def.max),
                "{} advertises a range it does not clamp to",
                def.key
            );
            assert_eq!(spec.default, def.default.to_string(), "{}", def.key);
            // Every knob is read via `setting_def`, which panics on an unknown key.
            assert_eq!(setting_def(def.key).key, def.key);
        }
    }

    #[test]
    fn database_loads() {
        let d = data();
        assert_eq!(d.locations.len(), 20);
        assert_eq!(d.locations[0].name, "Puddle");
        assert!(d
            .fish_by_location
            .get("The Void")
            .map(|v| !v.is_empty())
            .unwrap_or(false));
        assert!(d
            .fish_by_location
            .get("Purple Void")
            .is_some_and(|fish| fish.iter().any(|fish| fish.name == "Purple Carp")));
        assert!(d
            .fish_by_location
            .get("Prismatic Void")
            .is_some_and(|fish| fish.iter().any(|fish| fish.name == "The Prismatic Kraken")));
        assert!(!d.cast_messages.is_empty());
    }

    #[test]
    fn void_expansion_activates_at_q3_reset() {
        assert!(!expansion_active(VOID_EXPANSION_START - 1));
        assert_eq!(max_level(VOID_EXPANSION_START - 1), LEGACY_MAX_LEVEL);
        assert!(expansion_active(VOID_EXPANSION_START));
        assert_eq!(max_level(VOID_EXPANSION_START), EXPANSION_MAX_LEVEL);

        let mut player = Player {
            level: LEGACY_MAX_LEVEL,
            xp: xp_for_level(LEGACY_MAX_LEVEL),
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut player), Some(10));
    }

    #[test]
    fn elapsed_time_is_precise_at_hour_boundary() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(3_599), "59m 59s");
        assert_eq!(format_elapsed(3_600), "1h 0m 0s");
        assert_eq!(format_elapsed(3_661), "1h 1m 1s");
        assert_eq!(format_elapsed(-1), "0s");
    }

    #[test]
    fn vampire_shark_window_is_exactly_hour_666() {
        assert!(!is_vampire_hour(666 * 3600 - 1));
        assert!(is_vampire_hour(666 * 3600));
        assert!(is_vampire_hour(667 * 3600 - 1));
        assert!(!is_vampire_hour(667 * 3600));

        let shark = vampire_shark(666 * 3600).expect("hour 666 should guarantee the secret fish");
        assert_eq!(shark.name, "Vampire Shark");
        assert_eq!(shark.min_weight, 666.0);
        assert_eq!(shark.max_weight, 666.0);
        assert_eq!(shark.rarity, "legendary");
    }

    #[test]
    fn dynamite_hands_regrow_after_a_week() {
        let now = 1_000_000_i64;
        let mut player = Player {
            dynamite_hands_lost: 1,
            dynamite_hands_regrow_at: Some(now + HAND_REGROW_SECS),
            ..Default::default()
        };
        assert!(!settle_dynamite_hands(&mut player, now));
        assert_eq!(player.dynamite_hands_lost, 1);
        assert!(settle_dynamite_hands(&mut player, now + HAND_REGROW_SECS));
        assert_eq!(player.dynamite_hands_lost, 0);
        assert!(player.dynamite_hands_regrow_at.is_none());
    }

    #[test]
    fn legacy_one_hand_injury_is_restored_without_a_deadline() {
        let mut player = Player {
            dynamite_hands_lost: 1,
            ..Default::default()
        };
        assert!(settle_dynamite_hands(&mut player, 1_000_000));
        assert_eq!(player.dynamite_hands_lost, 0);
    }

    #[test]
    fn legacy_dynamite_ban_is_a_recovery_deadline() {
        let now = 1_000_000_i64;
        let mut player = Player {
            dynamite_hands_lost: 2,
            dynamite_banned_until: Some(now + HAND_REGROW_SECS),
            ..Default::default()
        };
        assert_eq!(
            active_dynamite_ban(&mut player, now),
            Some(now + HAND_REGROW_SECS)
        );
        assert_eq!(
            player.dynamite_hands_regrow_at,
            Some(now + HAND_REGROW_SECS)
        );
        assert_eq!(
            active_dynamite_ban(&mut player, now + HAND_REGROW_SECS),
            None
        );
        assert_eq!(player.dynamite_hands_lost, 0);
    }

    #[test]
    fn mastery_thresholds_are_exact() {
        assert_eq!(mastery_for(4), None);
        assert_eq!(mastery_for(5), Some("Bronze"));
        assert_eq!(mastery_for(25), Some("Silver"));
        assert_eq!(mastery_for(100), Some("Gold"));
        assert_eq!(mastery_for(250), Some("Iridescent"));
    }

    #[test]
    fn legacy_counts_migrate_to_location_qualified_species() {
        let mut player = Player::default();
        player.catches.insert("Koi".into(), 12);
        assert!(migrate_species_careers(&mut player));
        assert!(!migrate_species_careers(&mut player));
        let career = &player.species_careers[&species_key("Puddle", "Koi")];
        assert_eq!(career.catches, 12);
        assert_eq!(career.best_weight, 0.0);
        assert_eq!(mastery_for(career.catches), Some("Bronze"));
    }

    #[test]
    fn records_use_landed_weight_but_trophies_use_natural_quality() {
        let fish = Fish {
            name: "Testfish".into(),
            min_weight: 1.0,
            max_weight: 10.0,
            rarity: "common".into(),
        };
        let mut player = Player {
            species_careers_migrated: true,
            ..Default::default()
        };
        let boosted = record_species_catch(&mut player, "Test Lake", &fish, 12.0, 8.0);
        assert!(boosted.new_record);
        assert!(
            !boosted.trophy,
            "a size boost must not fabricate trophy quality"
        );
        let trophy = record_species_catch(&mut player, "Test Lake", &fish, 9.7, 9.7);
        assert!(
            !trophy.new_record,
            "the landed-weight record remains 12 lbs"
        );
        assert!(trophy.trophy);
        let career = &player.species_careers[&species_key("Test Lake", "Testfish")];
        assert_eq!(career.best_weight, 12.0);
        assert_eq!(career.best_record_quality, 0.8);
        assert!((career.best_quality - 0.97).abs() < f64::EPSILON);
        assert_eq!(career.catches, 2);
    }

    #[test]
    fn break_chance_floor_is_half_of_natural_at_max_strength() {
        // At strength 0 the break chance is the natural value unchanged.
        assert!((effective_break_chance(0.8, 0) - 0.8).abs() < f64::EPSILON);
        // At max strength (50) it is floored at 50% of natural — never below half.
        assert!((effective_break_chance(0.8, 50) - 0.4).abs() < f64::EPSILON);
        // A modest natural risk is halved, not quartered.
        assert!((effective_break_chance(0.4, 50) - 0.2).abs() < f64::EPSILON);
    }

    #[test]
    fn oversized_fish_always_retain_a_landing_chance() {
        // A Prismatic Kraken can reach 28,000 lb before lure/chum boosts. The raw legacy formula
        // yields 4.22 (422%), which made it impossible to land even with a reinforced rod.
        let prismatic_kraken_raw = 0.02 + (28_000.0 / 1000.0) * 0.15;
        assert!(prismatic_kraken_raw > 1.0);
        assert_eq!(
            effective_break_chance(prismatic_kraken_raw, 0),
            MAX_NATURAL_BREAK_CHANCE
        );
        assert_eq!(
            effective_break_chance(prismatic_kraken_raw, ROD_MAX_STRENGTH),
            MAX_NATURAL_BREAK_CHANCE * ROD_BREAK_FLOOR
        );
        assert!(effective_break_chance(prismatic_kraken_raw, ROD_MAX_STRENGTH) < 1.0);

        // The same invariant covers size-lure/chum combinations and future heavier fish.
        assert!(effective_break_chance(f64::MAX, 0) < 1.0);
    }

    #[test]
    fn break_chance_scales_linearly_below_the_floor() {
        // 25 strength = 25% reduction when that stays above the floor.
        assert!((effective_break_chance(0.8, 25) - 0.6).abs() < f64::EPSILON);
        // A small natural risk hits the floor before strength maxes: 0.2 at 25 strength would be
        // 0.15 raw, but the floor is 0.1, so 0.15 > 0.1 and the raw value is kept.
        assert!((effective_break_chance(0.2, 25) - 0.15).abs() < f64::EPSILON);
        // At 50 strength a 0.2 natural floors to 0.1 (half), not 0.1 from the raw reduction.
        assert!((effective_break_chance(0.2, 50) - 0.1).abs() < f64::EPSILON);
    }

    fn test_settings() -> FishingSettings {
        FishingSettings {
            danger_confirm_seconds: CONFIRM_SECS,
            danger_recovery_seconds: RECOVERY_SECS,
            danger_minor_injury_seconds: MINOR_INJURY_SECS,
            danger_weapon_drop_percent: DANGER_WEAPON_DROP_PERCENT,
            dynamite_hand_regrow_seconds: HAND_REGROW_SECS,
            chum_active_seconds: CHUM_ACTIVE_SECS,
            chum_cooldown_seconds: CHUM_COOLDOWN_SECS,
            lure_xp_cost: LURE_XP_COST,
            chum_xp_cost: CHUM_XP_COST,
            limb_heal_xp_cost: LIMB_HEAL_XP_COST,
            rod_max_strength: ROD_MAX_STRENGTH,
            rod_strength_per_level: ROD_STRENGTH_PER_LEVEL,
            brass_percent: 100,
        }
    }

    #[test]
    fn rod_strength_is_level_derived_and_retires_fix_fields() {
        let now = 1_000_000_i64;
        let settings = test_settings();
        // Below the unlock level there is no passive strength.
        let low = Player {
            level: ROD_UNLOCK_LEVEL - 1,
            ..Default::default()
        };
        assert_eq!(current_rod_strength(&low, now, &settings), 0);
        // Each level past 15 adds rod_strength_per_level, capped at the max.
        let mid = Player {
            level: ROD_UNLOCK_LEVEL + 2,
            ..Default::default()
        };
        assert_eq!(current_rod_strength(&mid, now, &settings), 10);
        let high = Player {
            level: 100,
            ..Default::default()
        };
        assert_eq!(
            current_rod_strength(&high, now, &settings),
            ROD_MAX_STRENGTH
        );
        // A legacy !fix bonus still counts, and pending fix fields are retired by settle_rod.
        let mut legacy = Player {
            level: ROD_UNLOCK_LEVEL,
            rod_strength: 10,
            fixing_until: Some(now + 3600),
            fixing_hours: Some(5),
            ..Default::default()
        };
        assert_eq!(current_rod_strength(&legacy, now, &settings), 10);
        assert!(settle_rod(&mut legacy, now, ROD_MAX_STRENGTH));
        assert!(legacy.fixing_until.is_none() && legacy.fixing_hours.is_none());
        assert_eq!(legacy.rod_strength, 10, "retired fix hours are not granted");
    }

    #[test]
    fn rod_wear_accrues_on_big_fish_and_regenerates_over_time() {
        let now = 1_000_000_i64;
        let settings = test_settings();
        let mut player = Player {
            level: ROD_UNLOCK_LEVEL + 4, // passive cap 20
            ..Default::default()
        };
        // 9 big fish add no wear; the 10th does.
        for _ in 0..(ROD_DECAY_EVERY - 1) {
            assert!(!apply_rod_wear(
                &mut player,
                ROD_BIG_FISH_THRESHOLD + 1.0,
                now,
                &settings
            ));
        }
        assert!(apply_rod_wear(
            &mut player,
            ROD_BIG_FISH_THRESHOLD + 1.0,
            now,
            &settings
        ));
        assert_eq!(player.rod_wear, 1);
        assert_eq!(current_rod_strength(&player, now, &settings), 19);
        // One wear point regenerates per hour.
        assert_eq!(rod_wear_now(&player, now + ROD_WEAR_REGEN_SECS), 0);
        assert_eq!(
            current_rod_strength(&player, now + ROD_WEAR_REGEN_SECS, &settings),
            20
        );
        // Small fish never add wear.
        assert!(!apply_rod_wear(
            &mut player,
            ROD_BIG_FISH_THRESHOLD,
            now,
            &settings
        ));
        // settle_rod folds the regen in durably.
        let mut worn = Player {
            level: ROD_UNLOCK_LEVEL + 4,
            rod_wear: 3,
            rod_wear_at: now,
            ..Default::default()
        };
        assert!(settle_rod(
            &mut worn,
            now + 2 * ROD_WEAR_REGEN_SECS,
            ROD_MAX_STRENGTH
        ));
        assert_eq!(worn.rod_wear, 1);
    }

    #[test]
    fn achievement_backfill_reports_absolute_reliable_totals_idempotently() {
        let mut state = State::default();
        state.players.insert(
            "net/profile-1".into(),
            Player {
                level: 15,
                total_fish: 123,
                lines_broken: 2,
                rare_catches: vec![RareCatch {
                    name: "Rare Fish".into(),
                    weight: 1.0,
                    rarity: "rare".into(),
                    location: "Puddle".into(),
                    caught_at: 1,
                }],
                artifact: data().artifacts.first().cloned(),
                ..Default::default()
            },
        );
        let request = AchievementBackfillRequest {
            server: "net".into(),
            entries: vec![jeeves_abi::ModuleKvEntry {
                key: "data".into(),
                value: serde_json::to_string(&state).unwrap(),
            }],
            previous_version: 0,
            catalog_version: 1,
        };
        let run = || achievement_backfill_response(request.clone()).unwrap();
        let first = run();
        let second = run();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        let value = |stat: &str| {
            first
                .values
                .iter()
                .find(|value| value.stat == stat)
                .unwrap()
                .value
        };
        assert_eq!(value("level"), 15);
        assert_eq!(value("catches"), 123);
        assert_eq!(value("rare_catches"), 1);
        assert_eq!(value("line_breaks"), 2);
    }

    fn entry(key: &str, value: &str) -> jeeves_abi::ModuleKvEntry {
        jeeves_abi::ModuleKvEntry {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn players_live_apart_from_shared_state_and_legacy_saves_still_load() {
        let mut legacy = State::default();
        legacy.players.insert(
            "net/old".into(),
            Player {
                level: 3,
                ..Default::default()
            },
        );
        legacy.players.insert(
            "net/moved".into(),
            Player {
                level: 1,
                ..Default::default()
            },
        );
        legacy.nonce = 7;
        let moved = serde_json::to_string(&Player {
            level: 9,
            ..Default::default()
        })
        .unwrap();
        let (state, snapshot) = state_from_entries(&[
            entry("data", &serde_json::to_string(&legacy).unwrap()),
            entry("player:net/moved", &moved),
            entry("player:net/gone", ""),
        ])
        .unwrap();
        assert_eq!(state.nonce, 7);
        assert_eq!(state.players["net/old"].level, 3, "legacy players load");
        assert_eq!(state.players["net/moved"].level, 9, "own entries win");
        assert!(!state.players.contains_key("net/gone"));
        assert!(
            snapshot.data.is_none(),
            "a data entry still holding players must be rewritten"
        );
        let shared = shared_json(&state).unwrap();
        assert!(!shared.contains("players"), "{shared}");
        let (reloaded, snapshot) =
            state_from_entries(&[entry("data", &shared), entry("player:net/moved", &moved)])
                .unwrap();
        assert_eq!(reloaded.nonce, 7);
        assert_eq!(snapshot.data.as_deref(), Some(shared.as_str()));
    }

    #[test]
    fn erasure_deletes_player_entries_and_legacy_copies() {
        let mut legacy = State::default();
        legacy.players.insert("net/p1".into(), Player::default());
        legacy.players.insert("net/p2".into(), Player::default());
        let player = serde_json::to_string(&Player::default()).unwrap();
        let request = ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: jeeves_abi::DataSubject {
                server: "net".into(),
                profile_id: "p1".into(),
            },
            aliases: Vec::new(),
            entries: vec![
                entry("data", &serde_json::to_string(&legacy).unwrap()),
                entry("player:net/p1", &player),
                entry("player:net/p2", &player),
                entry("player:other/p1", &player),
            ],
        };
        let plan = data_delete_plan(&request).unwrap();
        let keys: Vec<_> = plan.mutations.iter().map(|m| m.key.as_str()).collect();
        assert_eq!(keys, ["player:net/p1", "data"]);
        let data: State =
            serde_json::from_str(plan.mutations[1].value.as_deref().unwrap()).unwrap();
        assert!(!data.players.contains_key("net/p1") && data.players.contains_key("net/p2"));
        let (_, snapshot) = state_from_entries(&request.entries).unwrap();
        assert_eq!(snapshot.players.len(), 3);
    }

    #[test]
    fn rare_catches_keep_a_bounded_list_and_a_full_count() {
        let catch = |caught_at| RareCatch {
            name: "Rare Fish".into(),
            weight: 1.0,
            rarity: "rare".into(),
            location: "Puddle".into(),
            caught_at,
        };
        let mut player = Player {
            rare_catches: (0..3).map(catch).collect(),
            ..Default::default()
        };
        assert_eq!(player.rare_total(), 3, "older saves count their list");
        for at in 3..80 {
            player.record_rare(catch(at));
        }
        assert_eq!(player.rare_total(), 80);
        assert_eq!(player.rare_catches.len(), model::MAX_RARE_CATCHES_KEPT);
        assert_eq!(player.rare_catches.last().unwrap().caught_at, 79);
    }

    #[test]
    fn epithets_unlock_one_every_ten_levels() {
        assert_eq!(descriptors_unlocked(0), 0);
        assert_eq!(descriptors_unlocked(EXPANSION_MAX_LEVEL), 0);
        assert_eq!(descriptors_unlocked(EXPANSION_MAX_LEVEL + 1), 1);
        assert_eq!(descriptors_unlocked(29), 1);
        assert_eq!(descriptors_unlocked(30), 2);
        // Past the list, no new epithets — just the levels themselves.
        assert_eq!(descriptors_unlocked(200), FISH_DESCRIPTORS.len());
    }

    #[test]
    fn levels_climb_past_the_old_cap() {
        // Exactly enough XP for 40 levels: check_level_up must not stop at the old cap.
        let mut p = Player {
            xp: (0..40).map(xp_for_level).sum::<i64>(),
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut p), Some(40));
        assert_eq!(p.level, 40);
        assert_eq!(p.xp, 0);
        // Leftover XP stays spendable instead of being swallowed by the level-up.
        let mut p = Player {
            xp: xp_for_level(0) + 10,
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut p), Some(1));
        assert_eq!(p.xp, 10);
    }

    #[test]
    fn stashed_universes_merge_into_prime() {
        // The retired expedition system: a fresh expedition save is active while two maxed
        // worlds (Prime and an earlier expedition) sit stashed. Merging must fold all lifetime
        // XP into the active save, re-derive its level past the old cap, clear the universe
        // markers, and empty the stash.
        let mut state = State::default();
        let cap = max_level(VOID_EXPANSION_START);
        let maxed = |index: i64| Player {
            universe_index: index,
            level: cap,
            xp: 500,
            total_fish: 288,
            starred: true,
            ..Default::default()
        };
        state.players.insert(
            "net/p".into(),
            Player {
                nick: "styx".into(),
                universe_index: 2,
                universe_name: "the Ashen Depths".into(),
                universe_theme: "Ashen".into(),
                ..Default::default()
            },
        );
        state.stash.insert("net/p".into(), vec![maxed(0), maxed(1)]);
        let expected = 2 * ((0..cap).map(xp_for_level).sum::<i64>() + 500);
        merge_stashed_universes(&mut state);
        let p = &state.players["net/p"];
        assert!(
            p.level > cap,
            "merged XP should carry the angler past the old cap"
        );
        assert_eq!(lifetime_xp(p), expected);
        assert_eq!(p.universe_index, 0);
        assert!(p.universe_name.is_empty() && p.universe_theme.is_empty());
        assert!(state.stash.is_empty());
        // Idempotent: once merged, a later load merges nothing and preserves the totals.
        merge_stashed_universes(&mut state);
        assert_eq!(lifetime_xp(&state.players["net/p"]), expected);
    }

    #[test]
    fn legacy_save_deserializes_as_prime_with_no_stars() {
        // A pre-expedition Player JSON (no universe/star fields) must load as Prime, unstarred.
        let legacy = r#"{"nick":"old","level":9,"xp":100,"total_fish":50}"#;
        let p: Player = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.universe_index, 0);
        assert!(!p.starred);
        assert_eq!(p.universe_theme, "");
    }

    // ── Phase 8: nested voids ────────────────────────────────────────────────

    #[test]
    fn nested_voids_gate_at_the_q1_2027_boundary() {
        assert!(!nested_active(NESTED_VOID_START - 1));
        assert!(nested_active(NESTED_VOID_START));
        // Before the gate the coloured tiers are still the cap; after it nothing is dormant.
        assert_eq!(max_level(VOID_EXPANSION_START), EXPANSION_MAX_LEVEL);
        assert_eq!(max_level(NESTED_VOID_START - 1), EXPANSION_MAX_LEVEL);
        assert_eq!(max_level(NESTED_VOID_START), i64::MAX);
    }

    #[test]
    fn nest_formula_maps_levels_to_world_order() {
        assert_eq!(nest_for_level(19), None);
        assert_eq!(nest_for_level(20), Some((1, 0)));
        assert_eq!(nest_for_level(29), Some((1, 9)));
        assert_eq!(nest_for_level(30), Some((2, 0)));
        assert_eq!(nest_for_level(40), Some((3, 0)));
        assert_eq!(nest_for_level(119), Some((10, 9)));
    }

    #[test]
    fn ordinals_are_words_then_numbered() {
        assert_eq!(ordinal(1), "First");
        assert_eq!(ordinal(20), "Twentieth");
        assert_eq!(ordinal(21), "21st");
        assert_eq!(ordinal(22), "22nd");
        assert_eq!(ordinal(23), "23rd");
        assert_eq!(ordinal(24), "24th");
        assert_eq!(ordinal(111), "111th");
        assert_eq!(ordinal(112), "112th");
        assert_eq!(ordinal(113), "113th");
        assert_eq!(ordinal(121), "121st");
        assert_eq!(parse_ordinal("first"), Some(1));
        assert_eq!(parse_ordinal("Twentieth"), Some(20));
        assert_eq!(parse_ordinal("21st"), Some(21));
        assert_eq!(parse_ordinal("fir"), Some(1), "unique word prefix");
        assert_eq!(parse_ordinal("t"), None, "ambiguous prefix rejected");
        assert_eq!(parse_ordinal("void"), None);
    }

    #[test]
    fn nest_names_and_levels_are_generated() {
        let first = nest_location(1, 0);
        assert_eq!(first.name, "Puddle in the First Void");
        assert_eq!(first.level, 20);
        assert_eq!(first.kind, "space");
        let wrap = nest_location(2, 9);
        assert_eq!(wrap.name, "The Void in the Second Void");
        assert_eq!(wrap.level, 39);
        // Distance scales with the nest multiplier: Puddle is 5m, nest 2 is 5 * 1.5.
        assert!((nest_location(2, 0).max_distance - 7.5).abs() < f64::EPSILON);
        // Names round-trip through the parser.
        assert_eq!(parse_nest_location(&first.name), Some((1, 0)));
        assert_eq!(parse_nest_location(&wrap.name), Some((2, 9)));
        assert_eq!(parse_nest_location("pond in the 3rd void"), Some((3, 1)));
        assert_eq!(parse_nest_location("lake in the first void"), Some((1, 2)));
        assert_eq!(parse_nest_location("puddle"), None);
        assert_eq!(parse_nest_location("the void"), None);
    }

    #[test]
    fn nest_fish_are_renamed_and_rescaled_base_fish() {
        let fish = nest_fish(1, 0);
        assert_eq!(fish.len(), data().fish_by_location["Puddle"].len());
        assert!(fish
            .iter()
            .all(|f| f.name.ends_with("of the Puddle in the First Void")));
        let fish2 = nest_fish(2, 0);
        let base = &data().fish_by_location["Puddle"][0];
        assert!((fish2[0].min_weight - base.min_weight * 1.5).abs() < f64::EPSILON);
        assert!(fish_table("Puddle in the First Void").len() == fish.len());
        assert!(fish_table("Pond").len() == data().fish_by_location["Pond"].len());
        assert!(fish_table("nowhere").is_empty());
    }

    #[test]
    fn location_for_level_uses_nests_only_after_the_gate() {
        let before = VOID_EXPANSION_START;
        let after = NESTED_VOID_START;
        assert_eq!(location_for_level(40, before).name, "Prismatic Void");
        assert_eq!(
            location_for_level(40, after).name,
            "Puddle in the Third Void"
        );
        assert_eq!(
            location_for_level(20, after).name,
            "Puddle in the First Void"
        );
        assert_eq!(
            location_for_level(29, after).name,
            "The Void in the First Void"
        );
        assert_eq!(location_for_level(0, after).name, "Puddle");
        assert_eq!(location_for_level(19, after).name, "Prismatic Void");
    }

    #[test]
    fn find_location_matches_exact_unique_prefix_and_nests() {
        assert_eq!(find_location("puddle").unwrap().name, "Puddle");
        assert_eq!(find_location("THE VOID").unwrap().name, "The Void");
        assert_eq!(
            find_location("prismatic void").unwrap().name,
            "Prismatic Void"
        );
        assert_eq!(find_location("puddle in the first void").unwrap().level, 20);
        assert_eq!(
            find_location("lake in the 12th void").unwrap().name,
            "Lake in the Twelfth Void"
        );
        assert_eq!(
            find_location("lake in the 25th void").unwrap().name,
            "Lake in the 25th Void"
        );
        // The old loose substring fallback is gone: "void" matches nothing by itself.
        assert!(find_location("void").is_none());
        assert!(find_location("carp").is_none());
    }

    #[test]
    fn available_locations_and_summaries_stay_bounded() {
        let after = NESTED_VOID_START;
        let avail = available_location_names(40, after);
        assert_eq!(
            avail.len(),
            20 + 21,
            "20 bundled + 21 nest spots up to level 40"
        );
        assert!(avail.contains(&"Puddle in the Third Void".to_string()));
        let before = available_location_names(40, VOID_EXPANSION_START);
        assert_eq!(before.len(), 20, "pre-gate: only the bundled locations");
        // Short lists are listed whole; long ones collapse to current + next + count.
        assert!(!summarize_locations(&before, 40, VOID_EXPANSION_START).contains("…"));
        let summary = summarize_locations(&avail, 40, after);
        assert!(summary.contains("Puddle in the Third Void"));
        assert!(summary.contains("…and 39 more"), "got: {summary}");
    }

    #[test]
    fn void_events_cover_nest_locations_explicitly() {
        let solar = &data().events["solar_flare"].locations.clone().unwrap();
        assert!(event_allows_location(solar, "The Void"));
        assert!(event_allows_location(solar, "Prismatic Void"));
        assert!(event_allows_location(solar, "Puddle in the First Void"));
        assert!(!event_allows_location(solar, "Puddle"));
        assert!(!event_allows_location(solar, "Moon") || solar.contains(&"Moon".to_string()));
        assert!(is_void_location("The Void"));
        assert!(is_void_location("Puddle in the Second Void"));
        assert!(!is_void_location("Moon"));
        assert!(!is_void_location("Puddle"));
    }

    #[test]
    fn location_ids_encode_statically_and_as_nests() {
        let prismatic = data()
            .locations
            .iter()
            .position(|l| l.name == "Prismatic Void")
            .unwrap();
        assert_eq!(location_id("Prismatic Void"), Some(prismatic as i64));
        let nest_base = data().locations.len() as i64;
        assert_eq!(location_id("Puddle in the First Void"), Some(nest_base));
        assert_eq!(
            location_id("The Void in the Second Void"),
            Some(nest_base + 10 + 9)
        );
        assert_eq!(location_id("the Wormhole"), None);
        // Legacy name lists migrate into ids, once.
        let mut player = Player {
            locations_fished: vec!["Pond".into(), "Prismatic Void".into(), "Pond".into()],
            ..Default::default()
        };
        assert!(migrate_locations_fished(&mut player));
        assert!(!migrate_locations_fished(&mut player));
        assert_eq!(player.location_ids.len(), 2);
        assert!(player.locations_fished.is_empty());
        record_location(&mut player, "Puddle in the First Void");
        record_location(&mut player, "Puddle in the First Void");
        assert_eq!(player.location_ids.len(), 3, "no duplicate ids");
    }

    #[test]
    fn species_maps_are_capped_keeping_top_catches() {
        let mut player = Player::default();
        for i in 0..(MAX_SPECIES_CAREERS as i64 + 50) {
            player.species_careers.insert(
                format!("loc\u{1f}Fish {i}"),
                SpeciesCareer {
                    name: format!("Fish {i}"),
                    location: "loc".into(),
                    catches: i,
                    ..Default::default()
                },
            );
            player.catches.insert(format!("Fish {i}"), i);
        }
        cap_species_maps(&mut player);
        assert_eq!(player.species_careers.len(), MAX_SPECIES_CAREERS);
        assert_eq!(player.catches.len(), MAX_SPECIES_CAREERS);
        assert!(player
            .species_careers
            .values()
            .any(|c| c.catches == MAX_SPECIES_CAREERS as i64 + 49));
        assert!(!player.catches.contains_key("Fish 0"));
    }

    #[test]
    fn nest_xp_multiplier_compounds_per_nest() {
        let n = &data().void_nests;
        assert!((n.xp_multiplier_per_nest.powi(0) - 1.0).abs() < f64::EPSILON);
        assert!((n.xp_multiplier_per_nest.powi(2) - 9.0).abs() < f64::EPSILON);
    }
}
