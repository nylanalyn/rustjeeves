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
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Role, SendMessage, ThemeReq,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn theme(input: String) -> String;
    fn irc_casefold(input: String) -> String;
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
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            cast,
            command("reel", "Reel in a fishing line."),
            command("fishinfo", "Look up a fish."),
            command("aquarium", "Show your aquarium."),
            command("lure", "Manage fishing lures."),
            command("chum", "Use fishing chum."),
            command("discard", "Discard an aquarium item."),
            command("water", "Use the watering action."),
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
    /// `!water` curse: "YYYY-MM-DD" (UTC) for which every reel is junk.
    #[serde(default)]
    junk_curse_date: Option<String>,
    /// `!dynamite` damage: 0, 1, or 2 hands lost.
    #[serde(default)]
    dynamite_hands_lost: i64,
    /// `!dynamite` ban: unix seconds until fishing is allowed again.
    #[serde(default)]
    dynamite_banned_until: Option<i64>,
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

// ── tiny PRNG (no entropy in wasm; seed from now() + persisted nonce) ─────────

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

fn today_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_unix(secs);
    format!("{y:04}-{m:02}-{d:02}")
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

/// Compute the three champions (player keys) from a server's players. Ties broken by `total_fish`,
/// matching the Python `_compute_season_champions`.
fn compute_champions(
    players: &[(&String, &Player)],
) -> (Option<String>, Option<String>, Option<String>) {
    let best = |score: &dyn Fn(&Player) -> f64, ok: &dyn Fn(&Player) -> bool| -> Option<String> {
        players
            .iter()
            .filter(|(_, p)| ok(p))
            .max_by(|(_, a), (_, b)| {
                score(a)
                    .partial_cmp(&score(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.total_fish.cmp(&b.total_fish))
            })
            .map(|(k, _)| (*k).clone())
    };
    (
        best(&|p| p.level as f64, &|p| p.level > 0),
        best(&|p| p.furthest_cast, &|p| p.furthest_cast > 0.0),
        best(&|p| p.rare_catches.len() as f64, &|p| {
            !p.rare_catches.is_empty()
        }),
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
/// `now` passes a boundary, crowns champions, wipes that server's players/casts/events, advances the
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
        state
            .next_reset
            .insert(server.to_string(), boundary);
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
        champ.traveler_level = p.level;
        champ.traveler_location = location_for_level(p.level).name.clone();
    }
    if let Some(p) = caster.as_ref().and_then(|k| state.players.get(k)) {
        champ.caster_distance = p.furthest_cast;
    }
    if let Some(p) = collector.as_ref().and_then(|k| state.players.get(k)) {
        champ.collector_count = p.rare_catches.len() as i64;
    }

    let mut lines = vec![format!(
        "** SEASON RESET ** The sea has been cleared! {season} champions:"
    )];
    if traveler.is_some() {
        lines.push(format!(
            "the Traveler: {} (reached {}, level {}) — carries a +20% XP blessing into the new season",
            champ.traveler_name, champ.traveler_location, champ.traveler_level
        ));
    } else {
        lines.push("the Traveler: unclaimed (no one leveled up this season)".into());
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
    lines.push("Good luck to all in the new season!".into());

    champ.traveler = traveler;
    champ.caster = caster;
    champ.collector = collector;
    state.champions.insert(server.to_string(), champ);

    // Wipe this server's players, casts, and active event for the new season.
    state.players.retain(|k, _| !k.starts_with(&prefix));
    state.active_casts.retain(|k, _| !k.starts_with(&prefix));
    state.active_events.remove(server);
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
        "!lure" => cmd_lure(&ctx)?,
        "!chum" => cmd_chum(&ctx)?,
        "!discard" => cmd_discard(&ctx)?,
        "!water" => cmd_water(&ctx)?,
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
    fn rng(&self, state: &mut State) -> Rng {
        state.nonce = state.nonce.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let seed = (now_secs() as u64) ^ state.nonce ^ 0xD1B5_4A32_D192_ED03;
        Rng(seed | 1)
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

fn cmd_cast(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let now = now_secs();

    if let Some(cast) = state.active_casts.get(&key) {
        let hours = (now - cast.timestamp) as f64 / 3600.0;
        ctx.say_text(
            "cast_already_active",
            &format!(
            "{}, you already have a line in the water at {} ({:.1}h). Use !reel to bring it in.",
            ctx.addr, cast.location, hours
        ),
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
    let mut rng = ctx.rng(&mut state);
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
    let now = now_secs();
    let wait_hours = (now - cast.timestamp) as f64 / 3600.0;
    let location_name = cast.location.clone();
    let location = data()
        .locations
        .iter()
        .find(|l| l.name == location_name)
        .cloned()
        .unwrap_or_else(|| data().locations[0].clone());
    let mut rng = ctx.rng(&mut state);

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
        let bad_chance = (0.1 + (wait_hours - DANGER_THRESHOLD_HOURS) * 0.05).min(0.9);
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

    // `!water` curse — every reel today is junk, bypassing all protections.
    if !forced_rare {
        let cursed = state
            .players
            .get(&key)
            .and_then(|p| p.junk_curse_date.clone())
            == Some(today_utc(now));
        if cursed {
            let player = state.players.entry(key.clone()).or_default();
            player.nick = ctx.nick.to_string();
            player.junk_collected += 1;
            let junk = junk_item(&mut rng, &location.kind);
            save_state(&state)?;
            return ctx.say_text(
                "reel_cursed_junk",
                &format!("{} reels in... {}. The curse holds.", ctx.addr, junk),
            );
        }
    }

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
    let mut weight = calc_weight(&mut rng, &fish, effective_wait);
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

    // Line-break: bigger fish, bigger risk (a blessed catch never snaps).
    let break_chance = 0.02 + (weight / 1000.0) * 0.15;
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
    if weight > player.biggest_fish {
        player.biggest_fish = weight;
        player.biggest_fish_name = Some(fish.name.clone());
    }
    *player.catches.entry(fish.name.clone()).or_insert(0) += 1;
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
    response.push_str(lure_reveal);
    if let Some(lvl) = new_level {
        response.push_str(&format!(
            " LEVEL UP! You're now level {lvl} and can fish at {}!",
            location_for_level(lvl).name
        ));
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

fn cmd_stats(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let state = load_state()?;
    let level_cap = max_level(now_secs());
    let (key, who) = if arg.is_empty() {
        (ctx.key(), ctx.addr.to_string())
    } else {
        let prefix = format!("{}/", ctx.server);
        let folded_arg = fold_nick(ctx.server, arg);
        let found = state
            .players
            .iter()
            .find(|(key, player)| {
                key.starts_with(&prefix) && fold_nick(ctx.server, &player.nick) == folded_arg
            })
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| format!("{}/{}", ctx.server, folded_arg));
        (found, arg.to_string())
    };
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

fn cmd_help(ctx: &Ctx) -> Result<(), Error> {
    if expansion_active(now_secs()) {
        ctx.say("help_void_expansion", &["Fishing: !cast [location] [bait <100-1700 XP>] then wait (1h+, best ~24h, risky after 24h) and !reel. Bait spends 100 XP per virtual rarity hour. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite."], &[])
    } else {
        ctx.say("help", &["Fishing: !cast [location] then wait (1h+, best ~24h, risky after 24h) and !reel. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite."], &[])
    }
}

fn cmd_lure(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let mut rng = ctx.rng(&mut state);
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

fn cmd_water(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let today = today_utc(now_secs());
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.junk_curse_date.as_deref() == Some(today.as_str()) {
        return Ok(()); // already cursed today — stay silent, like the Python original
    }
    player.junk_curse_date = Some(today);
    save_state(&state)?;
    ctx.say_text(
        "water_curse",
        &format!(
            "Cheaters never prosper, {}. I curse you with junk for the remainder of the day.",
            ctx.addr
        ),
    )
}

fn cmd_dynamite(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let mut rng = ctx.rng(&mut state);
    {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
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
                *player.catches.entry(fish.name.clone()).or_insert(0) += 1;
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
                haul.push((fish.name, rarity.to_string(), weight));
            }
        }
        player.xp += grant;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_nick_keys_use_irc_default_casemapping() {
        assert_eq!(fold_nick("net", "Sailor[One]^"), "sailor{one}~");
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
    fn civil_date_round_trip() {
        // 2026-06-26 00:00:00 UTC == 1782432000.
        assert_eq!(unix_from_civil(2026, 6, 26), 1_782_432_000);
        assert_eq!(civil_from_unix(1_782_432_000), (2026, 6, 26));
        // Epoch.
        assert_eq!(civil_from_unix(0), (1970, 1, 1));
        // A leap day survives the round trip.
        let ts = unix_from_civil(2024, 2, 29);
        assert_eq!(civil_from_unix(ts), (2024, 2, 29));
        assert_eq!(today_utc(1_782_432_000 + 3600), "2026-06-26");
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
            level: 5,
            furthest_cast: 10.0,
            total_fish: 1,
            ..Default::default()
        };
        let mut b = Player {
            level: 5,
            furthest_cast: 50.0,
            total_fish: 9,
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
        // Tie on level (both 5) → broken by total_fish → b.
        assert_eq!(traveler.as_deref(), Some("s/b"));
        assert_eq!(caster.as_deref(), Some("s/b"));
        assert_eq!(collector.as_deref(), Some("s/b"));
    }

    #[test]
    fn seasonal_reset_schedules_then_wipes() {
        let mut st = State::default();
        st.players.insert(
            "s/a".into(),
            Player {
                level: 3,
                furthest_cast: 20.0,
                total_fish: 4,
                ..Default::default()
            },
        );
        let jun = unix_from_civil(2026, 6, 26);
        // First sight: schedules the boundary, no reset, players intact.
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, jun);
        assert!(lines.is_empty());
        assert!(state_changed, "the initial reset boundary must be persisted");
        assert!(st.players.contains_key("s/a"));
        assert_eq!(
            st.next_reset.get("s"),
            Some(&unix_from_civil(2026, 7, 1))
        );
        // Ordinary commands before the boundary neither reset nor rewrite the state.
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, jun + 1);
        assert!(lines.is_empty());
        assert!(!state_changed);
        // Jump past the Jul 1 boundary: crowns champions and wipes the server's players.
        let aug = unix_from_civil(2026, 8, 1);
        let (lines, state_changed) = maybe_seasonal_reset("s", &mut st, aug);
        assert!(!lines.is_empty());
        assert!(state_changed);
        assert!(!st.players.contains_key("s/a"));
        let champ = st.champions.get("s").unwrap();
        assert_eq!(champ.traveler.as_deref(), Some("s/a"));
        assert_eq!(champ.season, "Q2 2026");
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
        assert!(!st.players.contains_key("s/a"));
        assert_eq!(st.champions.get("s").unwrap().season, "Q2 2026");
        assert_eq!(
            st.next_reset.get("s"),
            Some(&unix_from_civil(2026, 10, 1))
        );
    }
}
