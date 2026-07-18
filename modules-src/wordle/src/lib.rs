//! Daily personal six-letter Wordle, modelled after the original Jeeves game.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, MessagePayload, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, RandomBytesRequest,
    RandomBytesResponse, Role, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::sync::OnceLock;

const WORD_LENGTH: usize = 6;
const DEFAULT_MAX_ATTEMPTS: i64 = 3;
const MAX_ACTIVE_USERS: usize = 2_000;
const MAX_STATS_USERS: usize = 2_000;
const USED_WORD_WINDOW: usize = 4_096;

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
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: [
            "letters",
            "positions",
            "wins",
            "first_guess",
            "final_attempt",
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
    let Some(entry) = request
        .entries
        .iter()
        .find(|entry| entry.key == stats_key(&request.server))
    else {
        return Ok(serde_json::to_string(
            &AchievementBackfillResponse::default(),
        )?);
    };
    let values = serde_json::from_str::<Vec<UserStats>>(&entry.value)?
        .into_iter()
        .filter(|stats| !stats.user_id.is_empty() && !stats.user_id.starts_with("nick:"))
        .map(|stats| AchievementSetMax {
            profile_id: stats.user_id,
            stat: "wins".into(),
            value: stats.wins as u64,
        })
        .collect();
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

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "word".into(),
                aliases: vec!["wordle".into()],
                description: "Play or inspect your daily personal six-letter Wordle.".into(),
                usage: "!word [<guess> | stats | top | new]".into(),
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
        settings: vec![SettingSpec {
            key: "max_attempts_per_user".into(),
            description: "Guesses each person receives per Wordle day.".into(),
            default: DEFAULT_MAX_ATTEMPTS.to_string(),
            kind: SettingKind::Integer { min: 1, max: 10 },
            scopes: vec![SettingScope::Global, SettingScope::Network],
            applies_immediately: true,
        }],
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
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct UserStats {
    user_id: String,
    display: String,
    wins: u32,
    games_played: u32,
}

fn words() -> &'static [&'static str] {
    static WORDS: OnceLock<Vec<&'static str>> = OnceLock::new();
    WORDS.get_or_init(|| {
        include_str!("../../../wordle-six-letter-words.txt")
            .lines()
            .filter(|word| {
                word.len() == WORD_LENGTH && word.bytes().all(|byte| byte.is_ascii_lowercase())
            })
            .collect()
    })
}

fn valid_word(word: &str) -> bool {
    words().binary_search(&word).is_ok()
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
    let data = if stats.is_none() && player.is_none() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "stats": stats, "current_game": player })
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
            let changed = before != daily.players.len();
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

fn attempts_setting(server: &str) -> i64 {
    (|| -> Option<i64> {
        unsafe {
            setting_get(
                serde_json::to_string(&SettingGet {
                    key: "max_attempts_per_user".into(),
                    server: Some(server.into()),
                    channel: None,
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

fn host_random(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    Ok(serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes)
}

fn choose_word(used: &[String], random: u64) -> String {
    let used = used.iter().map(String::as_str).collect::<BTreeSet<_>>();
    let available = words()
        .iter()
        .copied()
        .filter(|word| !used.contains(word))
        .collect::<Vec<_>>();
    let pool = if available.is_empty() {
        words().to_vec()
    } else {
        available
    };
    pool[(random as usize) % pool.len()].to_string()
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

fn new_word(previous: &PlayerDaily, day: i64) -> Result<PlayerDaily, Error> {
    let bytes = host_random(8)?;
    let random = u64::from_le_bytes(bytes.try_into().unwrap_or([0; 8]));
    Ok(fresh_player(
        previous,
        day,
        choose_word(&previous.used_words, random),
    ))
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
        *player = new_word(player, day)?;
    } else if !player.solved && player.day != day {
        player.day = day;
        player.guesses.clear();
        save_daily(server, &daily)?;
    }
    save_daily(server, &daily)?;
    Ok((daily, index))
}

fn reset_all_players(server: &str) -> Result<(), Error> {
    let mut daily = load_daily(server)?;
    let day = utc_day()?;
    for player in &mut daily.players {
        *player = new_word(player, day)?;
    }
    save_daily(server, &daily)
}

fn evaluate(guess: &str, answer: &str) -> [u8; WORD_LENGTH] {
    let guess = guess.as_bytes();
    let answer = answer.as_bytes();
    let mut result = [0; WORD_LENGTH];
    let mut used = [false; WORD_LENGTH];
    for index in 0..WORD_LENGTH {
        if guess[index] == answer[index] {
            result[index] = 2;
            used[index] = true;
        }
    }
    for index in 0..WORD_LENGTH {
        if result[index] == 2 {
            continue;
        }
        if let Some(found) =
            (0..WORD_LENGTH).find(|other| !used[*other] && guess[index] == answer[*other])
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
        });
    }
}

fn guess(server: &str, msg: &MessagePayload, raw: &str) -> Result<(), Error> {
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
    let max_attempts = attempts_setting(server) as usize;
    if daily.players[index].solved {
        return status(server, msg);
    }
    if daily.players[index].guesses.len() >= max_attempts {
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
    let result = evaluate(&guess, &daily.players[index].word);
    let (new_letters, new_positions) =
        update_discoveries(&mut daily.players[index], &guess, &result);
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
        if attempt == max_attempts {
            increments.push(("final_attempt", 1));
        }
        award(server, msg, increments)?;
        return Ok(());
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
    reply(server, channel, &themed(
        "wordle.guess",
        &["Your word contains {matched} of your letters, {exact} correctly placed: {pattern}. Misplaced: {misplaced}."],
        &[("matched", &matched.to_string()), ("exact", &exact.to_string()), ("pattern", &pattern(&daily.players[index])), ("misplaced", &letters(&misplaced.into_iter().collect::<Vec<_>>()))],
    )?)?;
    award(
        server,
        msg,
        vec![("letters", new_letters), ("positions", new_positions)],
    )
}

fn personal_stats(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let stats = load_stats(server)?;
    let entry = stats.iter().find(|entry| entry.user_id == identity(msg));
    let (wins, games) = entry
        .map(|entry| (entry.wins, entry.games_played))
        .unwrap_or_default();
    let rate = wins.saturating_mul(100).checked_div(games).unwrap_or(0);
    reply(
        server,
        &msg.target,
        &themed(
            "wordle.stats",
            &["{user}: {wins} win(s) in {games} game(s) ({rate}%)."],
            &[
                ("user", display(msg)),
                ("wins", &wins.to_string()),
                ("games", &games.to_string()),
                ("rate", &rate.to_string()),
            ],
        )?,
    )
}

fn top(server: &str, channel: &str) -> Result<(), Error> {
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
        "!word" | "!wordle" | "!guess" | "!wordlestats" | "!wstats"
    ) {
        return Ok(());
    }
    if msg.is_private {
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
    let argument = parts.next().unwrap_or("");
    match argument.to_ascii_lowercase().as_str() {
        "" => status(&env.server, &msg)?,
        "stats" => personal_stats(&env.server, &msg)?,
        "top" => top(&env.server, &msg.target)?,
        "new" if msg.role.is_some_and(|role| role.satisfies(Role::Admin)) => {
            reset_all_players(&env.server)?;
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
    fn unsolved_word_carries_into_next_day_for_its_owner() {
        let previous = PlayerDaily {
            day: 1,
            word: "crates".into(),
            correct: vec![Some('c'), None, None, None, None, None],
            ..Default::default()
        };
        let mut carried = previous.clone();
        carried.day = 2;
        carried.guesses.clear();
        assert_eq!(carried.word, "crates");
        assert_eq!(carried.correct[0], Some('c'));
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
        let chosen = choose_word(&[words()[0].into()], 0);
        assert_ne!(chosen, words()[0]);
    }
}
