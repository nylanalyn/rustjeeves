//! Pirate Isles — a persistent, asynchronous pirate-king IRC game (see PLAN-PIRATE.md).
//!
//! There is one serverwide game per IRC network, playable from any enabled room (and from PM):
//! captains run an island, send crew on timed voyages, raid each other with zero-warning
//! ambushes, hold prisoners for ransom, pay (or forget to pay) daily wages, dodge the Royal
//! Navy, and sail to a new sea with new rules every season. Rooms where pirate commands are used
//! are remembered and receive the game's public announcements; a channel-scoped `blacklisted`
//! setting refuses the game with a note instead.
//!
//! State lives in one JSON blob in the module's namespaced kv store (`data`); the durable host
//! scheduler drives voyage returns, the daily rollover, navy events, and season ends. All game
//! math is pure functions over the [`model`] tree so the unit tests never touch host functions.
//!
//! Where things live:
//!
//! - `lib.rs` — the extism ABI exports, host-function wrappers, persistence, settings, scheduler
//!   plumbing, and identity migration.
//! - [`commands`] — channel command dispatch and handlers (`!crew`, `!pay`, `!raid`, ...).
//! - [`pm`] — the guided PM menu state machine and PM-only commands.
//! - [`voyage`] / [`combat`] — the voyage catalog/resolution and raid combat math.
//! - [`buildings`] / [`rollover`] / [`navy`] / [`season`] — island systems and timers.
//! - [`achievements`] / [`lifecycle`] — achievement manifest/backfill and data export/delete.

mod achievements;
mod buildings;
mod combat;
mod commands;
mod lifecycle;
mod model;
mod navy;
mod pm;
mod prisoners;
mod rollover;
mod season;
mod voyage;

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AwardStatsRequest, Category, CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet,
    Level, LogReq, Profile, ProfileKey, RandomBytesRequest, RandomBytesResponse, ScheduleCancel,
    ScheduleList, ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ThemeReq, COMMAND_MANIFEST_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use model::{Game, KnownRoom, State};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn theme(input: String) -> String;
    fn irc_casefold(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn log(input: String) -> String;
}

#[plugin_fn]
pub fn init(_: String) -> FnResult<()> {
    let _ = unsafe {
        log(serde_json::to_string(&LogReq {
            level: Level::Info,
            category: Category::Message,
            message: "pirate module loaded".into(),
        })?)
    };
    Ok(())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            command(
                "signon",
                "Claim an isle and join the game; sends you the basics by PM.",
                "!signon",
            ),
            command(
                "crew",
                "Show your island: gold, rum, crew, buildings, voyages, and debuffs.",
                "!crew",
            ),
            command("pay", "Pay your crew's daily wages in gold.", "!pay"),
            command("rum", "Pay your crew's daily wages in rum.", "!rum"),
            command(
                "here",
                "Show the state of the seas: season, top captains, departures, unpaid isles.",
                "!here",
            ),
            command(
                "raid",
                "Raid an isle: silently on a fresh scout report, or by public declaration.",
                "!raid <crew> (after a scout) | !raid <nick> <crew> (public, +Notoriety)",
            ),
            command(
                "sail",
                "Break your Navy blockade or send a timed sortie to weaken another captain's blockade.",
                "!sail <crew> (your blockade) | !sail <captain> <crew> (harass)",
            ),
            command(
                "captain",
                "Show a captain's career profile and Legends.",
                "!captain [nick]",
            ),
            command(
                "collect",
                "Collect and review the spoils and reports from your returned voyages.",
                "!collect",
            ),
            command(
                "park",
                "Park your ship while away; pauses loyalty penalties and active gameplay.",
                "!park",
            ),
            command(
                "unpark",
                "Resume your parked Pirate Isles captain.",
                "!unpark",
            ),
            command(
                "build",
                "Show the shipwright's prices, or buy the next level of a building.",
                "!build (prices) | !build <vault|cove|walls|shipyard|tavern>",
            ),
            command("menu", "Open the captain's menu (via PM).", "!menu"),
            command(
                "pirate",
                "Answer an active Pirate Isles private menu.",
                "!pirate <option> | !pirate crew <count>",
            ),
            command(
                "ransom",
                "PM only: offer held prisoners back to their captain for gold.",
                "!ransom <amount>",
            ),
            command(
                "pressgang",
                "PM only: try to press held prisoners into your crew.",
                "!pressgang",
            ),
            command(
                "maroon",
                "PM only: maroon all held prisoners for Notoriety.",
                "!maroon",
            ),
            command(
                "payransom",
                "PM only: pay a pending ransom to free your crew.",
                "!payransom",
            ),
            command(
                "abandon",
                "PM only: abandon your ransomed crew to the sharks.",
                "!abandon",
            ),
            command(
                "flag",
                "PM only: buy a false flag so your next voyage flies another captain's colors.",
                "!flag <nick>",
            ),
        ],
    })?)
}

// ── settings ────────────────────────────────────────────────────────────────

/// One operator-tunable knob: the single source of truth for both its manifest entry and its
/// runtime clamp (fishing pattern). Declare a knob here and manifest + clamp cannot disagree.
struct SettingDef {
    key: &'static str,
    description: &'static str,
    default: i64,
    min: i64,
    max: i64,
}

const SETTING_DEFS: &[SettingDef] = &[
    SettingDef {
        key: "starting_gold",
        description: "Gold a new captain starts with.",
        default: 200,
        min: 0,
        max: 100_000,
    },
    SettingDef {
        key: "starting_rum",
        description: "Rum a new captain starts with.",
        default: 20,
        min: 0,
        max: 10_000,
    },
    SettingDef {
        key: "starting_regular_crew",
        description: "Regular crew a new captain starts with.",
        default: 3,
        min: 0,
        max: 50,
    },
    SettingDef {
        key: "loyal_crew_count",
        description: "Indestructible loyal crew per captain.",
        default: 2,
        min: 0,
        max: 10,
    },
    SettingDef {
        key: "crew_wage_gold",
        description: "Gold wage per crew per day.",
        default: 5,
        min: 0,
        max: 100,
    },
    SettingDef {
        key: "crew_wage_rum",
        description: "Rum wage per crew per day.",
        default: 1,
        min: 0,
        max: 100,
    },
    SettingDef {
        key: "crew_soft_cap",
        description: "Regular crew beyond this cost double wages.",
        default: 12,
        min: 1,
        max: 100,
    },
    SettingDef {
        key: "max_active_voyages",
        description: "Voyages one captain may have at sea at once.",
        default: 2,
        min: 1,
        max: 10,
    },
    SettingDef {
        key: "season_length_days",
        description: "Days per season before the fleet sails on.",
        default: 14,
        min: 1,
        max: 90,
    },
    SettingDef {
        key: "new_player_shield_hours",
        description: "Raid immunity for new captains.",
        default: 48,
        min: 0,
        max: 336,
    },
    SettingDef {
        key: "navy_interval_days_min",
        description: "Minimum days between Royal Navy sightings.",
        default: 3,
        min: 1,
        max: 30,
    },
    SettingDef {
        key: "navy_interval_days_max",
        description: "Maximum days between Royal Navy sightings.",
        default: 4,
        min: 1,
        max: 60,
    },
    SettingDef {
        key: "navy_strength_min",
        description: "Minimum hidden strength of a new Navy blockade.",
        default: 4,
        min: 1,
        max: 100,
    },
    SettingDef {
        key: "navy_strength_max",
        description: "Maximum baseline strength of a new Navy blockade.",
        default: 12,
        min: 1,
        max: 200,
    },
    SettingDef {
        key: "navy_escalation_strength",
        description: "Hidden strength added after a captain repulses the Navy.",
        default: 2,
        min: 1,
        max: 50,
    },
    SettingDef {
        key: "navy_harass_hours",
        description: "Hours an ally's Navy harassment sortie takes.",
        default: 1,
        min: 1,
        max: 24,
    },
    SettingDef {
        key: "navy_failure_loss_pct",
        description: "Percent of a failed Navy assault's sent regular crew and stores lost.",
        default: 10,
        min: 1,
        max: 50,
    },
    SettingDef {
        key: "rollover_hour_utc",
        description: "UTC hour of the daily payday rollover.",
        default: 0,
        min: 0,
        max: 23,
    },
    SettingDef {
        key: "voyage_options_count",
        description: "Voyage options the PM menu presents.",
        default: 3,
        min: 1,
        max: 6,
    },
    SettingDef {
        key: "raid_gold_pct_victory",
        description: "Percent of vulnerable gold stolen on a Victory.",
        default: 15,
        min: 1,
        max: 100,
    },
    SettingDef {
        key: "raid_gold_pct_crushing",
        description: "Percent of vulnerable gold stolen on a Crushing Victory.",
        default: 25,
        min: 1,
        max: 100,
    },
    SettingDef {
        key: "crew_loss_pct_defeat",
        description: "Percent of sent regular crew lost on a Defeat.",
        default: 50,
        min: 1,
        max: 100,
    },
    SettingDef {
        key: "notoriety_public_raid",
        description: "Notoriety gained for a public raid declaration.",
        default: 2,
        min: 0,
        max: 100,
    },
    SettingDef {
        key: "notoriety_maroon",
        description: "Notoriety gained per marooned prisoner.",
        default: 3,
        min: 0,
        max: 100,
    },
    SettingDef {
        key: "false_flag_cost",
        description: "Gold cost of a false flag.",
        default: 150,
        min: 0,
        max: 100_000,
    },
    SettingDef {
        key: "false_flag_cooldown_hours",
        description: "Hours between false-flag purchases.",
        default: 24,
        min: 1,
        max: 168,
    },
    SettingDef {
        key: "loyal_cove_cooldown_hours",
        description: "Hours loyal crew hide in the cove after a lost defense.",
        default: 6,
        min: 1,
        max: 72,
    },
    SettingDef {
        key: "humiliated_debuff_hours",
        description: "Hours the Humiliated debuff lasts.",
        default: 24,
        min: 1,
        max: 168,
    },
    SettingDef {
        key: "disloyal_scout_penalty_pct",
        description: "Defense penalty percent per unpaid day (capped at 25%).",
        default: 5,
        min: 0,
        max: 25,
    },
    SettingDef {
        key: "player_cap",
        description: "Maximum captains in the serverwide game.",
        default: 6,
        min: 1,
        max: 32,
    },
    SettingDef {
        key: "scout_intel_hours",
        description: "Hours a collected scout report stays fresh enough to raid on.",
        default: 12,
        min: 1,
        max: 168,
    },
    SettingDef {
        key: "raid_mercy_hours",
        description: "Hours a raided captain is out of the raid target pool. 0 disables.",
        default: 12,
        min: 0,
        max: 168,
    },
    SettingDef {
        key: "brothel_income_gold",
        description: "Gold earned per day per Brothel level.",
        default: 25,
        min: 0,
        max: 500,
    },
    SettingDef {
        key: "brothel_notoriety",
        description: "Notoriety gained per day per Brothel level (draws the Navy).",
        default: 1,
        min: 0,
        max: 10,
    },
];

fn setting_def(key: &str) -> &'static SettingDef {
    SETTING_DEFS
        .iter()
        .find(|def| def.key == key)
        .unwrap_or_else(|| panic!("unknown pirate setting key: {key}"))
}

/// A snapshot of every gameplay knob, read once per event so pure logic never calls host fns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PirateSettings {
    pub starting_gold: i64,
    pub starting_rum: i64,
    pub starting_regular_crew: i64,
    pub loyal_crew_count: i64,
    pub crew_wage_gold: i64,
    pub crew_wage_rum: i64,
    pub crew_soft_cap: i64,
    pub max_active_voyages: i64,
    pub season_length_days: i64,
    pub new_player_shield_hours: i64,
    pub navy_interval_days_min: i64,
    pub navy_interval_days_max: i64,
    pub navy_strength_min: i64,
    pub navy_strength_max: i64,
    pub navy_escalation_strength: i64,
    pub navy_harass_hours: i64,
    pub navy_failure_loss_pct: i64,
    pub rollover_hour_utc: i64,
    pub voyage_options_count: i64,
    pub raid_gold_pct_victory: i64,
    pub raid_gold_pct_crushing: i64,
    pub crew_loss_pct_defeat: i64,
    pub notoriety_public_raid: i64,
    pub notoriety_maroon: i64,
    pub false_flag_cost: i64,
    pub false_flag_cooldown_hours: i64,
    pub loyal_cove_cooldown_hours: i64,
    pub humiliated_debuff_hours: i64,
    pub disloyal_scout_penalty_pct: i64,
    pub player_cap: i64,
    pub scout_intel_hours: i64,
    pub raid_mercy_hours: i64,
    pub brothel_income_gold: i64,
    pub brothel_notoriety: i64,
}

impl PirateSettings {
    /// The advertised defaults, straight from [`SETTING_DEFS`]. Tests use this; production reads
    /// the same numbers through host settings with clamping.
    pub(crate) fn defaults() -> Self {
        let get = |key: &str| setting_def(key).default;
        Self {
            starting_gold: get("starting_gold"),
            starting_rum: get("starting_rum"),
            starting_regular_crew: get("starting_regular_crew"),
            loyal_crew_count: get("loyal_crew_count"),
            crew_wage_gold: get("crew_wage_gold"),
            crew_wage_rum: get("crew_wage_rum"),
            crew_soft_cap: get("crew_soft_cap"),
            max_active_voyages: get("max_active_voyages"),
            season_length_days: get("season_length_days"),
            new_player_shield_hours: get("new_player_shield_hours"),
            navy_interval_days_min: get("navy_interval_days_min"),
            navy_interval_days_max: get("navy_interval_days_max"),
            navy_strength_min: get("navy_strength_min"),
            navy_strength_max: get("navy_strength_max"),
            navy_escalation_strength: get("navy_escalation_strength"),
            navy_harass_hours: get("navy_harass_hours"),
            navy_failure_loss_pct: get("navy_failure_loss_pct"),
            rollover_hour_utc: get("rollover_hour_utc"),
            voyage_options_count: get("voyage_options_count"),
            raid_gold_pct_victory: get("raid_gold_pct_victory"),
            raid_gold_pct_crushing: get("raid_gold_pct_crushing"),
            crew_loss_pct_defeat: get("crew_loss_pct_defeat"),
            notoriety_public_raid: get("notoriety_public_raid"),
            notoriety_maroon: get("notoriety_maroon"),
            false_flag_cost: get("false_flag_cost"),
            false_flag_cooldown_hours: get("false_flag_cooldown_hours"),
            loyal_cove_cooldown_hours: get("loyal_cove_cooldown_hours"),
            humiliated_debuff_hours: get("humiliated_debuff_hours"),
            disloyal_scout_penalty_pct: get("disloyal_scout_penalty_pct"),
            player_cap: get("player_cap"),
            scout_intel_hours: get("scout_intel_hours"),
            raid_mercy_hours: get("raid_mercy_hours"),
            brothel_income_gold: get("brothel_income_gold"),
            brothel_notoriety: get("brothel_notoriety"),
        }
    }
}

impl Default for PirateSettings {
    fn default() -> Self {
        Self::defaults()
    }
}

/// Clamp a raw host setting value into the knob's range, falling back to its default when the
/// setting is unset or unparseable. Pure so it can be tested off-wasm.
fn clamp_setting(raw: Option<&str>, def: &SettingDef) -> i64 {
    raw.and_then(|value| value.trim().parse::<i64>().ok())
        .map(|value| value.clamp(def.min, def.max))
        .unwrap_or(def.default)
}

fn setting_raw(key: &str, server: &str, channel: Option<&str>) -> Option<String> {
    unsafe {
        setting_get(
            serde_json::to_string(&SettingGet {
                key: key.into(),
                server: Some(server.into()),
                channel: channel.map(str::to_string),
            })
            .unwrap_or_default(),
        )
    }
    .ok()
    .filter(|raw| !raw.is_empty())
}

fn setting_i64(key: &str, server: &str, channel: Option<&str>) -> i64 {
    clamp_setting(
        setting_raw(key, server, channel).as_deref(),
        setting_def(key),
    )
}

pub(crate) fn setting_enabled(server: &str, channel: &str) -> bool {
    setting_raw("enabled", server, Some(channel))
        .as_deref()
        .map(str::trim)
        == Some("true")
}

/// Channel-scope-only kill switch: a blacklisted room refuses the game with a note and never
/// receives announcements, no matter how `enabled` resolves.
pub(crate) fn is_blacklisted(server: &str, channel: &str) -> bool {
    setting_raw("blacklisted", server, Some(channel))
        .as_deref()
        .map(str::trim)
        == Some("true")
}

/// Whether the serverwide game is open for timed background work (voyage returns, navy, the
/// daily rollover): some remembered room still passes the enable gate and isn't blacklisted. A
/// game with no remembered rooms falls back to the network-scope enable so brand-new games tick.
pub(crate) fn game_open(server: &str, game: &Game) -> bool {
    if game.rooms.is_empty() {
        return setting_raw("enabled", server, None)
            .as_deref()
            .map(str::trim)
            == Some("true");
    }
    game.rooms
        .iter()
        .any(|room| !is_blacklisted(server, &room.name) && setting_enabled(server, &room.name))
}

pub(crate) fn pirate_settings(server: &str) -> PirateSettings {
    // Knobs resolve at network scope: one shared game must not behave differently per room.
    let get = |key: &str| setting_i64(key, server, None);
    PirateSettings {
        starting_gold: get("starting_gold"),
        starting_rum: get("starting_rum"),
        starting_regular_crew: get("starting_regular_crew"),
        loyal_crew_count: get("loyal_crew_count"),
        crew_wage_gold: get("crew_wage_gold"),
        crew_wage_rum: get("crew_wage_rum"),
        crew_soft_cap: get("crew_soft_cap"),
        max_active_voyages: get("max_active_voyages"),
        season_length_days: get("season_length_days"),
        new_player_shield_hours: get("new_player_shield_hours"),
        navy_interval_days_min: get("navy_interval_days_min"),
        navy_interval_days_max: get("navy_interval_days_max"),
        navy_strength_min: get("navy_strength_min"),
        navy_strength_max: get("navy_strength_max"),
        navy_escalation_strength: get("navy_escalation_strength"),
        navy_harass_hours: get("navy_harass_hours"),
        navy_failure_loss_pct: get("navy_failure_loss_pct"),
        rollover_hour_utc: get("rollover_hour_utc"),
        voyage_options_count: get("voyage_options_count"),
        raid_gold_pct_victory: get("raid_gold_pct_victory"),
        raid_gold_pct_crushing: get("raid_gold_pct_crushing"),
        crew_loss_pct_defeat: get("crew_loss_pct_defeat"),
        notoriety_public_raid: get("notoriety_public_raid"),
        notoriety_maroon: get("notoriety_maroon"),
        false_flag_cost: get("false_flag_cost"),
        false_flag_cooldown_hours: get("false_flag_cooldown_hours"),
        loyal_cove_cooldown_hours: get("loyal_cove_cooldown_hours"),
        humiliated_debuff_hours: get("humiliated_debuff_hours"),
        disloyal_scout_penalty_pct: get("disloyal_scout_penalty_pct"),
        player_cap: get("player_cap"),
        scout_intel_hours: get("scout_intel_hours"),
        raid_mercy_hours: get("raid_mercy_hours"),
        brothel_income_gold: get("brothel_income_gold"),
        brothel_notoriety: get("brothel_notoriety"),
    }
}

fn settings_manifest() -> SettingsManifest {
    let mut settings = vec![
        SettingSpec {
            key: "enabled".into(),
            description: "Whether the Pirate Isles accept commands in a channel (one serverwide \
                          game per network)."
                .into(),
            default: "false".into(),
            kind: SettingKind::Boolean,
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        },
        SettingSpec {
            key: "blacklisted".into(),
            description: "Blacklist the Pirate Isles in this channel: commands are refused with \
                          a note and announcements skip it. Channel scope only."
                .into(),
            default: "false".into(),
            kind: SettingKind::Boolean,
            scopes: vec![SettingScope::Channel],
            applies_immediately: true,
        },
    ];
    settings.extend(SETTING_DEFS.iter().map(|def| SettingSpec {
        key: def.key.into(),
        description: def.description.into(),
        default: def.default.to_string(),
        kind: SettingKind::Integer {
            min: def.min,
            max: def.max,
        },
        scopes: vec![
            SettingScope::Global,
            SettingScope::Network,
            SettingScope::Channel,
        ],
        applies_immediately: true,
    }));
    SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings,
    }
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&settings_manifest())?)
}

// ── host helpers ────────────────────────────────────────────────────────────

pub(crate) fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.into(),
        target: target.into(),
        text: text.into(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

pub(crate) fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

pub(crate) fn now_secs() -> i64 {
    unsafe { now(String::new()) }
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Award achievement stats to a stable profile id. Skipped silently when the id is empty (profile
/// resolution failed) or there is nothing to award. Callers invoke this only after the underlying
/// state change has been committed.
pub(crate) fn award_to(
    server: &str,
    profile_id: &str,
    display: &str,
    target: &str,
    increments: Vec<(&str, u64)>,
) -> Result<(), Error> {
    let increments = increments
        .into_iter()
        .filter(|(_, amount)| *amount > 0)
        .map(|(stat, amount)| StatIncrement {
            stat: stat.into(),
            amount,
        })
        .collect::<Vec<_>>();
    if profile_id.is_empty() || increments.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display.into(),
            target: target.into(),
            increments,
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn fold_nick(server: &str, nick: &str) -> String {
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
pub(crate) fn fold_nick(_server: &str, nick: &str) -> String {
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

pub(crate) fn load_state() -> Result<State, Error> {
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: "data".into() })?)? };
    let mut state = if raw.is_empty() {
        State::default()
    } else {
        // Persistent state must never be discarded just because one field is malformed. Returning
        // the parse error prevents a later event from saving an empty State over the original blob.
        serde_json::from_str(&raw)?
    };
    // One-time v1 -> v2 serverwide migration; also covers schema-version-only bumps so the
    // persisted blob always carries the current version after its first load.
    if let Some(legacy_pairs) = model::migrate_state(&mut state, now_secs()) {
        migrate_jobs(&legacy_pairs, &state);
        save_state(&state)?;
    }
    Ok(state)
}

/// Scheduler cleanup after a v1 -> v2 state migration: cancel the legacy recurring trio under
/// each folded channel's `pirate:v1:...` prefix and re-arm v2 voyage timers for unresolved
/// voyages so crew already at sea still come home (the `resolve_overdue` safety net covers
/// anything that slips). Best-effort: a scheduler hiccup must never fail state loading.
fn migrate_jobs(legacy_pairs: &[(String, String)], state: &State) {
    for (server, channel) in legacy_pairs {
        let prefix = legacy_job_prefix(server, channel);
        for kind in ["daily", "season_end", "navy_announce"] {
            let _ = cancel_schedule(&format!("{prefix}{kind}"));
        }
    }
    for (server, game) in &state.games {
        let Some(room) = game.rooms.first().map(|room| room.name.clone()) else {
            continue;
        };
        for voyage in &game.voyages {
            if voyage.resolved {
                continue;
            }
            let due = voyage.returns_at.max(now_secs() + 5);
            let _ = schedule(
                &voyage_job_id(server, voyage.id),
                server,
                &room,
                None,
                due,
                "",
            );
        }
    }
}

pub(crate) fn save_state(state: &State) -> Result<(), Error> {
    let req = KvSet {
        key: "data".into(),
        value: serde_json::to_string(state)?,
    };
    unsafe { kv_set(serde_json::to_string(&req)?)? };
    Ok(())
}

pub(crate) fn profile_for_nick(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
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

/// Resolve a nick argument to a captain's UUID within one game: the live profile service first,
/// then the game's nick cache (covers players whose profile lookup fails).
pub(crate) fn resolve_uuid(game: &Game, server: &str, arg: &str) -> Result<Option<String>, Error> {
    if let Some(profile) = profile_for_nick(server, arg)? {
        if !profile.id.is_empty() && game.players.contains_key(&profile.id) {
            return Ok(Some(profile.id));
        }
    }
    let folded = fold_nick(server, arg);
    Ok(game
        .players
        .iter()
        .find(|(_, player)| fold_nick(server, &player.nick_cache) == folded)
        .map(|(uuid, _)| uuid.clone()))
}

// ── small deterministic generator, seeded from host-provided OS randomness ───

pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        // Mix deterministic test seeds and host bytes before the xorshift step. Small raw seeds
        // otherwise remain clustered near zero and collapse probability tables.
        let mut value = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
        value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        Rng((value ^ (value >> 31)) | 1)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Uniform float in [0, 1).
    pub(crate) fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub(crate) fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.f64()
    }
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    /// Inclusive integer range.
    pub(crate) fn between(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            lo
        } else {
            lo + self.below((hi - lo + 1) as usize) as i64
        }
    }
    pub(crate) fn chance(&mut self, p: f64) -> bool {
        self.f64() < p
    }
    pub(crate) fn choice<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() {
            None
        } else {
            Some(&items[self.below(items.len())])
        }
    }
}

pub(crate) fn rng() -> Result<Rng, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let bytes = serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes;
    let seed = u64::from_le_bytes(
        bytes
            .try_into()
            .map_err(|_| Error::msg("random_bytes returned the wrong byte count"))?,
    );
    Ok(Rng::new(seed))
}

// ── scheduler plumbing ──────────────────────────────────────────────────────

/// Stable job-id prefix: `pirate:v2:{server}:<kind>[:<id>]`. Serverwide jobs carry a room name
/// only as metadata; announcements route through [`game.rooms`]. The `v2` bump keeps every
/// legacy per-channel `pirate:v1:...` job permanently unmatched by [`on_event`].
pub(crate) fn job_prefix(server: &str) -> String {
    format!("pirate:v2:{server}:")
}

/// Legacy per-channel job prefix (schema v1). Migration cancels the recurring trio under it.
pub(crate) fn legacy_job_prefix(server: &str, channel: &str) -> String {
    format!("pirate:v1:{server}:{channel}:")
}

pub(crate) fn daily_job_id(server: &str) -> String {
    format!("{}daily", job_prefix(server))
}
pub(crate) fn season_job_id(server: &str) -> String {
    format!("{}season_end", job_prefix(server))
}
pub(crate) fn navy_job_id(server: &str) -> String {
    format!("{}navy_announce", job_prefix(server))
}
pub(crate) fn navy_hit_job_id(server: &str) -> String {
    format!("{}navy_hit", job_prefix(server))
}
pub(crate) fn navy_harass_job_id(server: &str, sortie_id: u64) -> String {
    format!("{}navy_harass:{sortie_id}", job_prefix(server))
}
pub(crate) fn voyage_job_id(server: &str, voyage_id: u64) -> String {
    format!("{}voyage:{voyage_id}", job_prefix(server))
}
pub(crate) fn loyal_return_job_id(server: &str, uuid: &str) -> String {
    format!("{}loyal_return:{uuid}", job_prefix(server))
}

pub(crate) fn schedule(
    id: &str,
    server: &str,
    channel: &str,
    owner_profile_id: Option<String>,
    due_at: i64,
    payload: &str,
) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: id.into(),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id,
            due_at,
            payload: payload.into(),
        })?)?;
    }
    Ok(())
}

pub(crate) fn cancel_schedule(id: &str) -> Result<(), Error> {
    unsafe {
        schedule_cancel(serde_json::to_string(&ScheduleCancel { id: id.into() })?)?;
    }
    Ok(())
}

fn list_jobs(server: &str) -> Vec<ScheduledJob> {
    let raw = unsafe {
        schedule_list(
            serde_json::to_string(&ScheduleList {
                server: Some(server.into()),
                channel: None,
            })
            .unwrap_or_default(),
        )
    }
    .unwrap_or_default();
    let prefix = job_prefix(server);
    serde_json::from_str::<Vec<ScheduledJob>>(&raw)
        .unwrap_or_default()
        .into_iter()
        .filter(|job| job.id.starts_with(&prefix))
        .collect()
}

/// The next UTC instant whose hour is `hour` strictly after `now`.
pub(crate) fn next_rollover(now: i64, hour: i64) -> i64 {
    let day = now - now.rem_euclid(86_400);
    let mut at = day + hour.clamp(0, 23) * 3_600;
    if at <= now {
        at += 86_400;
    }
    at
}

/// Lazily create the serverwide scheduler jobs (daily rollover, season end, navy sightings) the
/// first time the game is played after load (or after state loss reset the flag). `room` is the
/// room the triggering command came from: the host scheduler requires a non-empty channel on
/// every job, but v2 jobs treat it as metadata only.
pub(crate) fn ensure_jobs(
    state: &mut State,
    server: &str,
    room: &str,
    settings: &PirateSettings,
    now: i64,
) -> Result<(), Error> {
    let game_key = server.to_string();
    let Some(game) = state.games.get(&game_key) else {
        return Ok(());
    };
    if game.jobs_ensured {
        return Ok(());
    }
    let jobs = list_jobs(server);
    let has = |id: &str| jobs.iter().any(|job| job.id == id);
    if !has(&daily_job_id(server)) {
        let due = next_rollover(now, settings.rollover_hour_utc);
        schedule(&daily_job_id(server), server, room, None, due, "")?;
    }
    if !has(&season_job_id(server)) {
        let mut due = game.season_started + settings.season_length_days * 86_400;
        if due <= now {
            due = now + 60;
        }
        schedule(&season_job_id(server), server, room, None, due, "")?;
    }
    if !has(&navy_job_id(server)) {
        let due = navy::next_navy_due(settings, now, &mut rng()?);
        schedule(&navy_job_id(server), server, room, None, due, "")?;
    }
    if let Some(game) = state.games.get_mut(&game_key) {
        game.jobs_ensured = true;
    }
    Ok(())
}

/// Broadcast one public line to every room the game is played in, skipping rooms that have since
/// been disabled or blacklisted. Room freshness is maintained by [`learn_room`], not here.
pub(crate) fn announce(
    server: &str,
    game: &Game,
    key: &str,
    defaults: &[&str],
    vars: &[(&str, &str)],
) -> Result<(), Error> {
    let text = themed(key, defaults, vars)?;
    for room in &game.rooms {
        if is_blacklisted(server, &room.name) || !setting_enabled(server, &room.name) {
            continue;
        }
        reply(server, &room.name, &text)?;
    }
    Ok(())
}

/// Record `channel` as a played room (and refresh its freshness), pruning stale rooms beyond
/// [`model::ROOM_STALE_SECS`] and capping the list so broadcasts cannot grow unbounded. Pure
/// state work; the caller saves.
pub(crate) fn learn_room(game: &mut Game, channel: &str, now: i64) {
    if let Some(room) = game.rooms.iter_mut().find(|room| room.name == channel) {
        room.last_seen = now;
    } else {
        game.rooms.push(KnownRoom {
            name: channel.to_string(),
            last_seen: now,
        });
    }
    game.rooms
        .retain(|room| now - room.last_seen <= model::ROOM_STALE_SECS);
    if game.rooms.len() > model::MAX_ROOMS {
        game.rooms.sort_by_key(|room| room.last_seen);
        let excess = game.rooms.len() - model::MAX_ROOMS;
        game.rooms.drain(0..excess);
    }
}

// ── entry points ────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    if msg.is_private {
        return Ok(pm::handle_pm(&server, &msg)?);
    }
    Ok(commands::handle_channel(&server, &msg)?)
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    // The timer's channel field is v1 legacy routing; serverwide jobs route through game.rooms.
    let Event::Timer { id, payload, .. } = env.event else {
        return Ok(());
    };
    let prefix = job_prefix(&server);
    let Some(kind) = id.strip_prefix(&prefix) else {
        return Ok(());
    };
    let game_key = server.clone();
    if let Some(rest) = kind.strip_prefix("voyage:") {
        if let Ok(voyage_id) = rest.parse::<u64>() {
            voyage::handle_voyage_timer(&server, &game_key, voyage_id)?;
        }
    } else if kind == "daily" {
        rollover::handle_daily(&server, &game_key)?;
    } else if kind == "season_end" {
        season::handle_season_end(&server, &game_key)?;
    } else if kind == "navy_announce" {
        navy::handle_navy_announce(&server, &game_key)?;
    } else if let Some(rest) = kind.strip_prefix("navy_harass:") {
        if let Ok(sortie_id) = rest.parse::<u64>() {
            navy::handle_harassment(&server, &game_key, sortie_id)?;
        }
    } else if kind.starts_with("navy_hit") {
        // Composite ids minted at raid resolution carry a `:{voyage_id}:{which}` suffix; the
        // hit handler only needs the payload.
        navy::handle_navy_hit(&server, &game_key, &payload)?;
    } else if let Some(uuid) = kind.strip_prefix("loyal_return:") {
        voyage::handle_loyal_return(&server, &game_key, uuid)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&achievements::manifest())?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: jeeves_abi::AchievementBackfillRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&achievements::backfill(request)?)?)
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: jeeves_abi::ModuleDataRequest = serde_json::from_str(&input)?;
    Ok(lifecycle::data_export(&request)?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: jeeves_abi::ModuleDataRequest = serde_json::from_str(&input)?;
    Ok(lifecycle::data_delete(&request)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_values_clamp_into_range_and_fall_back_to_the_default() {
        let def = SettingDef {
            key: "test_knob",
            description: "",
            default: 60,
            min: 5,
            max: 300,
        };
        assert_eq!(clamp_setting(Some("120"), &def), 120);
        assert_eq!(clamp_setting(Some("1"), &def), 5);
        assert_eq!(clamp_setting(Some("99999"), &def), 300);
        assert_eq!(clamp_setting(Some("lots"), &def), 60);
        assert_eq!(clamp_setting(None, &def), 60);
    }

    #[test]
    fn every_setting_default_lies_within_its_own_range() {
        for def in SETTING_DEFS {
            assert!(def.min <= def.max, "{}: min exceeds max", def.key);
            assert!(
                (def.min..=def.max).contains(&def.default),
                "{}: default {} outside {}..={}",
                def.key,
                def.default,
                def.min,
                def.max
            );
        }
    }

    #[test]
    fn setting_keys_are_unique_and_manifest_matches() {
        let mut keys: Vec<&str> = SETTING_DEFS.iter().map(|def| def.key).collect();
        let total = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), total, "duplicate setting key in SETTING_DEFS");

        let manifest = settings_manifest();
        assert_eq!(manifest.settings.len(), SETTING_DEFS.len() + 2);
        assert_eq!(manifest.settings[0].key, "enabled");
        assert_eq!(manifest.settings[0].default, "false");
        assert_eq!(manifest.settings[0].kind, SettingKind::Boolean);
        assert_eq!(manifest.settings[1].key, "blacklisted");
        assert_eq!(manifest.settings[1].default, "false");
        assert_eq!(manifest.settings[1].kind, SettingKind::Boolean);
        // The blacklist is deliberately channel-only: a global blacklist would brick the game.
        assert_eq!(manifest.settings[1].scopes, vec![SettingScope::Channel]);
        for (spec, def) in manifest.settings[2..].iter().zip(SETTING_DEFS) {
            assert_eq!(spec.key, def.key);
            assert_eq!(
                spec.kind,
                SettingKind::Integer {
                    min: def.min,
                    max: def.max
                },
                "{} advertises a range it does not clamp to",
                def.key
            );
            assert_eq!(spec.default, def.default.to_string());
        }
    }

    #[test]
    fn next_rollover_is_the_next_matching_utc_hour() {
        let day = 86_400 * 20_000;
        assert_eq!(next_rollover(day - 1, 0), day);
        assert_eq!(next_rollover(day, 0), day + 86_400);
        assert_eq!(next_rollover(day + 3_600, 6), day + 6 * 3_600);
        assert_eq!(next_rollover(day + 6 * 3_600, 6), day + 30 * 3_600);
    }

    #[test]
    fn old_state_blob_missing_new_fields_still_loads() {
        // A minimal legacy blob: unknown/missing fields must default, present fields survive.
        let state: State = serde_json::from_str(
            r#"{"games":{"net/#a":{"players":{"uuid-1":{"gold":500,"nick_cache":"Al"}}}}}"#,
        )
        .unwrap();
        let game = &state.games["net/#a"];
        assert_eq!(game.sea, "tortuga");
        assert!(game.voyages.is_empty());
        let player = &game.players["uuid-1"];
        assert_eq!(player.gold, 500);
        assert_eq!(player.loyalty_tier, 3);
        assert_eq!(player.buildings.cove, 1);
        assert_eq!(player.career_gold_plundered, 0);
        // Round-trip through JSON keeps the default state stable.
        let round: State =
            serde_json::from_str(&serde_json::to_string(&State::default()).unwrap()).unwrap();
        assert!(round.games.is_empty() && round.pm_sessions.is_empty() && round.next_id == 0);
        assert_eq!(round.schema_version, model::SCHEMA_VERSION);
    }

    #[test]
    fn v2_job_ids_are_serverwide_and_legacy_ids_never_match() {
        assert_eq!(job_prefix("net"), "pirate:v2:net:");
        assert_eq!(daily_job_id("net"), "pirate:v2:net:daily");
        assert_eq!(season_job_id("net"), "pirate:v2:net:season_end");
        assert_eq!(navy_job_id("net"), "pirate:v2:net:navy_announce");
        assert_eq!(navy_hit_job_id("net"), "pirate:v2:net:navy_hit");
        assert_eq!(voyage_job_id("net", 12), "pirate:v2:net:voyage:12");
        assert_eq!(navy_harass_job_id("net", 3), "pirate:v2:net:navy_harass:3");
        assert_eq!(
            loyal_return_job_id("net", "uuid"),
            "pirate:v2:net:loyal_return:uuid"
        );
        // Composite navy-hit ids minted at raid resolution parse under the shared prefix...
        let composite = format!("{}:9:a", navy_hit_job_id("net"));
        assert!(composite.strip_prefix(&job_prefix("net")).is_some());
        // ...while legacy per-channel v1 ids can never dispatch again.
        assert!("pirate:v1:net:#quest:daily"
            .strip_prefix(&job_prefix("net"))
            .is_none());
        assert_eq!(legacy_job_prefix("net", "#quest"), "pirate:v1:net:#quest:");
    }

    #[test]
    fn learn_room_refreshes_prunes_and_caps_broadcast_targets() {
        let mut game = Game::default();
        learn_room(&mut game, "#a", 1_000);
        learn_room(&mut game, "#b", 2_000);
        learn_room(&mut game, "#a", 3_000);
        assert_eq!(game.rooms.len(), 2, "no duplicates");
        assert_eq!(game.rooms[0].name, "#a");
        assert_eq!(
            game.rooms[0].last_seen, 3_000,
            "the known room was refreshed"
        );

        // Stale rooms age out of the broadcast list.
        learn_room(&mut game, "#c", model::ROOM_STALE_SECS + 3_000);
        assert_eq!(
            game.rooms
                .iter()
                .map(|room| room.name.as_str())
                .collect::<Vec<_>>(),
            vec!["#a", "#c"],
            "#b went stale and was dropped"
        );

        // The list is capped, dropping the least recently seen first.
        for i in 0..(model::MAX_ROOMS as i64 + 4) {
            learn_room(
                &mut game,
                &format!("#new{i}"),
                model::ROOM_STALE_SECS + 4_000 + i,
            );
        }
        assert_eq!(game.rooms.len(), model::MAX_ROOMS);
        assert!(
            !game.rooms.iter().any(|room| room.name == "#a"),
            "oldest rooms fall off first"
        );
    }
}
