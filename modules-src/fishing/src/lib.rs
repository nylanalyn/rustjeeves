//! Fishing mini-game for rustjeeves — a port of jeeves/modules/fishing.py.
//!
//! Phase 1: the core cast/reel loop, locations (Puddle -> The Void), leveling, weighted catches
//! by wait time, junk, line breaks, XP + bonuses, and the read-only displays. Events, artifacts,
//! lures, chum, champions, and the risk toys land in later phases.
//!
//! State lives in one JSON blob in the module's namespaced kv store (`data`). The fish database is
//! the real `fish_database.json`, bundled at compile time.

use extism_pdk::*;
#[cfg(target_arch = "wasm32")]
use jeeves_abi::IrcCasefold;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey,
    RandomBytesRequest, RandomBytesResponse, Role, SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    cast.usage = "!cast [location] [bait <100-1700 XP>]".into();
    let mut fish = command("fish", "Show fishing stats and subcommands.");
    fish.aliases = vec!["fishing".into(), "fishstats".into()];
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

#[derive(Debug, Clone, Deserialize)]
struct Location {
    name: String,
    level: i64,
    max_distance: f64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Fish {
    name: String,
    min_weight: f64,
    max_weight: f64,
    rarity: String,
}

#[derive(Debug, Clone, Deserialize)]
struct VoidTier {
    name: String,
    color: String,
    level: i64,
    max_distance: f64,
    weight_multiplier: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct VoidExpansion {
    tiers: Vec<VoidTier>,
    fish: Vec<Fish>,
}

/// A fishing artifact: bundled in the DB, and also stored on a player once found.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Artifact {
    name: String,
    cast_text: String,
    float_text: String,
    bonus_type: String,
    bonus_value: f64,
}

#[derive(Debug, Clone, Deserialize)]
struct EventDef {
    name: String,
    description: String,
    #[serde(default)]
    effect: Option<String>,
    #[serde(default = "one")]
    multiplier: f64,
    duration_minutes: i64,
    #[serde(default)]
    locations: Option<Vec<String>>,
}

fn one() -> f64 {
    1.0
}

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

#[derive(Debug, Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    players: HashMap<String, Player>,
    #[serde(default)]
    active_casts: HashMap<String, Cast>,
    /// Active random event per server label.
    #[serde(default)]
    active_events: HashMap<String, ActiveEvent>,
    /// Chum state per server label.
    #[serde(default)]
    chum: HashMap<String, Chum>,
    /// Crowned champions per server label (set at each seasonal reset).
    #[serde(default)]
    champions: HashMap<String, Champions>,
    /// Next quarterly reset boundary (unix seconds) per server label. 0/missing means "not yet
    /// scheduled" — the first command for a server sets it without resetting.
    #[serde(default)]
    next_reset: HashMap<String, i64>,
    #[serde(default)]
    nonce: u64,
}

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
    let data = if players.is_empty() && active_casts.is_empty() && chum.is_none() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "players": players, "active_casts": active_casts, "chum": chum })
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

/// The three seasonal champions for a server, with a snapshot of their winning stats.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Champions {
    season: String,
    #[serde(default)]
    traveler: Option<String>,
    #[serde(default)]
    caster: Option<String>,
    #[serde(default)]
    collector: Option<String>,
    #[serde(default)]
    traveler_name: String,
    #[serde(default)]
    caster_name: String,
    #[serde(default)]
    collector_name: String,
    #[serde(default)]
    traveler_level: i64,
    #[serde(default)]
    traveler_location: String,
    #[serde(default)]
    traveler_xp: i64,
    #[serde(default)]
    caster_distance: f64,
    #[serde(default)]
    collector_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveEvent {
    name: String,
    description: String,
    effect: Option<String>,
    multiplier: f64,
    expires: i64,
    /// Which event definition this is (for the `locations` restriction).
    type_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Chum {
    expires: i64,
    cooldown_until: i64,
    #[serde(default)]
    by_id: String,
    by_name: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Player {
    #[serde(default)]
    nick: String,
    #[serde(default)]
    level: i64,
    #[serde(default)]
    xp: i64,
    #[serde(default)]
    total_fish: i64,
    #[serde(default)]
    biggest_fish: f64,
    #[serde(default)]
    biggest_fish_name: Option<String>,
    #[serde(default)]
    total_casts: i64,
    #[serde(default)]
    furthest_cast: f64,
    #[serde(default)]
    lines_broken: i64,
    #[serde(default)]
    junk_collected: i64,
    #[serde(default)]
    catches: HashMap<String, i64>,
    /// Location-qualified species careers. Legacy name-only catch counts are migrated lazily.
    #[serde(default)]
    species_careers: HashMap<String, SpeciesCareer>,
    #[serde(default)]
    species_careers_migrated: bool,
    #[serde(default)]
    rare_catches: Vec<RareCatch>,
    #[serde(default)]
    locations_fished: Vec<String>,
    #[serde(default)]
    xp_boost_catches: i64,
    #[serde(default)]
    artifact: Option<Artifact>,
    /// Rigged lure type ("rarity" or "size"), consumed on the next successful catch.
    #[serde(default)]
    active_lure: Option<String>,
    /// Set by `!fish bless`: forces the next catch to be rare/legendary.
    #[serde(default)]
    force_rare_legendary: bool,
    /// `!dynamite` damage: 0, 1, or 2 hands lost.
    #[serde(default)]
    dynamite_hands_lost: i64,
    /// `!dynamite` ban: unix seconds until fishing is allowed again.
    #[serde(default)]
    dynamite_banned_until: Option<i64>,
    /// Reinforced-rod strength (0–50). Unlocked at level 15 via `!fix`. Each point reduces break
    /// chance by 1%, floored at 50% of natural risk. Decays 1 per 10 big-fish (>2000 lb) catches.
    #[serde(default)]
    rod_strength: u8,
    /// Pending committed `!fix` hours not yet folded into `rod_strength`. Cleared by `settle_rod`.
    #[serde(default)]
    fixing_hours: Option<u8>,
    /// Unix seconds until an in-progress `!fix` completes. `None` = not fixing. While in the
    /// future, `!cast` is refused (the rod is in the workshop); once elapsed, the pending
    /// `fixing_hours` are granted on next read.
    #[serde(default)]
    fixing_until: Option<i64>,
    /// Counter of big-fish (>2000 lb) catches since last rod decay; resets at `ROD_DECAY_EVERY`.
    #[serde(default)]
    big_catch_counter: u8,
    /// Operator-granted cosmetic catch pack. It never changes fishing mechanics.
    #[serde(default)]
    dlc_enabled: bool,
    /// Current-quarter counters. `None` identifies a pre-seasonal-stats save and is migrated from
    /// the lifetime fields on first use, which keeps restored backups backward-compatible.
    #[serde(default)]
    season_stats: Option<SeasonStats>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SeasonStats {
    #[serde(default)]
    xp_earned: i64,
    #[serde(default)]
    fish_caught: i64,
    #[serde(default)]
    unique_species: HashSet<String>,
    #[serde(default)]
    rare_catches: i64,
    #[serde(default)]
    heaviest_catch: f64,
    #[serde(default)]
    furthest_cast: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cast {
    timestamp: i64,
    distance: f64,
    location: String,
    allow_lower_fish: bool,
    /// XP-funded virtual hours used for rarity gates only. Added in the Q3 2026 expansion.
    #[serde(default)]
    bait_hours: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RareCatch {
    name: String,
    weight: f64,
    rarity: String,
    location: String,
    caught_at: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct SpeciesCareer {
    name: String,
    location: String,
    catches: i64,
    /// Best landed weight, including lure and chum multipliers.
    best_weight: f64,
    /// Natural quality of the catch which set `best_weight`.
    best_record_quality: f64,
    /// Best natural specimen quality, measured before external size multipliers.
    best_quality: f64,
}

#[derive(Debug, Default, PartialEq)]
struct CatchMilestones {
    previous_mastery: Option<&'static str>,
    mastery: Option<&'static str>,
    previous_record: f64,
    new_record: bool,
    trophy: bool,
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
fn select_rarity(
    rng: &mut Rng,
    wait_hours: f64,
    event_rare_mult: f64,
    rarity_boost: f64,
) -> String {
    let mut weights: Vec<(String, i64)> = data().rarity_weights.clone();
    let set = |w: &mut Vec<(String, i64)>, name: &str, val: i64| {
        if let Some(e) = w.iter_mut().find(|(k, _)| k == name) {
            e.1 = val;
        }
    };
    let get = |w: &[(String, i64)], name: &str| {
        w.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| *v)
            .unwrap_or(0)
    };
    if wait_hours < 6.0 {
        set(&mut weights, "uncommon", 5);
        set(&mut weights, "rare", 0);
        set(&mut weights, "legendary", 0);
    } else if wait_hours < 12.0 {
        set(&mut weights, "rare", 2);
        set(&mut weights, "legendary", 0);
    } else if wait_hours < 18.0 {
        set(&mut weights, "legendary", 0);
    }
    if event_rare_mult > 1.0 {
        let r = (get(&weights, "rare") as f64 * event_rare_mult) as i64;
        let l = (get(&weights, "legendary") as f64 * event_rare_mult) as i64;
        set(&mut weights, "rare", r);
        set(&mut weights, "legendary", l);
    }
    if rarity_boost > 0.0 {
        let common = get(&weights, "common") as f64;
        let reduction = common * rarity_boost;
        let rare = get(&weights, "rare") + (reduction * 0.6) as i64;
        let legendary = get(&weights, "legendary") + (reduction * 0.4) as i64;
        set(&mut weights, "common", (common - reduction).max(1.0) as i64);
        set(&mut weights, "rare", rare);
        set(&mut weights, "legendary", legendary);
    }
    let total: i64 = weights.iter().map(|(_, w)| *w).sum();
    if total <= 0 {
        return "common".into();
    }
    let mut roll = (rng.below(total as usize) + 1) as i64;
    for (rarity, w) in &weights {
        roll -= w;
        if roll <= 0 {
            return rarity.clone();
        }
    }
    "common".into()
}

fn select_fish<'a>(
    rng: &mut Rng,
    location: &str,
    rarity: &str,
    eligible: &[String],
    allow_fallback: bool,
) -> Option<&'a Fish> {
    let d = data();
    let pool: Vec<&Fish> = if eligible.is_empty() {
        d.fish_by_location
            .get(location)
            .map(|v| v.iter().collect())
            .unwrap_or_default()
    } else {
        eligible
            .iter()
            .filter_map(|l| d.fish_by_location.get(l))
            .flat_map(|v| v.iter())
            .collect()
    };
    let matching: Vec<&Fish> = pool
        .iter()
        .copied()
        .filter(|f| f.rarity == rarity)
        .collect();
    if matching.is_empty() {
        if !allow_fallback {
            return None;
        }
        let commons: Vec<&Fish> = pool
            .iter()
            .copied()
            .filter(|f| f.rarity == "common")
            .collect();
        rng.choice(&commons).copied()
    } else {
        rng.choice(&matching).copied()
    }
}

fn calc_weight(rng: &mut Rng, fish: &Fish, wait_hours: f64) -> f64 {
    let (min_w, max_w) = (fish.min_weight, fish.max_weight);
    let time_factor = (wait_hours / OPTIMAL_WAIT_HOURS).min(1.0);
    let base = min_w + (max_w - min_w) * time_factor;
    let variance = (max_w - min_w) * 0.2;
    let w = base + rng.range(-variance, variance);
    round2(w.clamp(min_w, max_w))
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}
fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

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

/// `!dynamite` ban gate: returns the future expiry if banned; clears an expired ban (regrowing both
/// hands) and returns `None`.
fn active_dynamite_ban(player: &mut Player, now: i64) -> Option<i64> {
    match player.dynamite_banned_until {
        Some(exp) if now < exp => Some(exp),
        Some(_) => {
            player.dynamite_banned_until = None;
            player.dynamite_hands_lost = 0;
            None
        }
        None => None,
    }
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

    match cmd {
        "!cast" => cmd_cast(&ctx, arg)?,
        "!reel" => cmd_reel(&ctx)?,
        "!fishinfo" => cmd_fishinfo(&ctx, arg)?,
        "!aquarium" => cmd_aquarium(&ctx)?,
        "!mastery" => cmd_mastery(&ctx, arg)?,
        "!records" => cmd_records(&ctx, arg)?,
        "!rod" => cmd_rod(&ctx)?,
        "!fix" => cmd_fix(&ctx, arg)?,
        "!lure" => cmd_lure(&ctx)?,
        "!chum" => cmd_chum(&ctx)?,
        "!discard" => cmd_discard(&ctx)?,
        "!dynamite" => cmd_dynamite(&ctx)?,
        "!fish" | "!fishing" | "!fishstats" => {
            let sub = arg.split_whitespace().next().unwrap_or("");
            let rest = arg
                .split_once(char::is_whitespace)
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            match sub {
                "top" => cmd_top(&ctx)?,
                "location" => cmd_location(&ctx)?,
                "help" => cmd_help(&ctx)?,
                "champions" | "champion" => cmd_champions(&ctx)?,
                "bless" => cmd_bless(&ctx, rest)?,
                "dlc" => cmd_dlc(&ctx, rest)?,
                _ => cmd_stats(&ctx, arg)?,
            }
        }
        _ => {}
    }
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

#[derive(Debug, PartialEq)]
struct CastRequest {
    location: String,
    bait_xp: i64,
}

fn parse_cast_request(arg: &str) -> Result<CastRequest, &'static str> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let Some(bait_index) = words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("bait"))
    else {
        return Ok(CastRequest {
            location: arg.trim().to_string(),
            bait_xp: 0,
        });
    };
    if bait_index + 2 != words.len() {
        return Err("Use !cast [location] bait <XP>, for example !cast Purple Void bait 500.");
    }
    let bait_xp = words[bait_index + 1]
        .parse::<i64>()
        .map_err(|_| "Bait must be an XP amount from 100 to 1700, in steps of 100.")?;
    if !(BAIT_XP_PER_HOUR..=MAX_BAIT_XP).contains(&bait_xp) || bait_xp % BAIT_XP_PER_HOUR != 0 {
        return Err("Bait must be an XP amount from 100 to 1700, in steps of 100.");
    }
    Ok(CastRequest {
        location: words[..bait_index].join(" "),
        bait_xp,
    })
}

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

fn cmd_cast(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let now = now_secs();

    if let Some(cast) = state.active_casts.get(&key) {
        let elapsed = format_elapsed(now - cast.timestamp);
        ctx.say(
            "cast_already_active",
            &["{user}, you already have a line in the water at {location} ({elapsed}). Use !reel to bring it in."],
            &[
                ("user", ctx.addr),
                ("location", &cast.location),
                ("elapsed", &elapsed),
            ],
        )?;
        return Ok(());
    }

    let request = match parse_cast_request(arg) {
        Ok(request) => request,
        Err(message) => return ctx.say_text("cast_usage", message),
    };
    if request.bait_xp > 0 && !expansion_active(now) {
        return ctx.say(
            "bait_not_available",
            &["Bait becomes available when the new fishing season begins."],
            &[],
        );
    }

    let player = state.players.entry(key.clone()).or_default();
    player.nick = ctx.nick.to_string();
    // Snapshot a legacy save before this cast changes any lifetime counters.
    season_stats_mut(player);

    // A rod in the workshop blocks new casts. An elapsed fix window is settled (committed hours
    // folded into rod_strength) so casting resumes the moment the fix completes.
    settle_rod(player, now);
    if rod_in_workshop(player, now) {
        let remaining = format_elapsed(player.fixing_until.unwrap() - now);
        return ctx.say(
            "cast_while_fixing",
            &["{user}, your rod is in the workshop — {remaining} until it's ready to fish again."],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }

    // No hands, no fishing — the price of a previous !dynamite.
    if let Some(exp) = active_dynamite_ban(player, now) {
        let days = (exp - now) / 86_400 + 1;
        return ctx.say_text(
            "cast_no_hands",
            &format!(
                "{} approaches the water's edge, holds up both stumps in quiet contemplation, \
             and shuffles back home. ({days} day(s) remaining on the ban)",
                ctx.addr
            ),
        );
    }
    let level = player.level;

    // Pick the location: a named (unlocked) one, or the best for the player's level.
    let (location, named) = if request.location.is_empty() {
        (location_for_level(level).clone(), false)
    } else {
        match find_location(&request.location) {
            Some(loc) if loc.level > max_level(now) => {
                return ctx.say(
                    "cast_location_dormant",
                    &["That part of the Void has not opened yet."],
                    &[],
                );
            }
            Some(loc) if loc.level <= level => (loc.clone(), true),
            Some(loc) => {
                ctx.say_text(
                    "cast_location_locked",
                    &format!(
                        "{}, you haven't unlocked {} yet — need level {} (you're {}).",
                        ctx.addr, loc.name, loc.level, level
                    ),
                )?;
                return Ok(());
            }
            None => {
                let avail: Vec<&str> = data()
                    .locations
                    .iter()
                    .filter(|l| l.level <= level && l.level <= max_level(now))
                    .map(|l| l.name.as_str())
                    .collect();
                ctx.say_text(
                    "cast_location_unknown",
                    &format!(
                        "{}, no such spot. You can fish: {}.",
                        ctx.addr,
                        avail.join(", ")
                    ),
                )?;
                return Ok(());
            }
        }
    };

    if request.bait_xp > player.xp {
        return ctx.say(
            "bait_no_xp",
            &["{user}, that bait costs {cost} XP, but you only have {xp}."],
            &[
                ("user", ctx.addr),
                ("cost", &request.bait_xp.to_string()),
                ("xp", &player.xp.to_string()),
            ],
        );
    }
    player.xp -= request.bait_xp;
    let bait_hours = request.bait_xp / BAIT_XP_PER_HOUR;

    let champ_dist = champion_bonus(&state, ctx.server, &key, "distance");
    let mut rng = ctx.rng(&mut state)?;
    let player = state.players.get_mut(&key).unwrap();
    let mut distance = cast_distance(&mut rng, level, &location);
    let art_dist = artifact_bonus(player, "distance");
    if art_dist > 0.0 {
        distance = round1(distance * (1.0 + art_dist));
    }
    if champ_dist > 0.0 {
        distance = round1(distance * (1.0 + champ_dist));
    }
    player.total_casts += 1;
    if distance > player.furthest_cast {
        player.furthest_cast = distance;
    }
    season_stats_mut(player).furthest_cast = season_stats_mut(player).furthest_cast.max(distance);
    let artifact = player.artifact.clone();
    state.active_casts.insert(
        key,
        Cast {
            timestamp: now,
            distance,
            location: location.name.clone(),
            allow_lower_fish: !named,
            bait_hours,
        },
    );

    let cast_msg = match &artifact {
        Some(a) => format!(
            "{}, it sails {}m {}, {}...",
            a.cast_text,
            distance,
            location_prep(&location),
            a.float_text
        ),
        None => {
            let template = rng
                .choice(&data().cast_messages)
                .cloned()
                .unwrap_or_else(|| "You cast {distance}m {loc}...".into());
            template
                .replace("{distance}", &format!("{distance}"))
                .replace("{loc}", &location_prep(&location))
        }
    };
    let announce = maybe_trigger_event(&mut rng, &mut state, ctx.server, &location.name, now);
    save_state(&state)?;
    if request.bait_xp > 0 {
        ctx.say(
            "cast_success_baited",
            &["{user}, {cast} The bait cost {cost} XP and brings peak rarity {hours}h closer for this cast."],
            &[
                ("user", ctx.addr),
                ("cast", &cast_msg),
                ("cost", &request.bait_xp.to_string()),
                ("hours", &bait_hours.to_string()),
            ],
        )?;
    } else {
        // Keep the existing theme key and placeholder contract stable for operators who already
        // customised ordinary cast messages.
        ctx.say_text("cast_success", &format!("{}, {}", ctx.addr, cast_msg))?;
    }
    if let Some(a) = announce {
        ctx.say_text("event_started", &a)?;
    }
    Ok(())
}

fn cmd_reel(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();

    let Some(cast) = state.active_casts.remove(&key) else {
        ctx.say(
            "reel_no_cast",
            &["{user}, you don't have a line out. Use !cast first."],
            &[("user", ctx.addr)],
        )?;
        return Ok(());
    };
    // Snapshot a legacy save before this reel changes any lifetime counters.
    {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        season_stats_mut(player);
    }
    let now = now_secs();
    let wait_hours = (now - cast.timestamp) as f64 / 3600.0;
    let location_name = cast.location.clone();
    let location = data()
        .locations
        .iter()
        .find(|l| l.name == location_name)
        .cloned()
        .unwrap_or_else(|| data().locations[0].clone());
    let mut rng = ctx.rng(&mut state)?;

    // Active event (and its effect) for this network/location.
    let event = active_event_for(&mut state, ctx.server, &location_name, now);
    let effect = event.as_ref().and_then(|e| e.effect.clone());
    let ev_mult = event.as_ref().map(|e| e.multiplier).unwrap_or(1.0);

    // A feeding-frenzy (time_boost) makes the line "wait" effectively longer.
    let effective_wait = if effect.as_deref() == Some("time_boost") {
        wait_hours / ev_mult
    } else {
        wait_hours
    };
    // Bait advances only the rarity gates. It cannot make an early reel valid, grow the fish,
    // or reduce the danger of leaving a line out past 24 hours.
    let rarity_wait = effective_wait + cast.bait_hours as f64;

    // Too early — the cast is consumed but the hook is empty.
    if effective_wait < MIN_WAIT_HOURS {
        let m = rng
            .choice(&data().too_early_messages)
            .cloned()
            .unwrap_or_else(|| "Nothing but an empty hook.".into());
        save_state(&state)?;
        return ctx.say_text("reel_too_early", &format!("{}, {}", ctx.addr, m));
    }

    // Danger zone — the longer past 24h, the likelier a bad outcome.
    if wait_hours > DANGER_THRESHOLD_HOURS {
        let natural_bad = (0.1 + (wait_hours - DANGER_THRESHOLD_HOURS) * 0.05).min(0.9);
        // A reinforced rod resists the wear of a neglected line (floored at 50% of natural risk).
        let rod = state
            .players
            .get(&key)
            .map(|p| current_rod_strength(p, now))
            .unwrap_or(0);
        let bad_chance = effective_break_chance(natural_bad, rod);
        if rng.f64() < bad_chance {
            let kind = ["line_break", "fish_escaped", "junk"][rng.below(3)];
            let player = state.players.entry(key.clone()).or_default();
            player.nick = ctx.nick.to_string();
            let text = if kind == "junk" {
                player.junk_collected += 1;
                let junk = junk_item(&mut rng, &location.kind);
                format!(
                    "After {:.1}h you reel in... {}. Maybe don't leave your line so long.",
                    wait_hours, junk
                )
            } else {
                if kind == "line_break" {
                    player.lines_broken += 1;
                }
                data()
                    .danger_zone_messages
                    .get(kind)
                    .and_then(|v| rng.choice(v))
                    .cloned()
                    .unwrap_or_else(|| "It got away.".into())
            };
            save_state(&state)?;
            return ctx.say_text("reel_danger", &format!("{}, {}", ctx.addr, text));
        }
    }

    // `!fish bless` forces a rare/legendary catch (and skips junk + line-break below).
    let forced_rare = state
        .players
        .get(&key)
        .map(|p| p.force_rare_legendary)
        .unwrap_or(false);

    // Plain junk — base 10%, boosted by murky-waters events, reduced by a junk-shield artifact.
    let mut junk_chance = 0.10;
    if effect.as_deref() == Some("junk_boost") {
        junk_chance *= ev_mult;
    }
    let shield = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "junk_shield"))
        .unwrap_or(0.0);
    junk_chance *= 1.0 - shield;
    if !forced_rare && rng.f64() < junk_chance {
        // 15% of the time, an artifact turns up instead of junk.
        if rng.f64() < 0.15 {
            if let Some(art) = rng.choice(&data().artifacts).cloned() {
                let player = state.players.entry(key.clone()).or_default();
                player.nick = ctx.nick.to_string();
                let old = player.artifact.replace(art.clone());
                save_state(&state)?;
                let mut resp = format!(
                    "{} reels in... something else is tangled in the line! You found the {}! Your casts will never be the same.",
                    ctx.addr, art.name
                );
                if let Some(o) = old {
                    resp.push_str(&format!(" (Replaced: {})", o.name));
                }
                return ctx.say_text("reel_artifact", &resp);
            }
        }
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.junk_collected += 1;
        player.xp += 5;
        season_stats_mut(player).xp_earned += 5;
        let junk = junk_item(&mut rng, &location.kind);
        save_state(&state)?;
        return ctx.say_text(
            "reel_junk",
            &format!(
                "{} reels in... {}. At least you're cleaning up! (+5 XP)",
                ctx.addr, junk
            ),
        );
    }

    // A catch. Gather player-derived boosts before mutating.
    let player_level = state.players.get(&key).map(|p| p.level).unwrap_or(0);
    let art_rarity = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "rarity"))
        .unwrap_or(0.0);
    let art_xp = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "xp"))
        .unwrap_or(0.0);
    let lure = state.players.get(&key).and_then(|p| p.active_lure.clone());
    let eligible: Vec<String> = if cast.allow_lower_fish {
        data()
            .locations
            .iter()
            .filter(|l| l.level <= player_level)
            .map(|l| l.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let lure_rarity = if lure.as_deref() == Some("rarity") {
        0.40
    } else {
        0.0
    };
    let event_rare_mult = if effect.as_deref() == Some("rare_boost") {
        ev_mult
    } else {
        1.0
    };
    let champ_rarity = champion_bonus(&state, ctx.server, &key, "rarity");
    let champ_xp = champion_bonus(&state, ctx.server, &key, "xp");
    let champ_titles = champion_titles(&state, ctx.server, &key);
    let mut rarity = select_rarity(
        &mut rng,
        rarity_wait,
        event_rare_mult,
        art_rarity + lure_rarity + champ_rarity,
    );
    // Forced rare/legendary (from !fish bless): try rare then legendary at this spot, no fallback.
    let mut forced_applied = false;
    let mut fish: Option<Fish> = None;
    if forced_rare {
        let mut order = ["rare", "legendary"];
        if rng.below(2) == 1 {
            order.swap(0, 1);
        }
        for f in order {
            if let Some(found) = select_fish(&mut rng, &location_name, f, &eligible, false) {
                fish = Some(found.clone());
                rarity = f.to_string();
                forced_applied = true;
                break;
            }
        }
    }
    let fish = match fish
        .or_else(|| select_fish(&mut rng, &location_name, &rarity, &eligible, true).cloned())
    {
        Some(f) => f,
        None => {
            save_state(&state)?;
            return ctx.say(
                "reel_escaped",
                &["The fish got away at the last moment!"],
                &[],
            );
        }
    };
    let natural_weight = calc_weight(&mut rng, &fish, effective_wait);
    let mut weight = natural_weight;
    if lure.as_deref() == Some("size") {
        weight = round2(weight * 1.30);
    }
    // Chum: server-wide +40% size while active; clear once past its cooldown.
    let chum_active = match state.chum.get(ctx.server) {
        Some(c) if now < c.expires => true,
        Some(c) if now >= c.cooldown_until => {
            state.chum.remove(ctx.server);
            false
        }
        _ => false,
    };
    if chum_active {
        weight = round2(weight * 1.40);
    }

    // Line-break: bigger fish, bigger risk (a blessed catch never snaps). A reinforced rod
    // reduces the snap chance, floored at 50% of the natural risk so megafauna stay survivable
    // but never safe.
    let natural_break = 0.02 + (weight / 1000.0) * 0.15;
    let rod = state
        .players
        .get(&key)
        .map(|p| current_rod_strength(p, now))
        .unwrap_or(0);
    let break_chance = effective_break_chance(natural_break, rod);
    if !forced_applied && rng.f64() < break_chance {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.lines_broken += 1;
        save_state(&state)?;
        return ctx.say_text(
            "reel_line_break",
            &format!(
            "{}, a massive tug — a {}! But it's too much... SNAP! The line breaks and it's gone.",
            ctx.addr, fish.name
        ),
        );
    }

    // Land it.
    let mut bonus_msgs: Vec<String> = Vec::new();
    let player = state.players.entry(key.clone()).or_default();
    player.nick = ctx.nick.to_string();
    player.total_fish += 1;
    // Fold any completed !fix into rod_strength before touching rod state, so committed time is
    // never lost. Big fish (>2000 lb) wear the line: every ROD_DECAY_EVERYth such catch costs 1
    // strength. Small fish never wear a deep-sea rod.
    settle_rod(player, now);
    if apply_rod_wear(player, weight) {
        bonus_msgs.push(themed(
            "rod_worn",
            &["Your rod's line shows its strain from that beast (-1 strength)."],
            &[],
        )?);
    }
    if weight > player.biggest_fish {
        player.biggest_fish = weight;
        player.biggest_fish_name = Some(fish.name.clone());
    }
    let milestones = record_species_catch(player, &location_name, &fish, weight, natural_weight);
    {
        let seasonal = season_stats_mut(player);
        seasonal.fish_caught += 1;
        seasonal.unique_species.insert(fish.name.clone());
        seasonal.heaviest_catch = seasonal.heaviest_catch.max(weight);
        if rarity == "rare" || rarity == "legendary" {
            seasonal.rare_catches += 1;
        }
    }
    if forced_applied {
        player.force_rare_legendary = false;
    }
    if !player.locations_fished.contains(&location_name) {
        player.locations_fished.push(location_name.clone());
    }
    if rarity == "rare" || rarity == "legendary" {
        player.rare_catches.push(RareCatch {
            name: fish.name.clone(),
            weight,
            rarity: rarity.clone(),
            location: location_name.clone(),
            caught_at: now,
        });
    }

    // XP: base * rarity multiplier * weight bonus, then event/artifact/boost-rod/random.
    let rarity_mult = data()
        .rarity_xp_multiplier
        .get(&rarity)
        .copied()
        .unwrap_or(1);
    let weight_bonus = 1.0 + (weight / 50.0);
    let mut xp = (10.0 * rarity_mult as f64 * weight_bonus) as i64;
    if effect.as_deref() == Some("xp_boost") {
        xp = (xp as f64 * ev_mult) as i64;
    }
    if art_xp > 0.0 {
        xp = (xp as f64 * (1.0 + art_xp)) as i64;
    }
    if champ_xp > 0.0 {
        xp = (xp as f64 * (1.0 + champ_xp)) as i64;
        bonus_msgs.push("Traveler's blessing: +20% XP.".into());
    }
    if player.xp_boost_catches > 0 {
        xp *= 2;
        player.xp_boost_catches -= 1;
        bonus_msgs.push("Rod boost! x2 XP.".into());
        if player.xp_boost_catches == 0 {
            bonus_msgs.push("The rod's glow fades.".into());
        }
    }
    let roll = rng.f64();
    let mut extra = 0i64;
    if roll < 0.01 {
        extra = 40 + rng.below(51) as i64; // 40-90
        bonus_msgs.push(format!("Treasure haul! +{extra} XP."));
    } else if roll < 0.05 {
        extra = 8 + rng.below(13) as i64; // 8-20
        bonus_msgs.push(format!("Lucky find! +{extra} XP."));
    }
    if player.xp_boost_catches == 0 && rng.f64() < 0.007 {
        player.xp_boost_catches = 5;
        bonus_msgs.push("You found a better rod! Next 5 catches give double XP.".into());
    }
    let total_xp = xp + extra;
    player.xp += total_xp;
    season_stats_mut(player).xp_earned += total_xp;

    // Consume a rigged lure and note its payoff.
    let lure_reveal = match lure.as_deref() {
        Some("rarity") => {
            player.active_lure = None;
            " The rarity lure pays off!"
        }
        Some("size") => {
            player.active_lure = None;
            " The size lure pays off!"
        }
        _ => "",
    };

    let level_before = player.level;
    let new_level = check_level_up(player, max_level(now));

    let article = match rarity.as_str() {
        "uncommon" => "an uncommon ".to_string(),
        "rare" => "a RARE ".to_string(),
        "legendary" => "a LEGENDARY ".to_string(),
        _ => "a ".to_string(),
    };
    let who = if champ_titles.is_empty() {
        ctx.addr.to_string()
    } else {
        format!("{} {}", ctx.addr, champ_titles)
    };
    let mut response = format!(
        "{} reels in {}{} weighing {:.2} lbs after {:.1}h! (+{} XP)",
        who, article, fish.name, weight, wait_hours, total_xp
    );
    if player.dlc_enabled {
        let skin = themed(
            "dlc_skins",
            &[
                "wearing a very small fedora",
                "dressed as a nautical butler",
                "wearing a monocle of unreasonable confidence",
            ],
            &[],
        )?;
        response.push_str(&themed(
            "dlc_flourish",
            &[" It is {skin}."],
            &[("skin", &skin)],
        )?);
    }
    if !bonus_msgs.is_empty() {
        response.push(' ');
        response.push_str(&bonus_msgs.join(" "));
    }
    if chum_active {
        response.push_str(" (chummed waters!)");
    }
    if cast.bait_hours > 0 {
        response.push_str(&format!(
            " Bait added {}h to the rarity roll.",
            cast.bait_hours
        ));
    }
    if milestones.new_record {
        if milestones.previous_record > 0.0 {
            let previous = format!("{:.2}", milestones.previous_record);
            response.push_str(&themed(
                "record_broken",
                &[" NEW PERSONAL RECORD! Previous: {previous} lbs."],
                &[("previous", &previous)],
            )?);
        } else {
            response.push_str(&themed(
                "record_first",
                &[" First personal record for this species!"],
                &[],
            )?);
        }
    }
    if milestones.trophy {
        response.push_str(&themed(
            "record_trophy",
            &[" Trophy specimen (95%+ natural size)!"],
            &[],
        )?);
    }
    if milestones.mastery != milestones.previous_mastery {
        if let Some(tier) = milestones.mastery {
            response.push_str(&themed(
                "mastery_achieved",
                &[" {tier} mastery achieved!"],
                &[("tier", tier)],
            )?);
        }
    }
    response.push_str(lure_reveal);
    if let Some(lvl) = new_level {
        response.push_str(&format!(
            " LEVEL UP! You're now level {lvl} and can fish at {}!",
            location_for_level(lvl).name
        ));
        // Crossing into level 15 unlocks the reinforced rod. Announce it once so the player
        // discovers the feature naturally rather than having to guess !rod exists.
        if level_before < ROD_UNLOCK_LEVEL && lvl >= ROD_UNLOCK_LEVEL {
            response.push_str(&themed(
                "rod_unlocked",
                &[" You can now reinforce your fishing rod! Use !rod to inspect it and !fix [1-24h] to add strength — a stronger line lands bigger fish."],
                &[],
            )?);
        }
    }
    save_state(&state)?;
    ctx.say_text("reel_catch", &response)
}

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

// ── commands: displays ──────────────────────────────────────────────────────

fn resolve_player_key(state: &State, ctx: &Ctx, arg: &str) -> (String, String) {
    if arg.is_empty() {
        return (ctx.key(), ctx.addr.to_string());
    }
    let prefix = format!("{}/", ctx.server);
    let folded_arg = fold_nick(ctx.server, arg);
    let key = state
        .players
        .iter()
        .find(|(key, player)| {
            key.starts_with(&prefix) && fold_nick(ctx.server, &player.nick) == folded_arg
        })
        .map(|(key, _)| key.clone())
        .unwrap_or_else(|| format!("{}/{}", ctx.server, folded_arg));
    (key, arg.to_string())
}

fn cmd_stats(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let state = load_state()?;
    let level_cap = max_level(now_secs());
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(p) = state.players.get(&key) else {
        return ctx.say_text(
            "stats_unknown",
            &format!("{} hasn't gone fishing yet.", who),
        );
    };
    let loc = location_for_level(p.level);
    let biggest = p
        .biggest_fish_name
        .as_ref()
        .map(|n| format!("{:.2} lbs ({})", p.biggest_fish, n))
        .unwrap_or_else(|| format!("{:.2} lbs", p.biggest_fish));
    let xp = if p.level >= level_cap {
        format!("{} spendable (MAX)", p.xp)
    } else {
        format!("{}/{}", p.xp, xp_for_level(p.level))
    };
    ctx.say_text(
        "stats",
        &format!(
        "Fishing stats for {}: Level {} ({}) | XP {} | Fish {} | Biggest {} | Casts {} | Junk {}",
        who, p.level, loc.name, xp, p.total_fish, biggest, p.total_casts, p.junk_collected
    ),
    )
}

fn cmd_top(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let prefix = format!("{}/", ctx.server);
    let mut players: Vec<&Player> = state
        .players
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, p)| p)
        .collect();
    if players.is_empty() {
        return ctx.say("top_empty", &["No one has gone fishing yet!"], &[]);
    }
    let mut by_fish = players.clone();
    by_fish.retain(|p| p.total_fish > 0);
    by_fish.sort_by_key(|p| std::cmp::Reverse(p.total_fish));
    let most: Vec<String> = by_fish
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, p)| format!("#{} {} ({})", i + 1, name_of(p), p.total_fish))
        .collect();

    players.retain(|p| p.biggest_fish > 0.0);
    players.sort_by(|a, b| {
        b.biggest_fish
            .partial_cmp(&a.biggest_fish)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let big: Vec<String> = players
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, p)| {
            format!(
                "#{} {} ({:.1} lbs {})",
                i + 1,
                name_of(p),
                p.biggest_fish,
                p.biggest_fish_name.clone().unwrap_or_default()
            )
        })
        .collect();

    let mut out = String::from("Fishing Leaderboards:");
    if !most.is_empty() {
        out.push_str(&format!(" Most Fish: {}", most.join(", ")));
    }
    if !big.is_empty() {
        out.push_str(&format!(" | Biggest: {}", big.join(", ")));
    }
    ctx.say_text("top", &out)
}

fn name_of(p: &Player) -> String {
    if p.nick.is_empty() {
        "Unknown".into()
    } else {
        p.nick.clone()
    }
}

fn cmd_location(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let level_cap = max_level(now_secs());
    let level = state.players.get(&ctx.key()).map(|p| p.level).unwrap_or(0);
    let loc = location_for_level(level);
    let next = data()
        .locations
        .iter()
        .find(|l| l.level == level + 1 && l.level <= level_cap);
    let next_txt = match next {
        Some(n) => format!(" Next: {} at level {}.", n.name, n.level),
        None => " You've reached the final frontier.".into(),
    };
    ctx.say_text(
        "location",
        &format!(
            "{}, you're level {} fishing at {}.{}",
            ctx.addr, level, loc.name, next_txt
        ),
    )
}

fn cmd_fishinfo(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let level_cap = max_level(now_secs());
    if arg.is_empty() {
        let names: Vec<&str> = data()
            .locations
            .iter()
            .filter(|location| location.level <= level_cap)
            .map(|location| location.name.as_str())
            .collect();
        return ctx.say_text(
            "fishinfo_help",
            &format!("Locations: {}. Try !fishinfo <location>.", names.join(", ")),
        );
    }
    let Some(loc) = find_location(arg) else {
        return ctx.say_text(
            "fishinfo_unknown",
            &format!("{}, no such location.", ctx.addr),
        );
    };
    if loc.level > level_cap {
        return ctx.say(
            "fishinfo_dormant",
            &["That part of the Void has not opened yet."],
            &[],
        );
    }
    let fish = data()
        .fish_by_location
        .get(&loc.name)
        .cloned()
        .unwrap_or_default();
    let names: Vec<String> = fish
        .iter()
        .take(12)
        .map(|f| format!("{} ({})", f.name, f.rarity))
        .collect();
    ctx.say_text(
        "fishinfo",
        &format!("{} (level {}): {}", loc.name, loc.level, names.join(", ")),
    )
}

fn cmd_aquarium(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let Some(p) = state.players.get(&ctx.key()) else {
        return ctx.say_text(
            "aquarium_empty",
            &format!("{}, your aquarium is empty — go fish!", ctx.addr),
        );
    };
    if p.rare_catches.is_empty() {
        return ctx.say_text(
            "aquarium_no_rare",
            &format!("{}, no rare or legendary catches yet.", ctx.addr),
        );
    }
    let mut recent = p.rare_catches.clone();
    recent.reverse();
    let items: Vec<String> = recent
        .iter()
        .take(6)
        .map(|c| format!("{} {} ({:.1} lbs)", c.rarity, c.name, c.weight))
        .collect();
    ctx.say_text(
        "aquarium",
        &format!(
            "{}'s aquarium ({} total): {}",
            ctx.addr,
            p.rare_catches.len(),
            items.join(", ")
        ),
    )
}

fn cmd_mastery(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say_text(
            "mastery_unknown",
            &format!("{who} hasn't gone fishing yet."),
        );
    };
    let changed = migrate_species_careers(player);
    let mut mastered: Vec<&SpeciesCareer> = player
        .species_careers
        .values()
        .filter(|career| mastery_for(career.catches).is_some())
        .collect();
    mastered.sort_by(|a, b| b.catches.cmp(&a.catches).then_with(|| a.name.cmp(&b.name)));
    let tiers = ["Bronze", "Silver", "Gold", "Iridescent"]
        .iter()
        .map(|tier| {
            let count = mastered
                .iter()
                .filter(|career| mastery_for(career.catches) == Some(*tier))
                .count();
            format!("{tier} {count}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let highlights = mastered
        .iter()
        .take(6)
        .map(|career| {
            format!(
                "{} {} ({})",
                career.name,
                mastery_for(career.catches).unwrap_or(""),
                career.catches
            )
        })
        .collect::<Vec<_>>();
    if changed {
        save_state(&state)?;
    }
    let detail = if highlights.is_empty() {
        "No mastered species yet; Bronze begins at 5 catches.".to_string()
    } else {
        highlights.join(", ")
    };
    ctx.say_text(
        "mastery",
        &format!("{who}'s species mastery: {tiers} | {detail}"),
    )
}

fn cmd_records(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say_text(
            "records_unknown",
            &format!("{who} hasn't gone fishing yet."),
        );
    };
    let changed = migrate_species_careers(player);
    let mut records: Vec<&SpeciesCareer> = player
        .species_careers
        .values()
        .filter(|career| career.best_weight > 0.0)
        .collect();
    records.sort_by(|a, b| {
        b.best_quality
            .partial_cmp(&a.best_quality)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    let items = records
        .iter()
        .take(6)
        .map(|career| {
            let trophy = if career.best_quality >= 0.95 {
                " ★"
            } else {
                ""
            };
            format!(
                "{} {:.2} lbs (record quality {:.0}%; best natural {:.0}%{})",
                career.name,
                career.best_weight,
                career.best_record_quality * 100.0,
                career.best_quality * 100.0,
                trophy
            )
        })
        .collect::<Vec<_>>();
    if changed {
        save_state(&state)?;
    }
    if items.is_empty() {
        return ctx.say_text(
            "records_empty",
            &format!("{who} has no measured personal records yet; legacy catches still count toward mastery."),
        );
    }
    ctx.say_text(
        "records",
        &format!(
            "{who}'s best specimens by natural quality (★ = 95%+): {}",
            items.join(", ")
        ),
    )
}

fn cmd_help(ctx: &Ctx) -> Result<(), Error> {
    if expansion_active(now_secs()) {
        ctx.say("help_void_expansion", &["Fishing: !cast [location] [bait <100-1700 XP>] then wait (1h+, best ~24h, risky after 24h) and !reel. Bait spends 100 XP per virtual rarity hour. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !mastery [nick], !records [nick], !rod/!fix [1-24h] (level 15+ reinforced rod, lowers break chance), !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite."], &[])
    } else {
        ctx.say("help", &["Fishing: !cast [location] then wait (1h+, best ~24h, risky after 24h) and !reel. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !mastery [nick], !records [nick], !rod/!fix [1-24h] (level 15+ reinforced rod, lowers break chance), !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite."], &[])
    }
}

fn cmd_lure(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let mut rng = ctx.rng(&mut state)?;
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.active_lure.is_some() {
        return ctx.say_text(
            "lure_active",
            &format!("{}, you already have a lure rigged up!", ctx.addr),
        );
    }
    if player.xp < 30 {
        return ctx.say_text(
            "lure_no_xp",
            &format!("{}, not enough XP (need 30, have {}).", ctx.addr, player.xp),
        );
    }
    player.xp -= 30;
    player.active_lure = Some(if rng.below(2) == 0 {
        "rarity".into()
    } else {
        "size".into()
    });
    save_state(&state)?;
    ctx.say_text(
        "lure_success",
        &format!(
            "{} spends 30 XP and rigs up a mystery lure. Let's see what it attracts!",
            ctx.addr
        ),
    )
}

fn cmd_chum(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    if let Some(c) = state.chum.get(ctx.server) {
        if now < c.expires {
            let mins = (c.expires - now) / 60 + 1;
            return ctx.say_text(
                "chum_active",
                &format!(
                    "{}, the water is already chummed! {} minute(s) left.",
                    ctx.addr, mins
                ),
            );
        }
        if now < c.cooldown_until {
            let mins = (c.cooldown_until - now) / 60 + 1;
            return ctx.say_text(
                "chum_cooldown",
                &format!(
                    "{}, the chum is on cooldown. {} minute(s) until it can be used again.",
                    ctx.addr, mins
                ),
            );
        }
    }
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.xp < 250 {
        return ctx.say_text(
            "chum_no_xp",
            &format!(
                "{}, not enough XP (need 250, have {}).",
                ctx.addr, player.xp
            ),
        );
    }
    player.xp -= 250;
    state.chum.insert(
        ctx.server.to_string(),
        Chum {
            expires: now + 20 * 60,
            cooldown_until: now + 50 * 60,
            by_id: ctx.key(),
            by_name: ctx.nick.to_string(),
        },
    );
    save_state(&state)?;
    ctx.say_text("chum_success", &format!("{} tosses a handful of chum into the water! Fish should run large for the next 20 minutes!", ctx.addr))
}

fn cmd_discard(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    match player.artifact.take() {
        Some(a) => {
            save_state(&state)?;
            ctx.say_text(
                "discard_success",
                &format!(
                    "{} tosses the {} into the water. All bonuses lost — casts return to normal.",
                    ctx.addr, a.name
                ),
            )
        }
        None => ctx.say_text(
            "discard_empty",
            &format!("{}, you don't have an artifact to discard.", ctx.addr),
        ),
    }
}

// ── commands: reinforced rod ────────────────────────────────────────────────

/// `!rod` — inspect the reinforced rod's current strength and any in-progress fix. Unlocks at
/// level [`ROD_UNLOCK_LEVEL`]; below that, the player is told to come back later.
fn cmd_rod(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let (settled, level, strength, fixing_until) = {
        let player = state.players.entry(ctx.key()).or_default();
        player.nick = ctx.nick.to_string();
        let settled = settle_rod(player, now);
        (
            settled,
            player.level,
            player.rod_strength,
            player.fixing_until,
        )
    };
    if settled {
        save_state(&state)?;
    }
    if level < ROD_UNLOCK_LEVEL {
        return ctx.say(
            "rod_locked",
            &["{user}, reinforced rods are an old fisher's secret. Come back at level {level}."],
            &[("user", ctx.addr), ("level", &ROD_UNLOCK_LEVEL.to_string())],
        );
    }
    if fixing_until.is_some_and(|until| now < until) {
        let remaining = format_elapsed(fixing_until.unwrap() - now);
        return ctx.say(
            "rod_fixing",
            &["{user}, your rod is in the workshop being strengthened (strength {strength}/{max}) — {remaining} until it's ready."],
            &[
                ("user", ctx.addr),
                ("strength", &strength.to_string()),
                ("max", &ROD_MAX_STRENGTH.to_string()),
                ("remaining", &remaining),
            ],
        );
    }
    ctx.say(
        "rod_status",
        &["{user}, your rod: strength {strength}/{max}. Each point lowers break chance, to a floor of half the natural risk. Use !fix [1-24h] to add strength."],
        &[
            ("user", ctx.addr),
            ("strength", &strength.to_string()),
            ("max", &ROD_MAX_STRENGTH.to_string()),
        ],
    )
}

/// `!fix [hours]` — commit time to strengthen the rod (+1 strength per hour, capped at 24h per
/// `!fix`). While fixing, `!cast` is refused. Strength is granted when the time window elapses,
/// so offline time counts and there's no "commit then cancel" exploit.
fn cmd_fix(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let settled = {
        let player = state.players.entry(ctx.key()).or_default();
        player.nick = ctx.nick.to_string();
        settle_rod(player, now)
    };
    if settled {
        save_state(&state)?;
    }
    let player = state.players.entry(ctx.key()).or_default();
    if player.level < ROD_UNLOCK_LEVEL {
        return ctx.say(
            "rod_locked",
            &["{user}, reinforced rods are an old fisher's secret. Come back at level {level}."],
            &[("user", ctx.addr), ("level", &ROD_UNLOCK_LEVEL.to_string())],
        );
    }
    if rod_in_workshop(player, now) {
        let remaining = format_elapsed(player.fixing_until.unwrap() - now);
        return ctx.say(
            "fix_already",
            &["{user}, you're already working on the rod — {remaining} until it's done."],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }
    if player.rod_strength >= ROD_MAX_STRENGTH {
        return ctx.say(
            "fix_maxed",
            &["{user}, your rod is already at maximum strength ({max}). Fish proud."],
            &[("user", ctx.addr), ("max", &ROD_MAX_STRENGTH.to_string())],
        );
    }
    // Parse hours: bare !fix = 1h; otherwise a whole number in 1..=ROD_FIX_MAX_HOURS.
    let hours = match parse_fix_hours(arg) {
        Ok(h) => h,
        Err(_) => {
            return ctx.say(
                "fix_usage",
                &["{user}, usage: !fix [hours 1-{max}]. Default is 1 hour per point of strength."],
                &[("user", ctx.addr), ("max", &ROD_FIX_MAX_HOURS.to_string())],
            );
        }
    };
    let until = now + (hours as i64) * 3600;
    player.fixing_until = Some(until);
    player.fixing_hours = Some(hours);
    save_state(&state)?;
    ctx.say(
        "fix_started",
        &["{user}, you set to work reinforcing the rod. Check back in {hours}h — casting is paused while it's in the workshop."],
        &[
            ("user", ctx.addr),
            ("hours", &hours.to_string()),
        ],
    )
}

/// Parse the `!fix` hours argument: empty = 1, otherwise a whole number in 1..=ROD_FIX_MAX_HOURS.
fn parse_fix_hours(arg: &str) -> Result<u8, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Ok(1);
    }
    let n: i64 = trimmed.parse().map_err(|_| "not a whole number")?;
    if !(1..=ROD_FIX_MAX_HOURS).contains(&n) {
        return Err("out of range");
    }
    Ok(n as u8)
}

// ── commands: champions, risk toys, admin ───────────────────────────────────

fn cmd_champions(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let crowned = state.champions.get(ctx.server);
    let has_any = crowned
        .is_some_and(|c| c.traveler.is_some() || c.caster.is_some() || c.collector.is_some());
    let Some(c) = crowned.filter(|_| has_any) else {
        return ctx.say(
            "champions_empty",
            &["No champions yet — the first champions will be crowned at the next season reset!"],
            &[],
        );
    };
    let mut parts = vec![format!("Fishing Champions ({}):", c.season)];
    if c.traveler.is_some() {
        parts.push(format!(
            "the Traveler: {} (level {}, {})",
            c.traveler_name, c.traveler_level, c.traveler_location
        ));
    }
    if c.caster.is_some() {
        parts.push(format!(
            "the Caster: {} ({:.1}m)",
            c.caster_name, c.caster_distance
        ));
    }
    if c.collector.is_some() {
        parts.push(format!(
            "the Collector: {} ({} rare/legendary catches)",
            c.collector_name, c.collector_count
        ));
    }
    ctx.say_text("champions", &parts.join(" | "))
}

fn cmd_dynamite(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let mut rng = ctx.rng(&mut state)?;
    {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        season_stats_mut(player);
    }

    // Already banned? No hands, no dynamite.
    if let Some(exp) = active_dynamite_ban(state.players.get_mut(&key).unwrap(), now) {
        let days = (exp - now) / 86_400 + 1;
        save_state(&state)?;
        return ctx.say_text(
            "dynamite_banned",
            &format!(
                "{} reaches into the tackle box with no hands left. There's no dynamite there, \
             and no plausible way to light it either. ({days} day(s) remaining)",
                ctx.addr
            ),
        );
    }

    let roll = rng.f64();

    // 10% — thinks better of it.
    if roll < 0.10 {
        let chicken = [
            format!("{} pulls out the dynamite, stares at it for a long moment... and puts it back. Some decisions don't need to be made today. Goes to get a cup of tea.", ctx.addr),
            format!("{} hefts the dynamite thoughtfully, then sets it gently on a rock. The tea is calling. The fish can wait.", ctx.addr),
            format!("{} gets halfway through lighting the fuse before reconsidering. Honestly, a nice biscuit sounds better right now.", ctx.addr),
            format!("{} holds the dynamite aloft dramatically... then pockets it and wanders off in search of a kettle.", ctx.addr),
            format!("{} considers the dynamite. Considers the fish. Considers their own mortality. Decides tea is the wiser investment.", ctx.addr),
        ];
        save_state(&state)?;
        return ctx.say_text("dynamite_chicken", &chicken[rng.below(chicken.len())]);
    }

    // 20% — glorious success: a rare/legendary haul + a big XP grant (two levels' worth).
    if roll < 0.30 {
        let player = state.players.get_mut(&key).unwrap();
        let (mut tl, mut tx, mut grant, mut levels) = (player.level, player.xp, 0i64, 0i64);
        let level_cap = max_level(now);
        while levels < 2 && tl < level_cap {
            grant += (xp_for_level(tl) - tx).max(0);
            tx = 0;
            tl += 1;
            levels += 1;
        }
        grant += 80 + rng.below(121) as i64; // 80-200

        let top = data().locations.iter().rfind(|l| l.level <= player.level);
        let loc_name = top
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "Puddle".into());
        let eligible: Vec<String> = data()
            .locations
            .iter()
            .filter(|l| l.level <= player.level)
            .map(|l| l.name.clone())
            .collect();
        let haul_count = 3 + rng.below(4); // 3-6
        let mut haul: Vec<(String, String, f64)> = Vec::new();
        for _ in 0..haul_count {
            let rarity = ["rare", "rare", "legendary"][rng.below(3)];
            if let Some(fish) = select_fish(&mut rng, &loc_name, rarity, &eligible, true) {
                let fish = fish.clone();
                let weight = round2(rng.range(fish.max_weight * 0.7, fish.max_weight));
                let milestones = record_species_catch(player, &loc_name, &fish, weight, weight);
                player.total_fish += 1;
                if weight > player.biggest_fish {
                    player.biggest_fish = weight;
                    player.biggest_fish_name = Some(fish.name.clone());
                }
                player.rare_catches.push(RareCatch {
                    name: fish.name.clone(),
                    weight,
                    rarity: rarity.to_string(),
                    location: loc_name.clone(),
                    caught_at: now,
                });
                let seasonal = season_stats_mut(player);
                seasonal.fish_caught += 1;
                seasonal.unique_species.insert(fish.name.clone());
                seasonal.rare_catches += 1;
                seasonal.heaviest_catch = seasonal.heaviest_catch.max(weight);
                let marker = if milestones.new_record {
                    themed("record_marker", &[" RECORD"], &[])?
                } else {
                    String::new()
                };
                haul.push((format!("{}{marker}", fish.name), rarity.to_string(), weight));
            }
        }
        player.xp += grant;
        season_stats_mut(player).xp_earned += grant;
        let new_level = check_level_up(player, level_cap);

        let haul_str = if haul.is_empty() {
            "an eerie silence".to_string()
        } else {
            haul.iter()
                .map(|(n, r, w)| format!("{n} ({w:.1} lbs, {r})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut resp = format!(
            "KABOOM! {} hurls the dynamite into the fishing hole! The water ERUPTS. \
             Belly-up on the surface: {}. +{} XP from the sheer audacity of it.",
            ctx.addr, haul_str, grant
        );
        if let Some(lvl) = new_level {
            resp.push_str(&format!(
                " LEVEL UP x{}! Now level {} — {} awaits!",
                levels,
                lvl,
                location_for_level(lvl).name
            ));
        }
        save_state(&state)?;
        return ctx.say_text("dynamite_success", &resp);
    }

    // 70% — catastrophe. First costs a hand; a second costs fishing access for a week.
    let hands_lost = state
        .players
        .get(&key)
        .map(|p| p.dynamite_hands_lost)
        .unwrap_or(0);
    if hands_lost < 1 {
        let player = state.players.get_mut(&key).unwrap();
        player.dynamite_hands_lost = 1;
        let lines = [
            format!("{} lights the dynamite. The dynamite does not wait. There is a flash, a bang, and suddenly one hand is a matter for historians. The other remains available for poor decisions.", ctx.addr),
            format!("{} fumbles the dynamite. It goes off immediately. In their hand. The fish are fine. The hand is not. One hand left.", ctx.addr),
            format!("{} finds the fuse much shorter than expected. The resulting lesson costs exactly one hand. Fishing privileges remain, technically.", ctx.addr),
        ];
        let msg = lines[rng.below(lines.len())].clone();
        save_state(&state)?;
        return ctx.say_text("dynamite_one_hand", &msg);
    }

    let ban_until = now + 7 * 86_400;
    {
        let player = state.players.get_mut(&key).unwrap();
        player.dynamite_hands_lost = 2;
        player.dynamite_banned_until = Some(ban_until);
    }
    state.active_casts.remove(&key);
    let lines = [
        format!("{} lights the dynamite with their remaining hand. A flash. A bang. A full accounting of previous warnings. No hands remain — a 7-day fishing ban has been issued.", ctx.addr),
        format!("{} fumbles the dynamite again, into the only hand they had left. The fish are fine. The hands are gone. Banned from fishing for 7 days.", ctx.addr),
        format!("{} has made the same terrible mistake twice. The lake files the paperwork. No hands left, no fishing for 7 days, no exceptions.", ctx.addr),
    ];
    let msg = lines[rng.below(lines.len())].clone();
    save_state(&state)?;
    ctx.say_text("dynamite_banned_result", &msg)
}

fn cmd_bless(ctx: &Ctx, target: &str) -> Result<(), Error> {
    if ctx.role != Some(Role::SuperAdmin) {
        return ctx.say_text(
            "bless_denied",
            &format!(
                "{}, only a super-admin may bestow such blessings.",
                ctx.addr
            ),
        );
    }
    if target.is_empty() {
        return ctx.say("bless_usage", &["Usage: !fish bless <nick>"], &[]);
    }
    let mut state = load_state()?;
    let tkey = format!("{}/{}", ctx.server, fold_nick(ctx.server, target));
    let player = state.players.entry(tkey).or_default();
    if player.nick.is_empty() {
        player.nick = target.to_string();
    }
    player.force_rare_legendary = true;
    save_state(&state)?;
    ctx.say_text(
        "bless_success",
        &format!("{}, your next catch will be rare or legendary.", target),
    )
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

fn cmd_dlc(ctx: &Ctx, args: &str) -> Result<(), Error> {
    if ctx.role != Some(Role::SuperAdmin) {
        return ctx.say(
            "dlc_denied",
            &["{user}, premium fish couture may only be administered by a super-admin."],
            &[("user", ctx.addr)],
        );
    }
    let mut parts = args.split_whitespace();
    let action = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if !matches!(action, "grant" | "revoke" | "status") || target.is_empty() || parts.next().is_some() {
        return ctx.say(
            "dlc_usage",
            &["Usage: !fish dlc grant|revoke|status <nick>"],
            &[],
        );
    }
    let Some(profile) = profile_for_nick(ctx.server, target)? else {
        return ctx.say(
            "dlc_unknown",
            &["I cannot locate a profile for {nick}; they must speak before acquiring premium fishwear."],
            &[("nick", target)],
        );
    };
    let key = format!("{}/{}", ctx.server, profile.id);
    let mut state = load_state()?;
    let enabled = state.players.get(&key).is_some_and(|p| p.dlc_enabled);
    match action {
        "status" => ctx.say(
            "dlc_status",
            &["Premium Fish Couture for {nick}: {status}."],
            &[("nick", &profile.nick), ("status", if enabled { "active" } else { "inactive" })],
        ),
        "grant" => {
            let player = state.players.entry(key).or_default();
            player.nick = profile.nick.clone();
            player.dlc_enabled = true;
            save_state(&state)?;
            ctx.say(
                "dlc_granted",
                &["Premium Fish Couture has been activated for {nick}. The invoice remains tastefully undisclosed."],
                &[("nick", &profile.nick)],
            )
        }
        "revoke" => {
            if let Some(player) = state.players.get_mut(&key) {
                player.dlc_enabled = false;
                save_state(&state)?;
            }
            ctx.say(
                "dlc_revoked",
                &["Premium Fish Couture has been withdrawn from {nick}. The fish return to ordinary nudity."],
                &[("nick", &profile.nick)],
            )
        }
        _ => unreachable!(),
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
    fn legacy_player_state_defaults_dlc_to_disabled() {
        let player: Player = serde_json::from_str("{}").unwrap();
        assert!(!player.dlc_enabled);
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
    fn rarity_respects_wait_gates() {
        let mut rng = Rng(123456789);
        // Under 6h: never rare/legendary.
        for _ in 0..500 {
            let r = select_rarity(&mut rng, 3.0, 1.0, 0.0);
            assert!(r == "common" || r == "uncommon", "got {r} at 3h");
        }
        // 20h: full table — at least one rare/legendary should appear.
        let mut seen_rare = false;
        for _ in 0..2000 {
            let r = select_rarity(&mut rng, 20.0, 1.0, 0.0);
            if r == "rare" || r == "legendary" {
                seen_rare = true;
                break;
            }
        }
        assert!(
            seen_rare,
            "expected a rare/legendary at 20h over many rolls"
        );
    }

    #[test]
    fn weight_stays_in_range_and_scales() {
        let mut rng = Rng(42);
        let fish = Fish {
            name: "Test".into(),
            min_weight: 2.0,
            max_weight: 10.0,
            rarity: "common".into(),
        };
        for _ in 0..200 {
            let w = calc_weight(&mut rng, &fish, 24.0);
            assert!((2.0..=10.0).contains(&w), "w={w}");
        }
        // Long waits trend heavier than very short ones (averaged).
        let avg = |hours: f64| {
            let mut r = Rng(7);
            let mut s = 0.0;
            for _ in 0..500 {
                s += calc_weight(&mut r, &fish, hours);
            }
            s / 500.0
        };
        assert!(avg(24.0) > avg(1.0));
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
    fn cast_bait_parser_is_bounded_and_keeps_multiword_locations() {
        assert_eq!(
            parse_cast_request("Purple Void bait 500"),
            Ok(CastRequest {
                location: "Purple Void".into(),
                bait_xp: 500,
            })
        );
        assert_eq!(
            parse_cast_request("bait 1700"),
            Ok(CastRequest {
                location: String::new(),
                bait_xp: 1700,
            })
        );
        assert!(parse_cast_request("bait 50").is_err());
        assert!(parse_cast_request("bait 1800").is_err());
        assert!(parse_cast_request("bait 500 extra").is_err());
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
    fn fix_hours_parser_accepts_bare_and_bounded_input() {
        assert_eq!(parse_fix_hours("").unwrap(), 1);
        assert_eq!(parse_fix_hours("8").unwrap(), 8);
        assert_eq!(parse_fix_hours("  24 ").unwrap(), 24);
        assert!(parse_fix_hours("0").is_err(), "zero is out of range");
        assert!(parse_fix_hours("25").is_err(), "above the 24h cap");
        assert!(parse_fix_hours("lots").is_err(), "non-numeric rejected");
        assert!(parse_fix_hours("-3").is_err(), "negative rejected");
    }
}
