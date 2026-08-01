//! Persisted game state: everything that round-trips through the host KV store.
//!
//! [`State`] is the single serialized root. Every field carries `#[serde(default)]` so that saves
//! written by older builds keep deserializing — treat that as a hard requirement when adding
//! fields, and never remove or repurpose one without a migration.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{catalog::Artifact, danger};

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub(super) players: HashMap<String, Player>,
    #[serde(default)]
    pub(super) active_casts: HashMap<String, Cast>,
    /// Active random event per server label.
    #[serde(default)]
    pub(super) active_events: HashMap<String, ActiveEvent>,
    /// Chum state per server label.
    #[serde(default)]
    pub(super) chum: HashMap<String, Chum>,
    /// Crowned champions per server label (set at each seasonal reset).
    #[serde(default)]
    pub(super) champions: HashMap<String, Champions>,
    /// Next quarterly reset boundary (unix seconds) per server label. 0/missing means "not yet
    /// scheduled" — the first command for a server sets it without resetting.
    #[serde(default)]
    pub(super) next_reset: HashMap<String, i64>,
    /// Inactive parallel universes ("expeditions") per identity key. The *active* universe always
    /// lives in `players`, so every existing gameplay path is untouched; these are the frozen
    /// worlds a player can `!fish jump` back to. Sealed: nothing transfers between them.
    #[serde(default)]
    pub(super) stash: HashMap<String, Vec<Player>>,
    /// Permanent "Deep Star" count per identity key: how many universes the player has taken to
    /// the level cap. Purely cosmetic bragging rights; never resets.
    #[serde(default)]
    pub(super) prestige: HashMap<String, i64>,
    #[serde(default)]
    pub(super) nonce: u64,
}

/// The three seasonal champions for a server, with a snapshot of their winning stats.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct Champions {
    pub(super) season: String,
    #[serde(default)]
    pub(super) traveler: Option<String>,
    #[serde(default)]
    pub(super) caster: Option<String>,
    #[serde(default)]
    pub(super) collector: Option<String>,
    #[serde(default)]
    pub(super) traveler_name: String,
    #[serde(default)]
    pub(super) caster_name: String,
    #[serde(default)]
    pub(super) collector_name: String,
    #[serde(default)]
    pub(super) traveler_level: i64,
    #[serde(default)]
    pub(super) traveler_location: String,
    #[serde(default)]
    pub(super) traveler_xp: i64,
    #[serde(default)]
    pub(super) caster_distance: f64,
    #[serde(default)]
    pub(super) collector_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct ActiveEvent {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) effect: Option<String>,
    pub(super) multiplier: f64,
    pub(super) expires: i64,
    /// Which event definition this is (for the `locations` restriction).
    pub(super) type_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Chum {
    pub(super) expires: i64,
    pub(super) cooldown_until: i64,
    /// Per-player one-warning markers for the shared chum state.
    #[serde(default)]
    pub(super) cooldown_notices: HashMap<String, i64>,
    #[serde(default)]
    pub(super) by_id: String,
    pub(super) by_name: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct Player {
    #[serde(default)]
    pub(super) nick: String,
    #[serde(default)]
    pub(super) level: i64,
    #[serde(default)]
    pub(super) xp: i64,
    #[serde(default)]
    pub(super) total_fish: i64,
    #[serde(default)]
    pub(super) biggest_fish: f64,
    #[serde(default)]
    pub(super) biggest_fish_name: Option<String>,
    #[serde(default)]
    pub(super) total_casts: i64,
    #[serde(default)]
    pub(super) furthest_cast: f64,
    #[serde(default)]
    pub(super) lines_broken: i64,
    #[serde(default)]
    pub(super) junk_collected: i64,
    #[serde(default)]
    pub(super) catches: HashMap<String, i64>,
    /// Location-qualified species careers. Legacy name-only catch counts are migrated lazily.
    #[serde(default)]
    pub(super) species_careers: HashMap<String, SpeciesCareer>,
    #[serde(default)]
    pub(super) species_careers_migrated: bool,
    #[serde(default)]
    pub(super) rare_catches: Vec<RareCatch>,
    #[serde(default)]
    pub(super) locations_fished: Vec<String>,
    #[serde(default)]
    pub(super) xp_boost_catches: i64,
    #[serde(default)]
    pub(super) artifact: Option<Artifact>,
    /// Rigged lure type ("rarity" or "size"), consumed on the next successful catch.
    #[serde(default)]
    pub(super) active_lure: Option<String>,
    /// Set by `!fish bless`: forces the next catch to be rare/legendary.
    #[serde(default)]
    pub(super) force_rare_legendary: bool,
    /// `!dynamite` damage: 0, 1, or 2 hands lost.
    #[serde(default)]
    pub(super) dynamite_hands_lost: i64,
    /// `!dynamite` ban: unix seconds until fishing is allowed again.
    #[serde(default)]
    pub(super) dynamite_banned_until: Option<i64>,
    /// Unix seconds when hands lost to `!dynamite` grow back.
    #[serde(default)]
    pub(super) dynamite_hands_regrow_at: Option<i64>,
    /// Opt-in DANGER MODE state. The ordinary fishing engine remains authoritative.
    #[serde(default)]
    pub(super) danger: danger::DangerState,
    /// Reinforced-rod strength (0–50). Unlocked at level 15 via `!fix`. Each point reduces break
    /// chance by 1%, floored at 50% of natural risk. Decays 1 per 10 big-fish (>2000 lb) catches.
    #[serde(default)]
    pub(super) rod_strength: u8,
    /// Pending committed `!fix` hours not yet folded into `rod_strength`. Cleared by `settle_rod`.
    #[serde(default)]
    pub(super) fixing_hours: Option<u8>,
    /// Unix seconds until an in-progress `!fix` completes. `None` = not fixing. While in the
    /// future, `!cast` is refused (the rod is in the workshop); once elapsed, the pending
    /// `fixing_hours` are granted on next read.
    #[serde(default)]
    pub(super) fixing_until: Option<i64>,
    /// Counter of big-fish (>2000 lb) catches since last rod decay; resets at `ROD_DECAY_EVERY`.
    #[serde(default)]
    pub(super) big_catch_counter: u8,
    /// Operator-granted cosmetic catch pack. It never changes fishing mechanics.
    #[serde(default)]
    pub(super) dlc_enabled: bool,
    /// Current-quarter counters. `None` identifies a pre-seasonal-stats save and is migrated from
    /// the lifetime fields on first use, which keeps restored backups backward-compatible.
    #[serde(default)]
    pub(super) season_stats: Option<SeasonStats>,
    /// Which parallel universe this save is. 0 = Prime (the original world; also the default for
    /// every pre-expedition save). 1.. = expeditions.
    #[serde(default)]
    pub(super) universe_index: i64,
    /// Display name of this universe ("" ⇒ Prime). Set when an expedition world is opened.
    #[serde(default)]
    pub(super) universe_name: String,
    /// Cosmetic adjective prefixed onto fish names in this universe ("" ⇒ none, i.e. Prime).
    #[serde(default)]
    pub(super) universe_theme: String,
    /// True once this universe has reached the level cap and awarded its Deep Star, so a maxed
    /// world is never double-counted.
    #[serde(default)]
    pub(super) starred: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct SeasonStats {
    #[serde(default)]
    pub(super) xp_earned: i64,
    #[serde(default)]
    pub(super) fish_caught: i64,
    #[serde(default)]
    pub(super) unique_species: HashSet<String>,
    #[serde(default)]
    pub(super) rare_catches: i64,
    #[serde(default)]
    pub(super) heaviest_catch: f64,
    #[serde(default)]
    pub(super) furthest_cast: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Cast {
    pub(super) timestamp: i64,
    pub(super) distance: f64,
    pub(super) location: String,
    pub(super) allow_lower_fish: bool,
    /// XP-funded virtual hours used for rarity gates only. Added in the Q3 2026 expansion.
    #[serde(default)]
    pub(super) bait_hours: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct RareCatch {
    pub(super) name: String,
    pub(super) weight: f64,
    pub(super) rarity: String,
    pub(super) location: String,
    pub(super) caught_at: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct SpeciesCareer {
    pub(super) name: String,
    pub(super) location: String,
    pub(super) catches: i64,
    /// Best landed weight, including lure and chum multipliers.
    pub(super) best_weight: f64,
    /// Natural quality of the catch which set `best_weight`.
    pub(super) best_record_quality: f64,
    /// Best natural specimen quality, measured before external size multipliers.
    pub(super) best_quality: f64,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct CatchMilestones {
    pub(super) previous_mastery: Option<&'static str>,
    pub(super) mastery: Option<&'static str>,
    pub(super) previous_record: f64,
    pub(super) new_record: bool,
    pub(super) trophy: bool,
}
