//! Daily personal six-letter Wordle and the resumable multi-floor Wordle Tower.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, EconomyTransactionRequest, Event, EventEnvelope, KvGet, KvSet, MessagePayload,
    ModuleAdminCommandRequest, ModuleAdminCommandResponse, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey,
    RandomBytesRequest, RandomBytesResponse, Role, SendMessage, SettingGet, SettingKind,
    SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::OnceLock;

const WORD_LENGTH: usize = 6;
const DEFAULT_MAX_ATTEMPTS: i64 = 3;
const MAX_ACTIVE_USERS: usize = 2_000;
const MAX_STATS_USERS: usize = 2_000;
const USED_WORD_WINDOW: usize = 4_096;
const MERCY_REROLL_AFTER_FAILED_DAYS: u8 = 2;
const TOWER_START_FLOOR: u8 = 5;
const TOWER_MAX_FLOOR: u8 = 8;
const TOWER_GUESSES: usize = 6;
const TOWER_PROMOTION_SOLVES: u8 = 4;
const TOWER_MAX_STRIKES: u8 = 3;
const TOWER_USED_WORD_WINDOW: usize = 512;
const MAX_FREE_ROOMS: usize = 64;
const DEFAULT_GAME_ROOM: &str = "#games";

#[cfg(not(test))]
#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn economy_award(input: String) -> String;
    fn profile_get(input: String) -> String;
}

// Native Rust test binaries cannot resolve Extism's WASM host imports. Keep the Wordle logic
// tests runnable on the host with deterministic doubles; the real imports above remain active
// for every production/WASM build.
#[cfg(test)]
unsafe fn send_message(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn theme(input: String) -> Result<String, Error> {
    Ok(input)
}

#[cfg(test)]
unsafe fn kv_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn kv_set(_: String) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
unsafe fn now(_: String) -> Result<String, Error> {
    Ok("0".into())
}

#[cfg(test)]
unsafe fn setting_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn random_bytes(input: String) -> Result<String, Error> {
    let request: RandomBytesRequest = serde_json::from_str(&input)?;
    let bytes = (0..request.count).map(|index| index as u8).collect();
    Ok(serde_json::to_string(&RandomBytesResponse { bytes })?)
}

#[cfg(test)]
unsafe fn award_stats(_: String) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
unsafe fn economy_award(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn profile_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("letter_opener", "Letter Opener", "letters", 10),
        (
            "alphabetical_advantage",
            "Alphabetical Advantage",
            "letters",
            50,
        ),
        ("knows_letters", "Knows Their Letters", "letters", 200),
        (
            "right_letter_place",
            "Right Letter, Right Place",
            "positions",
            10,
        ),
        (
            "pattern_behaviour",
            "A Pattern of Behaviour",
            "positions",
            50,
        ),
        (
            "everything_place",
            "Everything in Its Place",
            "positions",
            200,
        ),
        ("word_wise", "A Word to the Wise", "wins", 1),
        ("chosen_words", "Well Chosen Words", "wins", 10),
        (
            "lexicographer_victorious",
            "Lexicographer Victorious",
            "wins",
            25,
        ),
    ]
    .into_iter()
    .map(|(id, name, stat, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: match stat {
            "letters" => format!("Reveal {threshold} previously unknown present letters."),
            "positions" => format!("Reveal {threshold} previously unknown exact positions."),
            _ => format!("Solve {threshold} daily Wordles."),
        },
        stat: stat.into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.extend(
        [
            ("blind_luck", "Blind Luck, Sir", "first_guess"),
            (
                "skin_six_letters",
                "By the Skin of Six Letters",
                "final_attempt",
            ),
        ]
        .into_iter()
        .map(|(id, name, stat)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: if stat == "first_guess" {
                "Solve a Wordle with your first guess of the day.".into()
            } else {
                "Solve a Wordle on your final allowed attempt.".into()
            },
            stat: stat.into(),
            threshold: 1,
            optional: true,
            secret: true,
        }),
    );
    achievements.extend(
        [
            ("tower_solver", "The Tower Stirs", "tower_solves", 1),
            ("tower_ascender", "Upward Bound", "tower_promotions", 1),
            ("tower_veteran", "Tower Veteran", "tower_solves", 25),
        ]
        .into_iter()
        .map(|(id, name, stat, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: if stat == "tower_promotions" {
                "Promote to a higher Tower floor.".into()
            } else {
                format!("Solve {threshold} Tower puzzles.")
            },
            stat: stat.into(),
            threshold,
            optional: false,
            secret: false,
        }),
    );
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: [
            "letters",
            "positions",
            "wins",
            "first_guess",
            "final_attempt",
            "tower_solves",
            "tower_promotions",
            "tower_highest_floor",
        ]
        .into_iter()
        .map(|id| AchievementStat {
            id: id.into(),
            description: id.into(),
        })
        .collect(),
        achievements,
        prestige: vec![jeeves_abi::PrestigeSpec {
            id: "wordle_master".into(),
            name: "Wordle Master".into(),
            stat: "wins".into(),
            first_threshold: 50,
            every: 25,
        }],
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let stats_values = request
        .entries
        .iter()
        .find(|entry| entry.key == stats_key(&request.server))
        .map(|entry| serde_json::from_str::<Vec<UserStats>>(&entry.value))
        .transpose()?;
    let daily_values = request
        .entries
        .iter()
        .find(|entry| entry.key == state_key(&request.server))
        .map(|entry| serde_json::from_str::<Daily>(&entry.value))
        .transpose()?;
    let mut values = stats_values
        .unwrap_or_default()
        .into_iter()
        .filter(|stats| !stats.user_id.is_empty() && !stats.user_id.starts_with("nick:"))
        .map(|stats| AchievementSetMax {
            profile_id: stats.user_id,
            stat: "wins".into(),
            value: stats.wins as u64,
        })
        .collect::<Vec<_>>();
    if let Some(daily) = daily_values {
        values.extend(
            daily
                .tower
                .into_iter()
                .filter(|player| !player.user_id.is_empty() && !player.user_id.starts_with("nick:"))
                .flat_map(|player| {
                    [
                        AchievementSetMax {
                            profile_id: player.user_id.clone(),
                            stat: "tower_solves".into(),
                            value: player.total_solves as u64,
                        },
                        AchievementSetMax {
                            profile_id: player.user_id.clone(),
                            stat: "tower_highest_floor".into(),
                            value: player.highest_floor_ever as u64,
                        },
                    ]
                }),
        );
    }
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values,
    })?)
}

fn award(server: &str, msg: &MessagePayload, increments: Vec<(&str, u64)>) -> Result<(), Error> {
    let increments = increments
        .into_iter()
        .filter(|(_, amount)| *amount > 0)
        .map(|(stat, amount)| StatIncrement {
            stat: stat.into(),
            amount,
        })
        .collect::<Vec<_>>();
    if msg.user_id.is_empty() || increments.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments,
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

fn award_brass(
    server: &str,
    msg: &MessagePayload,
    amount: u64,
    event_id: &str,
    reason: &str,
) -> Result<(), Error> {
    if msg.user_id.is_empty() || amount == 0 {
        return Ok(());
    }
    unsafe {
        economy_award(serde_json::to_string(&EconomyTransactionRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            amount,
            event_id: event_id.into(),
            reason: reason.into(),
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
                name: "word".into(),
                aliases: vec!["wordle".into()],
                description: "Play or inspect your daily personal six-letter Wordle.".into(),
                usage: "!word [<guess> | stats | score | top | new]".into(),
            },
            CommandSpec {
                name: "tower".into(),
                aliases: vec!["wt".into()],
                description: "Climb the persistent personal Wordle Tower.".into(),
                usage: "!wordle tower [<guess> | stats | top]".into(),
            },
            CommandSpec {
                name: "guess".into(),
                aliases: Vec::new(),
                description: "Compatibility command for guessing today's Wordle.".into(),
                usage: "!guess <word>".into(),
            },
            CommandSpec {
                name: "wordlestats".into(),
                aliases: vec!["wstats".into()],
                description: "Show your daily Wordle record.".into(),
                usage: "!wordlestats".into(),
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
                key: "max_attempts_per_user".into(),
                description: "Guesses each person receives per Wordle day.".into(),
                default: DEFAULT_MAX_ATTEMPTS.to_string(),
                kind: SettingKind::Integer { min: 1, max: 10 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "free_play_enabled".into(),
                description: "Give this channel an independent endless Wordle and Tower game."
                    .into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "game_room".into(),
                description: "Channel where normal Wordle and Tower play is available.".into(),
                default: DEFAULT_GAME_ROOM.into(),
                kind: SettingKind::String { max_len: 64 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
            SettingSpec {
                key: "free_answer_pool".into(),
                description: "Answer pool used by free-play six-letter puzzles.".into(),
                default: "curated".into(),
                kind: SettingKind::Choice {
                    options: vec!["curated".into(), "full".into()],
                },
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct UserGuesses {
    user_id: String,
    display: String,
    guesses: Vec<String>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Yesterday {
    word: String,
    solved: bool,
    #[serde(default)]
    solved_by_id: String,
    solved_by: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Daily {
    #[serde(default)]
    players: Vec<PlayerDaily>,
    #[serde(default)]
    tower: Vec<TowerPlayer>,
    #[serde(default)]
    free_rooms: Vec<FreeRoom>,
    // Pre-personal-Wordle fields are retained solely to migrate an existing saved game.
    #[serde(default)]
    day: i64,
    #[serde(default)]
    word: String,
    #[serde(default)]
    solved: bool,
    #[serde(default)]
    solved_by_id: String,
    #[serde(default)]
    solved_by_display: String,
    #[serde(default)]
    guesses: Vec<UserGuesses>,
    #[serde(default)]
    correct: Vec<Option<char>>,
    #[serde(default)]
    present: Vec<char>,
    #[serde(default)]
    absent: Vec<char>,
    #[serde(default)]
    used_words: Vec<String>,
    #[serde(default)]
    yesterday: Option<Yesterday>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct FreeRoom {
    channel: String,
    #[serde(default)]
    players: Vec<PlayerDaily>,
    #[serde(default)]
    tower: Vec<TowerPlayer>,
    #[serde(default)]
    stats: Vec<UserStats>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct PlayerDaily {
    user_id: String,
    display: String,
    day: i64,
    word: String,
    solved: bool,
    guesses: Vec<String>,
    correct: Vec<Option<char>>,
    present: Vec<char>,
    absent: Vec<char>,
    used_words: Vec<String>,
    #[serde(default)]
    chances_remaining: Option<usize>,
    /// Number of UTC days on which this unsolved word used every available guess.
    #[serde(default)]
    failed_days: u8,
}

fn tower_start_floor() -> u8 {
    TOWER_START_FLOOR
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct TowerPlayer {
    user_id: String,
    display: String,
    #[serde(default = "tower_start_floor")]
    floor: u8,
    #[serde(default)]
    promotion_streak: u8,
    #[serde(default)]
    strikes: u8,
    #[serde(default)]
    answer: String,
    #[serde(default)]
    guesses: Vec<String>,
    #[serde(default)]
    correct: Vec<Option<char>>,
    #[serde(default)]
    present: Vec<char>,
    #[serde(default)]
    absent: Vec<char>,
    #[serde(default)]
    used_words: Vec<String>,
    #[serde(default)]
    locked_until_day: Option<i64>,
    #[serde(default)]
    run_solves: u32,
    #[serde(default)]
    run_started_at: Option<i64>,
    #[serde(default = "tower_start_floor")]
    highest_floor_ever: u8,
    #[serde(default)]
    total_solves: u32,
    #[serde(default)]
    longest_run: u32,
    #[serde(default)]
    fastest_promotion_secs: Option<i64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct UserStats {
    user_id: String,
    display: String,
    wins: u32,
    games_played: u32,
    #[serde(default)]
    total_attempts: u64,
}

fn six_letter_lines(raw: &'static str) -> Vec<&'static str> {
    raw.lines()
        .filter(|word| {
            word.len() == WORD_LENGTH && word.bytes().all(|byte| byte.is_ascii_lowercase())
        })
        .collect()
}

fn letter_lines(raw: &'static str, length: usize) -> Vec<&'static str> {
    raw.lines()
        .filter(|word| word.len() == length && word.bytes().all(|byte| byte.is_ascii_lowercase()))
        .collect()
}

/// The full permissive list — every accepted *guess*. Includes obscure but real words so a
/// player is never told a genuine word "isn't in the dictionary".
fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| six_letter_lines(include_str!("../../../wordle-six-letter-words.txt")))
}

/// The curated *answer* pool: common words only (frequency-filtered, profanity-stripped), a
/// strict subset of `words()`. This is what the bot actually hands players to solve, so nobody
/// gets stuck on a Scrabble oddity like "tuyers". Every answer is still a valid guess.
fn answers() -> &'static [&'static str] {
    static ANSWERS: OnceLock<Vec<&'static str>> = OnceLock::new();
    ANSWERS.get_or_init(|| {
        let answers = six_letter_lines(include_str!("../../../wordle-six-letter-answers.txt"));
        // Never leave the answer pool empty (e.g. a truncated file): fall back to the full list.
        if answers.is_empty() {
            words().to_vec()
        } else {
            answers
        }
    })
}

fn tower_five_words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| letter_lines(include_str!("../../../wordle-5-letter-words.txt"), 5))
}

fn tower_five_answers() -> &'static [&'static str] {
    static ANSWERS: OnceLock<Vec<&'static str>> = OnceLock::new();
    ANSWERS.get_or_init(|| letter_lines(include_str!("../../../wordle-5-letter-answers.txt"), 5))
}

fn tower_seven_words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| letter_lines(include_str!("../../../wordle-7-letter-words.txt"), 7))
}

fn tower_seven_answers() -> &'static [&'static str] {
    static ANSWERS: OnceLock<Vec<&'static str>> = OnceLock::new();
    ANSWERS.get_or_init(|| letter_lines(include_str!("../../../wordle-7-letter-answers.txt"), 7))
}

fn tower_eight_words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| letter_lines(include_str!("../../../wordle-8-letter-words.txt"), 8))
}

fn tower_eight_answers() -> &'static [&'static str] {
    static ANSWERS: OnceLock<Vec<&'static str>> = OnceLock::new();
    ANSWERS.get_or_init(|| letter_lines(include_str!("../../../wordle-8-letter-answers.txt"), 8))
}

fn tower_words(floor: u8) -> &'static [&'static str] {
    match floor {
        5 => tower_five_words(),
        6 => words(),
        7 => tower_seven_words(),
        8 => tower_eight_words(),
        _ => &[],
    }
}

fn tower_answers(floor: u8) -> &'static [&'static str] {
    match floor {
        5 => tower_five_answers(),
        6 => answers(),
        7 => tower_seven_answers(),
        8 => tower_eight_answers(),
        _ => &[],
    }
}

fn valid_word(word: &str) -> bool {
    words().binary_search(&word).is_ok()
}

fn valid_tower_word(word: &str, floor: u8) -> bool {
    tower_words(floor).binary_search(&word).is_ok()
}

fn state_key(server: &str) -> String {
    format!("daily:{server}")
}

fn stats_key(server: &str) -> String {
    format!("stats:{server}")
}

fn lifecycle_identity_matches(id: &str, display: &str, request: &ModuleDataRequest) -> bool {
    id == request.subject.profile_id
        || request.aliases.iter().any(|alias| {
            id.eq_ignore_ascii_case(alias)
                || display.eq_ignore_ascii_case(alias)
                || display
                    .to_ascii_lowercase()
                    .ends_with(&format!(" {}", alias.to_ascii_lowercase()))
        })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let daily = request
        .entries
        .iter()
        .find(|entry| entry.key == state_key(&request.subject.server))
        .map(|entry| {
            let mut daily = serde_json::from_str::<Daily>(&entry.value)?;
            migrate_shared_game(&mut daily);
            Ok::<_, serde_json::Error>(daily)
        })
        .transpose()?;
    let stats = request
        .entries
        .iter()
        .find(|entry| entry.key == stats_key(&request.subject.server))
        .map(|entry| serde_json::from_str::<Vec<UserStats>>(&entry.value))
        .transpose()?
        .and_then(|stats| {
            stats
                .into_iter()
                .find(|stats| lifecycle_identity_matches(&stats.user_id, &stats.display, &request))
        });
    let player = daily.as_ref().and_then(|daily| {
        daily
            .players
            .iter()
            .find(|player| lifecycle_identity_matches(&player.user_id, &player.display, &request))
            .cloned()
    });
    let tower = daily.as_ref().and_then(|daily| {
        daily
            .tower
            .iter()
            .find(|player| lifecycle_identity_matches(&player.user_id, &player.display, &request))
            .cloned()
    });
    let free_rooms = daily.as_ref().map(|daily| {
        daily
            .free_rooms
            .iter()
            .filter_map(|room| {
                let player = room
                    .players
                    .iter()
                    .find(|player| {
                        lifecycle_identity_matches(&player.user_id, &player.display, &request)
                    })
                    .cloned();
                let tower = room
                    .tower
                    .iter()
                    .find(|player| {
                        lifecycle_identity_matches(&player.user_id, &player.display, &request)
                    })
                    .cloned();
                let stats = room
                    .stats
                    .iter()
                    .find(|stats| {
                        lifecycle_identity_matches(&stats.user_id, &stats.display, &request)
                    })
                    .cloned();
                (player.is_some() || tower.is_some() || stats.is_some()).then_some(
                    serde_json::json!({
                        "channel": room.channel,
                        "stats": stats,
                        "current_game": player,
                        "tower": tower,
                    }),
                )
            })
            .collect::<Vec<_>>()
    });
    let has_free_rooms = free_rooms.as_ref().is_some_and(|rooms| !rooms.is_empty());
    let data = if stats.is_none() && player.is_none() && tower.is_none() && !has_free_rooms {
        serde_json::Value::Null
    } else {
        serde_json::json!({
            "stats": stats,
            "current_game": player,
            "tower": tower,
            "free_rooms": free_rooms.unwrap_or_default(),
        })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let daily_key = state_key(&request.subject.server);
    let stats_key = stats_key(&request.subject.server);
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if entry.key == daily_key {
            let mut daily: Daily = serde_json::from_str(&entry.value)?;
            migrate_shared_game(&mut daily);
            let before = daily.players.len();
            daily.players.retain(|player| {
                !lifecycle_identity_matches(&player.user_id, &player.display, &request)
            });
            let tower_before = daily.tower.len();
            daily.tower.retain(|player| {
                !lifecycle_identity_matches(&player.user_id, &player.display, &request)
            });
            let mut free_changed = false;
            for room in &mut daily.free_rooms {
                let room_players_before = room.players.len();
                room.players.retain(|player| {
                    !lifecycle_identity_matches(&player.user_id, &player.display, &request)
                });
                let room_tower_before = room.tower.len();
                room.tower.retain(|player| {
                    !lifecycle_identity_matches(&player.user_id, &player.display, &request)
                });
                let room_stats_before = room.stats.len();
                room.stats.retain(|stats| {
                    !lifecycle_identity_matches(&stats.user_id, &stats.display, &request)
                });
                free_changed |= room_players_before != room.players.len()
                    || room_tower_before != room.tower.len()
                    || room_stats_before != room.stats.len();
            }
            let free_rooms_before = daily.free_rooms.len();
            daily.free_rooms.retain(|room| {
                !room.players.is_empty() || !room.tower.is_empty() || !room.stats.is_empty()
            });
            free_changed |= free_rooms_before != daily.free_rooms.len();
            let changed =
                before != daily.players.len() || tower_before != daily.tower.len() || free_changed;
            if changed {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: Some(serde_json::to_string(&daily)?),
                });
            }
        } else if entry.key == stats_key {
            let mut stats: Vec<UserStats> = serde_json::from_str(&entry.value)?;
            let before = stats.len();
            stats.retain(|stats| {
                !lifecycle_identity_matches(&stats.user_id, &stats.display, &request)
            });
            if stats.len() != before {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: Some(serde_json::to_string(&stats)?),
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

fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn load_daily(server: &str) -> Result<Daily, Error> {
    let mut daily = serde_json::from_str(&kv_load(&state_key(server))?).unwrap_or_default();
    migrate_shared_game(&mut daily);
    Ok(daily)
}

fn save_daily(server: &str, daily: &Daily) -> Result<(), Error> {
    kv_save(&state_key(server), &serde_json::to_string(daily)?)
}

fn load_stats(server: &str) -> Result<Vec<UserStats>, Error> {
    Ok(serde_json::from_str(&kv_load(&stats_key(server))?).unwrap_or_default())
}

fn save_stats(server: &str, stats: &[UserStats]) -> Result<(), Error> {
    kv_save(&stats_key(server), &serde_json::to_string(stats)?)
}

fn now_secs() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn utc_day() -> Result<i64, Error> {
    Ok(now_secs()?.div_euclid(86_400))
}

fn setting_bool(key: &str, server: &str, channel: &str, fallback: bool) -> bool {
    (|| -> Option<bool> {
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

fn setting_string(key: &str, server: &str, channel: &str, fallback: &str) -> String {
    (|| -> Option<String> {
        let value = unsafe {
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
        let value = value.trim();
        (!value.is_empty()).then_some(value.to_string())
    })()
    .unwrap_or_else(|| fallback.into())
}

fn game_room(server: &str, channel: &str) -> String {
    setting_string("game_room", server, channel, DEFAULT_GAME_ROOM)
}

fn in_game_room(server: &str, channel: &str) -> bool {
    room_key(channel) == room_key(&game_room(server, channel))
}

fn room_redirect(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let room = game_room(server, &msg.target);
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.room_redirect",
            &["The games have decamped to {room}, {user}. Do join us there if you intend to make a spectacle of yourself."],
            &[("room", &room), ("user", display(msg))],
        )?,
    )
}

fn free_play_enabled(server: &str, channel: &str) -> bool {
    setting_bool("free_play_enabled", server, channel, false)
}

fn free_answer_pool_enabled(server: &str, channel: &str) -> bool {
    (|| -> Option<bool> {
        unsafe {
            let value = setting_get(
                serde_json::to_string(&SettingGet {
                    key: "free_answer_pool".into(),
                    server: Some(server.into()),
                    channel: Some(channel.into()),
                })
                .ok()?,
            )
            .ok()?;
            Some(value.eq_ignore_ascii_case("full"))
        }
    })()
    .unwrap_or(false)
}

fn attempts_setting(server: &str, channel: &str) -> i64 {
    (|| -> Option<i64> {
        unsafe {
            setting_get(
                serde_json::to_string(&SettingGet {
                    key: "max_attempts_per_user".into(),
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
    .unwrap_or(DEFAULT_MAX_ATTEMPTS)
}

fn remaining_attempts(player: &PlayerDaily, configured_max: usize) -> usize {
    player
        .chances_remaining
        .unwrap_or_else(|| configured_max.saturating_sub(player.guesses.len()))
}

fn consume_attempt(player: &mut PlayerDaily) {
    if let Some(remaining) = &mut player.chances_remaining {
        *remaining = remaining.saturating_sub(1);
    }
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn host_random(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    Ok(serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes)
}

fn choose_word(used: &[String], random: u64) -> String {
    choose_from_pool(answers(), used, random)
}

fn choose_free_word(used: &[String], random: u64, full_pool: bool) -> String {
    choose_from_pool(if full_pool { words() } else { answers() }, used, random)
}

fn choose_from_pool(answers: &'static [&'static str], used: &[String], random: u64) -> String {
    let used = used.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let available = answers
        .iter()
        .copied()
        .filter(|word| !used.contains(word))
        .collect::<Vec<_>>();
    let pool = if available.is_empty() {
        answers.to_vec()
    } else {
        available
    };
    pool.get((random as usize) % pool.len())
        .copied()
        .unwrap_or_default()
        .to_string()
}

fn choose_tower_word(
    floor: u8,
    used: &[String],
    random: u64,
    full_six_letter_pool: bool,
) -> String {
    let answers = if floor == WORD_LENGTH as u8 && full_six_letter_pool {
        words()
    } else {
        tower_answers(floor)
    };
    choose_from_pool(answers, used, random)
}

fn normalise_tower(player: &mut TowerPlayer) {
    player.floor = player.floor.clamp(TOWER_START_FLOOR, TOWER_MAX_FLOOR);
    player.highest_floor_ever = player
        .highest_floor_ever
        .clamp(TOWER_START_FLOOR, TOWER_MAX_FLOOR)
        .max(player.floor);
    player.promotion_streak = player.promotion_streak.min(TOWER_PROMOTION_SOLVES - 1);
    player.strikes = player.strikes.min(TOWER_MAX_STRIKES - 1);
}

fn start_tower_puzzle(
    player: &mut TowerPlayer,
    floor: u8,
    now: i64,
    full_six_letter_pool: bool,
) -> Result<(), Error> {
    let bytes = host_random(8)?;
    let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    let word = choose_tower_word(floor, &player.used_words, random, full_six_letter_pool);
    if word.is_empty() {
        return Err(Error::msg(format!("Tower has no words for Floor {floor}")));
    }
    player.floor = floor.clamp(TOWER_START_FLOOR, TOWER_MAX_FLOOR);
    player.answer = word.clone();
    player.guesses.clear();
    player.correct = vec![None; floor as usize];
    player.present.clear();
    player.absent.clear();
    player.used_words.push(word);
    if player.used_words.len() > TOWER_USED_WORD_WINDOW {
        player
            .used_words
            .drain(..player.used_words.len() - TOWER_USED_WORD_WINDOW);
    }
    if player.run_started_at.is_none() {
        player.run_started_at = Some(now);
    }
    Ok(())
}

fn migrate_shared_game(daily: &mut Daily) {
    if !daily.players.is_empty() || daily.word.is_empty() {
        return;
    }
    for guesses in &daily.guesses {
        daily.players.push(PlayerDaily {
            user_id: guesses.user_id.clone(),
            display: guesses.display.clone(),
            day: daily.day,
            word: daily.word.clone(),
            solved: daily.solved && guesses.user_id == daily.solved_by_id,
            guesses: guesses.guesses.clone(),
            correct: daily.correct.clone(),
            present: daily.present.clone(),
            absent: daily.absent.clone(),
            used_words: daily.used_words.clone(),
            chances_remaining: None,
            failed_days: 0,
        });
    }
    daily.word.clear();
    daily.guesses.clear();
    daily.correct.clear();
    daily.present.clear();
    daily.absent.clear();
    daily.used_words.clear();
    daily.solved = false;
    daily.solved_by_id.clear();
    daily.solved_by_display.clear();
    daily.yesterday = None;
}

fn fresh_player(previous: &PlayerDaily, day: i64, word: String) -> PlayerDaily {
    let mut used_words = previous.used_words.clone();
    used_words.push(word.clone());
    if used_words.len() > USED_WORD_WINDOW {
        used_words.drain(..used_words.len() - USED_WORD_WINDOW);
    }
    PlayerDaily {
        user_id: previous.user_id.clone(),
        display: previous.display.clone(),
        day,
        word,
        correct: vec![None; WORD_LENGTH],
        used_words,
        ..Default::default()
    }
}

fn mercy_player(previous: &PlayerDaily, day: i64, word: String) -> PlayerDaily {
    let failed_word = previous.word.as_str();
    let mut player = fresh_player(previous, day, word);
    // The failed answer is eligible again later, but cannot be selected immediately because it
    // was still present in the history passed to choose_word().
    player.used_words.retain(|used| used != failed_word);
    player
}

fn new_word(previous: &PlayerDaily, day: i64) -> Result<PlayerDaily, Error> {
    let bytes = host_random(8)?;
    let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    Ok(fresh_player(
        previous,
        day,
        choose_word(&previous.used_words, random),
    ))
}

fn new_free_word(previous: &PlayerDaily, day: i64, full_pool: bool) -> Result<PlayerDaily, Error> {
    let bytes = host_random(8)?;
    let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    Ok(fresh_player(
        previous,
        day,
        choose_free_word(&previous.used_words, random, full_pool),
    ))
}

fn mercy_word(previous: &PlayerDaily, day: i64) -> Result<PlayerDaily, Error> {
    let bytes = host_random(8)?;
    let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    Ok(mercy_player(
        previous,
        day,
        choose_word(&previous.used_words, random),
    ))
}

fn rollover_player(previous: &PlayerDaily, day: i64) -> Result<PlayerDaily, Error> {
    if previous.failed_days >= MERCY_REROLL_AFTER_FAILED_DAYS {
        mercy_word(previous, day)
    } else {
        let mut player = previous.clone();
        player.day = day;
        player.guesses.clear();
        player.chances_remaining = None;
        Ok(player)
    }
}

fn ensure_player(server: &str, msg: &MessagePayload) -> Result<(Daily, usize), Error> {
    let mut daily = load_daily(server)?;
    let day = utc_day()?;
    let user_id = identity(msg);
    let index = daily
        .players
        .iter()
        .position(|player| player.user_id == user_id);
    let index = match index {
        Some(index) => index,
        None => {
            if daily.players.len() >= MAX_ACTIVE_USERS {
                return Err(Error::msg("Wordle active-player limit reached"));
            }
            daily.players.push(PlayerDaily {
                user_id,
                display: display(msg).into(),
                ..Default::default()
            });
            daily.players.len() - 1
        }
    };
    let player = &mut daily.players[index];
    player.display = display(msg).into();
    if player.word.is_empty() || (player.solved && player.day != day) {
        // Brand-new player, or a *solved* board rolling into a new day: hand out a fresh word.
        *player = new_word(player, day)?;
    } else if player.day != day {
        // An unsolved board gets one fresh daily allowance against the same word. After two
        // fully exhausted daily rounds, quietly deal a replacement instead.
        let replacement = rollover_player(player, day)?;
        *player = replacement;
    }
    save_daily(server, &daily)?;
    Ok((daily, index))
}

fn room_key(channel: &str) -> String {
    channel.to_ascii_lowercase()
}

fn ensure_free_room(daily: &mut Daily, channel: &str) -> Result<usize, Error> {
    let channel = room_key(channel);
    if let Some(index) = daily
        .free_rooms
        .iter()
        .position(|room| room.channel == channel)
    {
        return Ok(index);
    }
    if daily.free_rooms.len() >= MAX_FREE_ROOMS {
        return Err(Error::msg("Wordle free-play room limit reached"));
    }
    daily.free_rooms.push(FreeRoom {
        channel,
        ..Default::default()
    });
    Ok(daily.free_rooms.len() - 1)
}

fn ensure_free_player(server: &str, msg: &MessagePayload) -> Result<(Daily, usize, usize), Error> {
    let mut daily = load_daily(server)?;
    let room_index = ensure_free_room(&mut daily, &msg.target)?;
    let day = utc_day()?;
    let full_pool = free_answer_pool_enabled(server, &msg.target);
    let user_id = identity(msg);
    let room = &mut daily.free_rooms[room_index];
    let player_index = match room
        .players
        .iter()
        .position(|player| player.user_id == user_id)
    {
        Some(index) => index,
        None => {
            if room.players.len() >= MAX_ACTIVE_USERS {
                return Err(Error::msg("Wordle free-play active-player limit reached"));
            }
            room.players.push(PlayerDaily {
                user_id,
                display: display(msg).into(),
                ..Default::default()
            });
            room.players.len() - 1
        }
    };
    let player = &mut room.players[player_index];
    player.display = display(msg).into();
    if player.word.is_empty() {
        *player = new_free_word(player, day, full_pool)?;
    }
    save_daily(server, &daily)?;
    Ok((daily, room_index, player_index))
}

fn free_status(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let (daily, room_index, player_index) = ensure_free_player(server, msg)?;
    let player = &daily.free_rooms[room_index].players[player_index];
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.free_status",
            &["{user}'s free-play word: {pattern} — present: {present} — absent: {absent}."],
            &[
                ("user", display(msg)),
                ("pattern", &pattern(player)),
                ("present", &letters(&player.present)),
                ("absent", &letters(&player.absent)),
            ],
        )?,
    )
}

fn reset_all_players(server: &str) -> Result<(), Error> {
    let mut daily = load_daily(server)?;
    let day = utc_day()?;
    for player in &mut daily.players {
        *player = new_word(player, day)?;
    }
    save_daily(server, &daily)
}

fn reset_free_players(server: &str, channel: &str) -> Result<(), Error> {
    let mut daily = load_daily(server)?;
    let channel_key = room_key(channel);
    let Some(room) = daily
        .free_rooms
        .iter_mut()
        .find(|room| room.channel == channel_key)
    else {
        return Ok(());
    };
    let day = utc_day()?;
    let full_pool = free_answer_pool_enabled(server, channel);
    for player in &mut room.players {
        *player = new_free_word(player, day, full_pool)?;
    }
    save_daily(server, &daily)
}

fn evaluate(guess: &str, answer: &str) -> [u8; WORD_LENGTH] {
    let values = evaluate_dynamic(guess, answer);
    let mut result = [0; WORD_LENGTH];
    result.copy_from_slice(&values);
    result
}

fn evaluate_dynamic(guess: &str, answer: &str) -> Vec<u8> {
    let guess = guess.as_bytes();
    let answer = answer.as_bytes();
    let mut result = vec![0; answer.len()];
    let mut used = vec![false; answer.len()];
    for index in 0..answer.len() {
        if guess[index] == answer[index] {
            result[index] = 2;
            used[index] = true;
        }
    }
    for index in 0..answer.len() {
        if result[index] == 2 {
            continue;
        }
        if let Some(found) =
            (0..answer.len()).find(|other| !used[*other] && guess[index] == answer[*other])
        {
            result[index] = 1;
            used[found] = true;
        }
    }
    result
}

fn update_discoveries(
    player: &mut PlayerDaily,
    guess: &str,
    result: &[u8; WORD_LENGTH],
) -> (u64, u64) {
    if player.correct.len() != WORD_LENGTH {
        player.correct = vec![None; WORD_LENGTH];
    }
    let known_before = player
        .present
        .iter()
        .copied()
        .chain(player.correct.iter().flatten().copied())
        .collect::<BTreeSet<_>>();
    let exact_before = player.correct.clone();
    let bytes = guess.as_bytes();
    for index in 0..WORD_LENGTH {
        let letter = bytes[index] as char;
        match result[index] {
            2 => player.correct[index] = Some(letter),
            1 if !player.present.contains(&letter) => player.present.push(letter),
            0 if !player.absent.contains(&letter) => player.absent.push(letter),
            _ => {}
        }
    }
    let correct = player
        .correct
        .iter()
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>();
    player.present.retain(|letter| !correct.contains(letter));
    let known = player
        .present
        .iter()
        .copied()
        .chain(correct)
        .collect::<BTreeSet<_>>();
    player.absent.retain(|letter| !known.contains(letter));
    player.present.sort_unstable();
    player.absent.sort_unstable();
    let new_positions = player
        .correct
        .iter()
        .enumerate()
        .filter(|(index, value)| {
            value.is_some() && exact_before.get(*index).is_none_or(Option::is_none)
        })
        .count() as u64;
    let new_misplaced = guess
        .chars()
        .zip(result.iter())
        .filter_map(|(letter, value)| {
            (*value == 1 && !known_before.contains(&letter)).then_some(letter)
        })
        .collect::<BTreeSet<_>>()
        .len() as u64;
    let new_letters = new_positions + new_misplaced;
    (new_letters, new_positions)
}

fn update_tower_discoveries(player: &mut TowerPlayer, guess: &str, result: &[u8]) -> (u64, u64) {
    let length = player.answer.len();
    if player.correct.len() != length {
        player.correct = vec![None; length];
    }
    let known_before = player
        .present
        .iter()
        .copied()
        .chain(player.correct.iter().flatten().copied())
        .collect::<BTreeSet<_>>();
    let exact_before = player.correct.clone();
    let bytes = guess.as_bytes();
    for index in 0..length {
        let letter = bytes[index] as char;
        match result[index] {
            2 => player.correct[index] = Some(letter),
            1 if !player.present.contains(&letter) => player.present.push(letter),
            0 if !player.absent.contains(&letter) => player.absent.push(letter),
            _ => {}
        }
    }
    let correct = player
        .correct
        .iter()
        .flatten()
        .copied()
        .collect::<BTreeSet<_>>();
    player.present.retain(|letter| !correct.contains(letter));
    let known = player
        .present
        .iter()
        .copied()
        .chain(correct)
        .collect::<BTreeSet<_>>();
    player.absent.retain(|letter| !known.contains(letter));
    player.present.sort_unstable();
    player.absent.sort_unstable();
    let new_positions = player
        .correct
        .iter()
        .enumerate()
        .filter(|(index, value)| {
            value.is_some() && exact_before.get(*index).is_none_or(Option::is_none)
        })
        .count() as u64;
    let new_misplaced = guess
        .chars()
        .zip(result.iter())
        .filter_map(|(letter, value)| {
            (*value == 1 && !known_before.contains(&letter)).then_some(letter)
        })
        .collect::<BTreeSet<_>>();
    (new_positions + new_misplaced.len() as u64, new_positions)
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

fn pattern(player: &PlayerDaily) -> String {
    (0..WORD_LENGTH)
        .map(|index| {
            player
                .correct
                .get(index)
                .and_then(|letter| *letter)
                .unwrap_or('_')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn letters(values: &[char]) -> String {
    if values.is_empty() {
        "none".into()
    } else {
        values
            .iter()
            .map(char::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn solvers_today(daily: &Daily, day: i64) -> Result<String, Error> {
    let names = daily
        .players
        .iter()
        .filter(|player| player.solved && player.day == day)
        .map(|player| player.display.as_str())
        .take(20)
        .collect::<Vec<_>>();
    if names.is_empty() {
        themed("wordle.no_solvers", &["none yet"], &[])
    } else {
        Ok(names.join(", "))
    }
}

fn status(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        return free_status(server, msg);
    }
    let (daily, index) = ensure_player(server, msg)?;
    let player = &daily.players[index];
    let solvers = solvers_today(&daily, utc_day()?)?;
    if player.solved {
        return reply(
            server,
            &msg.target,
            &themed(
                "wordle.solved",
                &["{user}, you solved today's word: {word}. A new puzzle awaits tomorrow. Today's solvers: {solvers}."],
                &[
                    ("word", &player.word.to_ascii_uppercase()),
                    ("user", display(msg)),
                    ("solvers", &solvers),
                ],
            )?,
        );
    }
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.status",
            &["{user}'s word: {pattern} — present: {present} — absent: {absent}. Today's solvers: {solvers}."],
            &[
                ("user", display(msg)),
                ("pattern", &pattern(player)),
                ("present", &letters(&player.present)),
                ("absent", &letters(&player.absent)),
                ("solvers", &solvers),
            ],
        )?,
    )
}

fn free_guess(server: &str, msg: &MessagePayload, raw: &str) -> Result<(), Error> {
    let channel = &msg.target;
    let guess = raw.trim().to_ascii_lowercase();
    if guess.len() != WORD_LENGTH || !guess.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.bad_length",
                &["A six-letter word is required."],
                &[],
            )?,
        );
    }
    if !valid_word(&guess) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.not_in_list",
                &["I'm afraid {word} is not in the dictionary."],
                &[("word", &guess)],
            )?,
        );
    }
    let (mut daily, room_index, player_index) = ensure_free_player(server, msg)?;
    let max_attempts = attempts_setting(server, channel) as usize;
    let full_pool = free_answer_pool_enabled(server, channel);
    if daily.free_rooms[room_index].players[player_index]
        .guesses
        .contains(&guess)
    {
        return reply(
            server,
            channel,
            &themed(
                "wordle.duplicate",
                &["You have already tried {word}."],
                &[("word", &guess)],
            )?,
        );
    }
    let remaining_before = remaining_attempts(
        &daily.free_rooms[room_index].players[player_index],
        max_attempts,
    );
    if remaining_before == 0 {
        return reply(
            server,
            channel,
            &themed(
                "wordle.free_exhausted",
                &["That word got away, {user}. A fresh free-play puzzle is ready."],
                &[("user", display(msg))],
            )?,
        );
    }
    let user_id = identity(msg);
    let first = daily.free_rooms[room_index].players[player_index]
        .guesses
        .is_empty();
    let answer = daily.free_rooms[room_index].players[player_index]
        .word
        .clone();
    let (result, attempt) = {
        let player = &mut daily.free_rooms[room_index].players[player_index];
        player.display = display(msg).into();
        player.guesses.push(guess.clone());
        consume_attempt(player);
        let result = evaluate(&guess, &answer);
        update_discoveries(player, &guess, &result);
        (result, player.guesses.len())
    };
    let exhausted = remaining_before == 1;
    if guess == answer {
        let room = &mut daily.free_rooms[room_index];
        if first {
            record_participation(&mut room.stats, &user_id, display(msg));
        }
        if let Some(entry) = room.stats.iter_mut().find(|entry| entry.user_id == user_id) {
            entry.display = display(msg).into();
            entry.wins += 1;
            entry.total_attempts += attempt as u64;
        }
        let old = room.players[player_index].clone();
        room.players[player_index] = new_free_word(&old, utc_day()?, full_pool)?;
        save_daily(server, &daily)?;
        return reply(
            server,
            channel,
            &themed(
                "wordle.free_win",
                &[
                    "{user} solved the free-play word: {word}! The next puzzle is ready immediately.",
                ],
                &[
                    ("user", display(msg)),
                    ("word", &answer.to_ascii_uppercase()),
                ],
            )?,
        );
    }

    let room = &mut daily.free_rooms[room_index];
    if first {
        record_participation(&mut room.stats, &user_id, display(msg));
    } else if let Some(entry) = room.stats.iter_mut().find(|entry| entry.user_id == user_id) {
        entry.display = display(msg).into();
    }
    let matched = result.iter().filter(|value| **value > 0).count();
    let exact = result.iter().filter(|value| **value == 2).count();
    let misplaced = guess
        .chars()
        .zip(result)
        .filter_map(|(letter, value)| (value == 1).then_some(letter))
        .collect::<BTreeSet<_>>();
    let pattern = pattern(&room.players[player_index]);
    let misplaced = letters(&misplaced.into_iter().collect::<Vec<_>>());
    if exhausted {
        let old = room.players[player_index].clone();
        room.players[player_index] = new_free_word(&old, utc_day()?, full_pool)?;
        save_daily(server, &daily)?;
        return reply(
            server,
            channel,
            &themed(
                "wordle.free_exhausted",
                &[
                    "{user}, that word escaped after {count} guesses. A fresh free-play puzzle is ready.",
                ],
                &[("user", display(msg)), ("count", &max_attempts.to_string())],
            )?,
        );
    }
    save_daily(server, &daily)?;
    reply(
        server,
        channel,
        &themed(
            "wordle.guess",
            &["Your word contains {matched} of your letters, {exact} correctly placed: {pattern}. Misplaced: {misplaced}."],
            &[
                ("user", display(msg)),
                ("matched", &matched.to_string()),
                ("exact", &exact.to_string()),
                ("pattern", &pattern),
                ("misplaced", &misplaced),
            ],
        )?,
    )?;
    Ok(())
}

fn record_participation(stats: &mut Vec<UserStats>, user_id: &str, display: &str) {
    if let Some(entry) = stats.iter_mut().find(|entry| entry.user_id == user_id) {
        entry.display = display.into();
        entry.games_played += 1;
    } else if stats.len() < MAX_STATS_USERS {
        stats.push(UserStats {
            user_id: user_id.into(),
            display: display.into(),
            games_played: 1,
            wins: 0,
            total_attempts: 0,
        });
    }
}

fn guess(server: &str, msg: &MessagePayload, raw: &str) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        return free_guess(server, msg, raw);
    }
    let channel = &msg.target;
    let guess = raw.trim().to_ascii_lowercase();
    if guess.len() != WORD_LENGTH || !guess.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.bad_length",
                &["A six-letter word is required."],
                &[],
            )?,
        );
    }
    if !valid_word(&guess) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.not_in_list",
                &["I'm afraid {word} is not in the dictionary."],
                &[("word", &guess)],
            )?,
        );
    }
    let (mut daily, index) = ensure_player(server, msg)?;
    let user_id = identity(msg);
    let max_attempts = attempts_setting(server, channel) as usize;
    if daily.players[index].solved {
        return status(server, msg);
    }
    let remaining_before = remaining_attempts(&daily.players[index], max_attempts);
    if remaining_before == 0 {
        return reply(
            server,
            channel,
            &themed(
                "wordle.exhausted",
                &["You have exhausted today's {count} attempt(s), {user}."],
                &[("count", &max_attempts.to_string()), ("user", display(msg))],
            )?,
        );
    }
    if daily.players[index].guesses.contains(&guess) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.duplicate",
                &["You have already tried {word}."],
                &[("word", &guess)],
            )?,
        );
    }
    let first = daily.players[index].guesses.is_empty();
    daily.players[index].display = display(msg).into();
    daily.players[index].guesses.push(guess.clone());
    consume_attempt(&mut daily.players[index]);
    let result = evaluate(&guess, &daily.players[index].word);
    let (new_letters, new_positions) =
        update_discoveries(&mut daily.players[index], &guess, &result);
    let exhausted_day = remaining_before == 1;
    let mut stats = load_stats(server)?;
    if first {
        record_participation(&mut stats, &user_id, display(msg));
    } else if let Some(entry) = stats.iter_mut().find(|entry| entry.user_id == user_id) {
        entry.display = display(msg).into();
    }
    if guess == daily.players[index].word {
        let attempt = daily.players[index].guesses.len();
        daily.players[index].solved = true;
        if let Some(entry) = stats.iter_mut().find(|entry| entry.user_id == user_id) {
            entry.wins += 1;
            entry.total_attempts += attempt as u64;
        }
        save_daily(server, &daily)?;
        save_stats(server, &stats)?;
        reply(
            server,
            channel,
            &themed(
                "wordle.win",
                &["{user} solved their word: {word}! A new puzzle awaits tomorrow."],
                &[
                    ("word", &daily.players[index].word.to_ascii_uppercase()),
                    ("user", display(msg)),
                ],
            )?,
        )?;
        let mut increments = vec![
            ("letters", new_letters),
            ("positions", new_positions),
            ("wins", 1),
        ];
        if attempt == 1 {
            increments.push(("first_guess", 1));
        }
        if remaining_before == 1 {
            increments.push(("final_attempt", 1));
        }
        award_brass(
            server,
            msg,
            10,
            &format!(
                "wordle:win:{}:{}:{}",
                msg.user_id, daily.players[index].day, daily.players[index].word
            ),
            "wordle_win",
        )?;
        award(server, msg, increments)?;
        return Ok(());
    }
    if exhausted_day {
        daily.players[index].failed_days = daily.players[index]
            .failed_days
            .saturating_add(1)
            .min(MERCY_REROLL_AFTER_FAILED_DAYS);
    }
    save_daily(server, &daily)?;
    save_stats(server, &stats)?;
    let matched = result.iter().filter(|value| **value > 0).count();
    let exact = result.iter().filter(|value| **value == 2).count();
    let misplaced = guess
        .chars()
        .zip(result)
        .filter_map(|(letter, value)| (value == 1).then_some(letter))
        .collect::<BTreeSet<_>>();
    let pattern = pattern(&daily.players[index]);
    let misplaced = letters(&misplaced.into_iter().collect::<Vec<_>>());
    let final_round =
        exhausted_day && daily.players[index].failed_days >= MERCY_REROLL_AFTER_FAILED_DAYS;
    let (key, default) = if final_round {
        (
            "wordle.too_difficult",
            "{user}, that word may have been a touch ambitious. I'll quietly put it back in circulation and give you a fresh word tomorrow. Your final guess found {matched} letter(s), {exact} correctly placed: {pattern}. Misplaced: {misplaced}.",
        )
    } else {
        (
            "wordle.guess",
            "Your word contains {matched} of your letters, {exact} correctly placed: {pattern}. Misplaced: {misplaced}.",
        )
    };
    reply(
        server,
        channel,
        &themed(
            key,
            &[default],
            &[
                ("user", display(msg)),
                ("matched", &matched.to_string()),
                ("exact", &exact.to_string()),
                ("pattern", &pattern),
                ("misplaced", &misplaced),
            ],
        )?,
    )?;
    award(
        server,
        msg,
        vec![("letters", new_letters), ("positions", new_positions)],
    )
}

fn personal_stats(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        let daily = load_daily(server)?;
        let stats = room_key(&msg.target);
        let stats = daily
            .free_rooms
            .iter()
            .find(|room| room.channel == stats)
            .and_then(|room| {
                room.stats
                    .iter()
                    .find(|entry| entry.user_id == identity(msg))
            });
        let (wins, games, total_attempts) = stats
            .map(|entry| (entry.wins, entry.games_played, entry.total_attempts))
            .unwrap_or_default();
        let rate = wins.saturating_mul(100).checked_div(games).unwrap_or(0);
        let average = if wins == 0 || total_attempts == 0 {
            "—".into()
        } else {
            format!("{:.1}", total_attempts as f64 / wins as f64)
        };
        return reply(
            server,
            &msg.target,
            &themed(
                "wordle.stats",
                &["{user}: {wins} free-play word(s) solved in {games} game(s) ({rate}%), averaging {average} valid guess(es)."],
                &[
                    ("user", display(msg)),
                    ("wins", &wins.to_string()),
                    ("games", &games.to_string()),
                    ("rate", &rate.to_string()),
                    ("average", &average),
                ],
            )?,
        );
    }
    let stats = load_stats(server)?;
    let entry = stats.iter().find(|entry| entry.user_id == identity(msg));
    let (wins, games, total_attempts) = entry
        .map(|entry| (entry.wins, entry.games_played, entry.total_attempts))
        .unwrap_or_default();
    let rate = wins.saturating_mul(100).checked_div(games).unwrap_or(0);
    let average = if wins == 0 || total_attempts == 0 {
        "—".into()
    } else {
        format!("{:.1}", total_attempts as f64 / wins as f64)
    };
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.stats",
            &["{user}: {wins} word(s) solved in {games} game(s) ({rate}%), averaging {average} valid guess(es)."],
            &[
                ("user", display(msg)),
                ("wins", &wins.to_string()),
                ("games", &games.to_string()),
                ("rate", &rate.to_string()),
                ("average", &average),
            ],
        )?,
    )
}

fn top(server: &str, channel: &str) -> Result<(), Error> {
    if free_play_enabled(server, channel) {
        let mut daily = load_daily(server)?;
        let channel_key = room_key(channel);
        let Some(room) = daily
            .free_rooms
            .iter_mut()
            .find(|room| room.channel == channel_key)
        else {
            return reply(
                server,
                channel,
                &themed(
                    "wordle.top",
                    &["No free-play laurels have yet been awarded."],
                    &[],
                )?,
            );
        };
        room.stats.retain(|entry| entry.wins > 0);
        room.stats.sort_by_key(|entry| {
            (
                std::cmp::Reverse(entry.wins),
                entry.games_played,
                entry.user_id.clone(),
            )
        });
        let leaders = room
            .stats
            .iter()
            .take(5)
            .map(|entry| format!("{} ({})", entry.display, entry.wins))
            .collect::<Vec<_>>()
            .join(", ");
        let leaders = if leaders.is_empty() {
            "No free-play laurels have yet been awarded.".into()
        } else {
            leaders
        };
        return reply(
            server,
            channel,
            &themed(
                "wordle.top",
                &["Free-play Wordle honours: {leaders}"],
                &[("leaders", &leaders)],
            )?,
        );
    }
    let mut stats = load_stats(server)?;
    stats.retain(|entry| entry.wins > 0);
    stats.sort_by_key(|entry| {
        (
            std::cmp::Reverse(entry.wins),
            entry.games_played,
            entry.user_id.clone(),
        )
    });
    let leaders = stats
        .iter()
        .take(5)
        .map(|entry| format!("{} ({})", entry.display, entry.wins))
        .collect::<Vec<_>>()
        .join(", ");
    let leaders = if leaders.is_empty() {
        "No laurels have yet been awarded.".into()
    } else {
        leaders
    };
    reply(
        server,
        channel,
        &themed(
            "wordle.top",
            &["Wordle honours: {leaders}"],
            &[("leaders", &leaders)],
        )?,
    )
}

fn tower_index(daily: &Daily, user_id: &str, nick: &str) -> Option<usize> {
    let legacy_id = format!("nick:{}", nick.to_ascii_lowercase());
    daily
        .tower
        .iter()
        .position(|player| player.user_id == user_id || player.user_id == legacy_id)
}

fn ensure_tower(server: &str, msg: &MessagePayload) -> Result<(Daily, usize), Error> {
    let mut daily = load_daily(server)?;
    let day = utc_day()?;
    let now = now_secs()?;
    let user_id = identity(msg);
    let index = match tower_index(&daily, &user_id, &msg.nick) {
        Some(index) => index,
        None => {
            if daily.tower.len() >= MAX_ACTIVE_USERS {
                return Err(Error::msg("Wordle Tower active-player limit reached"));
            }
            daily.tower.push(TowerPlayer {
                user_id,
                display: display(msg).into(),
                floor: TOWER_START_FLOOR,
                highest_floor_ever: TOWER_START_FLOOR,
                ..Default::default()
            });
            daily.tower.len() - 1
        }
    };
    let player = &mut daily.tower[index];
    player.user_id = identity(msg);
    player.display = display(msg).into();
    normalise_tower(player);
    if player.locked_until_day.is_some_and(|locked| locked > day) {
        save_daily(server, &daily)?;
        return Ok((daily, index));
    }
    if player.locked_until_day.is_some() {
        player.locked_until_day = None;
        player.answer.clear();
        player.guesses.clear();
        player.correct.clear();
        player.present.clear();
        player.absent.clear();
        player.run_solves = 0;
        player.promotion_streak = 0;
        player.run_started_at = None;
    }
    if player.answer.is_empty() {
        start_tower_puzzle(player, player.floor, now, false)?;
    }
    save_daily(server, &daily)?;
    Ok((daily, index))
}

fn ensure_free_tower(server: &str, msg: &MessagePayload) -> Result<(Daily, usize, usize), Error> {
    let mut daily = load_daily(server)?;
    let room_index = ensure_free_room(&mut daily, &msg.target)?;
    let now = now_secs()?;
    let full_pool = free_answer_pool_enabled(server, &msg.target);
    let user_id = identity(msg);
    let room = &mut daily.free_rooms[room_index];
    let player_index = match room
        .tower
        .iter()
        .position(|player| player.user_id == user_id)
    {
        Some(index) => index,
        None => {
            if room.tower.len() >= MAX_ACTIVE_USERS {
                return Err(Error::msg(
                    "Wordle free-play Tower active-player limit reached",
                ));
            }
            room.tower.push(TowerPlayer {
                user_id,
                display: display(msg).into(),
                floor: TOWER_START_FLOOR,
                highest_floor_ever: TOWER_START_FLOOR,
                ..Default::default()
            });
            room.tower.len() - 1
        }
    };
    let player = &mut room.tower[player_index];
    player.display = display(msg).into();
    // Free-play failures never create a lock. Clear a legacy lock if the channel was
    // previously configured differently, then continue from the same floor immediately.
    if player.locked_until_day.is_some() {
        player.locked_until_day = None;
        player.answer.clear();
        player.guesses.clear();
        player.correct.clear();
        player.present.clear();
        player.absent.clear();
        player.run_solves = 0;
        player.promotion_streak = 0;
        player.run_started_at = None;
    }
    normalise_tower(player);
    if player.answer.is_empty() {
        start_tower_puzzle(player, player.floor, now, full_pool)?;
    }
    save_daily(server, &daily)?;
    Ok((daily, room_index, player_index))
}

fn tower_strikes(strikes: u8) -> String {
    format!("{strikes}/{TOWER_MAX_STRIKES}")
}

fn tower_pattern(player: &TowerPlayer) -> String {
    (0..player.floor as usize)
        .map(|index| {
            player
                .correct
                .get(index)
                .and_then(|letter| *letter)
                .unwrap_or('_')
                .to_string()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn tower_feedback(player: &TowerPlayer, result: &[u8]) -> String {
    let matched = result.iter().filter(|value| **value > 0).count();
    let exact = result.iter().filter(|value| **value == 2).count();
    let misplaced = player
        .guesses
        .last()
        .into_iter()
        .flat_map(|guess| guess.chars().zip(result.iter()))
        .filter_map(|(letter, value)| (*value == 1).then_some(letter))
        .collect::<BTreeSet<_>>();
    let misplaced = letters(&misplaced.into_iter().collect::<Vec<_>>());
    format!(
        "The word contains {matched} of your letters, {exact} correctly placed: {}. Misplaced: {misplaced}.",
        tower_pattern(player)
    )
}

fn tower_status_text(player: &TowerPlayer) -> String {
    format!(
        "🗼 WORDLE TOWER • FLOOR {} | Puzzle {}/{} to ascend | Strikes: {} | Pattern: {} | Present: {} | Absent: {}",
        player.floor,
        player.promotion_streak as usize + 1,
        TOWER_PROMOTION_SOLVES,
        tower_strikes(player.strikes),
        tower_pattern(player),
        letters(&player.present),
        letters(&player.absent)
    )
}

fn tower_status(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        let (daily, room_index, player_index) = ensure_free_tower(server, msg)?;
        let player = &daily.free_rooms[room_index].tower[player_index];
        return reply(
            server,
            &msg.target,
            &themed(
                "wordle.tower.status",
                &["{user}'s {status} | !wordle tower <guess>"],
                &[
                    ("user", display(msg)),
                    ("status", &tower_status_text(player)),
                ],
            )?,
        );
    }
    let (daily, index) = ensure_tower(server, msg)?;
    let player = &daily.tower[index];
    if player.locked_until_day.is_some() {
        return reply(
            server,
            &msg.target,
            &themed(
                "wordle.tower.locked",
                &["☠ THE TOWER CLAIMS YOU | The doors reopen tomorrow. Floor {floor} • Strikes {strikes}"],
                &[
                    ("floor", &player.floor.to_string()),
                    ("strikes", &tower_strikes(player.strikes)),
                ],
            )?,
        );
    }
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.tower.status",
            &["{user}'s {status} | !wordle tower <guess>"],
            &[
                ("user", display(msg)),
                ("status", &tower_status_text(player)),
            ],
        )?,
    )
}

fn tower_stats(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        let daily = load_daily(server)?;
        let channel_key = room_key(&msg.target);
        let player = daily
            .free_rooms
            .iter()
            .find(|room| room.channel == channel_key)
            .and_then(|room| {
                room.tower
                    .iter()
                    .find(|player| player.user_id == identity(msg))
            });
        let (floor, highest, solves, longest, fastest) = player
            .map(|player| {
                (
                    player.floor,
                    player.highest_floor_ever,
                    player.total_solves,
                    player.longest_run,
                    player
                        .fastest_promotion_secs
                        .map(|seconds| format!("{seconds}s"))
                        .unwrap_or_else(|| "—".into()),
                )
            })
            .unwrap_or((TOWER_START_FLOOR, TOWER_START_FLOOR, 0, 0, "—".into()));
        return reply(
            server,
            &msg.target,
            &themed(
                "wordle.tower.stats",
                &["{user}: Free-play Floor {floor}; highest Floor {highest}; {solves} Tower solve(s); best run {longest}; fastest promotion {fastest}."],
                &[
                    ("user", display(msg)),
                    ("floor", &floor.to_string()),
                    ("highest", &highest.to_string()),
                    ("solves", &solves.to_string()),
                    ("longest", &longest.to_string()),
                    ("fastest", &fastest),
                ],
            )?,
        );
    }
    let daily = load_daily(server)?;
    let player = tower_index(&daily, &identity(msg), &msg.nick).map(|index| &daily.tower[index]);
    let (floor, highest, solves, longest, fastest) = player
        .map(|player| {
            (
                player.floor,
                player.highest_floor_ever,
                player.total_solves,
                player.longest_run,
                player
                    .fastest_promotion_secs
                    .map(|seconds| format!("{seconds}s"))
                    .unwrap_or_else(|| "—".into()),
            )
        })
        .unwrap_or((TOWER_START_FLOOR, TOWER_START_FLOOR, 0, 0, "—".into()));
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.tower.stats",
            &["{user}: Floor {floor}; highest Floor {highest}; {solves} Tower solve(s); best run {longest}; fastest promotion {fastest}."],
            &[
                ("user", display(msg)),
                ("floor", &floor.to_string()),
                ("highest", &highest.to_string()),
                ("solves", &solves.to_string()),
                ("longest", &longest.to_string()),
                ("fastest", &fastest),
            ],
        )?,
    )
}

fn tower_top(server: &str, channel: &str) -> Result<(), Error> {
    if free_play_enabled(server, channel) {
        let daily = load_daily(server)?;
        let channel_key = room_key(channel);
        let mut tower = daily
            .free_rooms
            .iter()
            .find(|room| room.channel == channel_key)
            .map(|room| room.tower.clone())
            .unwrap_or_default();
        tower.retain(|player| player.total_solves > 0);
        tower.sort_by_key(|player| {
            (
                std::cmp::Reverse(player.highest_floor_ever),
                std::cmp::Reverse(player.total_solves),
                player.user_id.clone(),
            )
        });
        let leaders = tower
            .iter()
            .take(5)
            .map(|player| {
                format!(
                    "{} (Floor {}, {} solves)",
                    player.display, player.highest_floor_ever, player.total_solves
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let leaders = if leaders.is_empty() {
            "The free-play Tower has no laurels yet.".into()
        } else {
            leaders
        };
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.top",
                &["Free-play Tower honours: {leaders}"],
                &[("leaders", &leaders)],
            )?,
        );
    }
    let mut daily = load_daily(server)?;
    daily.tower.retain(|player| player.total_solves > 0);
    daily.tower.sort_by_key(|player| {
        (
            std::cmp::Reverse(player.highest_floor_ever),
            std::cmp::Reverse(player.total_solves),
            player.user_id.clone(),
        )
    });
    let leaders = daily
        .tower
        .iter()
        .take(5)
        .map(|player| {
            format!(
                "{} (Floor {}, {} solves)",
                player.display, player.highest_floor_ever, player.total_solves
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let leaders = if leaders.is_empty() {
        "The Tower has no laurels yet.".into()
    } else {
        leaders
    };
    reply(
        server,
        channel,
        &themed(
            "wordle.tower.top",
            &["Tower honours: {leaders}"],
            &[("leaders", &leaders)],
        )?,
    )
}

fn record_tower_solve(player: &mut TowerPlayer, now: i64) -> (bool, bool) {
    player.total_solves = player.total_solves.saturating_add(1);
    player.run_solves = player.run_solves.saturating_add(1);
    player.promotion_streak = player.promotion_streak.saturating_add(1);
    let mut promoted = false;
    let mut cap_cleared = false;
    if player.promotion_streak >= TOWER_PROMOTION_SOLVES {
        player.promotion_streak = 0;
        if player.floor < TOWER_MAX_FLOOR {
            player.floor += 1;
            player.highest_floor_ever = player.highest_floor_ever.max(player.floor);
            player.strikes = 0;
            promoted = true;
            if let Some(started) = player.run_started_at {
                let elapsed = now.saturating_sub(started);
                player.fastest_promotion_secs = Some(
                    player
                        .fastest_promotion_secs
                        .map_or(elapsed, |best| best.min(elapsed)),
                );
            }
        } else {
            cap_cleared = true;
        }
    }
    player.longest_run = player.longest_run.max(player.run_solves);
    (promoted, cap_cleared)
}

fn free_tower_guess(server: &str, msg: &MessagePayload, raw: &str) -> Result<(), Error> {
    let (mut daily, room_index, player_index) = ensure_free_tower(server, msg)?;
    let channel = &msg.target;
    let full_pool = free_answer_pool_enabled(server, channel);
    let floor = daily.free_rooms[room_index].tower[player_index].floor;
    let guess = raw.trim().to_ascii_lowercase();
    if guess.len() != floor as usize || !guess.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.bad_length",
                &["Floor {floor} requires a {length}-letter word."],
                &[
                    ("floor", &floor.to_string()),
                    ("length", &floor.to_string()),
                ],
            )?,
        );
    }
    if !valid_tower_word(&guess, floor) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.not_in_list",
                &["I'm afraid {word} is not in the Floor {floor} dictionary."],
                &[("word", &guess), ("floor", &floor.to_string())],
            )?,
        );
    }
    if daily.free_rooms[room_index].tower[player_index]
        .guesses
        .contains(&guess)
    {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.duplicate",
                &["You have already tried {word} on this puzzle."],
                &[("word", &guess)],
            )?,
        );
    }

    let now = now_secs()?;
    let answer = daily.free_rooms[room_index].tower[player_index]
        .answer
        .clone();
    let result = evaluate_dynamic(&guess, &answer);
    let player = &mut daily.free_rooms[room_index].tower[player_index];
    player.display = display(msg).into();
    player.guesses.push(guess.clone());
    update_tower_discoveries(player, &guess, &result);
    let feedback = tower_feedback(player, &result);
    if guess == answer {
        let (promoted, cap_cleared) = record_tower_solve(player, now);
        let next_floor = player.floor;
        start_tower_puzzle(player, next_floor, now, full_pool)?;
        save_daily(server, &daily)?;
        let message = if promoted {
            format!(
                "🔔 FLOOR CLEARED! Four victories. Floor {} unlocked: {}-letter words.",
                next_floor, next_floor
            )
        } else if cap_cleared {
            "🔔 FLOOR 8 CLEARED! The summit holds. Eight-letter puzzles continue.".into()
        } else {
            "The next puzzle is ready immediately.".into()
        };
        let status = tower_status_text(&daily.free_rooms[room_index].tower[player_index]);
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.solve",
                &["{user} solved it: {feedback} | {message} | {status}"],
                &[
                    ("user", display(msg)),
                    ("feedback", &feedback),
                    ("message", &message),
                    ("status", &status),
                ],
            )?,
        );
    }

    let exhausted = daily.free_rooms[room_index].tower[player_index]
        .guesses
        .len()
        >= TOWER_GUESSES;
    if exhausted {
        let player = &mut daily.free_rooms[room_index].tower[player_index];
        let lost_floor = player.floor;
        let run_solves = player.run_solves;
        player.longest_run = player.longest_run.max(run_solves);
        player.strikes = player.strikes.saturating_add(1);
        let demoted = player.strikes >= TOWER_MAX_STRIKES;
        if demoted {
            player.floor = player.floor.saturating_sub(1).max(TOWER_START_FLOOR);
            player.strikes = 0;
        }
        let strikes = tower_strikes(player.strikes);
        let floor = player.floor;
        player.locked_until_day = None;
        player.answer.clear();
        player.guesses.clear();
        player.correct.clear();
        player.present.clear();
        player.absent.clear();
        player.run_solves = 0;
        player.promotion_streak = 0;
        player.run_started_at = None;
        start_tower_puzzle(player, floor, now, full_pool)?;
        save_daily(server, &daily)?;
        let demotion = if demoted {
            format!("Three strikes on Floor {lost_floor}; you descend to Floor {floor}.")
        } else {
            "The next puzzle is ready immediately.".into()
        };
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.free_death",
                &["{feedback} | The Tower claims this puzzle | {run} puzzle(s) solved this run. Floor {floor} • Strikes {strikes} | {demotion}"],
                &[
                    ("feedback", &feedback),
                    ("run", &run_solves.to_string()),
                    ("floor", &floor.to_string()),
                    ("strikes", &strikes),
                    ("demotion", &demotion),
                ],
            )?,
        );
    }
    save_daily(server, &daily)?;
    let status = tower_status_text(&daily.free_rooms[room_index].tower[player_index]);
    reply(
        server,
        channel,
        &themed(
            "wordle.tower.guess",
            &["{feedback} | {status}"],
            &[("feedback", &feedback), ("status", &status)],
        )?,
    )
}

fn tower_guess(server: &str, msg: &MessagePayload, raw: &str) -> Result<(), Error> {
    if free_play_enabled(server, &msg.target) {
        return free_tower_guess(server, msg, raw);
    }
    let (mut daily, index) = ensure_tower(server, msg)?;
    let channel = &msg.target;
    if daily.tower[index].locked_until_day.is_some() {
        return tower_status(server, msg);
    }
    let guess = raw.trim().to_ascii_lowercase();
    let floor = daily.tower[index].floor;
    if guess.len() != floor as usize || !guess.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.bad_length",
                &["Floor {floor} requires a {length}-letter word."],
                &[
                    ("floor", &floor.to_string()),
                    ("length", &floor.to_string()),
                ],
            )?,
        );
    }
    if !valid_tower_word(&guess, floor) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.not_in_list",
                &["I'm afraid {word} is not in the Floor {floor} dictionary."],
                &[("word", &guess), ("floor", &floor.to_string())],
            )?,
        );
    }
    if daily.tower[index].guesses.contains(&guess) {
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.duplicate",
                &["You have already tried {word} on this puzzle."],
                &[("word", &guess)],
            )?,
        );
    }

    let now = now_secs()?;
    let day = utc_day()?;
    let answer = daily.tower[index].answer.clone();
    let result = evaluate_dynamic(&guess, &answer);
    daily.tower[index].display = display(msg).into();
    daily.tower[index].guesses.push(guess.clone());
    update_tower_discoveries(&mut daily.tower[index], &guess, &result);
    let feedback = tower_feedback(&daily.tower[index], &result);
    if guess == answer {
        let player = &mut daily.tower[index];
        let (promoted, cap_cleared) = record_tower_solve(player, now);
        let next_floor = player.floor;
        start_tower_puzzle(player, next_floor, now, false)?;
        save_daily(server, &daily)?;
        let message = if promoted {
            format!(
                "🔔 FLOOR CLEARED! Four victories. Floor {} unlocked: {}-letter words.",
                next_floor, next_floor
            )
        } else if cap_cleared {
            "🔔 FLOOR 8 CLEARED! The summit holds. Eight-letter puzzles continue.".into()
        } else {
            "The next puzzle awaits.".into()
        };
        let status = tower_status_text(&daily.tower[index]);
        reply(
            server,
            channel,
            &themed(
                "wordle.tower.solve",
                &["{user} solved it: {feedback} | {message} | {status}"],
                &[
                    ("user", display(msg)),
                    ("feedback", &feedback),
                    ("message", &message),
                    ("status", &status),
                ],
            )?,
        )?;
        let mut increments = vec![("tower_solves", 1)];
        if promoted {
            increments.push(("tower_promotions", 1));
        }
        award(server, msg, increments)?;
        return Ok(());
    }

    let exhausted = daily.tower[index].guesses.len() >= TOWER_GUESSES;
    if exhausted {
        let player = &mut daily.tower[index];
        let lost_floor = player.floor;
        let run_solves = player.run_solves;
        player.longest_run = player.longest_run.max(run_solves);
        player.strikes = player.strikes.saturating_add(1);
        let demoted = player.strikes >= TOWER_MAX_STRIKES;
        if demoted {
            player.floor = player.floor.saturating_sub(1).max(TOWER_START_FLOOR);
            player.strikes = 0;
        }
        let strikes = tower_strikes(player.strikes);
        let floor = player.floor;
        player.locked_until_day = Some(day + 1);
        player.answer.clear();
        player.guesses.clear();
        player.correct.clear();
        player.present.clear();
        player.absent.clear();
        player.run_solves = 0;
        player.promotion_streak = 0;
        player.run_started_at = None;
        save_daily(server, &daily)?;
        let demotion = if demoted {
            format!("Three strikes on Floor {lost_floor}; you descend to Floor {floor}.")
        } else {
            "The doors reopen tomorrow.".into()
        };
        return reply(
            server,
            channel,
            &themed(
                "wordle.tower.death",
                &["{feedback} | ☠ THE TOWER CLAIMS YOU | The word was {word}. {run} puzzle(s) solved this run. Floor {floor} • Strikes {strikes} | {demotion}"],
                &[
                    ("feedback", &feedback),
                    ("word", &answer.to_ascii_uppercase()),
                    ("run", &run_solves.to_string()),
                    ("floor", &floor.to_string()),
                    ("strikes", &strikes),
                    ("demotion", &demotion),
                ],
            )?,
        );
    }
    save_daily(server, &daily)?;
    let status = tower_status_text(&daily.tower[index]);
    reply(
        server,
        channel,
        &themed(
            "wordle.tower.guess",
            &["{feedback} | {status}"],
            &[("feedback", &feedback), ("status", &status)],
        )?,
    )
}

fn tower_command<'a>(
    server: &str,
    msg: &MessagePayload,
    mut parts: impl Iterator<Item = &'a str>,
) -> Result<(), Error> {
    match parts.next().unwrap_or("").to_ascii_lowercase().as_str() {
        "" => tower_status(server, msg),
        "stats" | "score" => tower_stats(server, msg),
        "top" => tower_top(server, &msg.target),
        guess => tower_guess(server, msg, guess),
    }
}

fn player_index(daily: &Daily, profile: &Profile) -> Option<usize> {
    let legacy_id = format!("nick:{}", profile.nick.to_ascii_lowercase());
    daily
        .players
        .iter()
        .position(|player| player.user_id == profile.id || player.user_id == legacy_id)
}

fn assign_admin_word(
    daily: &mut Daily,
    profile: &Profile,
    day: i64,
    random: u64,
) -> Result<(), Error> {
    let index = match player_index(daily, profile) {
        Some(index) => index,
        None => {
            if daily.players.len() >= MAX_ACTIVE_USERS {
                return Err(Error::msg("Wordle active-player limit reached"));
            }
            daily.players.push(PlayerDaily {
                user_id: profile.id.clone(),
                display: profile.nick.clone(),
                ..Default::default()
            });
            daily.players.len() - 1
        }
    };
    daily.players[index].user_id = profile.id.clone();
    daily.players[index].display = profile.nick.clone();
    let word = choose_word(&daily.players[index].used_words, random);
    daily.players[index] = fresh_player(&daily.players[index], day, word);
    Ok(())
}

fn set_admin_chances(
    daily: &mut Daily,
    profile: &Profile,
    day: i64,
    chances: usize,
) -> Result<(), Error> {
    let Some(index) = player_index(daily, profile) else {
        return Err(Error::msg(format!(
            "{} does not have an assigned Wordle",
            profile.nick
        )));
    };
    if daily.players[index].word.is_empty() {
        return Err(Error::msg(format!(
            "{} does not have an assigned Wordle",
            profile.nick
        )));
    }
    if daily.players[index].solved {
        return Err(Error::msg(format!(
            "{} has already solved this Wordle; use 'new' instead",
            profile.nick
        )));
    }
    daily.players[index].user_id = profile.id.clone();
    daily.players[index].display = profile.nick.clone();
    if daily.players[index].day != day {
        daily.players[index].day = day;
        daily.players[index].guesses.clear();
    }
    daily.players[index].chances_remaining = Some(chances);
    Ok(())
}

#[plugin_fn]
pub fn admin_command(input: String) -> FnResult<String> {
    let request: ModuleAdminCommandRequest = serde_json::from_str(&input)?;
    let mut parts = request.args.split_whitespace();
    let nick = parts.next().unwrap_or("");
    let action = parts.next().unwrap_or("").to_ascii_lowercase();
    if nick.is_empty() || action.is_empty() {
        return Ok(serde_json::to_string(&ModuleAdminCommandResponse {
            messages: vec!["usage: wordle <nick> new | wordle <nick> chances <1-10>".into()],
        })?);
    }
    let Some(profile) = get_profile(&request.server, nick)? else {
        return Ok(serde_json::to_string(&ModuleAdminCommandResponse {
            messages: vec![format!("no profile found for {nick} on {}", request.server)],
        })?);
    };

    let message = match action.as_str() {
        "new" if parts.next().is_none() => {
            let mut daily = load_daily(&request.server)?;
            let bytes = host_random(8)?;
            let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
            assign_admin_word(&mut daily, &profile, utc_day()?, random)?;
            save_daily(&request.server, &daily)?;
            format!("{} now has a fresh Wordle.", profile.nick)
        }
        "chances" => {
            let value = parts.next().unwrap_or("");
            let Ok(chances) = value.parse::<usize>() else {
                return Ok(serde_json::to_string(&ModuleAdminCommandResponse {
                    messages: vec!["chances must be a number from 1 to 10".into()],
                })?);
            };
            if !(1..=10).contains(&chances) || parts.next().is_some() {
                return Ok(serde_json::to_string(&ModuleAdminCommandResponse {
                    messages: vec!["chances must be a number from 1 to 10".into()],
                })?);
            }
            let mut daily = load_daily(&request.server)?;
            if let Err(error) = set_admin_chances(&mut daily, &profile, utc_day()?, chances) {
                return Ok(serde_json::to_string(&ModuleAdminCommandResponse {
                    messages: vec![error.to_string()],
                })?);
            }
            save_daily(&request.server, &daily)?;
            format!(
                "{} now has {chances} Wordle chance(s) remaining.",
                profile.nick
            )
        }
        _ => "usage: wordle <nick> new | wordle <nick> chances <1-10>".into(),
    };
    Ok(serde_json::to_string(&ModuleAdminCommandResponse {
        messages: vec![message],
    })?)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let mut parts = text.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    if !matches!(
        command.as_str(),
        "!word" | "!wordle" | "!tower" | "!wt" | "!guess" | "!wordlestats" | "!wstats"
    ) {
        return Ok(());
    }
    if msg.is_private {
        return Ok(());
    }
    if !in_game_room(&env.server, &msg.target) {
        room_redirect(&env.server, &msg)?;
        return Ok(());
    }
    if matches!(command.as_str(), "!wordlestats" | "!wstats") {
        personal_stats(&env.server, &msg)?;
        return Ok(());
    }
    if command == "!guess" {
        guess(&env.server, &msg, parts.next().unwrap_or(""))?;
        return Ok(());
    }
    if matches!(command.as_str(), "!tower" | "!wt") {
        tower_command(&env.server, &msg, parts)?;
        return Ok(());
    }
    let argument = parts.next().unwrap_or("");
    if argument.eq_ignore_ascii_case("tower") {
        tower_command(&env.server, &msg, parts)?;
        return Ok(());
    }
    match argument.to_ascii_lowercase().as_str() {
        "" => status(&env.server, &msg)?,
        "stats" | "score" => personal_stats(&env.server, &msg)?,
        "top" => top(&env.server, &msg.target)?,
        "new" if msg.role.is_some_and(|role| role.satisfies(Role::Admin)) => {
            if free_play_enabled(&env.server, &msg.target) {
                reset_free_players(&env.server, &msg.target)?;
            } else {
                reset_all_players(&env.server)?;
            }
            reply(
                &env.server,
                &msg.target,
                &themed(
                    "wordle.new",
                    &["A fresh Wordle has been laid out for the household."],
                    &[],
                )?,
            )?;
        }
        "new" => reply(
            &env.server,
            &msg.target,
            &themed(
                "wordle.new_denied",
                &["Only an administrator may lay out a fresh Wordle."],
                &[],
            )?,
        )?,
        word => guess(&env.server, &msg, word)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_letters_are_consumed_once() {
        assert_eq!(evaluate("bbbbbb", "aaaaab"), [0, 0, 0, 0, 0, 2]);
        assert_eq!(evaluate("street", "crates"), [1, 1, 1, 0, 2, 0]);
    }

    #[test]
    fn discoveries_accumulate_per_player() {
        let mut player = PlayerDaily {
            word: "crates".into(),
            correct: vec![None; 6],
            ..Default::default()
        };
        let first = update_discoveries(&mut player, "street", &evaluate("street", "crates"));
        assert_eq!(first, (4, 1));
        assert_eq!(player.correct[4], Some('e'));
        assert!(player.present.contains(&'s'));
        assert_eq!(
            update_discoveries(&mut player, "street", &evaluate("street", "crates")),
            (0, 0)
        );

        let mut exact_after_present = PlayerDaily {
            word: "crates".into(),
            correct: vec![None; 6],
            present: vec!['c'],
            ..Default::default()
        };
        let result = evaluate("closer", "crates");
        assert_eq!(result[0], 2);
        let scored = update_discoveries(&mut exact_after_present, "closer", &result);
        assert!(
            scored.0 >= 1,
            "a newly exact placement also grants a letter point"
        );
        assert!(scored.1 >= 1);
    }

    #[test]
    fn unsolved_word_carries_into_next_day_with_found_letters() {
        // On a new day an unsolved board keeps its word and everything uncovered so far,
        // but the guess list is cleared to grant a fresh set of attempts (the carry-over
        // branch of `ensure_player`).
        let mut player = PlayerDaily {
            user_id: "profile-a".into(),
            display: "Ada".into(),
            day: 1,
            word: "crates".into(),
            solved: false,
            guesses: vec!["street".into(), "plaits".into()],
            correct: vec![Some('c'), None, None, None, None, None],
            present: vec!['a'],
            absent: vec!['x'],
            used_words: vec!["crates".into()],
            chances_remaining: Some(2),
            failed_days: 0,
        };
        player.day = 2;
        player.guesses.clear();
        player.chances_remaining = None;
        assert_eq!(player.word, "crates");
        assert_eq!(player.correct[0], Some('c'));
        assert_eq!(player.present, vec!['a']);
        assert_eq!(player.absent, vec!['x']);
        assert!(player.guesses.is_empty());
        assert_eq!(player.chances_remaining, None);
    }

    #[test]
    fn first_fully_failed_day_carries_the_word_for_one_more_round() {
        let previous = PlayerDaily {
            day: 1,
            word: "crates".into(),
            guesses: vec!["street".into(); 4],
            failed_days: 1,
            chances_remaining: Some(0),
            ..Default::default()
        };

        let next = rollover_player(&previous, 2).unwrap();

        assert_eq!(next.word, "crates");
        assert_eq!(next.day, 2);
        assert!(next.guesses.is_empty());
        assert_eq!(next.chances_remaining, None);
        assert_eq!(next.failed_days, 1);
    }

    #[test]
    fn mercy_replacement_returns_failed_word_to_recent_circulation() {
        let previous = PlayerDaily {
            day: 2,
            word: "crates".into(),
            used_words: vec!["olderr".into(), "crates".into()],
            failed_days: MERCY_REROLL_AFTER_FAILED_DAYS,
            ..Default::default()
        };

        let next = mercy_player(&previous, 3, "birler".into());

        assert_eq!(next.word, "birler");
        assert_eq!(next.day, 3);
        assert_eq!(next.failed_days, 0);
        assert!(!next.used_words.contains(&"crates".to_string()));
        assert!(next.used_words.contains(&"olderr".to_string()));
        assert!(next.used_words.contains(&"birler".to_string()));
    }

    #[test]
    fn solved_board_resets_to_a_fresh_word_for_its_owner() {
        // A solved board rolling into a new day gets a brand-new word with nothing revealed,
        // while identity and answer history are preserved so words don't repeat.
        let previous = PlayerDaily {
            user_id: "profile-a".into(),
            display: "Ada".into(),
            day: 1,
            word: "crates".into(),
            solved: true,
            guesses: vec!["crates".into()],
            correct: vec![
                Some('c'),
                Some('r'),
                Some('a'),
                Some('t'),
                Some('e'),
                Some('s'),
            ],
            present: vec![],
            absent: vec!['x'],
            used_words: vec!["crates".into()],
            chances_remaining: Some(1),
            failed_days: 0,
        };
        let fresh = fresh_player(&previous, 2, "birler".into());
        assert_eq!(fresh.user_id, "profile-a");
        assert_eq!(fresh.day, 2);
        assert_eq!(fresh.word, "birler");
        assert!(!fresh.solved);
        assert!(fresh.guesses.is_empty());
        assert!(fresh.present.is_empty());
        assert!(fresh.absent.is_empty());
        assert_eq!(fresh.correct, vec![None; WORD_LENGTH]);
        assert!(fresh.used_words.contains(&"crates".to_string()));
        assert!(fresh.used_words.contains(&"birler".to_string()));
        assert_eq!(fresh.chances_remaining, None);
    }

    #[test]
    fn admin_new_replaces_only_the_target_players_board() {
        let profile = Profile {
            id: "profile-a".into(),
            nick: "Ada".into(),
            ..Default::default()
        };
        let mut daily = Daily {
            players: vec![
                PlayerDaily {
                    user_id: profile.id.clone(),
                    display: profile.nick.clone(),
                    day: 1,
                    word: "crates".into(),
                    guesses: vec!["street".into()],
                    used_words: vec!["crates".into()],
                    chances_remaining: Some(2),
                    ..Default::default()
                },
                PlayerDaily {
                    user_id: "profile-b".into(),
                    display: "Bea".into(),
                    day: 1,
                    word: "planet".into(),
                    guesses: vec!["closer".into()],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let untouched = daily.players[1].clone();

        assign_admin_word(&mut daily, &profile, 9, 0).unwrap();

        assert_eq!(daily.players[0].day, 9);
        assert_ne!(daily.players[0].word, "crates");
        assert!(daily.players[0].guesses.is_empty());
        assert_eq!(daily.players[0].chances_remaining, None);
        assert_eq!(daily.players[1].word, untouched.word);
        assert_eq!(daily.players[1].guesses, untouched.guesses);
    }

    #[test]
    fn admin_chances_sets_exact_remaining_attempts_without_erasing_guesses() {
        let profile = Profile {
            id: "profile-a".into(),
            nick: "Ada".into(),
            ..Default::default()
        };
        let mut daily = Daily {
            players: vec![PlayerDaily {
                user_id: profile.id.clone(),
                display: profile.nick.clone(),
                word: "crates".into(),
                guesses: vec!["street".into(), "plaits".into()],
                ..Default::default()
            }],
            ..Default::default()
        };

        set_admin_chances(&mut daily, &profile, 0, 3).unwrap();

        assert_eq!(remaining_attempts(&daily.players[0], 3), 3);
        assert_eq!(daily.players[0].guesses, vec!["street", "plaits"]);
        consume_attempt(&mut daily.players[0]);
        assert_eq!(remaining_attempts(&daily.players[0], 3), 2);
    }

    #[test]
    fn admin_chances_rolls_a_stale_unsolved_board_into_today() {
        let profile = Profile {
            id: "profile-a".into(),
            nick: "Ada".into(),
            ..Default::default()
        };
        let mut daily = Daily {
            players: vec![PlayerDaily {
                user_id: profile.id.clone(),
                day: 4,
                word: "crates".into(),
                guesses: vec!["street".into()],
                ..Default::default()
            }],
            ..Default::default()
        };

        set_admin_chances(&mut daily, &profile, 5, 2).unwrap();

        assert_eq!(daily.players[0].day, 5);
        assert!(daily.players[0].guesses.is_empty());
        assert_eq!(remaining_attempts(&daily.players[0], 3), 2);
    }

    #[test]
    fn legacy_shared_game_migrates_each_participant() {
        let mut daily = Daily {
            day: 42,
            word: "crates".into(),
            guesses: vec![UserGuesses {
                user_id: "profile-a".into(),
                display: "Ada".into(),
                guesses: vec!["street".into()],
            }],
            correct: vec![Some('e'), None, None, None, None, None],
            ..Default::default()
        };
        migrate_shared_game(&mut daily);
        assert!(daily.word.is_empty());
        assert_eq!(daily.players.len(), 1);
        assert_eq!(daily.players[0].user_id, "profile-a");
        assert_eq!(daily.players[0].word, "crates");
        assert_eq!(daily.players[0].correct[0], Some('e'));
    }

    #[test]
    fn used_word_selection_avoids_recent_answers() {
        let chosen = choose_word(&[answers()[0].into()], 0);
        assert_ne!(chosen, answers()[0]);
    }

    #[test]
    fn answers_are_a_smaller_pool_of_valid_guesses() {
        // The answer pool is the curated subset: strictly smaller than the full list, and every
        // answer must itself be an accepted guess (so a chosen word is always guessable).
        assert!(!answers().is_empty());
        assert!(answers().len() < words().len());
        for answer in answers() {
            assert!(
                valid_word(answer),
                "answer {answer} is not an accepted guess"
            );
        }
    }

    #[test]
    fn tower_answer_pools_are_nonempty_and_guessable() {
        for floor in TOWER_START_FLOOR..=TOWER_MAX_FLOOR {
            let words = tower_words(floor);
            let answers = tower_answers(floor);
            assert!(!words.is_empty(), "Floor {floor} has no guesses");
            assert!(!answers.is_empty(), "Floor {floor} has no answers");
            assert!(answers.len() < words.len());
            assert!(answers.iter().all(|answer| {
                answer.len() == floor as usize && words.binary_search(answer).is_ok()
            }));
        }
    }

    #[test]
    fn tower_puzzle_uses_the_floor_length_and_remembers_answer() {
        let mut player = TowerPlayer {
            floor: 7,
            highest_floor_ever: 7,
            ..Default::default()
        };
        start_tower_puzzle(&mut player, 7, 100, false).unwrap();
        assert_eq!(player.answer.len(), 7);
        assert_eq!(player.correct.len(), 7);
        assert_eq!(player.guesses, Vec::<String>::new());
        assert_eq!(player.used_words, vec![player.answer.clone()]);
        assert_eq!(player.run_started_at, Some(100));
    }

    #[test]
    fn tower_status_uses_irc_safe_separators() {
        let player = TowerPlayer {
            floor: 5,
            highest_floor_ever: 5,
            answer: "crane".into(),
            guesses: vec!["amend".into(), "thorn".into()],
            correct: vec![None, None, None, None, Some('e')],
            present: vec!['a'],
            absent: vec!['d', 'h', 'm', 'o', 't'],
            ..Default::default()
        };

        let status = tower_status_text(&player);

        assert!(!status.contains('\n'));
        assert!(status.contains("FLOOR 5 | Puzzle"));
        assert!(status.contains("Strikes: 0/3 | Pattern: _ _ _ _ e"));
        assert!(status.contains("Present: a | Absent: d, h, m, o, t"));
    }

    #[test]
    fn tower_feedback_is_plain_text_and_client_independent() {
        let player = TowerPlayer {
            floor: 5,
            answer: "crane".into(),
            guesses: vec!["aroma".into()],
            correct: vec![None, Some('r'), None, None, None],
            present: vec!['a'],
            ..Default::default()
        };
        let result = evaluate_dynamic("aroma", "crane");

        let feedback = tower_feedback(&player, &result);

        assert_eq!(
            feedback,
            "The word contains 2 of your letters, 1 correctly placed: _ r _ _ _. Misplaced: a."
        );
        assert!(!feedback.contains('⬛'));
        assert!(!feedback.contains('🟨'));
        assert!(!feedback.contains('🟩'));
    }

    #[test]
    fn tower_promotion_advances_and_clears_strikes() {
        let mut player = TowerPlayer {
            floor: 5,
            highest_floor_ever: 5,
            promotion_streak: TOWER_PROMOTION_SOLVES - 1,
            strikes: 2,
            run_started_at: Some(100),
            ..Default::default()
        };

        let (promoted, cap_cleared) = record_tower_solve(&mut player, 160);

        assert!(promoted);
        assert!(!cap_cleared);
        assert_eq!(player.floor, 6);
        assert_eq!(player.highest_floor_ever, 6);
        assert_eq!(player.promotion_streak, 0);
        assert_eq!(player.strikes, 0);
        assert_eq!(player.fastest_promotion_secs, Some(60));
    }

    #[test]
    fn tower_floor_eight_is_a_stable_cap() {
        let mut player = TowerPlayer {
            floor: TOWER_MAX_FLOOR,
            highest_floor_ever: TOWER_MAX_FLOOR,
            promotion_streak: TOWER_PROMOTION_SOLVES - 1,
            strikes: 2,
            ..Default::default()
        };

        let (promoted, cap_cleared) = record_tower_solve(&mut player, 0);

        assert!(!promoted);
        assert!(cap_cleared);
        assert_eq!(player.floor, TOWER_MAX_FLOOR);
        assert_eq!(player.strikes, 2);
        assert_eq!(player.promotion_streak, 0);
    }

    #[test]
    fn legacy_tower_state_defaults_to_floor_five() {
        let mut player: TowerPlayer =
            serde_json::from_str(r#"{"user_id":"profile-a","display":"Ada"}"#).unwrap();
        normalise_tower(&mut player);
        assert_eq!(player.floor, TOWER_START_FLOOR);
        assert_eq!(player.highest_floor_ever, TOWER_START_FLOOR);
        assert_eq!(player.strikes, 0);
        assert!(player.answer.is_empty());
    }

    #[test]
    fn legacy_daily_state_has_no_free_rooms() {
        let daily: Daily = serde_json::from_str(r#"{"players":[],"tower":[]}"#).unwrap();
        assert!(daily.free_rooms.is_empty());
    }

    #[test]
    fn free_channel_keys_are_case_insensitive() {
        assert_eq!(room_key("#Games"), room_key("#games"));
    }

    #[test]
    fn full_free_pool_uses_the_large_six_letter_lexicon() {
        assert!(words().len() > answers().len());
        let word = choose_free_word(&[], 0, true);
        assert!(words().contains(&word.as_str()));
    }
}
