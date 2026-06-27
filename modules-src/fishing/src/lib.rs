//! Fishing mini-game for rustjeeves — a port of jeeves/modules/fishing.py.
//!
//! Phase 1: the core cast/reel loop, locations (Puddle -> The Void), leveling, weighted catches
//! by wait time, junk, line breaks, XP + bonuses, and the read-only displays. Events, artifacts,
//! lures, chum, champions, and the risk toys land in later phases.
//!
//! State lives in one JSON blob in the module's namespaced kv store (`data`). The fish database is
//! the real `fish_database.json`, bundled at compile time.

use extism_pdk::*;
use jeeves_abi::{Event, EventEnvelope, KvGet, KvSet, SendMessage};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::OnceLock;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
}

// ── host helpers ────────────────────────────────────────────────────────────

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage { server: server.into(), target: target.into(), text: text.into() };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

fn now_secs() -> i64 {
    unsafe { now(String::new()) }.ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

fn load_state() -> Result<State, Error> {
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: "data".into() })?)? };
    if raw.is_empty() {
        Ok(State::default())
    } else {
        Ok(serde_json::from_str(&raw).unwrap_or_default())
    }
}

fn save_state(state: &State) -> Result<(), Error> {
    let req = KvSet { key: "data".into(), value: serde_json::to_string(state)? };
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
        let v: serde_json::Value = serde_json::from_str(FISH_DB_JSON).expect("valid fish_database.json");
        let locations: Vec<Location> = serde_json::from_value(v["locations"].clone()).unwrap_or_default();
        let mut fish_by_location = HashMap::new();
        for loc in &locations {
            let fish: Vec<Fish> = serde_json::from_value(v[&loc.name].clone()).unwrap_or_default();
            fish_by_location.insert(loc.name.clone(), fish);
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
            rarity_xp_multiplier: serde_json::from_value(v["rarity_xp_multiplier"].clone()).unwrap_or_default(),
            cast_messages: serde_json::from_value(v["cast_messages"].clone()).unwrap_or_default(),
            too_early_messages: serde_json::from_value(v["too_early_messages"].clone()).unwrap_or_default(),
            danger_zone_messages: serde_json::from_value(v["danger_zone_messages"].clone()).unwrap_or_default(),
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
    #[serde(default)]
    nonce: u64,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cast {
    timestamp: i64,
    distance: f64,
    location: String,
    allow_lower_fish: bool,
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
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    fn choice<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { Some(&items[self.below(items.len())]) }
    }
}

// ── game math (pure, unit-tested) ───────────────────────────────────────────

const MIN_WAIT_HOURS: f64 = 1.0;
const OPTIMAL_WAIT_HOURS: f64 = 24.0;
const DANGER_THRESHOLD_HOURS: f64 = 24.0;
const MAX_LEVEL: i64 = 9;

fn xp_for_level(level: i64) -> i64 {
    (100.0 * ((level + 1) as f64).powf(1.5)) as i64
}

fn location_for_level(level: i64) -> &'static Location {
    let d = data();
    d.locations.iter().rev().find(|l| l.level <= level).unwrap_or(&d.locations[0])
}

fn find_location(query: &str) -> Option<&'static Location> {
    let q = query.trim().to_lowercase();
    let d = data();
    d.locations
        .iter()
        .find(|l| l.name.to_lowercase() == q)
        .or_else(|| d.locations.iter().find(|l| l.name.to_lowercase().contains(&q)))
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
    let level_bonus = (level as f64 / 9.0) * 0.3;
    let base_max = max * (0.7 + level_bonus);
    round1(rng.range(min, base_max))
}

/// Weighted rarity selection adjusted by wait time, an event rare-boost multiplier, and a combined
/// artifact/lure rarity boost (fraction of common weight shifted up to rare/legendary).
fn select_rarity(rng: &mut Rng, wait_hours: f64, event_rare_mult: f64, rarity_boost: f64) -> String {
    let mut weights: Vec<(String, i64)> = data().rarity_weights.clone();
    let set = |w: &mut Vec<(String, i64)>, name: &str, val: i64| {
        if let Some(e) = w.iter_mut().find(|(k, _)| k == name) {
            e.1 = val;
        }
    };
    let get = |w: &[(String, i64)], name: &str| w.iter().find(|(k, _)| k == name).map(|(_, v)| *v).unwrap_or(0);
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

fn select_fish<'a>(rng: &mut Rng, location: &str, rarity: &str, eligible: &[String]) -> Option<&'a Fish> {
    let d = data();
    let pool: Vec<&Fish> = if eligible.is_empty() {
        d.fish_by_location.get(location).map(|v| v.iter().collect()).unwrap_or_default()
    } else {
        eligible.iter().filter_map(|l| d.fish_by_location.get(l)).flat_map(|v| v.iter()).collect()
    };
    let matching: Vec<&Fish> = pool.iter().copied().filter(|f| f.rarity == rarity).collect();
    let chosen = if matching.is_empty() {
        let commons: Vec<&Fish> = pool.iter().copied().filter(|f| f.rarity == "common").collect();
        rng.choice(&commons).copied()
    } else {
        rng.choice(&matching).copied()
    };
    chosen
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
    player.artifact.as_ref().filter(|a| a.bonus_type == kind).map(|a| a.bonus_value).unwrap_or(0.0)
}

/// The active event for `server`, if present, unexpired, and valid for `location`. Clears expired.
fn active_event_for(state: &mut State, server: &str, location: &str, now: i64) -> Option<ActiveEvent> {
    let ev = state.active_events.get(server)?.clone();
    if now >= ev.expires {
        state.active_events.remove(server);
        return None;
    }
    if let Some(def) = data().events.get(&ev.type_id) {
        if let Some(locs) = &def.locations {
            if !locs.iter().any(|l| l == location) {
                return None;
            }
        }
    }
    Some(ev)
}

/// 5% chance to start a random (location-valid) event on cast. Returns an announce string.
fn maybe_trigger_event(rng: &mut Rng, state: &mut State, server: &str, location: &str, now: i64) -> Option<String> {
    if rng.f64() > 0.05 {
        return None;
    }
    let candidates: Vec<(&String, &EventDef)> = data()
        .events
        .iter()
        .filter(|(_, e)| e.locations.as_ref().is_none_or(|l| l.iter().any(|x| x == location)))
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

// ── entry point ─────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else { return Ok(()) };

    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }
    let dest = if msg.is_private { msg.nick.as_str() } else { msg.target.as_str() };
    let nick = msg.nick.as_str();
    let addr = if msg.display.is_empty() { nick } else { msg.display.as_str() };
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();

    let ctx = Ctx { server: &server, dest, nick, addr };
    match cmd {
        "!cast" => cmd_cast(&ctx, arg)?,
        "!reel" => cmd_reel(&ctx)?,
        "!fishinfo" => cmd_fishinfo(&ctx, arg)?,
        "!aquarium" => cmd_aquarium(&ctx)?,
        "!lure" => cmd_lure(&ctx)?,
        "!chum" => cmd_chum(&ctx)?,
        "!discard" => cmd_discard(&ctx)?,
        "!fish" | "!fishing" | "!fishstats" => {
            let sub = arg.split_whitespace().next().unwrap_or("");
            match sub {
                "top" => cmd_top(&ctx)?,
                "location" => cmd_location(&ctx)?,
                "help" => cmd_help(&ctx)?,
                "champions" | "champion" => reply(&server, dest, "No champions yet — they're crowned at the seasonal reset.")?,
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
}

impl Ctx<'_> {
    fn key(&self) -> String {
        format!("{}/{}", self.server, self.nick.to_lowercase())
    }
    fn rng(&self, state: &mut State) -> Rng {
        state.nonce = state.nonce.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let seed = (now_secs() as u64) ^ state.nonce ^ 0xD1B5_4A32_D192_ED03;
        Rng(seed | 1)
    }
    fn say(&self, text: &str) -> Result<(), Error> {
        reply(self.server, self.dest, text)
    }
}

// ── commands: core loop ─────────────────────────────────────────────────────

fn cmd_cast(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();

    if let Some(cast) = state.active_casts.get(&key) {
        let hours = (now_secs() - cast.timestamp) as f64 / 3600.0;
        ctx.say(&format!(
            "{}, you already have a line in the water at {} ({:.1}h). Use !reel to bring it in.",
            ctx.addr, cast.location, hours
        ))?;
        return Ok(());
    }

    let player = state.players.entry(key.clone()).or_default();
    player.nick = ctx.nick.to_string();
    let level = player.level;

    // Pick the location: a named (unlocked) one, or the best for the player's level.
    let (location, named) = if arg.is_empty() {
        (location_for_level(level).clone(), false)
    } else {
        match find_location(arg) {
            Some(loc) if loc.level <= level => (loc.clone(), true),
            Some(loc) => {
                ctx.say(&format!(
                    "{}, you haven't unlocked {} yet — need level {} (you're {}).",
                    ctx.addr, loc.name, loc.level, level
                ))?;
                return Ok(());
            }
            None => {
                let avail: Vec<&str> = data().locations.iter().filter(|l| l.level <= level).map(|l| l.name.as_str()).collect();
                ctx.say(&format!("{}, no such spot. You can fish: {}.", ctx.addr, avail.join(", ")))?;
                return Ok(());
            }
        }
    };

    let mut rng = ctx.rng(&mut state);
    let player = state.players.get_mut(&key).unwrap();
    let mut distance = cast_distance(&mut rng, level, &location);
    let art_dist = artifact_bonus(player, "distance");
    if art_dist > 0.0 {
        distance = round1(distance * (1.0 + art_dist));
    }
    player.total_casts += 1;
    if distance > player.furthest_cast {
        player.furthest_cast = distance;
    }
    let artifact = player.artifact.clone();
    state.active_casts.insert(
        key,
        Cast { timestamp: now_secs(), distance, location: location.name.clone(), allow_lower_fish: !named },
    );

    let cast_msg = match &artifact {
        Some(a) => format!("{}, it sails {}m {}, {}...", a.cast_text, distance, location_prep(&location), a.float_text),
        None => {
            let template = rng.choice(&data().cast_messages).cloned().unwrap_or_else(|| "You cast {distance}m {loc}...".into());
            template.replace("{distance}", &format!("{distance}")).replace("{loc}", &location_prep(&location))
        }
    };
    let announce = maybe_trigger_event(&mut rng, &mut state, ctx.server, &location.name, now_secs());
    save_state(&state)?;
    ctx.say(&format!("{}, {}", ctx.addr, cast_msg))?;
    if let Some(a) = announce {
        ctx.say(&a)?;
    }
    Ok(())
}

fn cmd_reel(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();

    let Some(cast) = state.active_casts.remove(&key) else {
        ctx.say(&format!("{}, you don't have a line out. Use !cast first.", ctx.addr))?;
        return Ok(());
    };
    let now = now_secs();
    let wait_hours = (now - cast.timestamp) as f64 / 3600.0;
    let location_name = cast.location.clone();
    let location = data().locations.iter().find(|l| l.name == location_name).cloned().unwrap_or_else(|| data().locations[0].clone());
    let mut rng = ctx.rng(&mut state);

    // Active event (and its effect) for this network/location.
    let event = active_event_for(&mut state, ctx.server, &location_name, now);
    let effect = event.as_ref().and_then(|e| e.effect.clone());
    let ev_mult = event.as_ref().map(|e| e.multiplier).unwrap_or(1.0);

    // A feeding-frenzy (time_boost) makes the line "wait" effectively longer.
    let effective_wait = if effect.as_deref() == Some("time_boost") { wait_hours / ev_mult } else { wait_hours };

    // Too early — the cast is consumed but the hook is empty.
    if effective_wait < MIN_WAIT_HOURS {
        let m = rng.choice(&data().too_early_messages).cloned().unwrap_or_else(|| "Nothing but an empty hook.".into());
        save_state(&state)?;
        return ctx.say(&format!("{}, {}", ctx.addr, m));
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
                format!("After {:.1}h you reel in... {}. Maybe don't leave your line so long.", wait_hours, junk)
            } else {
                if kind == "line_break" {
                    player.lines_broken += 1;
                }
                data().danger_zone_messages.get(kind).and_then(|v| rng.choice(v)).cloned().unwrap_or_else(|| "It got away.".into())
            };
            save_state(&state)?;
            return ctx.say(&format!("{}, {}", ctx.addr, text));
        }
    }

    // Plain junk — base 10%, boosted by murky-waters events, reduced by a junk-shield artifact.
    let mut junk_chance = 0.10;
    if effect.as_deref() == Some("junk_boost") {
        junk_chance *= ev_mult;
    }
    let shield = state.players.get(&key).map(|p| artifact_bonus(p, "junk_shield")).unwrap_or(0.0);
    junk_chance *= 1.0 - shield;
    if rng.f64() < junk_chance {
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
                return ctx.say(&resp);
            }
        }
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.junk_collected += 1;
        player.xp += 5;
        let junk = junk_item(&mut rng, &location.kind);
        save_state(&state)?;
        return ctx.say(&format!("{} reels in... {}. At least you're cleaning up! (+5 XP)", ctx.addr, junk));
    }

    // A catch. Gather player-derived boosts before mutating.
    let player_level = state.players.get(&key).map(|p| p.level).unwrap_or(0);
    let art_rarity = state.players.get(&key).map(|p| artifact_bonus(p, "rarity")).unwrap_or(0.0);
    let art_xp = state.players.get(&key).map(|p| artifact_bonus(p, "xp")).unwrap_or(0.0);
    let lure = state.players.get(&key).and_then(|p| p.active_lure.clone());
    let eligible: Vec<String> = if cast.allow_lower_fish {
        data().locations.iter().filter(|l| l.level <= player_level).map(|l| l.name.clone()).collect()
    } else {
        Vec::new()
    };
    let lure_rarity = if lure.as_deref() == Some("rarity") { 0.40 } else { 0.0 };
    let event_rare_mult = if effect.as_deref() == Some("rare_boost") { ev_mult } else { 1.0 };
    let rarity = select_rarity(&mut rng, effective_wait, event_rare_mult, art_rarity + lure_rarity);
    let Some(fish) = select_fish(&mut rng, &location_name, &rarity, &eligible) else {
        save_state(&state)?;
        return ctx.say("The fish got away at the last moment!");
    };
    let fish = fish.clone();
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

    // Line-break: bigger fish, bigger risk.
    let break_chance = 0.02 + (weight / 1000.0) * 0.15;
    if rng.f64() < break_chance {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.lines_broken += 1;
        save_state(&state)?;
        return ctx.say(&format!(
            "{}, a massive tug — a {}! But it's too much... SNAP! The line breaks and it's gone.",
            ctx.addr, fish.name
        ));
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
    let rarity_mult = data().rarity_xp_multiplier.get(&rarity).copied().unwrap_or(1);
    let weight_bonus = 1.0 + (weight / 50.0);
    let mut xp = (10.0 * rarity_mult as f64 * weight_bonus) as i64;
    if effect.as_deref() == Some("xp_boost") {
        xp = (xp as f64 * ev_mult) as i64;
    }
    if art_xp > 0.0 {
        xp = (xp as f64 * (1.0 + art_xp)) as i64;
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

    let new_level = check_level_up(player);

    let article = match rarity.as_str() {
        "uncommon" => "an uncommon ".to_string(),
        "rare" => "a RARE ".to_string(),
        "legendary" => "a LEGENDARY ".to_string(),
        _ => "a ".to_string(),
    };
    let mut response = format!(
        "{} reels in {}{} weighing {:.2} lbs after {:.1}h! (+{} XP)",
        ctx.addr, article, fish.name, weight, wait_hours, total_xp
    );
    if !bonus_msgs.is_empty() {
        response.push(' ');
        response.push_str(&bonus_msgs.join(" "));
    }
    if chum_active {
        response.push_str(" (chummed waters!)");
    }
    response.push_str(lure_reveal);
    if let Some(lvl) = new_level {
        response.push_str(&format!(" LEVEL UP! You're now level {lvl} and can fish at {}!", location_for_level(lvl).name));
    }
    save_state(&state)?;
    ctx.say(&response)
}

fn check_level_up(player: &mut Player) -> Option<i64> {
    let start = player.level;
    let mut level = player.level;
    let mut xp = player.xp;
    while level < MAX_LEVEL && xp >= xp_for_level(level) {
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
    let items = d.junk_items.get(location_kind).or_else(|| d.junk_items.get("terrestrial"));
    items.and_then(|v| rng.choice(v)).cloned().unwrap_or_else(|| "an old boot".into())
}

// ── commands: displays ──────────────────────────────────────────────────────

fn cmd_stats(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let state = load_state()?;
    let (lookup_nick, who) = if arg.is_empty() {
        (ctx.nick.to_string(), ctx.addr.to_string())
    } else {
        (arg.to_string(), arg.to_string())
    };
    let key = format!("{}/{}", ctx.server, lookup_nick.to_lowercase());
    let Some(p) = state.players.get(&key) else {
        return ctx.say(&format!("{} hasn't gone fishing yet.", who));
    };
    let loc = location_for_level(p.level);
    let biggest = p
        .biggest_fish_name
        .as_ref()
        .map(|n| format!("{:.2} lbs ({})", p.biggest_fish, n))
        .unwrap_or_else(|| format!("{:.2} lbs", p.biggest_fish));
    ctx.say(&format!(
        "Fishing stats for {}: Level {} ({}) | XP {}/{} | Fish {} | Biggest {} | Casts {} | Junk {}",
        who, p.level, loc.name, p.xp, xp_for_level(p.level), p.total_fish, biggest, p.total_casts, p.junk_collected
    ))
}

fn cmd_top(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let prefix = format!("{}/", ctx.server);
    let mut players: Vec<&Player> = state.players.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, p)| p).collect();
    if players.is_empty() {
        return ctx.say("No one has gone fishing yet!");
    }
    let mut by_fish = players.clone();
    by_fish.retain(|p| p.total_fish > 0);
    by_fish.sort_by_key(|p| std::cmp::Reverse(p.total_fish));
    let most: Vec<String> = by_fish.iter().take(5).enumerate().map(|(i, p)| format!("#{} {} ({})", i + 1, name_of(p), p.total_fish)).collect();

    players.retain(|p| p.biggest_fish > 0.0);
    players.sort_by(|a, b| b.biggest_fish.partial_cmp(&a.biggest_fish).unwrap_or(std::cmp::Ordering::Equal));
    let big: Vec<String> = players.iter().take(5).enumerate().map(|(i, p)| {
        format!("#{} {} ({:.1} lbs {})", i + 1, name_of(p), p.biggest_fish, p.biggest_fish_name.clone().unwrap_or_default())
    }).collect();

    let mut out = String::from("Fishing Leaderboards:");
    if !most.is_empty() {
        out.push_str(&format!(" Most Fish: {}", most.join(", ")));
    }
    if !big.is_empty() {
        out.push_str(&format!(" | Biggest: {}", big.join(", ")));
    }
    ctx.say(&out)
}

fn name_of(p: &Player) -> String {
    if p.nick.is_empty() { "Unknown".into() } else { p.nick.clone() }
}

fn cmd_location(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let level = state.players.get(&ctx.key()).map(|p| p.level).unwrap_or(0);
    let loc = location_for_level(level);
    let next = data().locations.iter().find(|l| l.level == level + 1);
    let next_txt = match next {
        Some(n) => format!(" Next: {} at level {}.", n.name, n.level),
        None => " You've reached the final frontier.".into(),
    };
    ctx.say(&format!("{}, you're level {} fishing at {}.{}", ctx.addr, level, loc.name, next_txt))
}

fn cmd_fishinfo(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        let names: Vec<&str> = data().locations.iter().map(|l| l.name.as_str()).collect();
        return ctx.say(&format!("Locations: {}. Try !fishinfo <location>.", names.join(", ")));
    }
    let Some(loc) = find_location(arg) else {
        return ctx.say(&format!("{}, no such location.", ctx.addr));
    };
    let fish = data().fish_by_location.get(&loc.name).cloned().unwrap_or_default();
    let names: Vec<String> = fish.iter().take(12).map(|f| format!("{} ({})", f.name, f.rarity)).collect();
    ctx.say(&format!("{} (level {}): {}", loc.name, loc.level, names.join(", ")))
}

fn cmd_aquarium(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let Some(p) = state.players.get(&ctx.key()) else {
        return ctx.say(&format!("{}, your aquarium is empty — go fish!", ctx.addr));
    };
    if p.rare_catches.is_empty() {
        return ctx.say(&format!("{}, no rare or legendary catches yet.", ctx.addr));
    }
    let mut recent = p.rare_catches.clone();
    recent.reverse();
    let items: Vec<String> = recent.iter().take(6).map(|c| format!("{} {} ({:.1} lbs)", c.rarity, c.name, c.weight)).collect();
    ctx.say(&format!("{}'s aquarium ({} total): {}", ctx.addr, p.rare_catches.len(), items.join(", ")))
}

fn cmd_help(ctx: &Ctx) -> Result<(), Error> {
    ctx.say("Fishing: !cast [location] then wait (1h+, best ~24h, risky after 24h) and !reel. Also !fishing [nick]/top/location, !fishinfo [loc], !aquarium, !lure (30xp), !chum (250xp), !discard.")
}

fn cmd_lure(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let mut rng = ctx.rng(&mut state);
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.active_lure.is_some() {
        return ctx.say(&format!("{}, you already have a lure rigged up!", ctx.addr));
    }
    if player.xp < 30 {
        return ctx.say(&format!("{}, not enough XP (need 30, have {}).", ctx.addr, player.xp));
    }
    player.xp -= 30;
    player.active_lure = Some(if rng.below(2) == 0 { "rarity".into() } else { "size".into() });
    save_state(&state)?;
    ctx.say(&format!("{} spends 30 XP and rigs up a mystery lure. Let's see what it attracts!", ctx.addr))
}

fn cmd_chum(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    if let Some(c) = state.chum.get(ctx.server) {
        if now < c.expires {
            let mins = (c.expires - now) / 60 + 1;
            return ctx.say(&format!("{}, the water is already chummed! {} minute(s) left.", ctx.addr, mins));
        }
        if now < c.cooldown_until {
            let mins = (c.cooldown_until - now) / 60 + 1;
            return ctx.say(&format!("{}, the chum is on cooldown. {} minute(s) until it can be used again.", ctx.addr, mins));
        }
    }
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.xp < 250 {
        return ctx.say(&format!("{}, not enough XP (need 250, have {}).", ctx.addr, player.xp));
    }
    player.xp -= 250;
    state.chum.insert(
        ctx.server.to_string(),
        Chum { expires: now + 20 * 60, cooldown_until: now + 50 * 60, by_name: ctx.nick.to_string() },
    );
    save_state(&state)?;
    ctx.say(&format!("{} tosses a handful of chum into the water! Fish should run large for the next 20 minutes!", ctx.addr))
}

fn cmd_discard(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    match player.artifact.take() {
        Some(a) => {
            save_state(&state)?;
            ctx.say(&format!("{} tosses the {} into the water. All bonuses lost — casts return to normal.", ctx.addr, a.name))
        }
        None => ctx.say(&format!("{}, you don't have an artifact to discard.", ctx.addr)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xp_curve() {
        assert_eq!(xp_for_level(0), 100);
        assert!(xp_for_level(1) > xp_for_level(0));
        assert!(xp_for_level(8) > xp_for_level(4));
    }

    #[test]
    fn leveling_consumes_xp() {
        let mut p = Player { xp: 100, ..Default::default() };
        assert_eq!(check_level_up(&mut p), Some(1));
        assert_eq!(p.level, 1);
        assert_eq!(p.xp, 0);
        // Not enough for the next level.
        assert_eq!(check_level_up(&mut p), None);
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
        assert!(seen_rare, "expected a rare/legendary at 20h over many rolls");
    }

    #[test]
    fn weight_stays_in_range_and_scales() {
        let mut rng = Rng(42);
        let fish = Fish { name: "Test".into(), min_weight: 2.0, max_weight: 10.0, rarity: "common".into() };
        for _ in 0..200 {
            let w = calc_weight(&mut rng, &fish, 24.0);
            assert!((2.0..=10.0).contains(&w), "w={w}");
        }
        // Long waits trend heavier than very short ones (averaged).
        let avg = |hours: f64| {
            let mut r = Rng(7);
            let mut s = 0.0;
            for _ in 0..500 { s += calc_weight(&mut r, &fish, hours); }
            s / 500.0
        };
        assert!(avg(24.0) > avg(1.0));
    }

    #[test]
    fn database_loads() {
        let d = data();
        assert_eq!(d.locations.len(), 10);
        assert_eq!(d.locations[0].name, "Puddle");
        assert!(d.fish_by_location.get("The Void").map(|v| !v.is_empty()).unwrap_or(false));
        assert!(!d.cast_messages.is_empty());
    }
}
