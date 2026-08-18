//! Channel-local high/low cards using one standard deck per active player.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, DataSubject, EconomyTransactionRequest, Event, EventEnvelope, KvGet, KvList,
    KvSet, MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse,
    ModuleKvMutation, Profile, ProfileKey, RandomBytesRequest, RandomBytesResponse, SendMessage,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const DEFAULT_GAME_ROOM: &str = "#games";
const DECK_SIZE: u8 = 52;

const RANKS: [&str; 13] = [
    "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "jack", "queen",
    "king", "ace",
];
const SUITS: [&str; 4] = ["clubs", "diamonds", "hearts", "spades"];

#[cfg(not(test))]
#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_list(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn economy_award(input: String) -> String;
}

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
unsafe fn kv_list(_: String) -> Result<String, Error> {
    Ok("[]".into())
}

#[cfg(test)]
unsafe fn kv_set(_: String) -> Result<(), Error> {
    Ok(())
}

#[cfg(test)]
unsafe fn random_bytes(input: String) -> Result<String, Error> {
    let request: RandomBytesRequest = serde_json::from_str(&input)?;
    let bytes = (0..request.count).map(|index| index as u8).collect();
    Ok(serde_json::to_string(&RandomBytesResponse { bytes })?)
}

#[cfg(test)]
unsafe fn setting_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn profile_get(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn award_stats(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[cfg(test)]
unsafe fn economy_award(_: String) -> Result<String, Error> {
    Ok(String::new())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Run {
    #[serde(default)]
    run_id: String,
    current: u8,
    remaining: Vec<u8>,
    streak: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Stats {
    display: String,
    best_streak: u64,
    completed_runs: u64,
    correct_guesses: u64,
    failed_runs: u64,
    ties: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Guess {
    High,
    Low,
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let achievements = vec![
        AchievementSpec {
            id: "first_prediction".into(),
            name: "A Sound Prediction".into(),
            description: "Make your first correct high/low prediction.".into(),
            stat: "first_successes".into(),
            threshold: 1,
            optional: false,
            secret: false,
        },
        AchievementSpec {
            id: "five_streak".into(),
            name: "A Promising Run".into(),
            description: "Reach a five-card prediction streak.".into(),
            stat: "five_streaks".into(),
            threshold: 1,
            optional: false,
            secret: false,
        },
        AchievementSpec {
            id: "ten_streak".into(),
            name: "Read the Pack".into(),
            description: "Reach a ten-card prediction streak.".into(),
            stat: "ten_streaks".into(),
            threshold: 1,
            optional: false,
            secret: false,
        },
        AchievementSpec {
            id: "twenty_streak".into(),
            name: "Pack Whisperer".into(),
            description: "Reach a twenty-card prediction streak.".into(),
            stat: "twenty_streaks".into(),
            threshold: 1,
            optional: false,
            secret: false,
        },
        AchievementSpec {
            id: "fifty_streak".into(),
            name: "The Deck Bows".into(),
            description: "Reach a fifty-card prediction streak.".into(),
            stat: "fifty_streaks".into(),
            threshold: 1,
            optional: true,
            secret: true,
        },
        AchievementSpec {
            id: "complete_deck".into(),
            name: "No Card Left Unturned".into(),
            description: "Predict correctly through an entire deck.".into(),
            stat: "complete_decks".into(),
            threshold: 1,
            optional: true,
            secret: true,
        },
    ];
    let stats = [
        "first_successes",
        "five_streaks",
        "ten_streaks",
        "twenty_streaks",
        "fifty_streaks",
        "complete_decks",
        "correct_guesses",
        "completed_runs",
        "failed_runs",
        "ties",
    ]
    .into_iter()
    .map(|id| AchievementStat {
        id: id.into(),
        description: id.replace('_', " "),
    })
    .collect();
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats,
        achievements,
        prestige: vec![jeeves_abi::PrestigeSpec {
            id: "highlow_master".into(),
            name: "High/Low Master".into(),
            stat: "correct_guesses".into(),
            first_threshold: 100,
            every: 100,
        }],
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "hl".into(),
                aliases: vec!["highlow".into()],
                description: "Play high/low with a standard deck of cards.".into(),
                usage: "!hl [score | <user>]".into(),
            },
            CommandSpec {
                name: "high".into(),
                aliases: Vec::new(),
                description: "Predict that the next card is higher.".into(),
                usage: "!high".into(),
            },
            CommandSpec {
                name: "low".into(),
                aliases: Vec::new(),
                description: "Predict that the next card is lower.".into(),
                usage: "!low".into(),
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "game_room".into(),
            description: "Channel where high/low cards is available.".into(),
            default: DEFAULT_GAME_ROOM.into(),
            kind: SettingKind::String { max_len: 64 },
            scopes: vec![SettingScope::Global, SettingScope::Network],
            applies_immediately: true,
        }],
    })?)
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let mut values = Vec::new();
    for entry in &request.entries {
        if key_belongs_to_subject(&entry.key, &request.subject, &request.aliases)
            && !entry.value.is_empty()
        {
            values.push(serde_json::json!({ "key": entry.key, "value": serde_json::from_str::<serde_json::Value>(&entry.value)? }));
        }
    }
    let data = if values.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "records": values })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let mutations = request
        .entries
        .iter()
        .filter(|entry| key_belongs_to_subject(&entry.key, &request.subject, &request.aliases))
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect();
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

fn key_belongs_to_subject(key: &str, subject: &DataSubject, aliases: &[String]) -> bool {
    let Some(identity) = key.rsplit(':').next() else {
        return false;
    };
    let subject_prefix = format!("{}:", subject.server);
    (key.starts_with(&format!("run:{subject_prefix}"))
        || key.starts_with(&format!("stats:{subject_prefix}")))
        && (identity == subject.profile_id
            || aliases
                .iter()
                .any(|alias| identity.eq_ignore_ascii_case(alias)))
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

fn room_key(channel: &str) -> String {
    channel.to_ascii_lowercase()
}

fn run_key(server: &str, channel: &str, user_id: &str) -> String {
    format!("run:{server}:{}:{user_id}", room_key(channel))
}

fn stats_key(server: &str, channel: &str, user_id: &str) -> String {
    format!("stats:{server}:{}:{user_id}", room_key(channel))
}

fn stats_prefix(server: &str, channel: &str) -> String {
    format!("stats:{server}:{}:", room_key(channel))
}

fn load_run(server: &str, channel: &str, user_id: &str) -> Result<Option<Run>, Error> {
    let raw = kv_load(&run_key(server, channel, user_id))?;
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn save_run(server: &str, channel: &str, user_id: &str, run: &Run) -> Result<(), Error> {
    kv_save(
        &run_key(server, channel, user_id),
        &serde_json::to_string(run)?,
    )
}

fn clear_run(server: &str, channel: &str, user_id: &str) -> Result<(), Error> {
    kv_save(&run_key(server, channel, user_id), "")
}

fn load_stats(server: &str, channel: &str, user_id: &str) -> Result<Stats, Error> {
    let raw = kv_load(&stats_key(server, channel, user_id))?;
    if raw.trim().is_empty() {
        Ok(Stats::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn save_stats(server: &str, channel: &str, user_id: &str, stats: &Stats) -> Result<(), Error> {
    kv_save(
        &stats_key(server, channel, user_id),
        &serde_json::to_string(stats)?,
    )
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

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn identity(msg: &MessagePayload) -> String {
    if msg.user_id.is_empty() {
        format!("nick:{}", msg.nick.to_ascii_lowercase())
    } else {
        msg.user_id.clone()
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
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

fn profile_for_nick(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&raw)?))
    }
}

fn random_index(upper: usize) -> Result<usize, Error> {
    if upper == 0 {
        return Err(Error::msg("cannot draw from an empty deck"));
    }
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    let bytes: [u8; 8] = response
        .bytes
        .get(..8)
        .ok_or_else(|| Error::msg("randomness host returned too few bytes"))?
        .try_into()
        .map_err(|_| Error::msg("randomness host returned an invalid length"))?;
    Ok((u64::from_le_bytes(bytes) % upper as u64) as usize)
}

fn random_token() -> Result<String, Error> {
    let raw = unsafe {
        random_bytes(serde_json::to_string(&RandomBytesRequest { count: 8 })?)?
    };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    if response.bytes.len() < 8 {
        return Err(Error::msg("randomness host returned too few bytes"));
    }
    Ok(response.bytes[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn draw(remaining: &mut Vec<u8>, index: usize) -> Option<u8> {
    (index < remaining.len()).then(|| remaining.swap_remove(index))
}

fn rank(card: u8) -> u8 {
    (card % 13) + 2
}

fn card_name(card: u8) -> String {
    format!(
        "{} of {}",
        RANKS[(card % 13) as usize],
        SUITS[(card / 13) as usize]
    )
}

fn correct(guess: Guess, current: u8, next: u8) -> bool {
    match guess {
        Guess::High => rank(next) > rank(current),
        Guess::Low => rank(next) < rank(current),
    }
}

fn increments_for_streak(streak: u64) -> Vec<StatIncrement> {
    let mut increments = vec![StatIncrement {
        stat: "correct_guesses".into(),
        amount: 1,
    }];
    if streak == 1 {
        increments.push(StatIncrement {
            stat: "first_successes".into(),
            amount: 1,
        });
    }
    for (threshold, stat) in [
        (5, "five_streaks"),
        (10, "ten_streaks"),
        (20, "twenty_streaks"),
        (50, "fifty_streaks"),
    ] {
        if streak == threshold {
            increments.push(StatIncrement {
                stat: stat.into(),
                amount: 1,
            });
        }
    }
    increments
}

fn brass_for_streak(streak: u64) -> Option<u64> {
    match streak {
        5 => Some(10),
        10 => Some(15),
        20 => Some(20),
        _ => None,
    }
}

fn award(
    server: &str,
    msg: &MessagePayload,
    increments: Vec<StatIncrement>,
    event: &str,
) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments,
            deduplication_id: Some(format!("{event}:{}:{}", room_key(&msg.target), msg.user_id)),
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

fn award_streak_brass(server: &str, msg: &MessagePayload, run: &Run) -> Result<(), Error> {
    if let Some(amount) = brass_for_streak(run.streak) {
        award_brass(
            server,
            msg,
            amount,
            &format!("cards:streak:{}:{}", run.run_id, run.streak),
            "highlow_streak",
        )?;
    }
    Ok(())
}

fn room_record(server: &str, channel: &str) -> Result<(u64, String), Error> {
    let prefix = stats_prefix(server, channel);
    let mut best = (0, String::from("nobody"));
    for entry in kv_list_entries()? {
        let Some(_) = entry.key.strip_prefix(&prefix) else {
            continue;
        };
        let Ok(stats) = serde_json::from_str::<Stats>(&entry.value) else {
            continue;
        };
        if stats.best_streak > best.0 {
            best = (stats.best_streak, stats.display);
        }
    }
    Ok(best)
}

fn start(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let user_id = identity(msg);
    if let Some(run) = load_run(server, &msg.target, &user_id)? {
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.already_playing",
                &["You are already holding the {card}, {user}, with a streak of {streak}. Call !high or !low."],
                &[
                    ("card", &card_name(run.current)),
                    ("user", display(msg)),
                    ("streak", &run.streak.to_string()),
                ],
            )?,
        );
    }
    let mut remaining = (0..DECK_SIZE).collect::<Vec<_>>();
    let opening_index = random_index(remaining.len())?;
    let current = draw(&mut remaining, opening_index)
        .ok_or_else(|| Error::msg("failed to draw the opening card"))?;
    save_run(
        server,
        &msg.target,
        &user_id,
        &Run {
            run_id: random_token()?,
            current,
            remaining,
            streak: 0,
        },
    )?;
    reply(
        server,
        &msg.target,
        &themed(
            "cards.start",
            &["{user} draws the {card}. Higher or lower?"],
            &[("user", display(msg)), ("card", &card_name(current))],
        )?,
    )
}

fn score(server: &str, msg: &MessagePayload, argument: &str) -> Result<(), Error> {
    if argument.is_empty() {
        let stats = load_stats(server, &msg.target, &identity(msg))?;
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.personal_score",
                &["{user}'s best high/low streak is {streak}."],
                &[
                    ("user", display(msg)),
                    ("streak", &stats.best_streak.to_string()),
                ],
            )?,
        );
    }
    if argument.eq_ignore_ascii_case("score") {
        let (streak, user) = room_record(server, &msg.target)?;
        let room = game_room(server, &msg.target);
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.room_score",
                &["The {room} high/low record is {streak}, held by {user}."],
                &[
                    ("room", &room),
                    ("streak", &streak.to_string()),
                    ("user", &user),
                ],
            )?,
        );
    }
    let nick = argument.trim_start_matches('$');
    let Some(profile) = profile_for_nick(server, nick)? else {
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.unknown_user",
                &["I have no high/low record for {user}, sir."],
                &[("user", nick)],
            )?,
        );
    };
    let stats = load_stats(server, &msg.target, &profile.id)?;
    reply(
        server,
        &msg.target,
        &themed(
            "cards.personal_score",
            &["{user}'s best high/low streak is {streak}."],
            &[("user", nick), ("streak", &stats.best_streak.to_string())],
        )?,
    )
}

fn guess(server: &str, msg: &MessagePayload, guess: Guess) -> Result<(), Error> {
    let user_id = identity(msg);
    let Some(mut run) = load_run(server, &msg.target, &user_id)? else {
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.no_run",
                &["There is no active deck before you, {user}. Begin with !hl."],
                &[("user", display(msg))],
            )?,
        );
    };
    let next_index = random_index(run.remaining.len())?;
    let next = draw(&mut run.remaining, next_index)
        .ok_or_else(|| Error::msg("active deck has no remaining cards"))?;
    let was_tie = rank(next) == rank(run.current);
    let was_correct = !was_tie && correct(guess, run.current, next);
    if was_correct {
        run.current = next;
        run.streak += 1;
        let mut stats = load_stats(server, &msg.target, &user_id)?;
        stats.display = display(msg).into();
        stats.correct_guesses += 1;
        stats.best_streak = stats.best_streak.max(run.streak);
        save_stats(server, &msg.target, &user_id, &stats)?;
        if run.remaining.is_empty() {
            clear_run(server, &msg.target, &user_id)?;
            stats.completed_runs += 1;
            save_stats(server, &msg.target, &user_id, &stats)?;
            award_streak_brass(server, msg, &run)?;
            let mut increments = increments_for_streak(run.streak);
            increments.push(StatIncrement {
                stat: "complete_decks".into(),
                amount: 1,
            });
            increments.push(StatIncrement {
                stat: "completed_runs".into(),
                amount: 1,
            });
            award(server, msg, increments, &format!("complete:{}:{}", run.run_id, run.streak))?;
            return reply(
                server,
                &msg.target,
                &themed(
                    "cards.complete",
                    &["{user} has correctly called the entire deck. A streak of {streak}; frankly, insufferable."],
                    &[("user", display(msg)), ("streak", &run.streak.to_string())],
                )?,
            );
        }
        save_run(server, &msg.target, &user_id, &run)?;
        award_streak_brass(server, msg, &run)?;
        award(
            server,
            msg,
            increments_for_streak(run.streak),
            &format!("success:{}:{}", run.run_id, run.streak),
        )?;
        return reply(
            server,
            &msg.target,
            &themed(
                "cards.correct",
                &["Correct, {user}. The next card is the {card}. Streak: {streak}."],
                &[
                    ("user", display(msg)),
                    ("card", &card_name(next)),
                    ("streak", &run.streak.to_string()),
                ],
            )?,
        );
    }

    let mut stats = load_stats(server, &msg.target, &user_id)?;
    stats.display = display(msg).into();
    stats.completed_runs += 1;
    stats.failed_runs += 1;
    stats.best_streak = stats.best_streak.max(run.streak);
    if was_tie {
        stats.ties += 1;
    }
    save_stats(server, &msg.target, &user_id, &stats)?;
    clear_run(server, &msg.target, &user_id)?;
    let mut increments = vec![
        StatIncrement {
            stat: "completed_runs".into(),
            amount: 1,
        },
        StatIncrement {
            stat: "failed_runs".into(),
            amount: 1,
        },
    ];
    if was_tie {
        increments.push(StatIncrement {
            stat: "ties".into(),
            amount: 1,
        });
    }
    award(
        server,
        msg,
        increments,
        &format!("failure:{}:{}:{}", run.run_id, run.streak, card_name(next)),
    )?;
    let ending = if was_tie {
        format!(
            "The next card is the {} as well. The tie ends",
            card_name(next)
        )
    } else {
        format!(
            "The next card is the {}. The call was wrong and the run ends",
            card_name(next)
        )
    };
    reply(
        server,
        &msg.target,
        &themed(
            "cards.failed",
            &["{ending} at streak {streak}, {user}."],
            &[
                ("ending", &ending),
                ("streak", &run.streak.to_string()),
                ("user", display(msg)),
            ],
        )?,
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut parts = msg.text.split_whitespace();
    let command = parts.next().unwrap_or("").to_ascii_lowercase();
    if !matches!(command.as_str(), "!hl" | "!highlow" | "!high" | "!low") {
        return Ok(());
    }
    if msg.is_private {
        let room = game_room(&env.server, &msg.nick);
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "cards.channel_only",
                &["High/low is played in {room}, sir."],
                &[("room", &room)],
            )?,
        )?;
        return Ok(());
    }
    if !in_game_room(&env.server, &msg.target) {
        let room = game_room(&env.server, &msg.target);
        reply(
            &env.server,
            &msg.target,
            &themed(
                "cards.room_redirect",
                &["The cards have decamped to {room}, {user}. Do join us there."],
                &[("room", &room), ("user", display(&msg))],
            )?,
        )?;
        return Ok(());
    }
    match command.as_str() {
        "!high" => guess(&env.server, &msg, Guess::High)?,
        "!low" => guess(&env.server, &msg, Guess::Low)?,
        _ => match parts.next().unwrap_or("").to_ascii_lowercase().as_str() {
            "" => start(&env.server, &msg)?,
            argument => score(&env.server, &msg, argument)?,
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deck_contains_each_card_once() {
        let mut deck = (0..DECK_SIZE).collect::<Vec<_>>();
        let mut drawn = Vec::new();
        while !deck.is_empty() {
            drawn.push(draw(&mut deck, 0).expect("card should be available"));
        }
        drawn.sort_unstable();
        assert_eq!(drawn, (0..DECK_SIZE).collect::<Vec<_>>());
    }

    #[test]
    fn high_and_low_require_strict_rank_changes() {
        assert!(correct(Guess::High, 0, 12));
        assert!(correct(Guess::Low, 12, 0));
        assert!(!correct(Guess::High, 6, 19));
        assert!(!correct(Guess::Low, 6, 19));
    }

    #[test]
    fn room_and_user_keys_are_isolated() {
        assert_ne!(
            run_key("irc", "#games", "profile-a"),
            run_key("irc", "#transience", "profile-a")
        );
        assert_ne!(
            stats_key("irc", "#games", "profile-a"),
            stats_key("irc", "#games", "profile-b")
        );
    }

    #[test]
    fn streak_achievements_award_only_at_threshold() {
        assert_eq!(increments_for_streak(4).len(), 1);
        assert_eq!(increments_for_streak(5).len(), 2);
        assert_eq!(increments_for_streak(10).len(), 2);
    }
}
