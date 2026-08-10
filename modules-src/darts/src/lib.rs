//! Asynchronous channel-local 301 darts, modelled after the original Jeeves game.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvList, KvSet, MessagePayload, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, RandomBytesRequest,
    RandomBytesResponse, Role, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

const STARTING_SCORE: u32 = 201;
const MAX_DARTS_PER_TURN: u8 = 3;
const DEFAULT_COOLDOWN_SECS: i64 = 30 * 60;
const MAX_PLAYERS: usize = 100;

/// A player may throw at most this many darts per day (two full turns of three), regardless
/// of how busy the channel is. This — not the cooldown — is what stops the room being drowned
/// in `!darts`.
const DEFAULT_DAILY_CAP: i64 = 6;
/// Skill lost for each whole day a player throws nothing, applied lazily on their next throw.
const DEFAULT_SKILL_DECAY: i64 = 5;
/// Skill runs 0..=100. Each dart thrown adds one; missed days subtract the decay.
const MAX_SKILL: i64 = 100;
/// Below this skill a player throws purely at the random board; from here aim ramps up.
const SKILL_AIM_START: i64 = 10;
/// Chance (percent) that a dart is aimed rather than random, at SKILL_AIM_START and MAX_SKILL.
const MIN_AIM_PERCENT: i64 = 10;
const MAX_AIM_PERCENT: i64 = 85;
/// The biggest score an *aimed* non-finishing dart will target, at SKILL_AIM_START and MAX_SKILL.
const MIN_AIM_CEILING: u32 = 20;
const MAX_AIM_CEILING: u32 = 60;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_list(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("first_flight", "First Flight", "wins", 1),
        ("on_oche", "On the Oche", "wins", 10),
        ("twenty_plenty", "Twenty Plenty", "wins", 20),
        ("nearly_sir", "Nearly, Sir.", "almost", 1),
        ("always_bridesmaid", "Always the Bridesmaid", "almost", 10),
        (
            "saint_close_calls",
            "Patron Saint of Close Calls",
            "almost",
            50,
        ),
    ]
    .into_iter()
    .map(|(id, name, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: match stat {
            "wins" => format!("Win {threshold} darts matches."),
            _ => format!("Finish close to the winner in {threshold} darts matches."),
        },
        stat: stat.into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "bust_move".into(),
        name: "Bust a Move".into(),
        description: "Throw a natural bust.".into(),
        stat: "busts".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: ["wins", "almost", "busts"]
            .into_iter()
            .map(|id| AchievementStat {
                id: id.into(),
                description: id.into(),
            })
            .collect(),
        achievements,
        prestige: vec![jeeves_abi::PrestigeSpec {
            id: "darts_master".into(),
            name: "Darts Master".into(),
            stat: "wins".into(),
            first_threshold: 40,
            every: 20,
        }],
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("stats:{}:", request.server);
    let values = request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&prefix) && !entry.value.is_empty())
        .map(|entry| {
            let profile_id = entry
                .key
                .strip_prefix(&prefix)
                .unwrap_or_default()
                .to_string();
            let stats: Stats = serde_json::from_str(&entry.value)?;
            Ok(AchievementSetMax {
                profile_id,
                stat: "wins".into(),
                value: stats.wins as u64,
            })
        })
        .collect::<Result<Vec<_>, serde_json::Error>>()?;
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values,
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display_name: &str,
    channel: &str,
    stat: &str,
) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: channel.into(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "darts".into(),
                aliases: Vec::new(),
                description: "Play the channel's asynchronous 301 darts match.".into(),
                usage: "!darts [1|2|3 | score | wins | reset]".into(),
            },
            CommandSpec {
                name: "dartsstats".into(),
                aliases: vec!["dstats".into()],
                description: "Show your lifetime darts record.".into(),
                usage: "!dartsstats".into(),
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "cooldown_secs".into(),
                description: "Rest between a player's two turns of three darts.".into(),
                default: DEFAULT_COOLDOWN_SECS.to_string(),
                kind: SettingKind::DurationSeconds {
                    min: 0,
                    max: 24 * 60 * 60,
                },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "daily_dart_cap".into(),
                description: "Maximum darts one player may throw per day.".into(),
                default: DEFAULT_DAILY_CAP.to_string(),
                kind: SettingKind::Integer { min: 1, max: 60 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "skill_decay_per_missed_day".into(),
                description: "Skill points lost for each day a player throws nothing.".into(),
                default: DEFAULT_SKILL_DECAY.to_string(),
                kind: SettingKind::Integer { min: 0, max: 100 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "starting_score".into(),
                description: "Score each player must reduce to exactly zero to win.".into(),
                default: STARTING_SCORE.to_string(),
                kind: SettingKind::Integer { min: 21, max: 1001 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
        ],
    })?)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Player {
    user_id: String,
    nick: String,
    display: String,
    remaining: u32,
    joined_at: i64,
    #[serde(default)]
    turn_darts: u8,
    #[serde(default)]
    cooldown_until: i64,
    /// The cooldown warning is deliberately sent at most once per rest. Without this,
    /// a user holding down Enter can make the bot repeat its cooldown line until the
    /// channel's flood protection intervenes.
    #[serde(default)]
    cooldown_notice_until: i64,
    #[serde(default)]
    match_darts: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Game {
    players: Vec<Player>,
    created_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Stats {
    #[serde(default)]
    display: String,
    wins: u32,
    total_darts: u64,
    best_darts: u32,
    /// Personal skill, 0..=100. Raises the odds an aimed dart lands where it's wanted.
    #[serde(default)]
    skill: i64,
    /// Darts thrown so far on `last_throw_day`; reset when a new day opens.
    #[serde(default)]
    throws_today: u8,
    /// UTC day (seconds/86400) of the player's most recent throw attempt.
    #[serde(default)]
    last_throw_day: i64,
    /// UTC day on which the "you're done for today" line was last sent, so it goes out once.
    #[serde(default)]
    cap_notice_day: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Dart {
    label: String,
    points: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Normal,
    Miss,
    Bust,
    Win,
}

fn game_key(server: &str, channel: &str) -> String {
    format!("game:{server}:{channel}")
}

fn stats_key(server: &str, user_id: &str) -> String {
    format!("stats:{server}:{user_id}")
}

fn lifecycle_stats_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| stats_key(&request.subject.server, identity))
        .collect()
}

fn lifecycle_player_matches(player: &Player, request: &ModuleDataRequest) -> bool {
    player.user_id == request.subject.profile_id
        || request.aliases.iter().any(|alias| {
            player.user_id.eq_ignore_ascii_case(alias) || player.nick.eq_ignore_ascii_case(alias)
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let stats_keys = lifecycle_stats_keys(&request);
    let game_prefix = format!("game:{}:", request.subject.server);
    let mut stats = Vec::new();
    let mut active_games = Vec::new();
    for entry in &request.entries {
        if stats_keys.contains(&entry.key) {
            if entry.value.is_empty() {
                continue;
            }
            stats.push(serde_json::from_str::<serde_json::Value>(&entry.value)?);
        } else if entry.key.starts_with(&game_prefix) {
            if entry.value.is_empty() {
                continue;
            }
            let game: Game = serde_json::from_str(&entry.value)?;
            if let Some(player) = game
                .players
                .into_iter()
                .find(|player| lifecycle_player_matches(player, &request))
            {
                active_games.push(serde_json::json!({ "key": entry.key, "player": player }));
            }
        }
    }
    let data = if stats.is_empty() && active_games.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "stats": stats, "active_games": active_games })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let stats_keys = lifecycle_stats_keys(&request);
    let game_prefix = format!("game:{}:", request.subject.server);
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if stats_keys.contains(&entry.key) {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            });
        } else if entry.key.starts_with(&game_prefix) {
            if entry.value.is_empty() {
                continue;
            }
            let mut game: Game = serde_json::from_str(&entry.value)?;
            let before = game.players.len();
            game.players
                .retain(|player| !lifecycle_player_matches(player, &request));
            if game.players.len() != before {
                let value = if game.players.is_empty() {
                    None
                } else {
                    Some(serde_json::to_string(&game)?)
                };
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value,
                });
            }
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_list_entries() -> Result<Vec<jeeves_abi::ModuleKvEntry>, Error> {
    Ok(serde_json::from_str(&unsafe {
        kv_list(serde_json::to_string(&KvList::default())?)?
    })?)
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

fn load_game(server: &str, channel: &str) -> Result<Game, Error> {
    let raw = kv_load(&game_key(server, channel))?;
    let mut game: Game = serde_json::from_str(&raw).unwrap_or_default();
    // Do not allow legacy nick-only entries to be claimed by a new owner of that nick.
    game.players.retain(|player| !player.user_id.is_empty());
    Ok(game)
}

fn save_game(server: &str, channel: &str, game: &Game) -> Result<(), Error> {
    kv_save(&game_key(server, channel), &serde_json::to_string(game)?)
}

fn clear_game(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&game_key(server, channel), "")
}

fn load_stats(server: &str, user_id: &str) -> Result<Stats, Error> {
    Ok(serde_json::from_str(&kv_load(&stats_key(server, user_id))?).unwrap_or_default())
}

fn save_stats(server: &str, user_id: &str, stats: &Stats) -> Result<(), Error> {
    kv_save(&stats_key(server, user_id), &serde_json::to_string(stats)?)
}

fn now_secs() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn utc_day() -> Result<i64, Error> {
    Ok(now_secs()?.div_euclid(86_400))
}

fn setting_i64(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    (|| -> Option<i64> {
        unsafe {
            setting_get(
                serde_json::to_string(&SettingGet {
                    key: key.into(),
                    server: Some(server.into()),
                    channel: Some(channel.into()),
                })
                .ok()?,
            )
            .ok()?
            .parse()
            .ok()
        }
    })()
    .unwrap_or(fallback)
}

fn host_random(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    Ok(serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes)
}

/// Weighted board from the original: singles 4, doubles 2, triples 1, outer bull 2,
/// bullseye 1, miss 2. Total weight: 145.
fn dart_from_roll(roll: u16) -> Dart {
    let roll = roll % 145;
    match roll {
        0..=79 => {
            let number = (roll / 4) as u32 + 1;
            Dart {
                label: number.to_string(),
                points: number,
            }
        }
        80..=119 => {
            let number = ((roll - 80) / 2) as u32 + 1;
            Dart {
                label: format!("double {number}"),
                points: number * 2,
            }
        }
        120..=139 => {
            let number = (roll - 120) as u32 + 1;
            Dart {
                label: format!("triple {number}"),
                points: number * 3,
            }
        }
        140..=141 => Dart {
            label: "outer bull".into(),
            points: 25,
        },
        142 => Dart {
            label: "bullseye".into(),
            points: 50,
        },
        _ => Dart {
            label: "miss".into(),
            points: 0,
        },
    }
}

/// Every value a single dart can score, mapped to a representative dart with the tidiest label
/// (a plain single beats the double/triple that lands on the same number).
fn legal_darts() -> &'static BTreeMap<u32, Dart> {
    static MAP: OnceLock<BTreeMap<u32, Dart>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut map = BTreeMap::new();
        for n in 1..=20u32 {
            map.insert(
                n * 3,
                Dart {
                    label: format!("triple {n}"),
                    points: n * 3,
                },
            );
        }
        for n in 1..=20u32 {
            map.insert(
                n * 2,
                Dart {
                    label: format!("double {n}"),
                    points: n * 2,
                },
            );
        }
        map.insert(
            25,
            Dart {
                label: "outer bull".into(),
                points: 25,
            },
        );
        map.insert(
            50,
            Dart {
                label: "bullseye".into(),
                points: 50,
            },
        );
        // Singles inserted last so they win the tie for 1..=20.
        for n in 1..=20u32 {
            map.insert(
                n,
                Dart {
                    label: n.to_string(),
                    points: n,
                },
            );
        }
        map
    })
}

/// Percent chance (0..=100) that a dart is aimed rather than thrown at the random board.
fn aim_percent(skill: i64) -> i64 {
    let skill = skill.clamp(0, MAX_SKILL);
    if skill < SKILL_AIM_START {
        return 0;
    }
    MIN_AIM_PERCENT
        + (skill - SKILL_AIM_START) * (MAX_AIM_PERCENT - MIN_AIM_PERCENT)
            / (MAX_SKILL - SKILL_AIM_START)
}

/// The largest score an aimed, non-finishing dart will reach for at this skill.
fn aim_ceiling(skill: i64) -> u32 {
    let skill = skill.clamp(SKILL_AIM_START, MAX_SKILL);
    MIN_AIM_CEILING
        + (skill - SKILL_AIM_START) as u32 * (MAX_AIM_CEILING - MIN_AIM_CEILING)
            / (MAX_SKILL - SKILL_AIM_START) as u32
}

/// If `remaining` can be cleared with a single legal dart, the dart that does it — an aimed
/// throw takes this to finish, regardless of skill ceiling ("hit the number you need to win").
fn checkout_dart(remaining: u32) -> Option<Dart> {
    if remaining == 0 || remaining > MAX_AIM_CEILING {
        return None;
    }
    legal_darts().get(&remaining).cloned()
}

/// An aimed dart when no single-dart finish is available: aim as high as skill allows without
/// busting, with a little jitter so throws vary. Always returns at least a single 1.
fn scoring_dart(remaining: u32, skill: i64, roll: u16) -> Dart {
    let ceiling = aim_ceiling(skill).min(remaining);
    let jitter = (roll % 7) as u32;
    let target = ceiling.saturating_sub(jitter).max(1);
    legal_darts()
        .range(..=target)
        .next_back()
        .map(|(_, dart)| dart.clone())
        .unwrap_or(Dart {
            label: "1".into(),
            points: 1,
        })
}

/// Pick the dart for a single throw: aimed (skill) or random (the weighted board).
fn pick_dart(remaining: u32, skill: i64, aim_roll: u8, value_roll: u16) -> Dart {
    let aimed = (aim_roll as i64 % 100) < aim_percent(skill);
    if aimed {
        checkout_dart(remaining).unwrap_or_else(|| scoring_dart(remaining, skill, value_roll))
    } else {
        dart_from_roll(value_roll)
    }
}

fn apply_dart(remaining: &mut u32, dart: &Dart) -> Outcome {
    if dart.points == 0 {
        return Outcome::Miss;
    }
    if dart.points > *remaining {
        return Outcome::Bust;
    }
    *remaining -= dart.points;
    if *remaining == 0 {
        Outcome::Win
    } else {
        Outcome::Normal
    }
}

fn almost_winners(game: &Game, winner_id: &str) -> Vec<Player> {
    game.players
        .iter()
        .filter(|player| player.user_id != winner_id && player.remaining <= 60)
        .cloned()
        .collect()
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
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

fn identity(msg: &MessagePayload) -> String {
    if msg.user_id.is_empty() {
        format!("nick:{}", msg.nick.to_ascii_lowercase())
    } else {
        msg.user_id.clone()
    }
}

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn throw(server: &str, msg: &MessagePayload, requested: u8) -> Result<(), Error> {
    let channel = &msg.target;
    let now = now_secs()?;
    let today = utc_day()?;
    let cooldown_secs = setting_i64("cooldown_secs", server, channel, DEFAULT_COOLDOWN_SECS);
    let daily_cap = setting_i64("daily_dart_cap", server, channel, DEFAULT_DAILY_CAP).max(1);
    let starting = setting_i64("starting_score", server, channel, STARTING_SCORE as i64)
        .clamp(21, 1001) as u32;
    let user_id = identity(msg);

    // Skill and the daily allowance live in per-player, server-wide stats. Roll the day over
    // first: a new day resets the daily throw count and docks skill for any days missed.
    let mut stats = load_stats(server, &user_id)?;
    stats.display = display(msg).into();
    if stats.last_throw_day != today {
        if stats.last_throw_day != 0 {
            let missed = today - stats.last_throw_day - 1;
            if missed > 0 {
                let decay = setting_i64(
                    "skill_decay_per_missed_day",
                    server,
                    channel,
                    DEFAULT_SKILL_DECAY,
                )
                .max(0);
                stats.skill = (stats.skill - missed * decay).max(0);
            }
        }
        stats.throws_today = 0;
        stats.last_throw_day = today;
        save_stats(server, &user_id, &stats)?;
    }

    // Hard daily cap — the real anti-spam gate. Announce it once, then stay quiet so the bot
    // itself doesn't spam a user who keeps trying.
    if stats.throws_today as i64 >= daily_cap {
        if stats.cap_notice_day == today {
            return Ok(());
        }
        stats.cap_notice_day = today;
        save_stats(server, &user_id, &stats)?;
        return reply(
            server,
            channel,
            &themed(
                "darts.daily_done",
                &["That's your {cap} darts for today, {user} — rest the arm and come back tomorrow to keep your skill sharp."],
                &[("cap", &daily_cap.to_string()), ("user", display(msg))],
            )?,
        );
    }

    let mut game = load_game(server, channel)?;
    if game.created_at == 0 {
        game.created_at = now;
    }

    let existing = game
        .players
        .iter()
        .position(|player| player.user_id == user_id);
    if existing.is_none() && game.players.len() >= MAX_PLAYERS {
        return reply(
            server,
            channel,
            &themed("darts.full", &["The darts match is full."], &[])?,
        );
    }
    if existing.is_none() {
        game.players.push(Player {
            user_id: user_id.clone(),
            nick: msg.nick.clone(),
            display: display(msg).into(),
            remaining: starting,
            joined_at: now,
            ..Default::default()
        });
    }
    let index = game
        .players
        .iter()
        .position(|player| player.user_id == user_id)
        .unwrap();
    if game.players[index].cooldown_until > now {
        let minutes = (game.players[index].cooldown_until - now + 59) / 60;
        let seconds = game.players[index].cooldown_until - now;
        if game.players[index].cooldown_notice_until > now {
            return Ok(());
        }
        game.players[index].cooldown_notice_until = game.players[index].cooldown_until;
        save_game(server, channel, &game)?;
        return reply(
            server,
            channel,
            &themed(
                "darts.cooldown",
                &["{user}'s throwing arm needs a rest: about {minutes} minute(s) remain before your next turn."],
                // `nick` and `secs` retain compatibility with the original cooldown
                // template, which operators may still have in theme.toml.
                &[
                    ("user", display(msg)),
                    ("minutes", &minutes.to_string()),
                    ("nick", display(msg)),
                    ("secs", &seconds.to_string()),
                ],
            )?,
        );
    }

    // Available now = darts left in this turn AND darts left in the day, whichever is smaller.
    let turn_available = MAX_DARTS_PER_TURN.saturating_sub(game.players[index].turn_darts);
    let daily_remaining = (daily_cap - stats.throws_today as i64).max(0) as u8;
    let available = turn_available.min(daily_remaining);
    if requested > available {
        return reply(
            server,
            channel,
            &themed(
                "darts.turn_limit",
                &["You have only {count} dart(s) left just now, {user}."],
                &[("count", &available.to_string()), ("user", display(msg))],
            )?,
        );
    }

    // Three bytes per dart: one to decide aimed-vs-random, two for the value.
    let bytes = host_random(requested as usize * 3)?;
    let mut results = Vec::new();
    let mut won = false;
    for chunk in bytes.chunks_exact(3) {
        let dart = pick_dart(
            game.players[index].remaining,
            stats.skill,
            chunk[0],
            u16::from_le_bytes([chunk[1], chunk[2]]),
        );
        let outcome = apply_dart(&mut game.players[index].remaining, &dart);
        game.players[index].turn_darts += 1;
        game.players[index].match_darts += 1;
        // Every dart thrown — hit, miss, or bust — earns a skill point (capped) and counts
        // against the daily allowance.
        stats.throws_today = stats.throws_today.saturating_add(1);
        stats.skill = (stats.skill + 1).min(MAX_SKILL);
        results.push((dart, outcome));
        if matches!(outcome, Outcome::Miss | Outcome::Bust | Outcome::Win) {
            won = outcome == Outcome::Win;
            break;
        }
    }
    game.players[index].nick = msg.nick.clone();
    game.players[index].display = display(msg).into();

    let details = results
        .iter()
        .map(|(dart, outcome)| match outcome {
            Outcome::Normal => format!("{} ({} pts)", dart.label, dart.points),
            Outcome::Miss => "miss (turn ends)".into(),
            Outcome::Bust => format!("{} ({} pts) — bust", dart.label, dart.points),
            Outcome::Win => format!("{} ({} pts) — exactly zero", dart.label, dart.points),
        })
        .collect::<Vec<_>>()
        .join(" · ");

    if won {
        let darts = game.players[index].match_darts;
        let almost = almost_winners(&game, &user_id);
        stats.wins += 1;
        stats.total_darts += darts as u64;
        if stats.best_darts == 0 || darts < stats.best_darts {
            stats.best_darts = darts;
        }
        save_stats(server, &user_id, &stats)?;
        clear_game(server, channel)?;
        reply(
            server,
            channel,
            &themed(
                "darts.win",
                &["{user} throws {throws}. Magnificent — exactly zero in {count} darts! The match is complete."],
                &[("user", display(msg)), ("throws", &details), ("count", &darts.to_string())],
            )?,
        )?;
        award(server, &user_id, display(msg), channel, "wins")?;
        for player in almost {
            award(server, &player.user_id, &player.display, channel, "almost")?;
        }
        return Ok(());
    }

    let remaining = game.players[index].remaining;
    if game.players[index].turn_darts >= MAX_DARTS_PER_TURN {
        game.players[index].turn_darts = 0;
        game.players[index].cooldown_until = now.saturating_add(cooldown_secs);
    }
    let resting = game.players[index].cooldown_until > now;
    let daily_done = stats.throws_today as i64 >= daily_cap;
    save_game(server, channel, &game)?;
    save_stats(server, &user_id, &stats)?;
    reply(
        server,
        channel,
        &themed(
            if daily_done {
                "darts.throw_last"
            } else if resting {
                "darts.throw_rest"
            } else {
                "darts.throw"
            },
            if daily_done {
                &["{user} throws: {throws}. {remaining} remain — that's all {cap} darts for today."]
            } else if resting {
                &["{user} throws: {throws}. {remaining} remain. That turn's done; rest the arm before your next three."]
            } else {
                &["{user} throws: {throws}. {remaining} remain."]
            },
            &[
                ("user", display(msg)),
                ("throws", &details),
                ("remaining", &remaining.to_string()),
                ("cap", &daily_cap.to_string()),
            ],
        )?,
    )?;
    if results.iter().any(|(_, outcome)| *outcome == Outcome::Bust) {
        award(server, &user_id, display(msg), channel, "busts")?;
    }
    Ok(())
}

fn score(server: &str, channel: &str) -> Result<(), Error> {
    let mut game = load_game(server, channel)?;
    if game.players.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "darts.empty",
                &["No active darts match. Use !darts to begin."],
                &[],
            )?,
        );
    }
    game.players.sort_by_key(|player| player.remaining);
    let board = game
        .players
        .iter()
        .take(10)
        .map(|player| format!("{}: {}", player.display, player.remaining))
        .collect::<Vec<_>>()
        .join(" | ");
    reply(
        server,
        channel,
        &themed("darts.score", &["{board}"], &[("board", &board)])?,
    )
}

fn stats(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let user_id = identity(msg);
    let mut stats = load_stats(server, &user_id)?;
    if stats.wins > 0 && stats.display != display(msg) {
        stats.display = display(msg).into();
        save_stats(server, &user_id, &stats)?;
    }
    let average = if stats.wins == 0 {
        "—".into()
    } else {
        format!("{:.1}", stats.total_darts as f64 / stats.wins as f64)
    };
    reply(
        server,
        &msg.target,
        &themed(
            "darts.stats",
            &["{user}: skill {skill}/100 — {wins} win(s), average {average} darts, best {best}."],
            &[
                ("user", display(msg)),
                ("skill", &stats.skill.clamp(0, MAX_SKILL).to_string()),
                ("wins", &stats.wins.to_string()),
                ("average", &average),
                ("best", &stats.best_darts.to_string()),
            ],
        )?,
    )
}

fn wins(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let user_id = identity(msg);
    let mut own_stats = load_stats(server, &user_id)?;
    if own_stats.wins > 0 && own_stats.display != display(msg) {
        own_stats.display = display(msg).into();
        save_stats(server, &user_id, &own_stats)?;
    }
    let prefix = format!("stats:{server}:");
    let mut leaders = kv_list_entries()?
        .into_iter()
        .filter_map(|entry| {
            let user_id = entry.key.strip_prefix(&prefix)?.to_string();
            let stats = serde_json::from_str::<Stats>(&entry.value).ok()?;
            (stats.wins > 0).then_some((stats, user_id))
        })
        .collect::<Vec<_>>();
    leaders.sort_by(|(left, left_id), (right, right_id)| {
        right
            .wins
            .cmp(&left.wins)
            .then_with(|| left.total_darts.cmp(&right.total_darts))
            .then_with(|| left_id.cmp(right_id))
    });
    let leaders = leaders
        .iter()
        .take(5)
        .map(|(stats, user_id)| {
            let display = if stats.display.is_empty() {
                user_id.as_str()
            } else {
                stats.display.as_str()
            };
            format!("{display} ({})", stats.wins)
        })
        .collect::<Vec<_>>()
        .join(", ");
    let leaders = if leaders.is_empty() {
        "No darts wins have been recorded yet.".into()
    } else {
        leaders
    };
    reply(
        server,
        &msg.target,
        &themed(
            "darts.wins",
            &["Darts wins: {leaders}"],
            &[("leaders", &leaders)],
        )?,
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let token = msg
        .text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(token.as_str(), "!darts" | "!dartsstats" | "!dstats") {
        return Ok(());
    }
    if msg.is_private {
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "darts.channel_only",
                &["Darts is played in a channel."],
                &[],
            )?,
        )?;
        return Ok(());
    }
    if matches!(token.as_str(), "!dartsstats" | "!dstats") {
        stats(&env.server, &msg)?;
        return Ok(());
    }
    let rest = msg.text[token.len()..].trim().to_ascii_lowercase();
    match rest.as_str() {
        "" => throw(&env.server, &msg, 1)?,
        "1" | "2" | "3" => throw(&env.server, &msg, rest.parse().unwrap_or(1))?,
        "score" | "board" => score(&env.server, &msg.target)?,
        "wins" => wins(&env.server, &msg)?,
        "reset" if msg.role.is_some_and(|role| role.satisfies(Role::Admin)) => {
            clear_game(&env.server, &msg.target)?;
            reply(
                &env.server,
                &msg.target,
                &themed("darts.reset", &["The darts match has been reset."], &[])?,
            )?;
        }
        "reset" => reply(
            &env.server,
            &msg.target,
            &themed(
                "darts.reset_denied",
                &["Only an administrator may reset the darts match."],
                &[],
            )?,
        )?,
        _ => reply(
            &env.server,
            &msg.target,
            &themed(
                "darts.usage",
                &["Usage: !darts [1|2|3 | score | wins | reset]"],
                &[],
            )?,
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_board_boundaries() {
        assert_eq!(
            dart_from_roll(0),
            Dart {
                label: "1".into(),
                points: 1
            }
        );
        assert_eq!(dart_from_roll(79).points, 20);
        assert_eq!(dart_from_roll(80).points, 2);
        assert_eq!(dart_from_roll(119).points, 40);
        assert_eq!(dart_from_roll(139).points, 60);
        assert_eq!(dart_from_roll(142).points, 50);
        assert_eq!(dart_from_roll(144).points, 0);
    }

    #[test]
    fn darts_are_applied_sequentially() {
        let mut remaining = 20;
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "5".into(),
                    points: 5
                }
            ),
            Outcome::Normal
        );
        assert_eq!(remaining, 15);
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "20".into(),
                    points: 20
                }
            ),
            Outcome::Bust
        );
        assert_eq!(remaining, 15);
    }

    #[test]
    fn exact_dart_wins_immediately() {
        let mut remaining = 20;
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "double 10".into(),
                    points: 20
                }
            ),
            Outcome::Win
        );
        assert_eq!(remaining, 0);
    }

    #[test]
    fn almost_winners_are_selected_only_at_sixty_or_less() {
        let player = |id: &str, remaining| Player {
            user_id: id.into(),
            remaining,
            ..Default::default()
        };
        let game = Game {
            players: vec![player("winner", 0), player("close", 60), player("far", 61)],
            created_at: 0,
        };
        assert_eq!(
            almost_winners(&game, "winner")
                .iter()
                .map(|player| player.user_id.as_str())
                .collect::<Vec<_>>(),
            ["close"]
        );
    }

    #[test]
    fn aim_chance_ramps_from_novice_to_expert() {
        assert_eq!(aim_percent(0), 0);
        assert_eq!(aim_percent(9), 0);
        assert_eq!(aim_percent(SKILL_AIM_START), MIN_AIM_PERCENT);
        assert_eq!(aim_percent(MAX_SKILL), MAX_AIM_PERCENT);
        assert_eq!(aim_percent(1_000), MAX_AIM_PERCENT); // clamped
        assert!(aim_percent(55) > aim_percent(30));
    }

    #[test]
    fn aim_ceiling_grows_with_skill() {
        assert_eq!(aim_ceiling(0), MIN_AIM_CEILING); // clamped up to the start
        assert_eq!(aim_ceiling(SKILL_AIM_START), MIN_AIM_CEILING);
        assert_eq!(aim_ceiling(MAX_SKILL), MAX_AIM_CEILING);
        assert!(aim_ceiling(60) > aim_ceiling(20));
    }

    #[test]
    fn legal_darts_prefer_the_tidiest_label() {
        let darts = legal_darts();
        assert_eq!(darts[&6].label, "6"); // plain single, not triple 2 / double 3
        assert_eq!(darts[&40].label, "double 20"); // no single reaches 40
        assert_eq!(darts[&60].label, "triple 20");
        assert_eq!(darts[&50].label, "bullseye");
        assert_eq!(darts[&25].label, "outer bull");
        assert!(!darts.contains_key(&59)); // no single dart makes 59
    }

    #[test]
    fn checkout_dart_finishes_when_possible() {
        assert_eq!(checkout_dart(40).map(|d| d.points), Some(40));
        assert_eq!(checkout_dart(50).map(|d| d.label), Some("bullseye".into()));
        assert_eq!(checkout_dart(0), None);
        assert_eq!(checkout_dart(61), None); // beyond a single dart's reach
        assert_eq!(checkout_dart(59), None); // no single-dart score equals 59
    }

    #[test]
    fn scoring_dart_never_busts_and_reaches_high_with_skill() {
        // Whatever the roll, an aimed scoring dart stays within the remaining score.
        for roll in 0..200u16 {
            let dart = scoring_dart(45, MAX_SKILL, roll);
            assert!(dart.points <= 45, "aimed dart {} busted 45", dart.points);
            assert!(dart.points >= 1);
        }
        // With a big score to chip at, an expert reaches for the top of the board.
        assert!(scoring_dart(180, MAX_SKILL, 0).points >= 50);
        // A near-novice aims far lower even with room to spare.
        assert!(scoring_dart(180, SKILL_AIM_START, 0).points <= MIN_AIM_CEILING);
    }

    #[test]
    fn pick_dart_is_pure_random_at_zero_skill() {
        // aim_percent(0) == 0, so no aim roll can select an aimed dart: it must match the board.
        for value in [0u16, 79, 80, 142, 144] {
            for aim_roll in [0u8, 128, 255] {
                assert_eq!(
                    pick_dart(200, 0, aim_roll, value),
                    dart_from_roll(value),
                    "zero-skill throw should be the plain random board"
                );
            }
        }
    }

    #[test]
    fn pick_dart_takes_the_checkout_when_aiming() {
        // aim_roll 0 is below any positive aim chance, so a skilled player aims — and a legal
        // finish is taken exactly.
        let dart = pick_dart(40, MAX_SKILL, 0, 12_345);
        assert_eq!(dart.points, 40);
    }
}
