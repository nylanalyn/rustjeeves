//! Spontaneous Victorian gentleman's excursion game for rustjeeves.
//!
//! Jeeves proposes a trip to a destination; players have a signup window to
//! join with !me; the party departs and returns after a while.
//! `!roadtrip` also starts a trip manually regardless of the `enabled` setting.
//! While a trip is already forming or travelling, another `!roadtrip` is silent.
//!
//! IMPORTANT: spontaneous trips require `enabled = true` per channel.
//!            Manual !roadtrip always works.
//!
//! Commands: !roadtrip [status | cancel], !me
//!
//! Theme keys (all under "roadtrip.*"):
//!   destinations (list — the pool of destinations; operators edit this in theme.toml),
//!   announce_me (spontaneous trip proposed; vars: destination, mins),
//!   propose_me (manual trip started; vars: nick, destination, mins),
//!   joined (confirmed join; vars: nick, destination),
//!   already_joined (tried to join twice; vars: nick),
//!   join_closed (no open signup; vars: nick),
//!   nobody (nobody joined, trip cancelled; vars: destination),
//!   depart (party departs; vars: passengers, destination, count),
//!   return_report (wraps a destination return report; vars: destination, story),
//!   story.<slug>.{solo,duo,group} (per-destination return story; one record per
//!     DEFAULT_DESTINATIONS entry plus fallback; vars: destination, p1, p2, passengers, count),
//!   status_signup_me (signup open; vars: destination, passengers, count, mins),
//!   status_travelling (on a trip; vars: destination, passengers, count),
//!   status_none (nothing planned),
//!   cancelled (admin cancelled; vars: destination),
//!   cancel_denied (non-admin tried to cancel; vars: nick)

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, RandomBytesRequest, RandomBytesResponse, Role,
    ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind,
    SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

// Default destination pool — operators override "roadtrip.destinations" in theme.toml.
// This is the legacy 20-location roadtrip roster; return stories are keyed off these names
// (exact, case-sensitive match) and an unknown operator-configured destination falls through
// to FALLBACK_STORY.
const DEFAULT_DESTINATIONS: &[&str] = &[
    "the neon boneyard",
    "the mountain hot springs",
    "the glasshouse conservatory",
    "the roadside flea market",
    "the decommissioned airfield",
    "the riverside park",
    "the old museum",
    "the observatory",
    "the seaside pier",
    "the midnight diner",
    "the abandoned drive-in",
    "the vintage bowling alley",
    "the old lighthouse",
    "the giant roadside statue",
    "the used bookstore",
    "the retro roller rink",
    "the canyon overlook",
    "the ghost town",
    "the vintage arcade",
    "the sunflower field",
];

// ── return-story catalog (allocation-free static data) ────────────────────────
//
// Each destination carries solo/duo/group story templates. A configured destination that
// does not exactly match a catalog entry selects FALLBACK_STORY. The `story!` macro keeps
// the 20 repetitive records readable while constructing explicit, compile-time theme keys
// via `concat!` — keys are never generated or normalized at runtime. Each key is seeded
// into theme.toml independently on first use, so operators can edit any single story.

struct StoryTemplate {
    key: &'static str,
    defaults: &'static [&'static str],
}

struct DestinationStory {
    destination: &'static str,
    solo: StoryTemplate,
    duo: StoryTemplate,
    group: StoryTemplate,
}

/// Build a `DestinationStory` with canonical `roadtrip.story.<slug>.<party>` keys.
macro_rules! story {
    (
        slug = $slug:literal,
        dest = $dest:literal,
        solo = $solo:literal,
        duo = $duo:literal,
        group = $group:literal $(,)?
    ) => {
        DestinationStory {
            destination: $dest,
            solo: StoryTemplate {
                key: concat!("roadtrip.story.", $slug, ".solo"),
                defaults: &[$solo],
            },
            duo: StoryTemplate {
                key: concat!("roadtrip.story.", $slug, ".duo"),
                defaults: &[$duo],
            },
            group: StoryTemplate {
                key: concat!("roadtrip.story.", $slug, ".group"),
                defaults: &[$group],
            },
        }
    };
}

const STORIES: &[DestinationStory] = &[
    story! {
        slug = "neon_boneyard", dest = "the neon boneyard",
        solo = "{p1} wandered between rusted marquees, imagining the shows they once lit up.",
        duo = "{p1} and {p2} kept daring each other to flip unknown switches until a single bulb flickered alive.",
        group = "{p1} and the others staged a mock award show on a toppled stage, complete with improvised speeches.",
    },
    story! {
        slug = "mountain_hot_springs", dest = "the mountain hot springs",
        solo = "{p1} soaked under drifting steam, watching clouds snag on the ridge line.",
        duo = "{p1} and {p2} timed their plunge into the cold pool together, then laughed too hard to speak.",
        group = "{p1} and the group turned the boardwalk into an impromptu footrace before collapsing back into the heat.",
    },
    story! {
        slug = "glasshouse_conservatory", dest = "the glasshouse conservatory",
        solo = "{p1} traced the misted names of rare orchids and left with a phone full of plant photos.",
        duo = "{p1} and {p2} tried to outdo each other mimicking bird calls; nearby parrots approved loudly.",
        group = "{p1} and the group invented a game of spotting the weirdest leaf, which somehow became surprisingly competitive.",
    },
    story! {
        slug = "roadside_flea_market", dest = "the roadside flea market",
        solo = "{p1} haggled for a mysterious brass compass that definitely points somewhere important.",
        duo = "{p1} and {p2} bought matching sunglasses and appointed themselves the household's official glare inspectors.",
        group = "{p1} and the others pooled coins to rescue a wobbling lava lamp that is now the group's mascot.",
    },
    story! {
        slug = "decommissioned_airfield", dest = "the decommissioned airfield",
        solo = "{p1} walked the empty runway counting faded numbers, savoring the echo of their footsteps.",
        duo = "{p1} and {p2} raced down the tarmac pretending to taxi invisible planes, complete with hand signals.",
        group = "{p1} and the group held a paper-plane tournament in the hangar; a rogue gust crowned an unexpected champion.",
    },
    story! {
        slug = "riverside_park", dest = "the riverside park",
        solo = "{p1} enjoyed a quiet moment by the water, skipping stones across the surface.",
        duo = "{p1} and {p2} had a long conversation on a park bench, watching the boats go by.",
        group = "{p1} and the others started an impromptu game of frisbee that went on for hours.",
    },
    story! {
        slug = "old_museum", dest = "the old museum",
        solo = "{p1} spent a thoughtful afternoon wandering the halls, completely losing track of time.",
        duo = "{p1} and {p2} got into a surprisingly intense debate about modern art in front of a very confusing sculpture.",
        group = "{p1} and the group accidentally set off a minor alarm in the dinosaur exhibit, but played it cool.",
    },
    story! {
        slug = "observatory", dest = "the observatory",
        solo = "{p1} looked through the grand telescope and felt a profound sense of cosmic insignificance, but in a good way.",
        duo = "{p1} and {p2} stayed up late, pointing out constellations to each other, both real and imagined.",
        group = "{p1} and the others watched a stunning meteor shower from the observatory dome.",
    },
    story! {
        slug = "seaside_pier", dest = "the seaside pier",
        solo = "{p1} ate a truly questionable hot dog while watching the waves crash against the pylons.",
        duo = "{p1} and {p2} tried their luck at the arcade games and left with a giant, impractical stuffed animal.",
        group = "{p1} and the group bravely rode the rickety old Ferris wheel, offering thrilling views and mild terror.",
    },
    story! {
        slug = "midnight_diner", dest = "the midnight diner",
        solo = "{p1} drank lukewarm coffee and listened to the old jukebox play forgotten songs.",
        duo = "{p1} and {p2} shared a plate of questionable fries and solved all the world's problems over three hours.",
        group = "{p1} and the group somehow started a friendly pancake-eating contest with the night-shift cook.",
    },
    story! {
        slug = "abandoned_drive_in", dest = "the abandoned drive-in",
        solo = "{p1} sat on the hood of the car and watched the empty screen, imagining old double features.",
        duo = "{p1} and {p2} tuned the radio to static and pretended it was the original broadcast frequency.",
        group = "{p1} and the others reenacted their favorite movie scenes on the cracked asphalt, to mixed reviews.",
    },
    story! {
        slug = "vintage_bowling_alley", dest = "the vintage bowling alley",
        solo = "{p1} bowled three games alone, developing a deeply personal rivalry with pin seven.",
        duo = "{p1} and {p2} made up increasingly absurd trick shot rules until the lane attendant asked them to stop.",
        group = "{p1} and the group discovered the cosmic bowling lights and immediately declared it a dance floor.",
    },
    story! {
        slug = "old_lighthouse", dest = "the old lighthouse",
        solo = "{p1} climbed all 127 steps and stood in the lantern room, feeling like the last person on earth.",
        duo = "{p1} and {p2} took turns pretending to spot ships on the horizon, complete with dramatic pointing.",
        group = "{p1} and the others got thoroughly lost in the fog on the way back, which only added to the adventure.",
    },
    story! {
        slug = "giant_roadside_statue", dest = "the giant roadside statue",
        solo = "{p1} took seventeen photos trying to get the perfect forced-perspective shot.",
        duo = "{p1} and {p2} debated the artistic merit of a 40-foot fiberglass lumberjack for longer than expected.",
        group = "{p1} and the group posed for an elaborate group photo that will definitely become a holiday card.",
    },
    story! {
        slug = "used_bookstore", dest = "the used bookstore",
        solo = "{p1} emerged three hours later with a stack of books and no memory of time passing.",
        duo = "{p1} and {p2} challenged each other to find the weirdest title, resulting in some truly baffling discoveries.",
        group = "{p1} and the others got separated in the labyrinthine stacks and had to regroup at the cat by the register.",
    },
    story! {
        slug = "retro_roller_rink", dest = "the retro roller rink",
        solo = "{p1} skated cautiously along the wall for the first hour, then briefly achieved grace before falling.",
        duo = "{p1} and {p2} attempted the couple's skate and only crashed into each other twice.",
        group = "{p1} and the group formed a wobbly chain that somehow made it one full lap before spectacular collapse.",
    },
    story! {
        slug = "canyon_overlook", dest = "the canyon overlook",
        solo = "{p1} sat on the guardrail eating a sandwich, watching hawks ride the thermals below.",
        duo = "{p1} and {p2} shouted into the canyon and timed the echoes with great scientific precision.",
        group = "{p1} and the others hiked to a hidden ledge and stayed until the sunset painted everything gold.",
    },
    story! {
        slug = "ghost_town", dest = "the ghost town",
        solo = "{p1} wandered the empty main street, imagining the bustle of a hundred years past.",
        duo = "{p1} and {p2} poked through the old general store and found a coin from 1887 in the floorboards.",
        group = "{p1} and the group staged an impromptu western showdown, complete with exaggerated falls.",
    },
    story! {
        slug = "vintage_arcade", dest = "the vintage arcade",
        solo = "{p1} pumped quarters into a pinball machine until achieving a zen-like high score trance.",
        duo = "{p1} and {p2} discovered an ancient co-op game and didn't leave until they beat it.",
        group = "{p1} and the others held a tournament bracket that got surprisingly heated over air hockey.",
    },
    story! {
        slug = "sunflower_field", dest = "the sunflower field",
        solo = "{p1} walked between the towering stalks and felt briefly like a character in a painting.",
        duo = "{p1} and {p2} got absolutely lost in the maze and had to navigate by sun position.",
        group = "{p1} and the group played hide and seek until someone startled a very indignant crow.",
    },
];

// Generic party-size fallback for operator-configured destinations outside the catalog.
// Prose transcribed from the legacy `fallback` record; its `{dest}` placeholder is normalized
// to `{destination}` so it substitutes through the same themed vars as the catalog stories.
const FALLBACK_STORY: DestinationStory = story! {
    slug = "fallback", dest = "",
    solo = "{p1} had a quiet, introspective time at {destination}.",
    duo = "{p1} and {p2} found a cozy corner at {destination} and chatted for hours.",
    group = "{p1} and the group explored {destination} and generally had a lovely time.",
};
const MAX_PASSENGERS: usize = 20;
const MAX_NAMES_IN_OUTPUT: usize = 8;
const MAX_DISPLAY_CHARS: usize = 48;

// ── host function imports ─────────────────────────────────────────────────────

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("pack_bag", "Pack a Bag", 1),
        ("seasoned_traveller", "Seasoned Traveller", 25),
        ("grand_tour", "The Grand Tour", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Complete {threshold} roadtrips."),
        stat: "completed".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "more_merrier".into(),
        name: "The More the Merrier".into(),
        description: "Complete a trip with at least five passengers.".into(),
        stat: "large_party".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "completed".into(),
                description: "Completed roadtrips".into(),
            },
            AchievementStat {
                id: "large_party".into(),
                description: "Trips completed with five passengers".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award_trip(
    server: &str,
    channel: &str,
    passenger: &Passenger,
    large: bool,
) -> Result<(), Error> {
    if passenger.user_id.is_empty() {
        return Ok(());
    }
    let mut increments = vec![StatIncrement {
        stat: "completed".into(),
        amount: 1,
    }];
    if large {
        increments.push(StatIncrement {
            stat: "large_party".into(),
            amount: 1,
        });
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: passenger.user_id.clone(),
            display_name: passenger.display.clone(),
            target: channel.into(),
            increments,
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

// ── command manifest ──────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "roadtrip".into(),
                description:
                    "Propose a Victorian excursion, inspect it, or cancel it as an administrator."
                        .into(),
                usage: "!roadtrip [status | cancel]".into(),
                aliases: vec!["rt".into()],
            },
            CommandSpec {
                name: "me".into(),
                description: "Join the roadtrip currently accepting passengers.".into(),
                usage: "!me".into(),
                aliases: Vec::new(),
            },
        ],
    })?)
}

// ── settings manifest ─────────────────────────────────────────────────────────

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether Jeeves proposes spontaneous trips in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "signup_secs".into(),
                description: "Seconds the signup window stays open after a trip is proposed."
                    .into(),
                default: "90".into(),
                kind: SettingKind::Integer { min: 30, max: 300 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_trip_mins".into(),
                description: "Minimum minutes a trip lasts before the party returns.".into(),
                default: "30".into(),
                kind: SettingKind::Integer { min: 5, max: 120 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_trip_mins".into(),
                description: "Maximum minutes a trip lasts before the party returns.".into(),
                default: "60".into(),
                kind: SettingKind::Integer { min: 5, max: 240 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_interval_mins".into(),
                description: "Minimum minutes between spontaneous trip proposals.".into(),
                default: "120".into(),
                kind: SettingKind::Integer { min: 30, max: 1440 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_interval_mins".into(),
                description: "Maximum minutes between spontaneous trip proposals.".into(),
                default: "360".into(),
                kind: SettingKind::Integer { min: 30, max: 2880 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── state structs ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct Passenger {
    /// Stable profile UUID. Empty values are legacy display-only passengers.
    user_id: String,
    nick: String,
    display: String,
}

#[derive(Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
enum TripPhase {
    #[default]
    None,
    Signup,
    Travelling,
}

#[derive(Serialize, Deserialize, Default)]
struct TripState {
    phase: TripPhase,
    destination: String,
    passengers: Vec<Passenger>,
}

fn lifecycle_passenger_matches(passenger: &Passenger, request: &ModuleDataRequest) -> bool {
    passenger.user_id == request.subject.profile_id
        || request.aliases.iter().any(|alias| {
            passenger.user_id.eq_ignore_ascii_case(alias)
                || passenger.nick.eq_ignore_ascii_case(alias)
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let prefix = format!("state:{}:", request.subject.server);
    let mut trips = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let state: TripState = serde_json::from_str(&entry.value)?;
        if let Some(passenger) = state
            .passengers
            .iter()
            .find(|passenger| lifecycle_passenger_matches(passenger, &request))
        {
            trips.push(serde_json::json!({ "key": entry.key, "phase": state.phase, "destination": state.destination, "passenger": passenger }));
        }
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if trips.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "trips": trips })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let prefix = format!("state:{}:", request.subject.server);
    let mut mutations = Vec::new();
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix))
    {
        if entry.value.is_empty() {
            continue;
        }
        let mut state: TripState = serde_json::from_str(&entry.value)?;
        let before = state.passengers.len();
        state
            .passengers
            .retain(|passenger| !lifecycle_passenger_matches(passenger, &request));
        if state.passengers.len() != before {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: if state.passengers.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&state)?)
                },
            });
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

// ── job ID helpers (encoded per server+channel to avoid cross-channel cancel) ─

fn next_job_id(server: &str, channel: &str) -> String {
    format!("next:{server}:{channel}")
}

fn depart_job_id(server: &str, channel: &str) -> String {
    format!("depart:{server}:{channel}")
}

fn return_job_id(server: &str, channel: &str) -> String {
    format!("return:{server}:{channel}")
}

// ── KV helpers ────────────────────────────────────────────────────────────────

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn state_key(server: &str, channel: &str) -> String {
    format!("trip:{server}:{channel}")
}

fn load_state(server: &str, channel: &str) -> Result<TripState, Error> {
    let raw = kv_load(&state_key(server, channel))?;
    if raw.is_empty() {
        return Ok(TripState::default());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_state(server: &str, channel: &str, state: &TripState) -> Result<(), Error> {
    kv_save(&state_key(server, channel), &serde_json::to_string(state)?)
}

fn clear_state(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&state_key(server, channel), "")
}

fn passenger_index_by_id(passengers: &[Passenger], user_id: &str) -> Option<usize> {
    (!user_id.is_empty())
        .then(|| {
            passengers
                .iter()
                .position(|passenger| passenger.user_id == user_id)
        })
        .flatten()
}

fn bounded_display(display: &str) -> String {
    display.chars().take(MAX_DISPLAY_CHARS).collect()
}

// ── host helpers ──────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    unsafe {
        now(String::new())
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?;
    }
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|s| s.to_string()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn read_setting_raw(key: &str, server: &str, channel: &str) -> Option<String> {
    let raw = unsafe {
        setting_get(
            serde_json::to_string(&SettingGet {
                key: key.into(),
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .ok()?,
        )
        .ok()?
    };
    Some(raw)
}

fn read_setting_bool(key: &str, server: &str, channel: &str, default: bool) -> bool {
    read_setting_raw(key, server, channel)
        .and_then(|s| match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn read_setting_i64(key: &str, server: &str, channel: &str, default: i64) -> i64 {
    read_setting_raw(key, server, channel)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn get_random_bytes(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

fn has_pending_job(server: &str, channel: &str, id: &str) -> bool {
    let raw = unsafe {
        schedule_list(
            serde_json::to_string(&ScheduleList {
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .unwrap_or_default(),
        )
        .unwrap_or_default()
    };
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
    jobs.iter().any(|j| j.id == id)
}

fn cancel_job(server: &str, channel: &str, id: &str) {
    let full_id = format!("{id}:{server}:{channel}");
    let _ = unsafe {
        schedule_cancel(serde_json::to_string(&ScheduleCancel { id: full_id }).unwrap_or_default())
    };
}

// ── scheduling helpers ────────────────────────────────────────────────────────

fn schedule_next_announce(server: &str, channel: &str) -> Result<(), Error> {
    let min = read_setting_i64("min_interval_mins", server, channel, 120);
    let max = read_setting_i64("max_interval_mins", server, channel, 360).max(min + 1);
    let bytes = get_random_bytes(4)?;
    let range = ((max - min) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min * 60 + (r % range) as i64;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: next_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn schedule_depart(server: &str, channel: &str, signup_secs: i64) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: depart_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + signup_secs,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn schedule_return(server: &str, channel: &str) -> Result<(), Error> {
    let min = read_setting_i64("min_trip_mins", server, channel, 30);
    let max = read_setting_i64("max_trip_mins", server, channel, 60).max(min + 1);
    let bytes = get_random_bytes(4)?;
    let range = ((max - min) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min * 60 + (r % range) as i64;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: return_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

fn ensure_next_scheduled(server: &str, channel: &str) -> Result<(), Error> {
    if !has_pending_job(server, channel, &next_job_id(server, channel))
        && !has_pending_job(server, channel, &depart_job_id(server, channel))
        && !has_pending_job(server, channel, &return_job_id(server, channel))
    {
        let state = load_state(server, channel)?;
        if state.phase == TripPhase::None {
            schedule_next_announce(server, channel)?;
        }
    }
    Ok(())
}

// ── formatting ────────────────────────────────────────────────────────────────

fn format_passengers(passengers: &[Passenger]) -> String {
    let visible = &passengers[..passengers.len().min(MAX_NAMES_IN_OUTPUT)];
    let names = match visible {
        [] => String::new(),
        [p] => p.display.clone(),
        [a, b] => format!("{} and {}", a.display, b.display),
        _ => {
            let init: Vec<&str> = visible[..visible.len() - 1]
                .iter()
                .map(|p| p.display.as_str())
                .collect();
            format!(
                "{}, and {}",
                init.join(", "),
                visible.last().unwrap().display
            )
        }
    };
    if passengers.len() > visible.len() {
        format!("{names}, and {} others", passengers.len() - visible.len())
    } else {
        names
    }
}

// ── core trip logic ───────────────────────────────────────────────────────────

/// Open a signup window: pick destination, store state, announce, schedule depart.
/// `initiator` identifies a manual proposer for the announcement; proposing does not join them.
fn open_signup(server: &str, channel: &str, initiator: Option<&Passenger>) -> Result<(), Error> {
    let destination = themed("roadtrip.destinations", DEFAULT_DESTINATIONS, &[])?;
    let signup_secs = read_setting_i64("signup_secs", server, channel, 90);
    let mins = (signup_secs / 60).max(1).to_string();

    let state = TripState {
        phase: TripPhase::Signup,
        destination: destination.clone(),
        passengers: Vec::new(),
    };
    save_state(server, channel, &state)?;
    schedule_depart(server, channel, signup_secs)?;

    match initiator {
        None => reply(
            server,
            channel,
            &themed(
                "roadtrip.announce_me",
                &[
                    "Jeeves has arranged an excursion to {destination}! Join with !me. Departing in {mins} minutes.",
                    "One has taken the liberty of booking passage to {destination}. Those wishing to accompany may say !me. Departure in {mins} minutes.",
                ],
                &[("destination", &destination), ("mins", &mins)],
            )?,
        )?,
        Some(p) => reply(
            server,
            channel,
            &themed(
                "roadtrip.propose_me",
                &[
                    "{nick} proposes an excursion to {destination}! Join with !me. Departing in {mins} minutes.",
                    "{nick} has suggested a trip to {destination}. Interested parties may say !me before departure in {mins} minutes.",
                ],
                &[
                    ("nick", &p.display),
                    ("destination", &destination),
                    ("mins", &mins),
                ],
            )?,
        )?,
    }

    Ok(())
}

// ── timer handlers ────────────────────────────────────────────────────────────

fn handle_next(server: &str, channel: &str) -> Result<(), Error> {
    if !read_setting_bool("enabled", server, channel, false) {
        return Ok(());
    }
    let state = load_state(server, channel)?;
    if state.phase != TripPhase::None {
        // A manual trip was started between the schedule and the fire — reschedule for later.
        if read_setting_bool("enabled", server, channel, false) {
            schedule_next_announce(server, channel)?;
        }
        return Ok(());
    }
    open_signup(server, channel, None)
}

fn handle_depart(server: &str, channel: &str) -> Result<(), Error> {
    let mut state = load_state(server, channel)?;
    if state.phase != TripPhase::Signup {
        return Ok(());
    }

    if state.passengers.is_empty() {
        clear_state(server, channel)?;
        reply(
            server,
            channel,
            &themed(
                "roadtrip.nobody",
                &[
                    "Nobody joined the trip to {destination}. Jeeves quietly unpacks the luggage.",
                    "The carriages for {destination} departed empty. Most unfortunate.",
                ],
                &[("destination", &state.destination)],
            )?,
        )?;
        if read_setting_bool("enabled", server, channel, false) {
            schedule_next_announce(server, channel)?;
        }
        return Ok(());
    }

    state.phase = TripPhase::Travelling;
    save_state(server, channel, &state)?;
    schedule_return(server, channel)?;

    let names = format_passengers(&state.passengers);
    let count = state.passengers.len().to_string();
    reply(
        server,
        channel,
        &themed(
            "roadtrip.depart",
            &[
                "The party sets off for {destination}! Bon voyage, {passengers}.",
                "Right, then! {passengers} are bound for {destination}. One trusts all will be orderly.",
                "Jeeves has arranged the carriages. {passengers} depart for {destination}.",
            ],
            &[
                ("passengers", &names),
                ("destination", &state.destination),
                ("count", &count),
            ],
        )?,
    )?;
    Ok(())
}

// ── return-story selection ────────────────────────────────────────────────────

/// Pure, host-function-free story selector for a trip return.
///
/// `1` → solo, `2` → duo, `≥3` → group. The destination is matched exactly (case-sensitive)
/// against the catalog; an operator-configured destination not present selects the generic
/// fallback. A zero-count (corrupted) travelling state resolves to the solo template rather
/// than panicking — the normal lifecycle never departs with zero passengers, so this only
/// guards persisted-state corruption.
fn return_story(destination: &str, passenger_count: usize) -> &'static StoryTemplate {
    let story = STORIES
        .iter()
        .find(|s| s.destination == destination)
        .unwrap_or(&FALLBACK_STORY);
    match passenger_count {
        2 => &story.duo,
        0 | 1 => &story.solo,
        _ => &story.group,
    }
}

fn handle_return(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    if state.phase != TripPhase::Travelling {
        return Ok(());
    }

    clear_state(server, channel)?;

    let names = format_passengers(&state.passengers);
    let count = state.passengers.len().to_string();
    let p1 = state
        .passengers
        .first()
        .map(|p| p.display.as_str())
        .unwrap_or("");
    let p2 = state
        .passengers
        .get(1)
        .map(|p| p.display.as_str())
        .unwrap_or("");

    let template = return_story(&state.destination, state.passengers.len());
    let story = themed(
        template.key,
        template.defaults,
        &[
            ("destination", state.destination.as_str()),
            ("p1", p1),
            ("p2", p2),
            ("passengers", names.as_str()),
            ("count", count.as_str()),
        ],
    )?;
    let report = themed(
        "roadtrip.return_report",
        &["A report from the roadtrip to {destination}: {story}"],
        &[
            ("destination", state.destination.as_str()),
            ("story", story.as_str()),
        ],
    )?;
    reply(server, channel, &report)?;

    let large = state.passengers.len() >= 5;
    for passenger in &state.passengers {
        award_trip(server, channel, passenger, large)?;
    }

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next_announce(server, channel)?;
    }
    Ok(())
}

// ── command handlers ──────────────────────────────────────────────────────────

fn cmd_start(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    match state.phase {
        TripPhase::Signup | TripPhase::Travelling => {}
        TripPhase::None => {
            if user_id.is_empty() {
                return reply(
                    server,
                    channel,
                    &themed(
                        "roadtrip.identity_unavailable",
                        &["I couldn't verify a stable profile for {nick}, so I cannot add you to an excursion."],
                        &[("nick", display)],
                    )?,
                );
            }
            // Cancel any pending next-announce and start immediately.
            cancel_job(server, channel, "next");
            let p = Passenger {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                display: bounded_display(display),
            };
            open_signup(server, channel, Some(&p))?;
        }
    }
    Ok(())
}

fn cmd_join(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
) -> Result<(), Error> {
    let mut state = load_state(server, channel)?;
    match state.phase {
        TripPhase::None | TripPhase::Travelling => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.join_closed",
                    &["There's no open trip signup right now, {nick}. Watch for the next announcement!"],
                    &[("nick", display)],
                )?,
            )?;
        }
        TripPhase::Signup => {
            if user_id.is_empty() {
                return reply(
                    server,
                    channel,
                    &themed(
                        "roadtrip.identity_unavailable",
                        &["I couldn't verify a stable profile for {nick}, so I cannot add you to the excursion."],
                        &[("nick", display)],
                    )?,
                );
            }
            if passenger_index_by_id(&state.passengers, user_id).is_some() {
                reply(
                    server,
                    channel,
                    &themed(
                        "roadtrip.already_joined",
                        &["{nick} is already in the party."],
                        &[("nick", display)],
                    )?,
                )?;
                return Ok(());
            }
            if state.passengers.len() >= MAX_PASSENGERS {
                return reply(
                    server,
                    channel,
                    &themed(
                        "roadtrip.full",
                        &["The excursion is full at {count} passengers, {nick}."],
                        &[("count", &MAX_PASSENGERS.to_string()), ("nick", display)],
                    )?,
                );
            }
            state.passengers.push(Passenger {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                display: bounded_display(display),
            });
            save_state(server, channel, &state)?;
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.joined",
                    &[
                        "{nick} joins the party for {destination}!",
                        "Splendid! {nick} is coming along to {destination}.",
                    ],
                    &[("nick", display), ("destination", &state.destination)],
                )?,
            )?;
        }
    }
    Ok(())
}

fn cmd_status(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    match state.phase {
        TripPhase::None => {
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_none",
                    &["No trip is currently planned. Say !roadtrip to propose one."],
                    &[],
                )?,
            )?;
        }
        TripPhase::Signup => {
            let names = format_passengers(&state.passengers);
            let count = state.passengers.len().to_string();
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_signup_me",
                    &["Trip to {destination} is forming ({count} aboard so far: {passengers}). Say !me to join!"],
                    &[
                        ("destination", &state.destination),
                        ("passengers", &names),
                        ("count", &count),
                    ],
                )?,
            )?;
        }
        TripPhase::Travelling => {
            let names = format_passengers(&state.passengers);
            let count = state.passengers.len().to_string();
            reply(
                server,
                channel,
                &themed(
                    "roadtrip.status_travelling",
                    &["The party of {count} ({passengers}) is currently travelling to {destination}."],
                    &[
                        ("destination", &state.destination),
                        ("passengers", &names),
                        ("count", &count),
                    ],
                )?,
            )?;
        }
    }
    Ok(())
}

fn cmd_cancel(server: &str, channel: &str) -> Result<(), Error> {
    let state = load_state(server, channel)?;
    if state.phase == TripPhase::None {
        return Ok(());
    }
    let destination = state.destination.clone();
    clear_state(server, channel)?;
    cancel_job(server, channel, "depart");
    cancel_job(server, channel, "return");
    reply(
        server,
        channel,
        &themed(
            "roadtrip.cancelled",
            &["The trip to {destination} has been cancelled. Jeeves repacks the trunks."],
            &[("destination", &destination)],
        )?,
    )?;
    if read_setting_bool("enabled", server, channel, false) {
        schedule_next_announce(server, channel)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RoadtripCommand {
    Start,
    Join,
    Status,
    Cancel,
}

fn parse_command(text: &str) -> Option<RoadtripCommand> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?.to_ascii_lowercase();
    let subcommand = parts.next().map(str::to_ascii_lowercase);
    match (command.as_str(), subcommand.as_deref(), parts.next()) {
        ("!me", None, None) => Some(RoadtripCommand::Join),
        ("!roadtrip" | "!rt", None, None) => Some(RoadtripCommand::Start),
        ("!roadtrip" | "!rt", Some("status"), None) => Some(RoadtripCommand::Status),
        ("!roadtrip" | "!rt", Some("cancel"), None) => Some(RoadtripCommand::Cancel),
        _ => None,
    }
}

// ── exports ───────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer { id, channel, .. } = env.event else {
        return Ok(());
    };

    if id.starts_with("next:") {
        handle_next(&server, &channel)?;
    } else if id.starts_with("depart:") {
        handle_depart(&server, &channel)?;
    } else if id.starts_with("return:") {
        handle_return(&server, &channel)?;
    }

    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    if msg.is_private {
        return Ok(());
    }

    let channel = &msg.target;

    if read_setting_bool("enabled", &server, channel, false) {
        ensure_next_scheduled(&server, channel)?;
    }

    let Some(command) = parse_command(msg.text.trim()) else {
        return Ok(());
    };

    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };
    let user_id = &msg.user_id;

    match command {
        RoadtripCommand::Start => cmd_start(&server, channel, nick, display, user_id)?,
        RoadtripCommand::Join => cmd_join(&server, channel, nick, display, user_id)?,
        RoadtripCommand::Status => cmd_status(&server, channel)?,
        RoadtripCommand::Cancel => {
            if msg.role.is_some_and(|r| r.satisfies(Role::Admin)) {
                cmd_cancel(&server, channel)?;
            } else {
                reply(
                    &server,
                    channel,
                    &themed(
                        "roadtrip.cancel_denied",
                        &["Only administrators may cancel an excursion, {nick}."],
                        &[("nick", display)],
                    )?,
                )?;
            }
        }
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_channel_scoped() {
        assert_ne!(next_job_id("net", "#a"), next_job_id("net", "#b"));
        assert_ne!(depart_job_id("net1", "#x"), depart_job_id("net2", "#x"));
        assert_ne!(return_job_id("net", "#a"), return_job_id("net", "#b"));
    }

    #[test]
    fn commands_use_me_for_joining_and_reject_old_join_syntax() {
        assert_eq!(parse_command("!roadtrip"), Some(RoadtripCommand::Start));
        assert_eq!(parse_command("!RT"), Some(RoadtripCommand::Start));
        assert_eq!(parse_command("!me"), Some(RoadtripCommand::Join));
        assert_eq!(
            parse_command("!roadtrip status"),
            Some(RoadtripCommand::Status)
        );
        assert_eq!(parse_command("!roadtrip join"), None);
        assert_eq!(parse_command("!roadtrip again"), None);
        assert_eq!(parse_command("!roadtrip-extra"), None);
    }

    #[test]
    fn default_destinations_nonempty() {
        assert!(!DEFAULT_DESTINATIONS.is_empty());
        assert!(DEFAULT_DESTINATIONS.iter().all(|d| !d.is_empty()));
    }

    #[test]
    fn passenger_list_solo() {
        let p = vec![Passenger {
            user_id: String::new(),
            nick: "alice".into(),
            display: "alice".into(),
        }];
        assert_eq!(format_passengers(&p), "alice");
    }

    #[test]
    fn passenger_list_duo() {
        let passengers = vec![
            Passenger {
                user_id: String::new(),
                nick: "alice".into(),
                display: "alice".into(),
            },
            Passenger {
                user_id: String::new(),
                nick: "bob".into(),
                display: "bob".into(),
            },
        ];
        assert_eq!(format_passengers(&passengers), "alice and bob");
    }

    #[test]
    fn passenger_list_trio() {
        let passengers = vec![
            Passenger {
                user_id: String::new(),
                nick: "alice".into(),
                display: "alice".into(),
            },
            Passenger {
                user_id: String::new(),
                nick: "bob".into(),
                display: "bob".into(),
            },
            Passenger {
                user_id: String::new(),
                nick: "carol".into(),
                display: "carol".into(),
            },
        ];
        assert_eq!(format_passengers(&passengers), "alice, bob, and carol");
    }

    #[test]
    fn random_delay_in_range() {
        let min_mins: i64 = 120;
        let max_mins: i64 = 360;
        let range = ((max_mins - min_mins) * 60).max(1) as u64;
        for bytes in [[0u8, 0, 0, 0], [255, 255, 255, 255], [42, 13, 7, 99]] {
            let r = u32::from_le_bytes(bytes) as u64;
            let delay = min_mins * 60 + (r % range) as i64;
            assert!(delay >= min_mins * 60);
            assert!(delay < max_mins * 60);
        }
    }

    #[test]
    fn stable_id_never_falls_back_to_matching_nick() {
        let passengers = vec![Passenger {
            user_id: "old-profile".into(),
            nick: "alice".into(),
            display: "alice".into(),
        }];
        assert_eq!(passenger_index_by_id(&passengers, "old-profile"), Some(0));
        assert_eq!(passenger_index_by_id(&passengers, "new-profile"), None);
        assert_eq!(passenger_index_by_id(&passengers, ""), None);
    }

    #[test]
    fn long_passenger_lists_are_bounded_in_output() {
        let passengers = (0..MAX_PASSENGERS)
            .map(|index| Passenger {
                user_id: format!("profile-{index}"),
                nick: format!("user{index}"),
                display: format!("user{index}"),
            })
            .collect::<Vec<_>>();
        let rendered = format_passengers(&passengers);
        assert!(rendered.contains("12 others"));
        assert!(!rendered.contains("user19"));
    }
    #[test]
    fn every_default_destination_has_all_three_templates() {
        for dest in DEFAULT_DESTINATIONS {
            for &party in &[1usize, 2, 3] {
                let template = return_story(dest, party);
                assert!(
                    !template.key.starts_with("roadtrip.story.fallback."),
                    "{dest:?} at party={party} fell back to {}",
                    template.key
                );
                assert!(!template.defaults.is_empty());
            }
        }
        // The legacy roster is exactly 20 destinations.
        assert_eq!(DEFAULT_DESTINATIONS.len(), 20);
    }

    #[test]
    fn observatory_stories_match_legacy_keys_and_text() {
        let solo = return_story("the observatory", 1);
        assert_eq!(solo.key, "roadtrip.story.observatory.solo");
        assert_eq!(solo.defaults.len(), 1);
        assert_eq!(
            solo.defaults[0],
            "{p1} looked through the grand telescope and felt a profound sense of cosmic insignificance, but in a good way."
        );

        let duo = return_story("the observatory", 2);
        assert_eq!(duo.key, "roadtrip.story.observatory.duo");
        assert_eq!(duo.defaults.len(), 1);
        assert_eq!(
            duo.defaults[0],
            "{p1} and {p2} stayed up late, pointing out constellations to each other, both real and imagined."
        );

        let group = return_story("the observatory", 3);
        assert_eq!(group.key, "roadtrip.story.observatory.group");
        assert_eq!(group.defaults.len(), 1);
        assert_eq!(
            group.defaults[0],
            "{p1} and the others watched a stunning meteor shower from the observatory dome."
        );
    }

    #[test]
    fn unknown_destination_uses_party_size_fallback() {
        assert_eq!(
            return_story("operator's moon base", 1).key,
            "roadtrip.story.fallback.solo"
        );
        assert_eq!(
            return_story("operator's moon base", 2).key,
            "roadtrip.story.fallback.duo"
        );
        assert_eq!(
            return_story("operator's moon base", 5).key,
            "roadtrip.story.fallback.group"
        );
    }

    #[test]
    fn party_sizes_select_distinct_categories() {
        let solo = return_story("the observatory", 1).key;
        let duo = return_story("the observatory", 2).key;
        let group = return_story("the observatory", 3).key;
        assert_ne!(solo, duo);
        assert_ne!(duo, group);
        assert_ne!(solo, group);
    }

    #[test]
    fn zero_passenger_corrupt_state_resolves_to_solo_category() {
        // A corrupted persisted state (zero passengers) must not panic; it resolves to
        // the solo template for a known destination and the solo fallback otherwise.
        assert_eq!(
            return_story("the observatory", 0).key,
            "roadtrip.story.observatory.solo"
        );
        assert_eq!(
            return_story("no such place", 0).key,
            "roadtrip.story.fallback.solo"
        );
    }
}
