//! Word scramble: first to unscramble a common word wins.
//!
//! `!scramble` posts a word's letters out of order (never the word itself, never another real
//! word); the first to type it, or any other real word from the same letters, wins
//! `brass_per_word`. A hint (first and last letters) comes at `hint_seconds` and the word at
//! `reveal_seconds`. `!scramble fastest` shows the channel's quickest solvers, `!scramble top
//! [week|all]` the most solves, and `!scramble me` your own. Where `popups` is on (off by default)
//! a word also turns up by itself now and then, at most every `popup_minutes`, and only while the
//! channel is lively. Words come from wordle's curated lists; the word in play lives in KV and the
//! scheduler drives the hint and reveal, so a reload carries on.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, EconomyTransactionRequest, Event, EventEnvelope, MessagePayload,
    ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvEntry, ModuleKvMutation,
    ScheduleCancel, ScheduleSet, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{
    display, encode, honorific, kv_list_prefix, kv_load, kv_save, no_highlight, reply, setting,
    setting_i64, themed, timestamp, Entropy,
};
use model::{answers, hint, lively, scramble, solves, spaced, Career, QUICK_SECONDS};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, VecDeque};

mod model;

/// Words remembered per channel so they don't come round again soon.
const RECENT_KEPT: usize = 200;
/// Lines remembered per channel for judging whether it's lively.
const ACTIVITY_KEPT: usize = 30;
const LIVELY_WINDOW_SECONDS: i64 = 10 * 60;
const LIVELY_LINES: usize = 8;
const BOARD_SIZE: usize = 5;

#[host_fn]
extern "ExtismHost" {
    fn award_stats(input: String) -> String;
    fn economy_award(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "scramble".into(),
            aliases: Vec::new(),
            description:
                "Unscramble a word before anyone else; any real word from the same letters counts."
                    .into(),
            usage: "!scramble | fastest | top [week|all] | me".into(),
            ..Default::default()
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all = vec![
        SettingScope::Global,
        SettingScope::Network,
        SettingScope::Channel,
    ];
    let integer = |key: &str, description: &str, default: i64, min: i64, max: i64| SettingSpec {
        key: key.into(),
        description: description.into(),
        default: default.to_string(),
        kind: SettingKind::Integer { min, max },
        scopes: all.clone(),
        applies_immediately: true,
    };
    let boolean = |key: &str, description: &str, default: bool| SettingSpec {
        key: key.into(),
        description: description.into(),
        default: default.to_string(),
        kind: SettingKind::Boolean,
        scopes: all.clone(),
        applies_immediately: true,
    };
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            boolean("enabled", "Allow scrambles here.", true),
            boolean(
                "popups",
                "Post a scramble by itself now and then while the channel is lively.",
                false,
            ),
            integer(
                "popup_minutes",
                "Least time between pop-up scrambles.",
                45,
                10,
                480,
            ),
            integer("hint_seconds", "Seconds before the hint.", 20, 5, 120),
            integer(
                "reveal_seconds",
                "Seconds before the word is revealed.",
                40,
                10,
                300,
            ),
            integer("brass_per_word", "Brass for unscrambling a word.", 3, 0, 50),
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let stats = [
        ("solved", "Words unscrambled"),
        ("quick_solves", "Words unscrambled within three seconds"),
    ]
    .into_iter()
    .map(|(id, description)| AchievementStat {
        id: id.into(),
        description: description.into(),
    })
    .collect();
    let achievements = [
        (
            "unscrambled",
            "Unscrambled",
            "Unscramble a word.",
            "solved",
            1,
            false,
        ),
        (
            "anagrammarian",
            "Anagrammarian",
            "Unscramble 100 words.",
            "solved",
            100,
            false,
        ),
        (
            "lexicographer",
            "Lexicographer",
            "Unscramble 1,000 words.",
            "solved",
            1_000,
            false,
        ),
        (
            "quick_wit",
            "Quick Wit",
            "Unscramble a word within three seconds.",
            "quick_solves",
            1,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(id, name, description, stat, threshold, optional)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            stat: stat.into(),
            threshold,
            optional,
            secret: false,
        },
    )
    .collect();
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats,
        achievements,
        prestige: Vec::new(),
    })?)
}

// ── the word in play ────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Puzzle {
    server: String,
    channel: String,
    id: String,
    word: String,
    letters: String,
    asked_at: i64,
    #[serde(default)]
    hinted: bool,
    /// Bumped whenever a timer is booked, so an older timer is recognised and ignored.
    #[serde(default)]
    seq: u64,
}

type ChannelKey = (String, String);

thread_local! {
    static PUZZLES: RefCell<Option<HashMap<ChannelKey, Puzzle>>> = const { RefCell::new(None) };
    /// Recent lines per channel (time, who), for judging liveliness; memory only.
    static ACTIVITY: RefCell<HashMap<ChannelKey, VecDeque<(i64, String)>>> =
        RefCell::new(HashMap::new());
}

fn puzzle_key(server: &str, channel: &str) -> String {
    format!("word:{}:{}", encode(server), encode(channel))
}

fn timer_id(server: &str, channel: &str) -> String {
    format!("w:{}:{}", encode(server), encode(channel))
}

fn career_prefix(server: &str, channel: &str) -> String {
    format!("career:{}:{}:", encode(server), encode(channel))
}

/// When the channel last had a scramble, and the words it has seen lately.
#[derive(Default, Serialize, Deserialize)]
struct History {
    #[serde(default)]
    last_at: i64,
    #[serde(default)]
    recent: Vec<String>,
}

fn history_key(server: &str, channel: &str) -> String {
    format!("history:{}:{}", encode(server), encode(channel))
}

fn load<T: serde::de::DeserializeOwned + Default>(key: &str) -> Result<T, Error> {
    let raw = kv_load(key)?;
    if raw.trim().is_empty() {
        Ok(T::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn with_puzzles<T>(f: impl FnOnce(&mut HashMap<ChannelKey, Puzzle>) -> T) -> Result<T, Error> {
    let loaded = PUZZLES.with(|puzzles| puzzles.borrow().is_some());
    if !loaded {
        let mut puzzles = HashMap::new();
        for entry in kv_list_prefix("word:")? {
            if entry.value.trim().is_empty() {
                continue;
            }
            let puzzle: Puzzle = serde_json::from_str(&entry.value)?;
            puzzles.insert((puzzle.server.clone(), puzzle.channel.clone()), puzzle);
        }
        PUZZLES.with(|cell| *cell.borrow_mut() = Some(puzzles));
    }
    Ok(PUZZLES.with(|puzzles| f(puzzles.borrow_mut().as_mut().expect("loaded above"))))
}

fn puzzle_in(server: &str, channel: &str) -> Result<Option<Puzzle>, Error> {
    with_puzzles(|puzzles| {
        puzzles
            .get(&(server.to_string(), channel.to_string()))
            .cloned()
    })
}

fn book(puzzle: &mut Puzzle, due_at: i64) -> Result<(), Error> {
    puzzle.seq += 1;
    kv_save(
        &puzzle_key(&puzzle.server, &puzzle.channel),
        &serde_json::to_string(puzzle)?,
    )?;
    with_puzzles(|puzzles| {
        puzzles.insert(
            (puzzle.server.clone(), puzzle.channel.clone()),
            puzzle.clone(),
        )
    })?;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: timer_id(&puzzle.server, &puzzle.channel),
            server: puzzle.server.clone(),
            channel: puzzle.channel.clone(),
            owner_profile_id: None,
            due_at,
            payload: puzzle.seq.to_string(),
        })?)?
    };
    Ok(())
}

fn clear(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&puzzle_key(server, channel), "")?;
    with_puzzles(|puzzles| puzzles.remove(&(server.to_string(), channel.to_string())))?;
    unsafe {
        schedule_cancel(serde_json::to_string(&ScheduleCancel {
            id: timer_id(server, channel),
        })?)?
    };
    Ok(())
}

/// Sets a new word in the channel and posts it.
fn start(server: &str, channel: &str, now: i64) -> Result<(), Error> {
    let mut entropy = Entropy::default();
    let history_key = history_key(server, channel);
    let mut history: History = load(&history_key)?;
    let length = match entropy.below(20)? {
        0..=7 => 5,
        8..=14 => 6,
        _ => 7,
    };
    let pool = answers(length);
    let mut word = pool[entropy.below(pool.len() as u32)? as usize];
    for _ in 0..30 {
        if !history.recent.iter().any(|seen| seen == word) {
            break;
        }
        word = pool[entropy.below(pool.len() as u32)? as usize];
    }
    let mut draw = |n: u32| entropy.below(n).unwrap_or(0);
    let Some(letters) = scramble(word, &mut draw) else {
        return Err(Error::msg(format!("could not scramble {word}")));
    };
    history.last_at = now;
    history.recent.push(word.to_string());
    if history.recent.len() > RECENT_KEPT {
        let excess = history.recent.len() - RECENT_KEPT;
        history.recent.drain(..excess);
    }
    kv_save(&history_key, &serde_json::to_string(&history)?)?;
    let mut puzzle = Puzzle {
        server: server.into(),
        channel: channel.into(),
        id: format!(
            "{:08x}{:08x}",
            entropy.below(u32::MAX)?,
            entropy.below(u32::MAX)?
        ),
        word: word.into(),
        letters: letters.clone(),
        asked_at: now,
        hinted: false,
        seq: 0,
    };
    reply(
        server,
        channel,
        &themed(
            "scramble.word",
            &["🔤 Unscramble: {letters}"],
            &[("letters", &spaced(&letters))],
        )?,
    )?;
    let hint_at = setting_i64("hint_seconds", server, Some(channel), 20).clamp(5, 120);
    book(&mut puzzle, now + hint_at)
}

/// The scheduler's knock: the hint, then the reveal.
fn step(server: &str, channel: &str, seq: u64, now: i64) -> Result<(), Error> {
    let Some(mut puzzle) = puzzle_in(server, channel)? else {
        return Ok(());
    };
    if puzzle.seq != seq {
        return Ok(());
    }
    if !puzzle.hinted {
        puzzle.hinted = true;
        reply(
            server,
            channel,
            &themed(
                "scramble.hint",
                &["💡 {letters} → {hint}"],
                &[
                    ("letters", &spaced(&puzzle.letters)),
                    ("hint", &hint(&puzzle.word)),
                ],
            )?,
        )?;
        let reveal = setting_i64("reveal_seconds", server, Some(channel), 40).clamp(10, 300);
        let due_at = (puzzle.asked_at + reveal).max(now + 1);
        return book(&mut puzzle, due_at);
    }
    clear(server, channel)?;
    reply(
        server,
        channel,
        &themed(
            "scramble.reveal",
            &["⏰ Nobody got it: {word}."],
            &[("word", &puzzle.word.to_ascii_uppercase())],
        )?,
    )
}

fn award(server: &str, msg: &MessagePayload, stat: &str, event_id: &str) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: Some(event_id.into()),
        })?)?
    };
    Ok(())
}

fn solved(puzzle: &Puzzle, msg: &MessagePayload, guess: &str, now: i64) -> Result<(), Error> {
    let (server, channel) = (puzzle.server.as_str(), puzzle.channel.as_str());
    clear(server, channel)?;
    let seconds = (now - puzzle.asked_at).max(0);
    let name = display(msg).to_string();
    let key = format!("{}{}", career_prefix(server, channel), msg.user_id);
    let mut career: Career = load(&key)?;
    // The channel's record, before this solve joins the careers.
    let record = careers_in(server, channel)?
        .iter()
        .filter_map(|(_, career)| career.best_seconds)
        .min();
    let personal_best = career.add(&name, seconds, now);
    kv_save(&key, &serde_json::to_string(&career)?)?;
    let brass = setting_i64("brass_per_word", server, Some(channel), 3).clamp(0, 50);
    if brass > 0 {
        unsafe {
            economy_award(serde_json::to_string(&EconomyTransactionRequest {
                server: server.into(),
                profile_id: msg.user_id.clone(),
                amount: brass as u64,
                event_id: format!("scramble:{}", puzzle.id),
                reason: "scramble".into(),
            })?)?
        };
    }
    let guess = guess
        .trim()
        .trim_end_matches(['!', '.', '?'])
        .to_ascii_uppercase();
    let word = puzzle.word.to_ascii_uppercase();
    let also = if guess == word {
        String::new()
    } else {
        themed("scramble.also", &[" (I had {word})"], &[("word", &word)])?
    };
    let note = if record.is_none_or(|record| seconds < record) {
        themed("scramble.record", &[" · a new channel record!"], &[])?
    } else if personal_best && career.solved > 1 {
        themed("scramble.personal_best", &[" · a personal best"], &[])?
    } else {
        String::new()
    };
    let brass_text = if brass > 0 {
        themed(
            "scramble.brass",
            &[" +{brass} brass"],
            &[("brass", &brass.to_string())],
        )?
    } else {
        String::new()
    };
    reply(
        server,
        channel,
        &themed(
            "scramble.solved",
            &["✅ {user}: {guess}{also} in {seconds}s{brass}{note}"],
            &[
                ("user", &name),
                ("guess", &guess),
                ("also", &also),
                ("seconds", &seconds.to_string()),
                ("brass", &brass_text),
                ("note", &note),
            ],
        )?,
    )?;
    let event = format!("scramble:{}", puzzle.id);
    award(server, msg, "solved", &event)?;
    if seconds <= QUICK_SECONDS {
        award(server, msg, "quick_solves", &format!("{event}:quick"))?;
    }
    Ok(())
}

// ── pop-ups ─────────────────────────────────────────────────────────────────

/// Notes a line of chat, and sets a word if pop-ups are on, it's been long enough, and the
/// channel is lively.
fn maybe_pop_up(server: &str, msg: &MessagePayload, now: i64) -> Result<(), Error> {
    let key = (server.to_string(), msg.target.clone());
    let recent = ACTIVITY.with(|activity| {
        let mut activity = activity.borrow_mut();
        let lines = activity.entry(key).or_default();
        lines.push_back((now, msg.user_id.clone()));
        while lines.len() > ACTIVITY_KEPT {
            lines.pop_front();
        }
        lines.iter().cloned().collect::<Vec<_>>()
    });
    if !lively(&recent, now, LIVELY_WINDOW_SECONDS, LIVELY_LINES)
        || setting("popups", server, Some(&msg.target))? != "true"
    {
        return Ok(());
    }
    let history: History = load(&history_key(server, &msg.target))?;
    let minutes = setting_i64("popup_minutes", server, Some(&msg.target), 45).clamp(10, 480);
    if now - history.last_at < minutes * 60 {
        return Ok(());
    }
    start(server, &msg.target, now)
}

// ── commands ────────────────────────────────────────────────────────────────

fn say(
    msg: &MessagePayload,
    key: &str,
    default: &str,
    vars: &[(&str, &str)],
) -> Result<String, Error> {
    let mut all = vec![("user", display(msg)), ("honorific", honorific(msg))];
    all.extend_from_slice(vars);
    themed(key, &[default], &all)
}

fn careers_in(server: &str, channel: &str) -> Result<Vec<(String, Career)>, Error> {
    let prefix = career_prefix(server, channel);
    let mut careers = Vec::new();
    for entry in kv_list_prefix(&prefix)? {
        if entry.value.trim().is_empty() {
            continue;
        }
        careers.push((
            entry.key[prefix.len()..].to_string(),
            serde_json::from_str::<Career>(&entry.value)?,
        ));
    }
    Ok(careers)
}

fn cmd_fastest(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    let mut fastest = careers_in(server, &msg.target)?
        .into_iter()
        .filter_map(|(_, career)| career.best_seconds.map(|best| (best, career.name)))
        .collect::<Vec<_>>();
    fastest.sort();
    if fastest.is_empty() {
        return say(
            msg,
            "scramble.fastest_empty",
            "Nobody has unscrambled a word in {channel} yet, {honorific}.",
            &[("channel", &msg.target)],
        );
    }
    let board = fastest
        .iter()
        .take(BOARD_SIZE)
        .enumerate()
        .map(|(index, (seconds, name))| format!("{}. {} {seconds}s", index + 1, no_highlight(name)))
        .collect::<Vec<_>>()
        .join(" · ");
    say(
        msg,
        "scramble.fastest",
        "⚡ Quickest in {channel}: {board}",
        &[("channel", &msg.target), ("board", &board)],
    )
}

fn cmd_top(server: &str, msg: &MessagePayload, argument: &str, now: i64) -> Result<String, Error> {
    let all_time = matches!(
        argument.trim().to_ascii_lowercase().as_str(),
        "all" | "ever"
    );
    let mut ranked = careers_in(server, &msg.target)?
        .into_iter()
        .map(|(_, career)| {
            let count = if all_time {
                career.solved
            } else {
                career.solved_this_week(now)
            };
            (career.name, count)
        })
        .filter(|(_, count)| *count > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let period = if all_time {
        themed("scramble.period_all", &["all time"], &[])?
    } else {
        themed("scramble.period_week", &["this week"], &[])?
    };
    if ranked.is_empty() {
        return say(
            msg,
            "scramble.top_empty",
            "No words unscrambled in {channel} {period} yet, {honorific}.",
            &[("channel", &msg.target), ("period", &period)],
        );
    }
    let board = ranked
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, (name, count))| format!("{}. {} {count}", index + 1, no_highlight(name)))
        .collect::<Vec<_>>()
        .join(" · ");
    say(
        msg,
        "scramble.top",
        "🔤 Scrambles in {channel}, {period}: {board}",
        &[
            ("channel", &msg.target),
            ("period", &period),
            ("board", &board),
        ],
    )
}

fn cmd_me(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    let key = format!("{}{}", career_prefix(server, &msg.target), msg.user_id);
    let career: Career = load(&key)?;
    let Some(best) = career.best_seconds else {
        return say(
            msg,
            "scramble.me_empty",
            "You've not unscrambled a word in {channel} yet, {honorific}.",
            &[("channel", &msg.target)],
        );
    };
    say(
        msg,
        "scramble.me",
        "{user} in {channel}: solved {solved} · quickest {best}s",
        &[
            ("channel", &msg.target),
            ("solved", &career.solved.to_string()),
            ("best", &best.to_string()),
        ],
    )
}

fn handle_command(
    server: &str,
    msg: &MessagePayload,
    argument: &str,
    now: i64,
) -> Result<(), Error> {
    let destination = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let text = if msg.is_private {
        Some(say(
            msg,
            "scramble.channel_only",
            "Scrambles are played in channels, {honorific}.",
            &[],
        )?)
    } else if msg.user_id.is_empty() {
        Some(say(
            msg,
            "scramble.profile_missing",
            "I cannot place your profile just now, {honorific}; do try again shortly.",
            &[],
        )?)
    } else if setting("enabled", server, Some(&msg.target))? != "true" {
        Some(say(
            msg,
            "scramble.disabled",
            "Scrambles are switched off in {channel}, {honorific}.",
            &[("channel", &msg.target)],
        )?)
    } else {
        let (sub, rest) = argument
            .split_once(char::is_whitespace)
            .map(|(sub, rest)| (sub.to_ascii_lowercase(), rest.trim()))
            .unwrap_or((argument.to_ascii_lowercase(), ""));
        match sub.as_str() {
            "fastest" | "records" => Some(cmd_fastest(server, msg)?),
            "top" => Some(cmd_top(server, msg, rest, now)?),
            "me" => Some(cmd_me(server, msg)?),
            _ => match puzzle_in(server, &msg.target)? {
                // Asking again repeats the word in play.
                Some(puzzle) => Some(themed(
                    "scramble.word",
                    &["🔤 Unscramble: {letters}"],
                    &[("letters", &spaced(&puzzle.letters))],
                )?),
                None => {
                    start(server, &msg.target, now)?;
                    None
                }
            },
        }
    };
    if let Some(text) = text {
        reply(server, destination, &text)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let server = env.server.as_str();
    let text = msg.text.trim();
    let (command, argument) = text
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((text, ""));
    if command.eq_ignore_ascii_case("!scramble") {
        handle_command(server, &msg, argument, timestamp()?)?;
        return Ok(());
    }
    if msg.is_private || msg.user_id.is_empty() || text.starts_with('!') {
        return Ok(());
    }
    let now = timestamp()?;
    match puzzle_in(server, &msg.target)? {
        Some(puzzle) if solves(&puzzle.word, text) => solved(&puzzle, &msg, text, now)?,
        Some(_) => {}
        None => maybe_pop_up(server, &msg, now)?,
    }
    Ok(())
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Timer {
        channel, payload, ..
    } = &env.event
    else {
        return Ok(());
    };
    let Ok(seq) = payload.parse::<u64>() else {
        return Ok(());
    };
    step(&env.server, channel, seq, timestamp()?)?;
    Ok(())
}

// ── lifecycle ───────────────────────────────────────────────────────────────

fn subject_careers(request: &ModuleDataRequest) -> impl Iterator<Item = &ModuleKvEntry> {
    let prefix = format!("career:{}:", encode(&request.subject.server));
    let suffix = format!(":{}", request.subject.profile_id);
    request.entries.iter().filter(move |entry| {
        entry.key.starts_with(&prefix)
            && entry.key.ends_with(&suffix)
            && !entry.value.trim().is_empty()
    })
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let careers = subject_careers(&request)
        .map(|entry| serde_json::from_str::<Career>(&entry.value))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if careers.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "careers": careers })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations: subject_careers(&request)
            .map(|entry| ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            })
            .collect(),
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("career:{}:", encode(&request.server));
    let mut totals: BTreeMap<String, u64> = BTreeMap::new();
    for entry in &request.entries {
        let Some((_, profile_id)) = entry
            .key
            .strip_prefix(&prefix)
            .and_then(|rest| rest.split_once(':'))
        else {
            continue;
        };
        if entry.value.trim().is_empty() {
            continue;
        }
        let career: Career = serde_json::from_str(&entry.value)?;
        *totals.entry(profile_id.to_string()).or_default() += career.solved;
    }
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values: totals
            .into_iter()
            .map(|(profile_id, solved)| AchievementSetMax {
                profile_id,
                stat: "solved".into(),
                value: solved,
            })
            .collect(),
    })?)
}
