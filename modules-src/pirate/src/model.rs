//! The persisted state tree. Everything lives in one JSON blob under the module's namespaced KV
//! key `"data"`; every field is `#[serde(default)]` so older blobs keep loading as the schema
//! grows. All persistent identity is the host-stamped stable profile UUID — never a nick.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Hard cap on captains in the serverwide game (the `player_cap` knob is the soft operator cap).
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
/// Cap on concurrent ally sorties against one Navy blockade.
pub const MAX_NAVY_HARASSMENTS: usize = 32;
/// Cap on remembered played rooms (announcement broadcast targets).
pub const MAX_ROOMS: usize = 16;
/// A remembered room that has seen no eligible pirate activity for this long stops receiving
/// announcements.
pub const ROOM_STALE_SECS: i64 = 30 * 86_400;

/// Current schema version of the persisted blob.
pub const SCHEMA_VERSION: u32 = 2;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Game key is `"{server}"`: one serverwide game per network, playable from any enabled room.
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

/// How much "richer" one captain's record is than another, for the serverwide merge: gold first,
/// career voyages as the tiebreaker. Both records describe the same captain from two legacy
/// per-channel games; the stronger isle survives.
fn player_richness(player: &Player) -> (i64, i64) {
    (player.gold, player.career_voyages)
}

/// Fold legacy per-channel game `b` into the serverwide game `a`. Pure and order-tolerant: the
/// more advanced season/navy state wins, collections concatenate with id dedupe, and each
/// captain keeps their stronger of the two isles.
fn merge_game(a: &mut Game, b: &Game) {
    // Season progress: a started season beats an unstarted one, a later index beats an earlier.
    if b.season_started != 0 && (a.season_started == 0 || b.season_index > a.season_index) {
        a.season_started = b.season_started;
        a.season_index = b.season_index;
        a.sea = b.sea.clone();
    }
    for (uuid, player) in &b.players {
        match a.players.get(uuid) {
            None => {
                a.players.insert(uuid.clone(), player.clone());
            }
            Some(kept) => {
                if player_richness(player) > player_richness(kept) {
                    a.players.insert(uuid.clone(), player.clone());
                }
            }
        }
    }
    for voyage in &b.voyages {
        if !a.voyages.iter().any(|kept| kept.id == voyage.id) {
            a.voyages.push(voyage.clone());
        }
    }
    for prisoner in &b.prisoners {
        if !a.prisoners.iter().any(|kept| kept.id == prisoner.id) {
            a.prisoners.push(prisoner.clone());
        }
    }
    for ransom in &b.ransoms {
        if !a.ransoms.iter().any(|kept| kept.id == ransom.id) {
            a.ransoms.push(ransom.clone());
        }
    }
    a.recent_departures
        .extend(b.recent_departures.iter().cloned());
    a.recent_departures.sort_by_key(|departure| departure.at);
    if a.recent_departures.len() > MAX_DEPARTURES {
        let excess = a.recent_departures.len() - MAX_DEPARTURES;
        a.recent_departures.drain(0..excess);
    }
    // Navy pressure is per-serverworld now: the loudest pending blockade and the highest
    // escalation survive the merge.
    if b.navy_pending_hit_at > a.navy_pending_hit_at {
        a.navy_pending_target = b.navy_pending_target.clone();
        a.navy_pending_hit_at = b.navy_pending_hit_at;
    }
    a.navy_escalation = a.navy_escalation.max(b.navy_escalation);
    for sortie in &b.navy_harassments {
        if !a.navy_harassments.iter().any(|kept| kept.id == sortie.id) {
            a.navy_harassments.push(sortie.clone());
        }
    }
}

/// Migrate a legacy per-channel state blob (schema 1: games keyed `"{server}/{channel}"`) into
/// the serverwide layout (schema 2: one game keyed `"{server}"`). Idempotent: returns `None`
/// when there is nothing left to migrate. Otherwise returns the folded legacy
/// `(server, channel)` pairs so the caller can retire legacy scheduler jobs. Pure — no host
/// calls — so the data lifecycle hooks can run it over host-supplied blobs too.
pub(crate) fn migrate_state(state: &mut State, now: i64) -> Option<Vec<(String, String)>> {
    let mut legacy_keys: Vec<String> = state
        .games
        .keys()
        .filter(|key| key.contains('/'))
        .cloned()
        .collect();
    legacy_keys.sort();
    if legacy_keys.is_empty() {
        if state.schema_version == SCHEMA_VERSION {
            return None;
        }
        state.schema_version = SCHEMA_VERSION;
        return Some(Vec::new());
    }
    // Group the legacy keys by server segment, then fold each group into one fresh game whose
    // rooms are the channels the games used to live in.
    let mut servers: Vec<String> = legacy_keys
        .iter()
        .map(|key| key.split('/').next().unwrap_or(key).to_string())
        .collect();
    servers.sort();
    servers.dedup();
    let mut folded: Vec<(String, Vec<String>)> = Vec::new();
    for server in &servers {
        let mut merged = Game::default();
        let mut channels = Vec::new();
        for key in &legacy_keys {
            let (key_server, key_channel) = match key.split_once('/') {
                Some(parts) => parts,
                None => continue,
            };
            if key_server != server {
                continue;
            }
            channels.push(key_channel.to_string());
            if let Some(game) = state.games.get(key) {
                merge_game(&mut merged, game);
            }
        }
        merged.rooms = channels
            .iter()
            .map(|name| KnownRoom {
                name: name.clone(),
                last_seen: now,
            })
            .collect();
        folded.push((server.clone(), channels));
        state.games.insert(server.clone(), merged);
    }
    for key in &legacy_keys {
        state.games.remove(key);
    }
    // Legacy PM sessions point at `"{server}/{channel}"` games; rebind them to the server.
    for session in state.pm_sessions.values_mut() {
        if let Some((server, _)) = session.game.split_once('/') {
            session.game = server.to_string();
        }
    }
    state.schema_version = SCHEMA_VERSION;
    Some(
        folded
            .into_iter()
            .flat_map(|(server, channels)| {
                channels
                    .into_iter()
                    .map(move |channel| (server.clone(), channel))
            })
            .collect(),
    )
}

fn default_sea() -> String {
    crate::season::SEAS[0].0.into()
}

/// A room where the serverwide game is played, learned from eligible pirate commands. Timed
/// announcements broadcast to every remembered room that still passes the enable/blacklist gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownRoom {
    pub name: String,
    pub last_seen: i64,
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
    /// Hidden strength added to the next Navy encounter after a successful repulse.
    #[serde(default)]
    pub navy_escalation: i64,
    /// Timed ally sorties currently harassing the active blockade.
    #[serde(default)]
    pub navy_harassments: Vec<NavyHarassment>,
    /// Rooms where the game is played (announcement broadcast targets), freshest last seen.
    #[serde(default)]
    pub rooms: Vec<KnownRoom>,
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
            navy_escalation: 0,
            navy_harassments: Vec::new(),
            rooms: Vec::new(),
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
    /// When absence mode began; used to pause personal timers until `!unpark`.
    #[serde(default)]
    pub parked_at: i64,
    /// Royal Navy blockade: no launches, half gold income.
    #[serde(default)]
    pub navy_blockade_until: i64,
    /// Hidden strength of the current blockade. Never shown in user-facing state.
    #[serde(default)]
    pub navy_blockade_strength: i64,
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

/// A timed sortie sent by another captain to weaken an active Navy blockade.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NavyHarassment {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub owner_uuid: String,
    #[serde(default)]
    pub target_uuid: String,
    #[serde(default)]
    pub crew_regular: i64,
    #[serde(default)]
    pub crew_loyal: i64,
    #[serde(default)]
    pub started_at: i64,
    #[serde(default)]
    pub returns_at: i64,
    #[serde(default)]
    pub resolved: bool,
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
    /// Expiry set when the scout actually returns; collection must not extend it.
    #[serde(default)]
    pub intel_expires_at: i64,
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
    /// Serverwide game key (`"{server}"`) this session plays in.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A v1-shaped blob mirroring the live split that motivated the serverwide merge: three
    /// per-channel games on one network (krnl enrolled twice), plus one game on another network.
    fn legacy_state() -> State {
        serde_json::from_str(
            r#"{
                "schema_version": 1,
                "games": {
                    "styxnet/#quest": {"players": {
                        "krnl": {"gold": 1024, "nick_cache": "krnl", "crew_regular": 3},
                        "jelly": {"gold": 415, "nick_cache": "jelly"}
                    }, "voyages": [{"id": 3, "owner_uuid": "krnl", "returns_at": 9000}]},
                    "styxnet/#games": {"players": {
                        "krnl": {"gold": 200, "nick_cache": "krnl"},
                        "lando": {"gold": 352, "nick_cache": "Lando-HoloNet"}
                    }},
                    "styxnet/#transience": {"players": {
                        "wite": {"gold": 200, "nick_cache": "witeshark2"}
                    }},
                    "othernet/#elsewhere": {"players": {"zed": {"gold": 5, "nick_cache": "Zed"}}}
                },
                "pm_sessions": {
                    "styxnet/krnl": {"game": "styxnet/#quest", "level": "menu"}
                },
                "next_id": 7
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn migration_folds_legacy_channel_games_into_one_serverwide_game() {
        let mut state = legacy_state();
        let folded = migrate_state(&mut state, 5_000).expect("legacy blob migrates");

        assert_eq!(state.schema_version, SCHEMA_VERSION);
        let mut keys: Vec<_> = state.games.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["othernet", "styxnet"]);

        let styx = &state.games["styxnet"];
        assert_eq!(styx.players.len(), 4, "krnl's two isles merge into one");
        assert_eq!(styx.players["krnl"].gold, 1024, "the richer isle survives");
        assert_eq!(styx.players["krnl"].crew_regular, 3);
        assert_eq!(styx.voyages.len(), 1, "voyages at sea are carried over");
        assert_eq!(state.next_id, 7, "the id counter is untouched");
        assert!(!styx.jobs_ensured, "serverwide jobs must be re-armed");

        let rooms: Vec<_> = styx.rooms.iter().map(|room| room.name.as_str()).collect();
        assert_eq!(
            rooms,
            vec!["#games", "#quest", "#transience"],
            "old rooms become broadcast targets"
        );
        assert!(styx.rooms.iter().all(|room| room.last_seen == 5_000));

        assert_eq!(
            state.pm_sessions["styxnet/krnl"].game, "styxnet",
            "legacy PM sessions rebind to the serverwide key"
        );

        assert!(
            folded.contains(&("styxnet".to_string(), "#quest".to_string())),
            "folded pairs let the caller retire legacy scheduler jobs"
        );
        assert_eq!(folded.len(), 4);
    }

    #[test]
    fn migration_is_idempotent_and_bumps_the_version() {
        let mut state = legacy_state();
        assert!(migrate_state(&mut state, 5_000).is_some());
        assert!(
            migrate_state(&mut state, 9_000).is_none(),
            "a migrated blob has nothing left to fold"
        );
        assert_eq!(
            state.games["styxnet"].rooms[0].last_seen, 5_000,
            "a second pass leaves the folded state untouched"
        );

        // A current-version blob with no legacy keys is a no-op...
        let mut fresh = State::default();
        assert!(migrate_state(&mut fresh, 0).is_none());
        // ...but an old-version blob without games still bumps so the version persists.
        fresh.schema_version = 1;
        assert!(migrate_state(&mut fresh, 0).is_some());
        assert_eq!(fresh.schema_version, SCHEMA_VERSION);
    }
}
