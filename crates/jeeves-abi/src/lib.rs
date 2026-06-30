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
}

/// One command entry as returned by the `commands_list` host function. Reflects the effective
/// aliases (after operator overrides), not the module's built-in defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandInfo {
    pub module: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    pub aliases: Vec<String>,
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

/// A current-weather request by coordinates (`weather` host function).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeatherQuery {
    pub lat: f64,
    pub lon: f64,
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
    pub temperature: f64,
    pub max_tokens: u32,
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
    pub timezone: String,
    /// Defaults to the host's current time. Primarily useful for deterministic consumers/tests.
    #[serde(default)]
    pub unix_seconds: Option<i64>,
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
