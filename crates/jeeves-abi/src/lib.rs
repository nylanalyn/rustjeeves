//! Shared ABI types crossing the host <-> guest (WASM module) boundary.
//!
//! Everything is exchanged as JSON. The host serializes [`Event`] and passes it to a module's
//! `on_message` / `on_event` export; modules call host functions with the request structs below.
//! This crate is the single source of truth for that contract — both `jeeves` (host) and every
//! module depend on it.

use serde::{Deserialize, Serialize};

/// An event plus the network it came from. This is the actual JSON payload passed to a module's
/// `on_message` / `on_event` export.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Label of the server/network this event originated from.
    pub server: String,
    pub event: Event,
}

/// Current version of the optional command metadata export.
pub const COMMAND_MANIFEST_VERSION: u32 = 1;

/// Current version of the optional module-settings metadata export.
pub const SETTINGS_MANIFEST_VERSION: u32 = 1;

/// Current version of the achievement metadata and award/query protocol.
pub const ACHIEVEMENT_MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementManifest {
    pub version: u32,
    /// Increment-only counters owned by this module.
    pub stats: Vec<AchievementStat>,
    /// Finite collectible achievements. IDs and stat names are module-local.
    pub achievements: Vec<AchievementSpec>,
    #[serde(default)]
    pub prestige: Vec<PrestigeSpec>,
    /// Increment when the module's backfill interpretation changes.
    #[serde(default)]
    pub catalog_version: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementStat {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    pub stat: String,
    pub threshold: u64,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrestigeSpec {
    pub id: String,
    pub name: String,
    pub stat: String,
    pub first_threshold: u64,
    pub every: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StatIncrement {
    pub stat: String,
    pub amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AwardStatsRequest {
    pub server: String,
    pub profile_id: String,
    pub display_name: String,
    pub target: String,
    pub increments: Vec<StatIncrement>,
    /// Stable event identity for retry-safe awards.
    #[serde(default)]
    pub deduplication_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AwardStatsResponse {
    pub unlocked: Vec<AchievementUnlock>,
    pub prestige: Vec<PrestigeRank>,
    #[serde(default)]
    pub duplicate: bool,
}

/// A request to read the host-owned spendable balance for one stable profile.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EconomyBalanceRequest {
    pub server: String,
    pub profile_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EconomyBalanceResponse {
    pub balance: u64,
}

/// An idempotent host-owned currency transaction. `amount` is always positive; the host function
/// determines whether this request awards or spends it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EconomyTransactionRequest {
    pub server: String,
    pub profile_id: String,
    pub amount: u64,
    pub event_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EconomyTransactionResponse {
    pub balance: u64,
    pub applied: bool,
    pub duplicate: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementUnlock {
    pub module: String,
    pub id: String,
    pub name: String,
    pub unlocked_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrestigeRank {
    pub module: String,
    pub id: String,
    pub name: String,
    pub rank: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementBackfillRequest {
    pub server: String,
    pub entries: Vec<ModuleKvEntry>,
    pub previous_version: u32,
    pub catalog_version: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementBackfillResponse {
    pub values: Vec<AchievementSetMax>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementSetMax {
    pub profile_id: String,
    pub stat: String,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum AchievementsGetRequest {
    Profile {
        server: String,
        profile_id: String,
    },
    Catalog {
        server: String,
        profile_id: Option<String>,
        module: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementProfileSummary {
    pub earned: u64,
    pub available: u64,
    pub recent: Vec<AchievementUnlock>,
    pub closest: Vec<AchievementProgress>,
    pub modules: Vec<AchievementModuleProgress>,
}

/// Host-owned achievement data included in privacy exports.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AchievementDataExport {
    pub stats: Vec<AchievementStatValue>,
    pub unlocks: Vec<AchievementUnlock>,
    pub prestige: Vec<PrestigeRank>,
    pub backfills: Vec<AchievementBackfillMarker>,
    pub deduplication: Vec<AchievementDedupExport>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementDedupExport {
    pub module: String,
    pub event_id: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementStatValue {
    pub module: String,
    pub stat: String,
    pub value: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementBackfillMarker {
    pub module: String,
    pub catalog_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementProgress {
    pub module: String,
    pub id: String,
    pub name: String,
    pub current: u64,
    pub threshold: u64,
    pub earned: bool,
    pub secret: bool,
    pub optional: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AchievementModuleProgress {
    pub module: String,
    pub earned: u64,
    pub available: u64,
    pub achievements: Vec<AchievementProgress>,
    pub prestige: Vec<PrestigeRank>,
}

/// Metadata returned by a module's optional `commands` export.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandManifest {
    pub version: u32,
    pub commands: Vec<CommandSpec>,
}

/// One command owned by a WASM module. Names and aliases omit the leading `!`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub usage: String,
    /// Top-level shortcuts into this command's subcommands, e.g. `!yes` → `!fish yes`. They are
    /// default aliases with an expansion: operators keep or remove them in the alias editor, and
    /// removing one frees the name for other modules.
    #[serde(default)]
    pub shortcuts: Vec<CommandShortcut>,
}

/// A top-level name that the host rewrites to `!{command} {expands_to}` before dispatch.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandShortcut {
    /// Name without the leading `!`.
    pub name: String,
    /// Words inserted after the canonical command, e.g. `"danger yes"`.
    pub expands_to: String,
    /// Optional help text for the subcommand, shown by `!help <shortcut>`.
    #[serde(default)]
    pub description: String,
    /// Optional usage in shortcut form, e.g. `"!raid <crew>"`.
    #[serde(default)]
    pub usage: String,
}

impl CommandShortcut {
    pub fn new(name: &str, expands_to: &str) -> Self {
        Self {
            name: name.into(),
            expands_to: expands_to.into(),
            ..Default::default()
        }
    }

    /// Attach help text: what the subcommand does and how to call it.
    pub fn described(mut self, description: &str, usage: &str) -> Self {
        self.description = description.into();
        self.usage = usage.into();
        self
    }
}

/// One command entry as returned by the `commands_list` host function. Reflects the effective
/// aliases (after operator overrides), not the module's built-in defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub module: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    /// Plain aliases: another name for the command itself.
    pub aliases: Vec<String>,
    /// Effective shortcuts into subcommands (a subset of the module's defaults).
    #[serde(default)]
    pub shortcuts: Vec<CommandShortcut>,
}

/// Metadata returned by a module's optional `settings` export.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingsManifest {
    pub version: u32,
    pub settings: Vec<SettingSpec>,
}

/// A scope at which an operator may override a module setting.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    Global,
    Network,
    Channel,
}

/// Supported setting types. Values cross the host boundary as their textual representation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettingKind {
    Boolean,
    Integer { min: i64, max: i64 },
    DurationSeconds { min: i64, max: i64 },
    String { max_len: usize },
    Choice { options: Vec<String> },
}

/// One operator-configurable setting owned by a module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SettingSpec {
    pub key: String,
    #[serde(default)]
    pub description: String,
    pub default: String,
    pub kind: SettingKind,
    #[serde(default = "default_setting_scopes")]
    pub scopes: Vec<SettingScope>,
    /// Whether the module observes a saved override without being reloaded.
    #[serde(default = "setting_applies_immediately")]
    pub applies_immediately: bool,
}

fn default_setting_scopes() -> Vec<SettingScope> {
    vec![SettingScope::Global]
}

fn setting_applies_immediately() -> bool {
    true
}

/// An event delivered from the host to a module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Successfully connected and registered with the server.
    Connected,
    /// Disconnected from the server.
    Disconnected,
    /// The bot joined `channel`.
    Joined { channel: String },
    /// The bot parted `channel`.
    Parted { channel: String },
    /// A user changed nickname. The host uses this to keep stable profile aliases current.
    NickChanged {
        old_nick: String,
        new_nick: String,
        #[serde(default)]
        account: Option<String>,
    },
    /// A durable scheduled job delivered only to its owning module.
    Timer {
        id: String,
        channel: String,
        due_at: i64,
        payload: String,
    },
    /// A PRIVMSG addressed to a channel or directly to the bot.
    Message(MessagePayload),
    /// Any other raw IRC command the host chose to forward.
    Raw { command: String, args: Vec<String> },
}

/// A channel or private message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePayload {
    /// Stable host-assigned profile UUID. Empty only when profile resolution failed.
    #[serde(default)]
    pub user_id: String,
    /// Nick of the sender (best-effort; empty if unknown). This is the stable identity (profile
    /// key, what clients highlight on) — use it for lookups, not for addressing.
    pub nick: String,
    /// How to address the sender in posted text: their title + nick if a title is set (e.g.
    /// "sir aureate"), otherwise just the nick. Set by the host. Modules should use this for the
    /// `{user}` placeholder.
    #[serde(default)]
    pub display: String,
    /// Username (ident) of the sender, if known.
    #[serde(default)]
    pub user: String,
    /// Hostname of the sender, if known.
    #[serde(default)]
    pub host: String,
    /// Where the message was sent — a channel (`#foo`) or the bot's nick for a PM.
    pub target: String,
    /// The message text.
    pub text: String,
    /// True if this was a private message to the bot rather than a channel message.
    pub is_private: bool,
    /// IRCv3 message tags, if any.
    #[serde(default)]
    pub tags: Vec<(String, Option<String>)>,
    /// The sender's resolved permission role on this network, if any. Set by the host's permission
    /// resolver before dispatch; modules enforce access by checking this.
    #[serde(default)]
    pub role: Option<Role>,
    /// A respectful form of address for the `{honorific}` placeholder, derived from the sender's
    /// saved pronouns: "sir" (he), "madam" (she), otherwise their display name, so nobody is
    /// misgendered. Set by the host; empty only from an older host.
    #[serde(default)]
    pub honorific: String,
}

/// Permission roles. `SuperAdmin` implies all `Admin` rights.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Admin,
    SuperAdmin,
}

impl Role {
    /// Whether this role satisfies a required role (super-admin satisfies admin).
    pub fn satisfies(self, required: Role) -> bool {
        matches!(
            (self, required),
            (Role::SuperAdmin, _) | (Role::Admin, Role::Admin)
        )
    }
}

// ---- Host function request payloads (guest -> host) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessage {
    /// Network label to send on.
    pub server: String,
    pub target: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendNotice {
    /// Network label to send on.
    pub server: String,
    pub target: String,
    pub text: String,
}

/// A narrowly-scoped channel moderation request. The host validates every field and exposes only
/// these operations to modules with the `channel_operator` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelOperator {
    /// Network label to act on.
    pub server: String,
    pub channel: String,
    pub action: ChannelOperatorAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ChannelOperatorAction {
    /// Add or remove a channel user/list mode (`b`, `o`, `h`, or `v`).
    Mode {
        mode: ChannelOperatorMode,
        adding: bool,
        target: String,
    },
    Kick {
        nick: String,
        reason: String,
    },
    Topic {
        topic: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelOperatorMode {
    Ban,
    Op,
    Halfop,
    Voice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    /// Network label to act on.
    pub server: String,
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerQuery {
    pub server: String,
}

/// Read recent lines from the host's in-memory channel buffer (`recent_lines` capability). The
/// buffer is volatile, bounded, and at most an hour deep.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RecentLinesRequest {
    pub server: String,
    pub channel: String,
    /// Newest lines to return (the host caps this).
    pub limit: usize,
    /// Ignore lines older than this many seconds.
    pub max_age_seconds: i64,
    /// Only this profile's lines.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Skip lines that were recognised bot commands.
    #[serde(default)]
    pub exclude_commands: bool,
}

/// One buffered channel line, oldest first in a response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentLine {
    pub user_id: String,
    pub nick: String,
    pub display: String,
    pub text: String,
    pub timestamp: i64,
    /// True when the line resolved to a registered bot command.
    pub is_command: bool,
}

/// Fold an IRC identifier using the network's negotiated `005 CASEMAPPING` value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IrcCasefold {
    pub server: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvGet {
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KvList {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvSet {
    pub key: String,
    pub value: String,
}

/// Read the calling module's effective setting for a network/channel context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettingGet {
    pub key: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
}

/// Create or replace a durable job owned by the calling module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleSet {
    pub id: String,
    pub server: String,
    pub channel: String,
    #[serde(default)]
    pub owner_profile_id: Option<String>,
    pub due_at: i64,
    pub payload: String,
}

/// Cancel one durable job owned by the calling module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleCancel {
    pub id: String,
}

/// List the calling module's pending jobs, optionally limited to one network/channel.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleList {
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
}

/// A persisted durable job. The host supplies `module`; guests only see their own jobs.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduledJob {
    pub module: String,
    pub id: String,
    pub server: String,
    pub channel: String,
    /// Stable profile UUID for user-owned jobs. Channel/system jobs leave this unset.
    #[serde(default)]
    pub owner_profile_id: Option<String>,
    pub due_at: i64,
    pub payload: String,
    pub created_at: i64,
}

pub const DATA_EXPORT_VERSION: u32 = 1;
pub const DATA_LIFECYCLE_VERSION: u32 = 1;

/// Subject used by lifecycle exports and, later, idempotent deletion hooks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DataSubject {
    pub server: String,
    pub profile_id: String,
}

/// Versioned data returned by one module for a profile. Stage 1 reserves this section; bundled
/// module hooks are added alongside the user-facing lifecycle controls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDataExport {
    pub module: String,
    pub data: serde_json::Value,
}

/// One opaque KV entry supplied to its owning module for lifecycle processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleKvEntry {
    pub key: String,
    pub value: String,
}

/// Input to a module's pure `data_export` and `data_delete` hooks. The host supplies only that
/// module's namespaced KV entries; aliases allow cleanup of pre-UUID legacy records.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDataRequest {
    pub version: u32,
    pub subject: DataSubject,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub entries: Vec<ModuleKvEntry>,
}

/// Response from `data_export`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDataResponse {
    pub version: u32,
    pub data: serde_json::Value,
}

/// A deletion hook may remove an entry or replace it with a rewritten aggregate. The host rejects
/// mutations for keys not present in the request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleKvMutation {
    pub key: String,
    pub value: Option<String>,
}

/// Idempotent mutation plan returned by `data_delete`; the host applies it transactionally.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleDataDeletePlan {
    pub version: u32,
    #[serde(default)]
    pub mutations: Vec<ModuleKvMutation>,
}

/// Nick alias attached to a stable profile UUID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProfileAliasExport {
    pub nick: String,
    pub last_seen: i64,
}

/// Operator-readable JSON export assembled by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileDataExport {
    pub version: u32,
    pub exported_at: i64,
    pub subject: DataSubject,
    pub profile: Profile,
    pub aliases: Vec<ProfileAliasExport>,
    pub accounts: Vec<String>,
    pub scheduled_jobs: Vec<ScheduledJob>,
    #[serde(default)]
    pub achievements: AchievementDataExport,
    #[serde(default)]
    pub modules: Vec<ModuleDataExport>,
}

/// Log severity. Maps to the TUI/stdout log levels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    Error,
    Info,
    Debug,
}

/// Log category used for filtering in the TUI logs screen.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Category {
    Error,
    Debug,
    Message,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogReq {
    pub level: Level,
    pub category: Category,
    pub message: String,
}

// ---- User profiles (host-level service, shared across modules) ----

/// Identifies a person on a network. Nick matching is case-insensitive.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileKey {
    pub server: String,
    pub nick: String,
}

/// A user's stored profile. Returned by the `profile_get` host function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Profile {
    /// Stable UUID used across nick changes on this network.
    #[serde(default)]
    pub id: String,
    pub server: String,
    pub nick: String,
    /// Unix seconds of first contact.
    pub created: i64,
    /// Unix seconds of most recent message.
    pub last_seen: i64,
    pub title: Option<String>,
    /// Normalized birthday: `MM-DD` or `MM-DD-YYYY`.
    pub birthday: Option<String>,
    pub pronoun_subject: Option<String>,
    pub pronoun_object: Option<String>,
    pub pronoun_possessive: Option<String>,
    /// The location text the user typed (always shown to channels).
    pub location_display: Option<String>,
    /// The geocoder's canonical label, kept for reference/disambiguation.
    pub location_label: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    /// IANA timezone returned by the geocoder, e.g. `America/New_York`.
    pub timezone: Option<String>,
    /// When `Some(true)`, the user has opted out of achievements: the host drops every
    /// `award_stats` call for them and their progress is wiped. `None`/`Some(false)` = opted in.
    #[serde(default)]
    pub achievements_opt_out: Option<bool>,
    /// Whether this profile has explicitly opted into the public achievement gallery.
    #[serde(default)]
    pub achievements_public: Option<bool>,
}

/// Partial update to a profile. Only `Some` fields are written (merged). Passed to `profile_set`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileUpdate {
    pub server: String,
    pub nick: String,
    pub title: Option<String>,
    pub birthday: Option<String>,
    pub pronoun_subject: Option<String>,
    pub pronoun_object: Option<String>,
    pub pronoun_possessive: Option<String>,
    pub location_display: Option<String>,
    pub location_label: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub timezone: Option<String>,
}

/// Atomically toggle a profile's achievements opt-out flag (`achievement_optout` host function).
/// When `opt_out` is true the host sets the flag and deletes all of the user's achievement rows in
/// one transaction. Modules cannot wipe those host-owned tables directly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementOptOutRequest {
    pub server: String,
    pub profile_id: String,
    pub opt_out: bool,
}

/// Change whether a profile may appear in the public achievement gallery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AchievementPublicRequest {
    pub server: String,
    pub profile_id: String,
    pub public: bool,
}

/// A geocoding request (`geocode` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoQuery {
    pub query: String,
}

/// Clear a single field group on a profile (`profile_clear` host function). `field` is one of
/// `title`, `birthday`, `pronouns`, `location`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfileClear {
    pub server: String,
    pub nick: String,
    pub field: String,
}

/// Request delivered to an optional module `admin_command` export.
///
/// The authenticated admin API selects the network; the module owns parsing the remaining
/// arguments so its private persisted-state schema stays encapsulated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleAdminCommandRequest {
    pub server: String,
    pub args: String,
}

/// Human-readable result lines returned by a module `admin_command` export.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModuleAdminCommandResponse {
    pub messages: Vec<String>,
}

/// A current-weather request by coordinates (`weather` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherQuery {
    pub lat: f64,
    pub lon: f64,
}

/// One active alert returned by the US National Weather Service for a coordinate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeatherAlert {
    pub event: String,
    pub severity: String,
}

/// Active NWS alerts for a coordinate. An empty list means no alerts were available.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeatherAlertsResult {
    pub alerts: Vec<WeatherAlert>,
}

/// Current conditions from Open-Meteo. Temperatures in °C, wind in km/h; the consumer derives
/// imperial units for display. `weather` returns `null` JSON on failure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherResult {
    pub temp_c: f64,
    pub apparent_c: f64,
    pub humidity: f64,
    pub wind_kmh: f64,
    /// WMO weather interpretation code.
    pub code: i64,
    pub is_day: bool,
    /// Consolidated United States AQI from Open-Meteo/CAMS, when available.
    #[serde(default)]
    pub us_aqi: Option<f64>,
    /// Near-surface particulate concentrations in µg/m³, when available.
    #[serde(default)]
    pub pm2_5: Option<f64>,
    #[serde(default)]
    pub pm10: Option<f64>,
    /// Open-Meteo's forecast liquid-rain total for the location's current local calendar day.
    #[serde(default)]
    pub forecast_rain_mm: Option<f64>,
    /// Direction the wind blows from, in degrees (0 = north).
    #[serde(default)]
    pub wind_direction_deg: Option<f64>,
    #[serde(default)]
    pub gusts_kmh: Option<f64>,
    #[serde(default)]
    pub uv_index: Option<f64>,
    /// Today and the next two days, in the location's local calendar.
    #[serde(default)]
    pub daily: Vec<DailyWeather>,
}

/// One local calendar day of forecast.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DailyWeather {
    /// ISO date, e.g. "2026-09-28".
    pub date: String,
    /// English weekday name for `date`.
    pub weekday: String,
    /// WMO weather interpretation code.
    pub code: i64,
    pub max_c: Option<f64>,
    pub min_c: Option<f64>,
    /// Highest hourly chance of precipitation that day, in percent.
    pub precipitation_probability: Option<f64>,
    pub rain_mm: Option<f64>,
    /// Local clock times, e.g. "06:52".
    pub sunrise: Option<String>,
    pub sunset: Option<String>,
}

/// Normalized current conditions from the operator-configured WeatherLink station.
///
/// WeatherLink exposes product-specific sensor records, so every observation except the station
/// label is optional. `error` is a safe display category and never contains provider response
/// text or credentials.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WeatherLinkResult {
    pub station: String,
    #[serde(default)]
    pub observed_at: Option<i64>,
    #[serde(default)]
    pub temp_f: Option<f64>,
    #[serde(default)]
    pub apparent_f: Option<f64>,
    #[serde(default)]
    pub humidity: Option<f64>,
    #[serde(default)]
    pub wind_mph: Option<f64>,
    #[serde(default)]
    pub wind_gust_mph: Option<f64>,
    #[serde(default)]
    pub wind_dir_degrees: Option<f64>,
    #[serde(default)]
    pub pressure_inhg: Option<f64>,
    #[serde(default)]
    pub rain_daily_in: Option<f64>,
    #[serde(default)]
    pub rain_rate_in_hr: Option<f64>,
    #[serde(default)]
    pub error: Option<String>,
}

/// A web-search request (`web_search` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub query: String,
}

/// One ranked web-search result.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Search host response. `error` is a safe, user-displayable category rather than provider
/// response text, which may contain account details.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    pub error: Option<String>,
}

/// A provider-neutral GIF search request (`gif_search` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GifSearchRequest {
    pub query: String,
    pub limit: u32,
}

/// One bounded GIF result. `url` is an HTTPS media URL validated by the host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GifSearchResult {
    pub url: String,
    pub title: String,
}

/// GIF-search host response. Provider details and failures remain host-owned.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GifSearchResponse {
    pub results: Vec<GifSearchResult>,
    pub provider: String,
    pub error: Option<String>,
}

/// A dictionary lookup request (`dictionary_lookup` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictionaryQuery {
    pub word: String,
}

/// One bounded dictionary sense.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DictionarySense {
    pub part_of_speech: String,
    pub definition: String,
}

/// Dictionary host response. Provider failures are reduced to safe error categories.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct DictionaryResponse {
    pub word: Option<String>,
    pub phonetic: Option<String>,
    pub senses: Vec<DictionarySense>,
    pub error: Option<String>,
    /// A few synonyms, when the source provides them.
    #[serde(default)]
    pub synonyms: Vec<String>,
}

/// English etymology from Wiktionary (`etymology_lookup`, gated by the `dictionary_lookup`
/// capability). Request is a [`DictionaryQuery`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct EtymologyResponse {
    /// The Wiktionary entry that answered (its capitalisation may differ from the query).
    pub word: Option<String>,
    /// One plain-text paragraph per English etymology section ("Etymology 1", "Etymology 2"…),
    /// bounded in count and length.
    #[serde(default)]
    pub etymologies: Vec<String>,
    pub url: Option<String>,
    pub error: Option<String>,
}

/// Convert between fiat currencies (ECB reference rates) and cryptocurrencies (CoinGecko), via
/// the `money` capability. `from`/`to` may be ISO codes, symbols (`$`, `£`), common names
/// ("euros", "quid"), or crypto tickers ("btc"); the host resolves them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MoneyConvertRequest {
    pub amount: f64,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MoneyConvertResponse {
    pub result: Option<f64>,
    /// Resolved codes, e.g. "USD", "BTC".
    pub from: Option<String>,
    pub to: Option<String>,
    /// Units of `to` per one unit of `from`.
    pub rate: Option<f64>,
    /// "ECB", "CoinGecko", or both, for attribution.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Rate date (ECB publishes working-day reference rates).
    pub as_of: Option<String>,
    /// `unknown_from`, `unknown_to`, `unavailable`, or `rate_limited`.
    pub error: Option<String>,
}

/// A cryptocurrency price (`money` capability).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CryptoQuoteRequest {
    /// Ticker or name: "btc", "ethereum", "doge".
    pub symbol: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CryptoQuoteResponse {
    pub symbol: Option<String>,
    pub name: Option<String>,
    pub price_usd: Option<f64>,
    /// Percent change over 24 hours.
    pub change_24h: Option<f64>,
    /// The same price in GBP and EUR via ECB rates, when available.
    pub price_gbp: Option<f64>,
    pub price_eur: Option<f64>,
    /// `unknown`, `unavailable`, or `rate_limited`.
    pub error: Option<String>,
}

/// A random animal picture (`animal_image` host function). The host owns the catalogue of
/// animals and their image sources; modules never choose URLs or search terms.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnimalImageRequest {
    /// Animal name or alias ("capy", "red panda"); empty picks one at random.
    #[serde(default)]
    pub kind: String,
    /// Return the catalogue in `kinds` instead of fetching a picture.
    #[serde(default)]
    pub list: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AnimalImageResponse {
    /// Canonical animal name the picture is of.
    pub kind: Option<String>,
    pub emoji: Option<String>,
    pub url: Option<String>,
    /// The catalogue's canonical names, for `list` requests.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// `unknown` (not in the catalogue), `unavailable`, or `rate_limited`.
    pub error: Option<String>,
}

/// A Wikipedia search request (`wikipedia_lookup` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikipediaQuery {
    pub query: String,
}

/// A bounded Wikipedia article introduction with a stable attribution link.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WikipediaResponse {
    pub title: Option<String>,
    pub extract: Option<String>,
    pub url: Option<String>,
    pub error: Option<String>,
    /// For a disambiguation page: the articles it points to, in page order (main meanings
    /// first). Empty for ordinary articles.
    #[serde(default)]
    pub options: Vec<String>,
}

/// A Wikiquote request (`wikiquote` host function). An empty topic asks for today's quote of the
/// day; otherwise the host finds the topic's page and picks one of its quotes using `pick`
/// (supplied by the module from `random_bytes`, so the host needs no randomness of its own).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WikiquoteQuery {
    pub topic: String,
    #[serde(default)]
    pub pick: u64,
}

/// One bounded Wikiquote quote with its attribution.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WikiquoteResponse {
    /// The page the quote came from.
    pub title: Option<String>,
    pub quote: Option<String>,
    /// The work or section the quote belongs to ("Small Gods (1992)"), or the author for the
    /// quote of the day.
    pub source: Option<String>,
    pub url: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoutubeLookup {
    pub ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct YoutubeSearch {
    pub query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct YoutubeResult {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub view_count: u64,
    pub like_count: Option<u64>,
    pub duration_seconds: u64,
    pub published_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct YoutubeResponse {
    pub results: Vec<YoutubeResult>,
    pub error: Option<String>,
}

/// A text-translation request (`translate` host function). If `source_lang` is omitted, DeepL
/// detects it automatically.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranslateQuery {
    pub text: String,
    pub target_lang: String,
    pub source_lang: Option<String>,
}

/// Translation host response. Provider failures are reduced to safe error categories.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslateResponse {
    pub text: Option<String>,
    pub detected_source_language: Option<String>,
    pub error: Option<String>,
}

/// A bounded text-generation request (`ai_chat` host function). Provider credentials, endpoint,
/// model, and system prompt remain host-owned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiChatRequest {
    pub prompt: String,
    /// Recent conversation supplied by the calling module. The host validates and labels this as
    /// untrusted context before sending it to the provider.
    #[serde(default)]
    pub context: Vec<AiChatContextLine>,
    /// Ask the host to include its live command registry as trusted reference material. The host
    /// generates and bounds the reference; callers cannot supply trusted prompt text themselves.
    #[serde(default)]
    pub include_command_reference: bool,
    pub temperature: f64,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiChatContextLine {
    pub speaker: String,
    pub text: String,
}

/// Safe AI response returned to a module. Provider response bodies are never exposed on failure.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AiChatResponse {
    pub text: Option<String>,
    pub error: Option<String>,
}

/// A geocoding result (best match). `geocode` returns `null` JSON when nothing matched.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeoResult {
    pub name: String,
    pub admin1: Option<String>,
    pub admin2: Option<String>,
    pub country: Option<String>,
    pub lat: f64,
    pub lon: f64,
    /// IANA timezone, suitable for daylight-saving-aware local-time conversion.
    pub timezone: String,
}

/// Convert a Unix instant to civil time in an IANA timezone (`local_time` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalTimeQuery {
    /// An IANA zone ("Europe/London", any case), a common abbreviation ("PST", "BST", "IST"), or a
    /// UTC offset ("UTC+5:30", "GMT-3").
    pub timezone: String,
    /// Defaults to the host's current time. Primarily useful for deterministic consumers/tests.
    #[serde(default)]
    pub unix_seconds: Option<i64>,
    /// A wall-clock time in `timezone` to resolve instead of an instant, e.g. "3pm PST today".
    /// Ambiguous times (DST fall-back) take the earlier instant; skipped times move forward.
    #[serde(default)]
    pub local: Option<LocalWallTime>,
}

/// A civil date and time without a zone.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalWallTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Daylight-saving-aware local civil time returned by the host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LocalTimeResult {
    pub timezone: String,
    pub abbreviation: String,
    pub utc_offset: String,
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub weekday: String,
    pub hour_24: u32,
    pub minute: u32,
    /// The instant this result describes.
    #[serde(default)]
    pub unix_seconds: i64,
}

/// Request OS-random bytes from the host. `count` is capped at 64 by the host.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RandomBytesRequest {
    pub count: usize,
}

/// OS-random bytes returned by the host for the `random_bytes` capability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RandomBytesResponse {
    pub bytes: Vec<u8>,
}

/// A request for a themed (user-configurable) string. The host looks up `[<module>].<key>` in the
/// theme file (writing `default` if absent), picks one entry at random if it's a list, substitutes
/// `{var}` placeholders from `vars`, and returns the rendered text.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeReq {
    pub key: String,
    /// Default phrasing(s) to seed on first use. One entry → stored as a string; multiple → a list.
    pub default: Vec<String>,
    /// Placeholder substitutions, e.g. `("user", "bob")` replaces `{user}`.
    pub vars: Vec<(String, String)>,
}
