//! The persisted state tree. Everything lives in one JSON blob under the module's namespaced KV
//! key `"data"`; every field is `#[serde(default)]` so older blobs keep loading as the schema
//! grows. All persistent identity is the host-stamped stable profile UUID — never a nick.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Cap on captains per game channel.
pub const MAX_PLAYERS: usize = 32;
/// Cap on the prisoners held across one game.
pub const MAX_PRISONERS: usize = 64;
/// Cap on pending ransom offers across one game.
pub const MAX_RANSOMS: usize = 32;
/// Cap on the recent-departures log shown by `!here`.
pub const MAX_DEPARTURES: usize = 12;
/// Cap on tracked Legends per captain.
pub const MAX_LEGENDS: usize = 24;
/// Cap on concurrent PM menu sessions; expired sessions are pruned first.
pub const MAX_PM_STATES: usize = 256;
/// Cap on cached nick length.
pub const MAX_NICK_CHARS: usize = 32;

/// Current schema version of the persisted blob.
pub const SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Game key is `"{server}/{channel}"`.
    #[serde(default)]
    pub games: HashMap<String, Game>,
    /// PM menu sessions, keyed `"{server}/{uuid}"`.
    #[serde(default)]
    pub pm_sessions: HashMap<String, PmState>,
    /// Monotone id source for voyages, prisoner groups, and ransom offers.
    #[serde(default)]
    pub next_id: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            games: HashMap::new(),
            pm_sessions: HashMap::new(),
            next_id: 0,
        }
    }
}

impl State {
    pub fn alloc_id(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }
}

fn default_sea() -> String {
    crate::season::SEAS[0].0.into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Game {
    /// Current sea id (see [`crate::season::SEAS`]).
    #[serde(default = "default_sea")]
    pub sea: String,
    #[serde(default)]
    pub season_started: i64,
    #[serde(default)]
    pub season_index: u32,
    /// Captain stable UUID -> player.
    #[serde(default)]
    pub players: HashMap<String, Player>,
    /// Active voyages plus resolved voyages awaiting `!collect`. Collected voyages are pruned.
    #[serde(default)]
    pub voyages: Vec<Voyage>,
    #[serde(default)]
    pub prisoners: Vec<Prisoner>,
    #[serde(default)]
    pub ransoms: Vec<Ransom>,
    /// Whether the daily/season/navy scheduler jobs have been lazily created for this game.
    #[serde(default)]
    pub jobs_ensured: bool,
    /// Public voyage departures from the last hours, for `!here`.
    #[serde(default)]
    pub recent_departures: Vec<Departure>,
    /// Captain the navy has announced it will blockade (set by navy_announce, consumed by navy_hit).
    #[serde(default)]
    pub navy_pending_target: Option<String>,
    #[serde(default)]
    pub navy_pending_hit_at: i64,
}

impl Default for Game {
    fn default() -> Self {
        Game {
            sea: default_sea(),
            season_started: 0,
            season_index: 0,
            players: HashMap::new(),
            voyages: Vec::new(),
            prisoners: Vec::new(),
            ransoms: Vec::new(),
            jobs_ensured: false,
            recent_departures: Vec::new(),
            navy_pending_target: None,
            navy_pending_hit_at: 0,
        }
    }
}

fn default_loyalty() -> i64 {
    3
}

fn default_cove() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Buildings {
    #[serde(default)]
    pub vault: u8,
    /// New captains start with Cove L1 (new-player protection), hence the non-zero default.
    #[serde(default = "default_cove")]
    pub cove: u8,
    #[serde(default)]
    pub walls: u8,
    #[serde(default)]
    pub shipyard: u8,
    #[serde(default)]
    pub tavern: u8,
}

impl Default for Buildings {
    fn default() -> Self {
        Self {
            vault: 0,
            cove: default_cove(),
            walls: 0,
            shipyard: 0,
            tavern: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Player {
    /// Display-only cache of the last-seen nick. Never used as an identity key.
    #[serde(default)]
    pub nick_cache: String,
    #[serde(default)]
    pub gold: i64,
    #[serde(default)]
    pub rum: i64,
    #[serde(default)]
    pub crew_regular: i64,
    #[serde(default)]
    pub crew_loyal: i64,
    #[serde(default)]
    pub notoriety: i64,
    /// 3 (high) down to 0 (deserting).
    #[serde(default = "default_loyalty")]
    pub loyalty_tier: i64,
    #[serde(default)]
    pub paid_today: bool,
    #[serde(default)]
    pub unpaid_days: u32,
    #[serde(default)]
    pub buildings: Buildings,
    /// New-player raid immunity.
    #[serde(default)]
    pub shield_until: i64,
    /// Loyal crew hiding in the cove after a lost defense until this timestamp.
    #[serde(default)]
    pub loyal_cove_until: i64,
    /// -10% attack debuff after a crushing defeat.
    #[serde(default)]
    pub humiliated_until: i64,
    /// Explicit absence mode: pauses payday penalties while disabling active gameplay.
    #[serde(default)]
    pub parked: bool,
    /// Royal Navy blockade: no launches, half gold income.
    #[serde(default)]
    pub navy_blockade_until: i64,
    /// Licking wounds: raids that land here put this isle out of the target pool for a while, so
    /// no captain can be picked on day after day.
    #[serde(default)]
    pub raid_mercy_until: i64,
    /// A collected scout report, and the raid it unlocks against that isle until it goes stale.
    #[serde(default)]
    pub raid_intel: Option<RaidIntel>,
    /// One-shot disguise consumed by the next voyage launch.
    #[serde(default)]
    pub false_flag: Option<FalseFlag>,
    #[serde(default)]
    pub false_flag_ready_at: i64,
    #[serde(default)]
    pub legends: Vec<String>,
    #[serde(default)]
    pub seasons_played: u32,
    /// Per-season counters for the season awards; reset at season end.
    #[serde(default)]
    pub season_raids_won: i64,
    #[serde(default)]
    pub season_defenses_won: i64,
    /// Times this captain's isle was breached this season (attacker won).
    #[serde(default)]
    pub season_breaches: i64,
    /// Career counters; never reset. These back the achievement backfill.
    #[serde(default)]
    pub career_voyages: i64,
    #[serde(default)]
    pub career_raids_won: i64,
    #[serde(default)]
    pub career_defenses_won: i64,
    #[serde(default)]
    pub career_gold_plundered: i64,
    #[serde(default)]
    pub career_prisoners_taken: i64,
    #[serde(default)]
    pub career_prisoners_marooned: i64,
    #[serde(default)]
    pub career_rum_collected: i64,
    #[serde(default)]
    pub career_crew_lost: i64,
    #[serde(default)]
    pub created_at: i64,
}

impl Player {
    /// Crew physically on the island right now (voyage crew were deducted at launch).
    pub fn home_regular(&self) -> i64 {
        self.crew_regular.max(0)
    }
    /// Loyal crew available now (not hiding in the cove).
    pub fn home_loyal(&self, now: i64) -> i64 {
        if now < self.loyal_cove_until {
            0
        } else {
            self.crew_loyal.max(0)
        }
    }
    pub fn home_crew(&self, now: i64) -> i64 {
        self.home_regular() + self.home_loyal(now)
    }
    pub fn shielded(&self, now: i64) -> bool {
        now < self.shield_until
    }
    pub fn humiliated(&self, now: i64) -> bool {
        now < self.humiliated_until
    }
    pub fn blockaded(&self, now: i64) -> bool {
        now < self.navy_blockade_until
    }
    /// Recently raided, and so out of every captain's target pool for now.
    pub fn licking_wounds(&self, now: i64) -> bool {
        now < self.raid_mercy_until
    }
    /// The isle this captain currently holds actionable intel on, if the report is still fresh.
    pub fn fresh_intel(&self, now: i64) -> Option<&RaidIntel> {
        self.raid_intel
            .as_ref()
            .filter(|intel| now < intel.expires_at)
    }
}

/// A collected scout report, standing in as a one-shot licence to raid that isle. Earned by
/// chance — the scout target is rolled, never chosen — so a raid traces back to luck, not a grudge.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaidIntel {
    #[serde(default)]
    pub target_uuid: String,
    /// Display-only, for the reminder line; the uuid is the identity.
    #[serde(default)]
    pub target_nick: String,
    #[serde(default)]
    pub expires_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FalseFlag {
    /// The nick the next departure will appear to belong to.
    #[serde(default)]
    pub nick: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoyageKind {
    #[default]
    Merchant,
    Rum,
    Pressgang,
    Smuggler,
    NavyPayroll,
    Explore,
    Raid,
    Scout,
}

impl VoyageKind {
    pub fn is_pvp(self) -> bool {
        matches!(self, VoyageKind::Raid | VoyageKind::Scout)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Voyage {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub owner_uuid: String,
    #[serde(default)]
    pub kind: VoyageKind,
    #[serde(default)]
    pub target_uuid: Option<String>,
    #[serde(default)]
    pub crew_regular: i64,
    #[serde(default)]
    pub crew_loyal: i64,
    /// Public `!raid` declaration.
    #[serde(default)]
    pub is_public: bool,
    /// Nick displayed at departure when sailing under a false flag.
    #[serde(default)]
    pub false_flag_nick: Option<String>,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub returns_at: i64,
    #[serde(default)]
    pub resolved: bool,
    #[serde(default)]
    pub collected: bool,
    #[serde(default)]
    pub result: Option<VoyageResult>,
}

impl Voyage {
    pub fn crew_sent(&self) -> i64 {
        self.crew_regular + self.crew_loyal
    }
}

/// Rewards waiting for `!collect`. Crew returned home at resolution; only the loot waits.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VoyageResult {
    #[serde(default)]
    pub gold: i64,
    #[serde(default)]
    pub rum: i64,
    #[serde(default)]
    pub new_crew: i64,
    #[serde(default)]
    pub crew_lost: i64,
    /// Present for raid voyages: what happened at the target isle.
    #[serde(default)]
    pub raid: Option<RaidResult>,
    /// Present for scout voyages: the private intelligence snapshot.
    #[serde(default)]
    pub scout: Option<ScoutResult>,
    /// True when the target vanished before the voyage could complete normally.
    #[serde(default)]
    pub fizzled: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RaidResult {
    /// "crushing_victory" | "victory" | "defeat" | "crushing_defeat"
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub target_uuid: String,
    #[serde(default)]
    pub target_nick: String,
    #[serde(default)]
    pub prisoners_lost: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScoutResult {
    /// Stable identity of the scouted isle; drives the raid this report unlocks on collection.
    #[serde(default)]
    pub target_uuid: String,
    #[serde(default)]
    pub target_nick: String,
    #[serde(default)]
    pub visible_crew: i64,
    #[serde(default)]
    pub approx_gold: i64,
    #[serde(default)]
    pub buildings: String,
    #[serde(default)]
    pub low_morale: bool,
    #[serde(default)]
    pub leaked: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prisoner {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub holder_uuid: String,
    #[serde(default)]
    pub origin_uuid: String,
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub captured_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Ransom {
    #[serde(default)]
    pub id: u64,
    /// The [`Prisoner`] group this offer covers. An offer is only honoured while that group is
    /// still held; marooning or press-ganging it cancels the offer.
    #[serde(default)]
    pub prisoner_id: u64,
    #[serde(default)]
    pub holder_uuid: String,
    #[serde(default)]
    pub target_uuid: String,
    #[serde(default)]
    pub amount: i64,
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub offered_at: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Departure {
    /// Displayed nick — the false-flag nick when one was flown.
    #[serde(default)]
    pub nick: String,
    #[serde(default)]
    pub crew: i64,
    #[serde(default)]
    pub at: i64,
}

/// PM guided-menu session. `level` names the current prompt; `data` holds its scratch JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PmState {
    /// Game key this session is bound to.
    #[serde(default)]
    pub game: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub data: serde_json::Value,
    #[serde(default)]
    pub last_active: i64,
}

/// Trim a nick for storage/display.
pub fn clean_nick(nick: &str) -> String {
    nick.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NICK_CHARS)
        .collect()
}
