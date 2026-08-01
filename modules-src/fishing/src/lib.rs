//! Fishing mini-game for rustjeeves — a port of jeeves/modules/fishing.py.
//!
//! The cast/reel loop over locations (Puddle -> The Void), with levelling, weighted catches by
//! wait time, junk, line breaks, XP and bonuses, random events, artifacts, lures, chum, seasonal
//! champions, parallel universes ("expeditions"), and the risk toys (`!dynamite`, danger mode).
//!
//! State lives in one JSON blob in the module's namespaced kv store (`data`). The fish database is
//! the real `fish_database.json`, bundled at compile time.
//!
//! Where things live:
//!
//! - `lib.rs` — the extism ABI exports, host-function wrappers, persistence, and the shared game
//!   mechanics the rest of the module builds on: RNG, levelling, seasons/champions, rod wear,
//!   identity migration.
//! - [`commands`] — the `dispatch` table and most command handlers. Start here to trace a command.
//! - [`cast`] / [`reel`] / [`danger`] — feature areas big enough to own a file, including their
//!   own commands.
//! - [`catalog`] — the static fish database and the pure roll tables over it.
//! - [`model`] — the persisted `State` tree.

mod cast;
mod catalog;
mod commands;
mod danger;
mod model;
mod reel;

use catalog::{
    calc_weight, round1, round2, select_fish, select_rarity, Artifact, EventDef, Fish, Location,
    VoidExpansion,
};
use commands::name_of;
use model::{
    ActiveEvent, Cast, CatchMilestones, Champions, Chum, Player, RareCatch, SeasonStats,
    SpeciesCareer, State,
};

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, RandomBytesRequest,
    RandomBytesResponse, Role, SendMessage, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
};
use std::collections::HashMap;
use std::sync::OnceLock;

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
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("no_longer_tiddling", "No Longer Tiddling", "level", 5),
        ("old_salt", "Old Salt", "level", 10),
        ("reinforced_resolve", "Reinforced Resolve", "level", 15),
        // The level cap is EXPANSION_MAX_LEVEL (19); a level-20 threshold was unreachable.
        (
            "compleat_angler",
            "The Compleat Angler",
            "level",
            EXPANSION_MAX_LEVEL as u64,
        ),
        ("something_biting", "Something’s Biting", "catches", 1),
        ("fine_kettle", "A Fine Kettle of Fish", "catches", 100),
        ("more_in_sea", "Plenty More in the Sea", "catches", 500),
        ("one_records", "One for the Records", "rare_catches", 1),
        ("aquarium", "It Belongs in an Aquarium", "artifacts", 1),
    ]
    .into_iter()
    .map(|(id, name, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: match stat {
            "level" => format!("Reach fishing level {threshold}."),
            "catches" => format!("Land {threshold} fish."),
            "rare_catches" => "Land a rare or legendary fish.".into(),
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
        // optional DANGER MODE catalog additions, then to 4 for the hour-666 secret.
        catalog_version: 4,
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
    let Some(entry) = request.entries.iter().find(|entry| entry.key == "data") else {
        return Ok(AchievementBackfillResponse::default());
    };
    let state: State = serde_json::from_str(&entry.value)?;
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
                ("rare_catches", player.rare_catches.len() as u64),
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
        "Show fishing stats and subcommands; universe lists worlds, jump <name|number> switches worlds (use prime to return to Prime), and expedition opens a new world at max level.",
    );
    fish.aliases = vec!["fishing".into(), "fishstats".into()];
    fish.usage =
        "!fish [nick | top | location | champions | help | universe | jump <world> | expedition]"
            .into();
    let mut mastery = command("mastery", "Show lifetime species mastery.");
    mastery.usage = "!mastery [nick]".into();
    let mut records = command("records", "Show personal specimen records.");
    records.usage = "!records [nick]".into();
    let mut rod = command("rod", "Inspect your fishing rod's strength (level 15+).");
    rod.usage = "!rod".into();
    let mut fix = command("fix", "Spend time strengthening your rod (level 15+).");
    fix.usage = "!fix [hours 1-24]".into();
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            cast,
            command("reel", "Reel in a fishing line."),
            command("fishinfo", "Look up a fish."),
            command("aquarium", "Show your aquarium."),
            mastery,
            records,
            rod,
            fix,
            command("lure", "Manage fishing lures."),
            command("chum", "Use fishing chum."),
            command("discard", "Discard an aquarium item."),
            command("dynamite", "Use dynamite while fishing."),
            command("hands", "Check your hands and dynamite recovery time."),
            command("danger", "Request enlistment in DANGER MODE."),
            command("yes", "Answer a pending DANGER MODE warning."),
            command("no", "Answer a pending DANGER MODE warning."),
            command("safety", "Leave DANGER MODE."),
            command("limbs", "Inspect your DANGER MODE limbs and equipment."),
            fish,
        ],
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.into(),
        target: target.into(),
        text: text.into(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
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

fn now_secs() -> i64 {
    unsafe { now(String::new()) }
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
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

fn load_state() -> Result<State, Error> {
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: "data".into() })?)? };
    if raw.is_empty() {
        Ok(State::default())
    } else {
        // Persistent state must never be discarded just because one field is malformed. Returning
        // the parse error prevents a later command from saving an empty State over the original
        // blob and makes migration/schema mistakes visible in the module logs.
        Ok(serde_json::from_str(&raw)?)
    }
}

fn save_state(state: &State) -> Result<(), Error> {
    let req = KvSet {
        key: "data".into(),
        value: serde_json::to_string(state)?,
    };
    unsafe { kv_set(serde_json::to_string(&req)?)? };
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
    let Some(entry) = request.entries.iter().find(|entry| entry.key == "data") else {
        return Ok(serde_json::to_string(&ModuleDataResponse {
            version: DATA_LIFECYCLE_VERSION,
            data: serde_json::Value::Null,
        })?);
    };
    let state: State = serde_json::from_str(&entry.value)?;
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
    let Some(entry) = request.entries.iter().find(|entry| entry.key == "data") else {
        return Ok(serde_json::to_string(&ModuleDataDeletePlan {
            version: DATA_LIFECYCLE_VERSION,
            mutations: Vec::new(),
        })?);
    };
    let mut state: State = serde_json::from_str(&entry.value)?;
    let keys = lifecycle_player_keys(&request);
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
        .is_some_and(|chum| lifecycle_chum_matches(chum, &request, &keys))
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
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations: if changed {
            vec![ModuleKvMutation {
                key: entry.key.clone(),
                value: Some(serde_json::to_string(&state)?),
            }]
        } else {
            Vec::new()
        },
    })?)
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
    CatchMilestones {
        previous_mastery,
        mastery,
        previous_record,
        new_record,
        trophy: quality >= 0.95,
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
const BAIT_XP_PER_HOUR: i64 = 100;
const MAX_BAIT_XP: i64 = 1_700;

// Reinforced rod: a permanent strength sink for level 15+ players that lowers line-break chance.
// Each point reduces break chance by 1%, floored at ROD_BREAK_FLOOR of the capped natural risk, so
// megafauna stay scary but every catch remains possible. Built with `!fix` (time), worn only by
// big fish.
const ROD_UNLOCK_LEVEL: i64 = 15;
const ROD_MAX_STRENGTH: u8 = 50;
const ROD_FIX_MAX_HOURS: i64 = 24;
const ROD_BIG_FISH_THRESHOLD: f64 = 2000.0;
const ROD_DECAY_EVERY: u8 = 10;
const HAND_REGROW_SECS: i64 = 7 * 86_400;
/// Even an unreinforced line retains this much landing chance. This also bounds future fish and
/// size multipliers instead of relying on one-off exceptions for today's heaviest species.
const MAX_NATURAL_BREAK_CHANCE: f64 = 0.95;
/// Break chance never drops below this fraction of its natural value (0.5 = 50% of natural risk).
const ROD_BREAK_FLOOR: f64 = 0.5;

fn expansion_active(at: i64) -> bool {
    at >= VOID_EXPANSION_START
}

fn max_level(at: i64) -> i64 {
    if expansion_active(at) {
        EXPANSION_MAX_LEVEL
    } else {
        LEGACY_MAX_LEVEL
    }
}

fn xp_for_level(level: i64) -> i64 {
    (100.0 * ((level + 1) as f64).powf(1.5)) as i64
}

fn location_for_level(level: i64) -> &'static Location {
    let d = data();
    d.locations
        .iter()
        .rev()
        .find(|l| l.level <= level)
        .unwrap_or(&d.locations[0])
}

// ── expeditions (parallel universes) ─────────────────────────────────────────

/// Soft cap on universes per person, to keep the save blob bounded.
const MAX_UNIVERSES: usize = 10;

/// Flavour for each expedition world: (display name, fish-name adjective). Indexed by
/// `universe_index - 1`; wraps with a numeric suffix past the list so it never runs out.
const EXPEDITION_WORLDS: &[(&str, &str)] = &[
    ("the Verdant Reach", "Verdant"),
    ("the Ashen Depths", "Ashen"),
    ("the Cerulean Expanse", "Cerulean"),
    ("the Crimson Shoals", "Crimson"),
    ("the Obsidian Trench", "Obsidian"),
    ("the Gilded Shallows", "Gilded"),
    ("the Frostbound Marches", "Frostbound"),
    ("the Umbral Sea", "Umbral"),
    ("the Radiant Atoll", "Radiant"),
    ("the Duskwater Fen", "Duskwater"),
];

/// Name and theme adjective for an expedition of the given 1-based index.
fn expedition_flavour(index: i64) -> (String, String) {
    let n = EXPEDITION_WORLDS.len() as i64;
    let (name, theme) = EXPEDITION_WORLDS[((index - 1).rem_euclid(n)) as usize];
    if index > n {
        // Past the curated list, disambiguate reused flavour with the loop number.
        let loop_no = (index - 1) / n + 1;
        (format!("{name} ({loop_no})"), theme.to_string())
    } else {
        (name.to_string(), theme.to_string())
    }
}

/// Human label for a universe. Prime is always "Prime"; expeditions use their stored name.
fn universe_label(p: &Player) -> String {
    if p.universe_index == 0 {
        "Prime".to_string()
    } else if p.universe_name.is_empty() {
        format!("Expedition {}", p.universe_index)
    } else {
        p.universe_name.clone()
    }
}

/// Reskin a fish name for a themed universe (cosmetic only). Prime returns the name unchanged.
fn themed_fish_name(theme: &str, name: &str) -> String {
    if theme.is_empty() {
        name.to_string()
    } else {
        format!("{theme} {name}")
    }
}

/// Does `arg` refer to this universe? Matches Prime, the index number, or the (folded) name.
fn universe_matches(server: &str, p: &Player, arg: &str) -> bool {
    let arg = arg.trim();
    if arg.is_empty() {
        return false;
    }
    if let Ok(n) = arg.parse::<i64>() {
        if n == p.universe_index {
            return true;
        }
    }
    let folded = fold_nick(server, arg);
    if p.universe_index == 0 && (folded == "prime" || folded == "0") {
        return true;
    }
    let label = fold_nick(server, &universe_label(p));
    label == folded || label.contains(&folded)
}

/// Star count for an identity.
fn star_count(state: &State, key: &str) -> i64 {
    state.prestige.get(key).copied().unwrap_or(0)
}

/// If the identity's active universe has reached the cap but not yet earned its Deep Star, award
/// it now. Returns true if a star was newly granted. Idempotent per universe.
fn claim_star_if_maxed(state: &mut State, key: &str, now: i64) -> bool {
    let newly = state
        .players
        .get(key)
        .is_some_and(|p| p.level >= max_level(now) && !p.starred);
    if newly {
        if let Some(p) = state.players.get_mut(key) {
            p.starred = true;
        }
        *state.prestige.entry(key.to_string()).or_insert(0) += 1;
    }
    newly
}

fn find_location(query: &str) -> Option<&'static Location> {
    let q = query.trim().to_lowercase();
    let d = data();
    d.locations
        .iter()
        .find(|l| l.name.to_lowercase() == q)
        .or_else(|| {
            d.locations
                .iter()
                .find(|l| l.name.to_lowercase().contains(&q))
        })
}

fn location_prep(loc: &Location) -> String {
    if loc.kind == "space" {
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
        || (location.ends_with(" Void")
            && locations.iter().any(|candidate| candidate == "The Void"))
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

/// The player's current rod strength, including any `!fix` whose time window has elapsed. Does
/// not mutate; callers that go on to write rod state should use [`settle_rod`] first.
fn current_rod_strength(player: &Player, now: i64) -> u8 {
    let mut strength = player.rod_strength;
    if player.fixing_until.is_some_and(|until| now >= until) {
        if let Some(hours) = player.fixing_hours {
            strength = strength.saturating_add(hours).min(ROD_MAX_STRENGTH);
        }
    }
    strength
}

/// Fold any completed `!fix` into `rod_strength` and clear the pending fix fields. Call this
/// before any mutation of `rod_strength` (decay, or starting a new fix) so committed time is never
/// lost and never double-counted.
fn settle_rod(player: &mut Player, now: i64) -> bool {
    if player.fixing_until.is_some_and(|until| now >= until) {
        if let Some(hours) = player.fixing_hours {
            player.rod_strength = player
                .rod_strength
                .saturating_add(hours)
                .min(ROD_MAX_STRENGTH);
        }
        player.fixing_until = None;
        player.fixing_hours = None;
        true
    } else {
        false
    }
}

/// Whether the player is currently locked out of `!cast` because a `!fix` is in progress.
fn rod_in_workshop(player: &Player, now: i64) -> bool {
    player.fixing_until.is_some_and(|until| now < until)
}

/// Apply wear from one landed catch. Returns true when a strength point was consumed.
fn apply_rod_wear(player: &mut Player, weight: f64) -> bool {
    if weight <= ROD_BIG_FISH_THRESHOLD || player.rod_strength == 0 {
        return false;
    }
    player.big_catch_counter = player.big_catch_counter.saturating_add(1);
    if player.big_catch_counter < ROD_DECAY_EVERY {
        return false;
    }
    player.rod_strength -= 1;
    player.big_catch_counter = 0;
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

/// 5% chance to start a random (location-valid) event on cast. Returns an announce string.
fn maybe_trigger_event(
    rng: &mut Rng,
    state: &mut State,
    server: &str,
    location: &str,
    now: i64,
) -> Option<String> {
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
    let announce = format!("** {} ** - {}", def.name, def.description);
    state.active_events.insert(server.to_string(), ev);
    Some(announce)
}

// ── dates: seasonal reset boundaries (no scheduler in wasm) ──────────────────

/// Convert unix seconds to a UTC `(year, month, day)` (Howard Hinnant's civil-from-days).
fn civil_from_unix(secs: i64) -> (i64, u32, u32) {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

/// Inverse: midnight UTC of `(year, month, day)` as unix seconds.
fn unix_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400
}

/// Midnight UTC of the next quarter boundary (Jan/Apr/Jul/Oct 1) strictly after `secs`.
fn next_quarter_start(secs: i64) -> i64 {
    let (y, _, _) = civil_from_unix(secs);
    for &qm in &[1u32, 4, 7, 10] {
        let ts = unix_from_civil(y, qm, 1);
        if ts > secs {
            return ts;
        }
    }
    unix_from_civil(y + 1, 1, 1)
}

/// The season label a reset at `secs` concludes (Apr 1 concludes Q1, Jan 1 concludes the prior Q4).
fn compute_reset_season(secs: i64) -> String {
    let (y, m, _) = civil_from_unix(secs);
    match m {
        1 => format!("Q4 {}", y - 1),
        4 => format!("Q1 {y}"),
        7 => format!("Q2 {y}"),
        10 => format!("Q3 {y}"),
        _ => format!("Q? {y}"),
    }
}

// ── champions ────────────────────────────────────────────────────────────────

fn legacy_season_stats(player: &Player) -> SeasonStats {
    // Before dedicated seasonal counters, every quarter wiped the lifetime fields. A restored old
    // save therefore contains one season's totals. Reconstruct earned XP from progression; XP
    // spent on consumables cannot be recovered, but this preserves the old Traveler ordering as
    // closely as the legacy schema permits.
    let level_xp = (0..player.level).map(xp_for_level).sum::<i64>();
    SeasonStats {
        xp_earned: level_xp.saturating_add(player.xp),
        fish_caught: player.total_fish,
        unique_species: player.catches.keys().cloned().collect(),
        rare_catches: player.rare_catches.len() as i64,
        heaviest_catch: player.biggest_fish,
        furthest_cast: player.furthest_cast,
    }
}

fn season_stats(player: &Player) -> SeasonStats {
    player
        .season_stats
        .clone()
        .unwrap_or_else(|| legacy_season_stats(player))
}

fn season_stats_mut(player: &mut Player) -> &mut SeasonStats {
    if player.season_stats.is_none() {
        player.season_stats = Some(legacy_season_stats(player));
    }
    player.season_stats.as_mut().unwrap()
}

/// Compute the three champions (player keys) from current-quarter counters. Ties are broken by
/// seasonal fish caught, then lifetime fish caught.
fn compute_champions(
    players: &[(&String, &Player)],
) -> (Option<String>, Option<String>, Option<String>) {
    let best = |score: &dyn Fn(&SeasonStats) -> f64,
                ok: &dyn Fn(&SeasonStats) -> bool|
     -> Option<String> {
        players
            .iter()
            .filter(|(_, p)| ok(&season_stats(p)))
            .max_by(|(_, a), (_, b)| {
                let sa = season_stats(a);
                let sb = season_stats(b);
                score(&sa)
                    .partial_cmp(&score(&sb))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(sa.fish_caught.cmp(&sb.fish_caught))
                    .then(a.total_fish.cmp(&b.total_fish))
            })
            .map(|(k, _)| (*k).clone())
    };
    (
        best(&|s| s.xp_earned as f64, &|s| s.xp_earned > 0),
        best(&|s| s.furthest_cast, &|s| s.furthest_cast > 0.0),
        best(&|s| s.rare_catches as f64, &|s| s.rare_catches > 0),
    )
}

/// Active champion bonus (0.20) for a player key: "xp" (Traveler), "distance" (Caster),
/// "rarity" (Collector). 0.0 if not a champion.
fn champion_bonus(state: &State, server: &str, key: &str, kind: &str) -> f64 {
    let Some(c) = state.champions.get(server) else {
        return 0.0;
    };
    let is = |w: &Option<String>| w.as_deref() == Some(key);
    let hit = match kind {
        "xp" => is(&c.traveler),
        "distance" => is(&c.caster),
        "rarity" => is(&c.collector),
        _ => false,
    };
    if hit {
        0.20
    } else {
        0.0
    }
}

/// Champion title suffix shown within fishing messages (e.g. "the Traveler the Collector").
fn champion_titles(state: &State, server: &str, key: &str) -> String {
    let Some(c) = state.champions.get(server) else {
        return String::new();
    };
    let is = |w: &Option<String>| w.as_deref() == Some(key);
    let mut parts = Vec::new();
    if is(&c.traveler) {
        parts.push("the Traveler");
    }
    if is(&c.caster) {
        parts.push("the Caster");
    }
    if is(&c.collector) {
        parts.push("the Collector");
    }
    parts.join(" ")
}

/// Lazy quarterly reset for `ctx.server`. First sight schedules the boundary without resetting; once
/// `now` passes a boundary, crowns champions, clears only seasonal counters, advances the
/// boundary, and returns `(announce_lines, state_changed)` (may fire for several elapsed
/// boundaries). `state_changed` is deliberately separate from the announcements: first sight of a
/// server only persists its initial boundary and has nothing to announce.
fn maybe_seasonal_reset(server: &str, state: &mut State, now: i64) -> (Vec<String>, bool) {
    let mut lines = Vec::new();
    let mut state_changed = false;
    if !matches!(state.next_reset.get(server), Some(&b) if b != 0) {
        let prefix = format!("{server}/");
        let has_existing_season = state.players.keys().any(|key| key.starts_with(&prefix));
        // The original scheduler failed to persist its initial boundary. Existing seasons that
        // encounter the fixed module after the Q3 expansion must still receive the missed July 1
        // reset; empty/new servers can safely begin at the next boundary.
        let boundary = if has_existing_season && now >= VOID_EXPANSION_START {
            VOID_EXPANSION_START
        } else {
            next_quarter_start(now)
        };
        state.next_reset.insert(server.to_string(), boundary);
        state_changed = true;
        if boundary > now {
            return (lines, state_changed);
        }
    }
    while let Some(&boundary) = state.next_reset.get(server) {
        if boundary == 0 || now < boundary {
            break;
        }
        let season = compute_reset_season(boundary);
        lines.extend(run_season_reset(state, server, &season));
        state
            .next_reset
            .insert(server.to_string(), next_quarter_start(boundary));
        state_changed = true;
    }
    (lines, state_changed)
}

fn run_season_reset(state: &mut State, server: &str, season: &str) -> Vec<String> {
    let prefix = format!("{server}/");
    let players: Vec<(&String, &Player)> = state
        .players
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .collect();
    let (traveler, caster, collector) = compute_champions(&players);
    drop(players);

    let mut champ = Champions {
        season: season.to_string(),
        ..Default::default()
    };
    champ.traveler_name = traveler
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    champ.caster_name = caster
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    champ.collector_name = collector
        .as_ref()
        .and_then(|k| state.players.get(k))
        .map(name_of)
        .unwrap_or_default();
    if let Some(p) = traveler.as_ref().and_then(|k| state.players.get(k)) {
        champ.traveler_xp = season_stats(p).xp_earned;
        champ.traveler_level = p.level;
        champ.traveler_location = location_for_level(p.level).name.clone();
    }
    if let Some(p) = caster.as_ref().and_then(|k| state.players.get(k)) {
        champ.caster_distance = season_stats(p).furthest_cast;
    }
    if let Some(p) = collector.as_ref().and_then(|k| state.players.get(k)) {
        champ.collector_count = season_stats(p).rare_catches;
    }

    let mut lines = vec![format!(
        "** NEW FISHING SEASON ** Career progress is safe! {season} champions:"
    )];
    if traveler.is_some() {
        lines.push(format!(
            "the Traveler: {} (earned {} XP) — carries a +20% XP blessing into the new season",
            champ.traveler_name, champ.traveler_xp
        ));
    } else {
        lines.push("the Traveler: unclaimed (no XP earned this season)".into());
    }
    if caster.is_some() {
        lines.push(format!(
            "the Caster: {} (cast {:.1}m) — carries a +20% distance blessing",
            champ.caster_name, champ.caster_distance
        ));
    } else {
        lines.push("the Caster: unclaimed (no casts recorded this season)".into());
    }
    if collector.is_some() {
        lines.push(format!(
            "the Collector: {} ({} rare/legendary catches) — carries a +20% rare blessing",
            champ.collector_name, champ.collector_count
        ));
    } else {
        lines.push("the Collector: unclaimed (no rare catches this season)".into());
    }
    lines.push("A new season begins; levels, catches, records, artifacts, XP, and active casts all carry forward.".into());

    champ.traveler = traveler;
    champ.caster = caster;
    champ.collector = collector;
    state.champions.insert(server.to_string(), champ);

    // Only competition counters reset. Career progress and in-flight gameplay are permanent.
    for (key, player) in &mut state.players {
        if key.starts_with(&prefix) {
            player.season_stats = Some(SeasonStats::default());
        }
    }
    lines
}

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
        let (lines, state_changed) = maybe_seasonal_reset(&server, &mut state, now_secs());
        if state_changed {
            save_state(&state)?;
        }
        if !lines.is_empty() {
            for l in &lines {
                ctx.say("season_announcement", &["{text}"], &[("text", l)])?;
            }
        }
    }

    commands::dispatch(&ctx, cmd, arg)?;
    Ok(())
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
    fn say(&self, key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<(), Error> {
        reply(self.server, self.dest, &themed(key, defaults, vars)?)
    }
    fn say_text(&self, key: &str, text: &str) -> Result<(), Error> {
        self.say(key, &["{text}"], &[("text", text)])
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

fn check_level_up(player: &mut Player, level_cap: i64) -> Option<i64> {
    let start = player.level;
    let mut level = player.level;
    let mut xp = player.xp;
    while level < level_cap && xp >= xp_for_level(level) {
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
        assert_eq!(check_level_up(&mut p, LEGACY_MAX_LEVEL), Some(1));
        assert_eq!(p.level, 1);
        assert_eq!(p.xp, 0);
        // Not enough for the next level.
        assert_eq!(check_level_up(&mut p, LEGACY_MAX_LEVEL), None);
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
        assert_eq!(check_level_up(&mut player, LEGACY_MAX_LEVEL), None);
        assert_eq!(player.level, LEGACY_MAX_LEVEL);
        assert_eq!(check_level_up(&mut player, EXPANSION_MAX_LEVEL), Some(10));
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
    fn civil_date_round_trip() {
        // 2026-06-26 00:00:00 UTC == 1782432000.
        assert_eq!(unix_from_civil(2026, 6, 26), 1_782_432_000);
        assert_eq!(civil_from_unix(1_782_432_000), (2026, 6, 26));
        // Epoch.
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        // A leap day survives the round trip.
        let ts = unix_from_civil(2024, 2, 29);
        assert_eq!(civil_from_unix(ts), (2024, 2, 29));
    }

    #[test]
    fn quarter_boundaries_and_seasons() {
        // From late June 2026, the next boundary is Jul 1; resetting then concludes Q2.
        let jun = unix_from_civil(2026, 6, 26);
        let next = next_quarter_start(jun);
        assert_eq!(civil_from_unix(next), (2026, 7, 1));
        assert_eq!(compute_reset_season(next), "Q2 2026");
        // Exactly on a boundary advances to the following quarter (strictly after).
        let jul = unix_from_civil(2026, 7, 1);
        assert_eq!(civil_from_unix(next_quarter_start(jul)), (2026, 10, 1));
        // Jan 1 concludes the prior year's Q4.
        let jan = unix_from_civil(2027, 1, 1);
        assert_eq!(compute_reset_season(jan), "Q4 2026");
    }

    #[test]
    fn champions_pick_leaders_with_tiebreak() {
        let a = Player {
            total_fish: 50,
            season_stats: Some(SeasonStats {
                xp_earned: 100,
                fish_caught: 5,
                furthest_cast: 10.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let mut b = Player {
            total_fish: 9,
            season_stats: Some(SeasonStats {
                xp_earned: 100,
                fish_caught: 9,
                rare_catches: 1,
                furthest_cast: 50.0,
                ..Default::default()
            }),
            ..Default::default()
        };
        b.rare_catches.push(RareCatch {
            name: "x".into(),
            weight: 1.0,
            rarity: "rare".into(),
            location: "Puddle".into(),
            caught_at: 0,
        });
        let (ka, kb) = ("s/a".to_string(), "s/b".to_string());
        let players = vec![(&ka, &a), (&kb, &b)];
        let (traveler, caster, collector) = compute_champions(&players);
        // Tie on seasonal XP → broken by seasonal fish caught → b.
        assert_eq!(traveler.as_deref(), Some("s/b"));
        assert_eq!(caster.as_deref(), Some("s/b"));
        assert_eq!(collector.as_deref(), Some("s/b"));
    }

    #[test]
    fn seasonal_reset_preserves_career_and_clears_only_season_stats() {
        let mut st = State::default();
        st.players.insert(
            "s/a".into(),
            Player {
                level: 3,
                furthest_cast: 20.0,
                total_fish: 4,
                season_stats: Some(SeasonStats {
                    xp_earned: 900,
                    fish_caught: 4,
                    furthest_cast: 20.0,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let jun = unix_from_civil(2026, 6, 26);
        // First sight: schedules the boundary, no reset, players intact.
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, jun);
        assert!(lines.is_empty());
        assert!(
            state_changed,
            "the initial reset boundary must be persisted"
        );
        assert!(st.players.contains_key("s/a"));
        assert_eq!(st.next_reset.get("s"), Some(&unix_from_civil(2026, 7, 1)));
        // Ordinary commands before the boundary neither reset nor rewrite the state.
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, jun + 1);
        assert!(lines.is_empty());
        assert!(!state_changed);
        // Jump past Jul 1: crown champions, preserve career progress, clear seasonal counters.
        let aug = unix_from_civil(2026, 8, 1);
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, aug);
        assert!(!lines.is_empty());
        assert!(state_changed);
        let player = st.players.get("s/a").unwrap();
        assert_eq!(player.level, 3);
        assert_eq!(player.total_fish, 4);
        assert_eq!(player.season_stats.as_ref().unwrap().fish_caught, 0);
        let champ = st.champions.get("s").unwrap();
        assert_eq!(champ.traveler.as_deref(), Some("s/a"));
        assert_eq!(champ.season, "Q2 2026");
        assert_eq!(champ.traveler_xp, 900);
        assert_eq!(champion_bonus(&st, "s", "s/a", "xp"), 0.20);
    }

    #[test]
    fn missing_schedule_catches_up_the_q3_reset_for_an_existing_season() {
        let mut st = State::default();
        st.players.insert(
            "s/a".into(),
            Player {
                level: 3,
                ..Default::default()
            },
        );

        let after_boundary = unix_from_civil(2026, 7, 1) + 1;
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, after_boundary);

        assert!(state_changed);
        assert!(!lines.is_empty());
        assert!(st.players.contains_key("s/a"));
        assert_eq!(
            st.players["s/a"].season_stats.as_ref().unwrap().xp_earned,
            0
        );
        assert_eq!(st.champions.get("s").unwrap().season, "Q2 2026");
        assert_eq!(st.next_reset.get("s"), Some(&unix_from_civil(2026, 10, 1)));
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

    #[test]
    fn rod_settle_grants_committed_fix_hours_capped() {
        // A completed !fix folds its hours into rod_strength.
        let now = 1_000_000_i64;
        let mut player = Player {
            rod_strength: 10,
            fixing_until: Some(now - 1), // already elapsed
            fixing_hours: Some(5),
            ..Default::default()
        };
        assert!(settle_rod(&mut player, now));
        assert_eq!(player.rod_strength, 15);
        assert!(player.fixing_until.is_none() && player.fixing_hours.is_none());

        // An incomplete fix is left untouched (granted on later read, not early).
        let mut p2 = Player {
            rod_strength: 10,
            fixing_until: Some(now + 3600), // 1h in the future
            fixing_hours: Some(5),
            ..Default::default()
        };
        assert!(!settle_rod(&mut p2, now));
        assert_eq!(
            p2.rod_strength, 10,
            "an unfinished fix must not grant early strength"
        );

        // Strength caps at ROD_MAX_STRENGTH even with a large committed fix.
        let mut p3 = Player {
            rod_strength: ROD_MAX_STRENGTH - 3,
            fixing_until: Some(now - 1),
            fixing_hours: Some(24),
            ..Default::default()
        };
        assert!(settle_rod(&mut p3, now));
        assert_eq!(p3.rod_strength, ROD_MAX_STRENGTH);
    }

    #[test]
    fn current_strength_reads_pending_fix_without_mutating() {
        let now = 1_000_000_i64;
        // Completed fix: effective strength includes the committed hours.
        let done = Player {
            rod_strength: 20,
            fixing_until: Some(now - 1),
            fixing_hours: Some(3),
            ..Default::default()
        };
        assert_eq!(current_rod_strength(&done, now), 23);
        // Fields are untouched (read-only).
        assert_eq!(done.rod_strength, 20);
        assert_eq!(done.fixing_hours, Some(3));
        // In-progress fix: only the banked strength counts.
        let pending = Player {
            rod_strength: 20,
            fixing_until: Some(now + 3600),
            fixing_hours: Some(3),
            ..Default::default()
        };
        assert_eq!(current_rod_strength(&pending, now), 20);
    }

    #[test]
    fn rod_wears_only_on_big_fish_every_tenth_catch() {
        let mut player = Player {
            level: ROD_UNLOCK_LEVEL,
            rod_strength: 10,
            ..Default::default()
        };
        // Exercise the production wear function: 9 big fish do not cost strength, the 10th does.
        for i in 0..(ROD_DECAY_EVERY - 1) {
            assert!(!apply_rod_wear(&mut player, ROD_BIG_FISH_THRESHOLD + 1.0));
            assert_eq!(
                player.rod_strength, 10,
                "no decay before the 10th big fish (i={i})"
            );
        }
        assert!(apply_rod_wear(&mut player, ROD_BIG_FISH_THRESHOLD + 1.0));
        assert_eq!(player.rod_strength, 9);
        assert!(!apply_rod_wear(&mut player, ROD_BIG_FISH_THRESHOLD));
        assert_eq!(player.big_catch_counter, 0, "small fish must not add wear");
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

    #[test]
    fn expedition_flavour_is_stable_and_wraps() {
        assert_eq!(expedition_flavour(1).0, "the Verdant Reach");
        assert_eq!(expedition_flavour(1).1, "Verdant");
        let n = EXPEDITION_WORLDS.len() as i64;
        // Past the curated list it reuses flavour but disambiguates with a loop number.
        assert_eq!(expedition_flavour(n + 1).1, "Verdant");
        assert!(expedition_flavour(n + 1).0.contains("(2)"));
    }

    #[test]
    fn universe_label_and_reskin() {
        let prime = Player::default();
        assert_eq!(universe_label(&prime), "Prime");
        assert_eq!(themed_fish_name(&prime.universe_theme, "Bass"), "Bass");
        let exp = Player {
            universe_index: 1,
            universe_name: "the Verdant Reach".into(),
            universe_theme: "Verdant".into(),
            ..Default::default()
        };
        assert_eq!(universe_label(&exp), "the Verdant Reach");
        assert_eq!(
            themed_fish_name(&exp.universe_theme, "Bass"),
            "Verdant Bass"
        );
    }

    #[test]
    fn universe_matches_by_prime_index_and_name() {
        let exp = Player {
            universe_index: 2,
            universe_name: "the Ashen Depths".into(),
            ..Default::default()
        };
        assert!(universe_matches("net", &exp, "2"));
        assert!(universe_matches("net", &exp, "ashen"));
        assert!(universe_matches("net", &exp, "the Ashen Depths"));
        assert!(!universe_matches("net", &exp, "prime"));
        let prime = Player::default();
        assert!(universe_matches("net", &prime, "prime"));
        assert!(universe_matches("net", &prime, "0"));
        assert!(!universe_matches("net", &prime, "ashen"));
    }

    #[test]
    fn deep_star_is_granted_once_per_maxed_world() {
        let mut state = State::default();
        let cap = max_level(VOID_EXPANSION_START);
        state.players.insert(
            "net/p".into(),
            Player {
                level: cap,
                ..Default::default()
            },
        );
        assert!(claim_star_if_maxed(
            &mut state,
            "net/p",
            VOID_EXPANSION_START
        ));
        assert_eq!(star_count(&state, "net/p"), 1);
        assert!(state.players["net/p"].starred);
        // Idempotent: a still-maxed, already-starred world grants nothing more.
        assert!(!claim_star_if_maxed(
            &mut state,
            "net/p",
            VOID_EXPANSION_START
        ));
        assert_eq!(star_count(&state, "net/p"), 1);
    }

    #[test]
    fn expedition_stashes_the_old_world_and_starts_fresh() {
        // A maxed Prime world; launching an expedition should freeze it and drop into a fresh L0.
        let mut state = State::default();
        let cap = max_level(VOID_EXPANSION_START);
        state.players.insert(
            "net/p".into(),
            Player {
                nick: "styx".into(),
                level: cap,
                total_fish: 288,
                starred: true,
                ..Default::default()
            },
        );
        // Simulate the core of cmd_expedition's stash-and-replace.
        let old = state.players.remove("net/p").unwrap();
        state.stash.entry("net/p".into()).or_default().push(old);
        let (name, theme) = expedition_flavour(1);
        state.players.insert(
            "net/p".into(),
            Player {
                nick: "styx".into(),
                universe_index: 1,
                universe_name: name,
                universe_theme: theme,
                ..Default::default()
            },
        );
        // Fresh active world ...
        assert_eq!(state.players["net/p"].level, 0);
        assert_eq!(state.players["net/p"].total_fish, 0);
        assert_eq!(state.players["net/p"].universe_index, 1);
        // ... and Prime is preserved untouched, ready to jump back to.
        let stash = &state.stash["net/p"];
        assert_eq!(stash.len(), 1);
        assert_eq!(stash[0].total_fish, 288);
        assert_eq!(stash[0].universe_index, 0);
        assert!(stash[0].starred);
    }

    #[test]
    fn legacy_save_deserializes_as_prime_with_no_stars() {
        // A pre-expedition Player JSON (no universe/star fields) must load as Prime, unstarred.
        let legacy = r#"{"nick":"old","level":9,"xp":100,"total_fish":50}"#;
        let p: Player = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.universe_index, 0);
        assert_eq!(universe_label(&p), "Prime");
        assert!(!p.starred);
        assert_eq!(p.universe_theme, "");
    }
}
