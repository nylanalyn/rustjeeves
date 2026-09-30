//! Trivia rounds, answered by just typing.
//!
//! `!trivia [n]` starts a round of questions in the channel (`round_length`, default ten). Each
//! question gets a hint halfway (`hint_seconds`) and is revealed at `question_seconds`; the first
//! right answer scores, more before the hint than after, plus a bonus for a streak of answers in
//! a row. Correct answers pay `brass_per_answer` and the round's winner `round_bonus`. Rounds end
//! on their own after three unanswered questions. `!trivia top [week|all]`, `!trivia me`, and
//! `!trivia stop` (the starter or an admin) round it out. Allowed anywhere; `enabled` switches a
//! channel off.
//!
//! Questions come from a bundled pack of original questions, mixed with Open Trivia DB questions
//! (CC BY-SA 4.0, credited when asked) through the host's `trivia_fetch` when the `opentdb`
//! setting is on and the service answers. Rounds live in KV and the scheduler drives them, so a
//! reload mid-round carries on.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, EconomyTransactionRequest, Event, EventEnvelope, FetchedQuestion, MessagePayload,
    ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvEntry, ModuleKvMutation,
    Role, ScheduleCancel, ScheduleSet, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, TriviaFetchRequest, TriviaFetchResponse, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{
    display, encode, honorific, kv_list_prefix, kv_load, kv_save, no_highlight, reply, setting,
    setting_i64, themed, timestamp, Entropy,
};
use model::{
    fetched_question, judge, letters_hint, points, ruled_out, standings, winners, Career, Entry,
    Guess, Kind, PackEntry, Question, HOT_STREAK, IDLE_LIMIT,
};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

mod model;

const PACK_JSON: &str = include_str!("../questions.json");
/// Seconds between one question's end and the next.
const GAP_SECONDS: i64 = 4;
/// Question ids remembered per channel so they don't come round again soon.
const RECENT_KEPT: usize = 250;
/// Fetched questions kept waiting, and how many to ask for at once.
const POOL_LOW: usize = 5;
const FETCH_AMOUNT: u32 = 20;
const FETCH_EVERY_SECONDS: i64 = 10;

#[host_fn]
extern "ExtismHost" {
    fn award_stats(input: String) -> String;
    fn economy_award(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn trivia_fetch(input: String) -> String;
}

fn pack() -> &'static [PackEntry] {
    static PACK: OnceLock<Vec<PackEntry>> = OnceLock::new();
    PACK.get_or_init(|| serde_json::from_str(PACK_JSON).unwrap_or_default())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "trivia".into(),
            aliases: vec!["quiz".into()],
            description: "Trivia rounds: just type your answers. Some questions come from Open Trivia DB (opentdb.com, CC BY-SA 4.0).".into(),
            usage: "!trivia [questions] | top [week|all] | me | stop".into(),
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
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Allow trivia rounds here.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: all.clone(),
                applies_immediately: true,
            },
            integer("round_length", "Questions in a round.", 10, 3, 25),
            integer(
                "question_seconds",
                "Seconds before a question is revealed.",
                30,
                15,
                120,
            ),
            integer("hint_seconds", "Seconds before the hint.", 15, 5, 60),
            integer(
                "brass_per_answer",
                "Brass for each correct answer.",
                3,
                0,
                50,
            ),
            integer("round_bonus", "Brass for winning a round.", 15, 0, 200),
            SettingSpec {
                key: "opentdb".into(),
                description: "Mix in questions from Open Trivia DB when it answers.".into(),
                default: "true".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let stats = [
        ("correct", "Trivia questions answered first"),
        ("rounds_won", "Trivia rounds won"),
        ("hot_streaks", "Five correct answers in a row"),
        ("perfect_rounds", "Rounds where you answered every question"),
    ]
    .into_iter()
    .map(|(id, description)| AchievementStat {
        id: id.into(),
        description: description.into(),
    })
    .collect();
    let achievements = [
        (
            "quick_study",
            "Quick Study",
            "Answer a trivia question first.",
            "correct",
            1,
            false,
            false,
        ),
        (
            "well_read",
            "Well Read",
            "Answer 100 trivia questions first.",
            "correct",
            100,
            false,
            false,
        ),
        (
            "encyclopaedia",
            "Walking Encyclopaedia",
            "Answer 1,000 trivia questions first.",
            "correct",
            1_000,
            false,
            false,
        ),
        (
            "quizmaster",
            "Quizmaster",
            "Win a trivia round.",
            "rounds_won",
            1,
            false,
            false,
        ),
        (
            "grand_quizmaster",
            "Grand Quizmaster",
            "Win 25 trivia rounds.",
            "rounds_won",
            25,
            false,
            false,
        ),
        (
            "on_a_roll",
            "On a Roll",
            "Answer five trivia questions in a row.",
            "hot_streaks",
            1,
            true,
            false,
        ),
        (
            "clean_sweep",
            "Clean Sweep",
            "Answer every question in a round of five or more.",
            "perfect_rounds",
            1,
            true,
            true,
        ),
    ]
    .into_iter()
    .map(
        |(id, name, description, stat, threshold, optional, secret)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: description.into(),
            stat: stat.into(),
            threshold,
            optional,
            secret,
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

// ── rounds ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Asked {
    question: Question,
    asked_at: i64,
    hinted: bool,
    /// Who guessed wrong on a choice question, and can't guess again.
    #[serde(default)]
    out: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Round {
    server: String,
    channel: String,
    /// Identifies the round in brass and achievement events.
    id: String,
    starter: String,
    total: u32,
    number: u32,
    #[serde(default)]
    asked: Option<Asked>,
    #[serde(default)]
    scores: BTreeMap<String, Entry>,
    /// (profile, answers in a row).
    #[serde(default)]
    streak: Option<(String, u32)>,
    /// Questions in a row nobody answered.
    #[serde(default)]
    idle: u32,
    /// Bumped whenever a timer is booked, so an older timer is recognised and ignored.
    #[serde(default)]
    seq: u64,
}

type ChannelKey = (String, String);

thread_local! {
    /// Active rounds, mirrored from KV; loaded on first use.
    static ROUNDS: RefCell<Option<HashMap<ChannelKey, Round>>> = const { RefCell::new(None) };
    /// Fetched questions waiting to be asked, and when the host was last asked for more.
    static POOL: RefCell<(Vec<FetchedQuestion>, i64)> = const { RefCell::new((Vec::new(), 0)) };
}

fn round_key(server: &str, channel: &str) -> String {
    format!("round:{}:{}", encode(server), encode(channel))
}

fn timer_id(server: &str, channel: &str) -> String {
    format!("q:{}:{}", encode(server), encode(channel))
}

fn career_prefix(server: &str, channel: &str) -> String {
    format!("career:{}:{}:", encode(server), encode(channel))
}

fn recent_key(server: &str, channel: &str) -> String {
    format!("recent:{}:{}", encode(server), encode(channel))
}

fn with_rounds<T>(f: impl FnOnce(&mut HashMap<ChannelKey, Round>) -> T) -> Result<T, Error> {
    let loaded = ROUNDS.with(|rounds| rounds.borrow().is_some());
    if !loaded {
        let mut rounds = HashMap::new();
        for entry in kv_list_prefix("round:")? {
            if entry.value.trim().is_empty() {
                continue;
            }
            let round: Round = serde_json::from_str(&entry.value)?;
            rounds.insert((round.server.clone(), round.channel.clone()), round);
        }
        ROUNDS.with(|cell| *cell.borrow_mut() = Some(rounds));
    }
    Ok(ROUNDS.with(|rounds| f(rounds.borrow_mut().as_mut().expect("loaded above"))))
}

fn round_in(server: &str, channel: &str) -> Result<Option<Round>, Error> {
    with_rounds(|rounds| {
        rounds
            .get(&(server.to_string(), channel.to_string()))
            .cloned()
    })
}

fn save_round(round: &Round) -> Result<(), Error> {
    kv_save(
        &round_key(&round.server, &round.channel),
        &serde_json::to_string(round)?,
    )?;
    with_rounds(|rounds| {
        rounds.insert((round.server.clone(), round.channel.clone()), round.clone())
    })?;
    Ok(())
}

fn drop_round(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&round_key(server, channel), "")?;
    with_rounds(|rounds| rounds.remove(&(server.to_string(), channel.to_string())))?;
    unsafe {
        schedule_cancel(serde_json::to_string(&ScheduleCancel {
            id: timer_id(server, channel),
        })?)?
    };
    Ok(())
}

/// Books the round's next step and saves it.
fn book(round: &mut Round, due_at: i64) -> Result<(), Error> {
    round.seq += 1;
    save_round(round)?;
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: timer_id(&round.server, &round.channel),
            server: round.server.clone(),
            channel: round.channel.clone(),
            owner_profile_id: None,
            due_at,
            payload: round.seq.to_string(),
        })?)?
    };
    Ok(())
}

fn setting_seconds(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    setting_i64(key, server, Some(channel), fallback)
}

// ── choosing questions ──────────────────────────────────────────────────────

fn top_up_pool(server: &str, channel: &str, now: i64) -> Result<(), Error> {
    if setting("opentdb", server, Some(channel))? != "true" {
        return Ok(());
    }
    let (low, last) = POOL.with(|pool| {
        let pool = pool.borrow();
        (pool.0.len() < POOL_LOW, pool.1)
    });
    if !low || now - last < FETCH_EVERY_SECONDS {
        return Ok(());
    }
    POOL.with(|pool| pool.borrow_mut().1 = now);
    let raw = unsafe {
        trivia_fetch(serde_json::to_string(&TriviaFetchRequest {
            amount: FETCH_AMOUNT,
        })?)?
    };
    let response: TriviaFetchResponse = serde_json::from_str(&raw)?;
    POOL.with(|pool| pool.borrow_mut().0.extend(response.questions));
    Ok(())
}

fn next_question(
    server: &str,
    channel: &str,
    now: i64,
    entropy: &mut Entropy,
) -> Result<Question, Error> {
    top_up_pool(server, channel, now)?;
    let recent_key = recent_key(server, channel);
    let raw = kv_load(&recent_key)?;
    let mut recent: Vec<String> = if raw.trim().is_empty() {
        Vec::new()
    } else {
        serde_json::from_str(&raw)?
    };
    let mut question = None;
    // Half the time a fetched question, when there are any.
    if entropy.below(2)? == 0 {
        while let Some(fetched) = POOL.with(|pool| pool.borrow_mut().0.pop()) {
            let position = entropy.below(fetched.incorrect.len() as u32 + 1)? as usize;
            let candidate = fetched_question(
                &fetched.category,
                &fetched.question,
                &fetched.kind,
                &fetched.correct,
                &fetched.incorrect,
                position,
            );
            if let Some(candidate) = candidate.filter(|q| !recent.contains(&q.id)) {
                question = Some(candidate);
                break;
            }
        }
    }
    let question = match question {
        Some(question) => question,
        None => {
            let pack = pack();
            if pack.is_empty() {
                return Err(Error::msg("the trivia pack failed to load"));
            }
            let mut pick = &pack[entropy.below(pack.len() as u32)? as usize];
            for _ in 0..30 {
                if !recent.contains(&pick.id) {
                    break;
                }
                pick = &pack[entropy.below(pack.len() as u32)? as usize];
            }
            pick.question()
        }
    };
    recent.retain(|id| id != &question.id);
    recent.push(question.id.clone());
    if recent.len() > RECENT_KEPT {
        let excess = recent.len() - RECENT_KEPT;
        recent.drain(..excess);
    }
    kv_save(&recent_key, &serde_json::to_string(&recent)?)?;
    Ok(question)
}

// ── the game ────────────────────────────────────────────────────────────────

fn ask(round: &mut Round, now: i64) -> Result<(), Error> {
    let mut entropy = Entropy::default();
    let question = next_question(&round.server, &round.channel, now, &mut entropy)?;
    round.number += 1;
    let number = round.number.to_string();
    let total = round.total.to_string();
    let credit = if question.fetched {
        themed("trivia.credit", &[" (opentdb.com)"], &[])?
    } else {
        String::new()
    };
    let vars = [
        ("number", number.as_str()),
        ("total", total.as_str()),
        ("category", question.category.as_str()),
        ("question", question.text.as_str()),
        ("credit", credit.as_str()),
    ];
    let text = match question.kind {
        Kind::Free => themed(
            "trivia.question",
            &["🧠 Q{number}/{total} · {category} · {question}{credit}"],
            &vars,
        )?,
        Kind::TrueFalse => themed(
            "trivia.question_truefalse",
            &["🧠 Q{number}/{total} · {category} · True or false: {question}{credit}"],
            &vars,
        )?,
        Kind::Choice => {
            let options = question
                .options
                .iter()
                .enumerate()
                .map(|(index, option)| format!("{}) {option}", model::letter(index)))
                .collect::<Vec<_>>()
                .join(" · ");
            let mut vars = vars.to_vec();
            vars.push(("options", &options));
            themed(
                "trivia.question_choice",
                &["🧠 Q{number}/{total} · {category} · {question} — {options}{credit}"],
                &vars,
            )?
        }
    };
    reply(&round.server, &round.channel, &text)?;
    round.asked = Some(Asked {
        question,
        asked_at: now,
        hinted: false,
        out: Vec::new(),
    });
    let hint = setting_seconds("hint_seconds", &round.server, &round.channel, 15);
    book(round, now + hint)
}

fn hint(round: &mut Round, now: i64) -> Result<(), Error> {
    let Some(asked) = round.asked.as_mut() else {
        return Ok(());
    };
    asked.hinted = true;
    let question = asked.question.clone();
    let asked_at = asked.asked_at;
    let text = match question.kind {
        Kind::Free => Some(themed(
            "trivia.hint",
            &["💡 Hint: {hint}"],
            &[("hint", &letters_hint(&question.answer))],
        )?),
        Kind::Choice => {
            let pick = Entropy::default().below(4)? as usize;
            let out = ruled_out(&question, pick)
                .iter()
                .map(char::to_string)
                .collect::<Vec<_>>()
                .join(" or ");
            Some(themed(
                "trivia.hint_choice",
                &["💡 Hint: it isn't {options}."],
                &[("options", &out)],
            )?)
        }
        // Nothing to give away on a coin toss.
        Kind::TrueFalse => None,
    };
    if let Some(text) = text {
        reply(&round.server, &round.channel, &text)?;
    }
    let seconds = setting_seconds("question_seconds", &round.server, &round.channel, 30);
    book(round, (asked_at + seconds).max(now + 1))
}

fn reveal(round: &mut Round, now: i64) -> Result<(), Error> {
    let Some(asked) = round.asked.take() else {
        return Ok(());
    };
    reply(
        &round.server,
        &round.channel,
        &themed(
            "trivia.reveal",
            &["⏰ Nobody got it: {answer}."],
            &[("answer", &asked.question.answer)],
        )?,
    )?;
    round.idle += 1;
    round.streak = None;
    if round.idle >= IDLE_LIMIT {
        return finish(round, Ending::Idle);
    }
    book(round, now + GAP_SECONDS)
}

enum Ending {
    Complete,
    Idle,
    Stopped,
}

fn finish(round: &mut Round, ending: Ending) -> Result<(), Error> {
    let (server, channel) = (round.server.clone(), round.channel.clone());
    drop_round(&server, &channel)?;
    let board = standings(&round.scores)
        .iter()
        .take(5)
        .map(|(_, entry)| format!("{} {}", no_highlight(&entry.name), entry.points))
        .collect::<Vec<_>>()
        .join(", ");
    let (key, default) = match ending {
        Ending::Complete => ("trivia.round_over", "🏁 Round over! {board}"),
        Ending::Idle => (
            "trivia.round_idle",
            "🏁 Nobody's playing, so I'll stop there. {board}",
        ),
        Ending::Stopped => ("trivia.round_stopped", "🏁 Round stopped. {board}"),
    };
    let board = if board.is_empty() {
        themed("trivia.nobody_scored", &["Nobody scored."], &[])?
    } else {
        board
    };
    reply(
        &server,
        &channel,
        &themed(key, &[default], &[("board", &board)])?,
    )?;
    let won = winners(&round.scores)
        .into_iter()
        .map(|(id, entry)| (id.clone(), entry.clone()))
        .collect::<Vec<_>>();
    if won.is_empty() {
        return Ok(());
    }
    let bonus = setting_i64("round_bonus", &server, Some(&channel), 15).clamp(0, 200) as u64;
    for (profile_id, entry) in &won {
        let key = format!("{}{profile_id}", career_prefix(&server, &channel));
        let mut career = load_career(&key)?;
        career.rounds_won += 1;
        kv_save(&key, &serde_json::to_string(&career)?)?;
        if bonus > 0 {
            economy(
                &server,
                profile_id,
                bonus,
                &format!("trivia:{}:win:{profile_id}", round.id),
                "trivia_round",
            )?;
        }
        award(
            &server,
            profile_id,
            &entry.name,
            &channel,
            "rounds_won",
            &format!("trivia:{}:won", round.id),
        )?;
    }
    let names = won
        .iter()
        .map(|(_, entry)| entry.name.as_str())
        .collect::<Vec<_>>()
        .join(" and ");
    let bonus_text = bonus.to_string();
    reply(
        &server,
        &channel,
        &themed(
            if won.len() == 1 {
                "trivia.winner"
            } else {
                "trivia.winners"
            },
            &[if won.len() == 1 {
                "🏆 {names} takes the round (+{bonus} brass)."
            } else {
                "🏆 {names} share the round (+{bonus} brass each)."
            }],
            &[("names", &names), ("bonus", &bonus_text)],
        )?,
    )?;
    // Everything asked, all answered by one person, in a full round.
    if matches!(ending, Ending::Complete) && round.total >= 5 {
        for (profile_id, entry) in &round.scores {
            if entry.correct >= round.total {
                award(
                    &server,
                    profile_id,
                    &entry.name,
                    &channel,
                    "perfect_rounds",
                    &format!("trivia:{}:perfect", round.id),
                )?;
            }
        }
    }
    Ok(())
}

fn load_career(key: &str) -> Result<Career, Error> {
    let raw = kv_load(key)?;
    if raw.trim().is_empty() {
        Ok(Career::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

fn economy(
    server: &str,
    profile_id: &str,
    amount: u64,
    event_id: &str,
    reason: &str,
) -> Result<(), Error> {
    unsafe {
        economy_award(serde_json::to_string(&EconomyTransactionRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            amount,
            event_id: event_id.into(),
            reason: reason.into(),
        })?)?
    };
    Ok(())
}

fn award(
    server: &str,
    profile_id: &str,
    name: &str,
    target: &str,
    stat: &str,
    event_id: &str,
) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: Some(event_id.into()),
        })?)?
    };
    Ok(())
}

/// A line said while a question is open.
fn answer(round: &mut Round, msg: &MessagePayload, now: i64) -> Result<(), Error> {
    let Some(asked) = round.asked.as_mut() else {
        return Ok(());
    };
    if asked.out.contains(&msg.user_id) {
        return Ok(());
    }
    match judge(&asked.question, &msg.text) {
        Guess::Ignored => Ok(()),
        Guess::Wrong => {
            asked.out.push(msg.user_id.clone());
            save_round(round)
        }
        Guess::Correct => {
            let asked = round.asked.take().expect("checked above");
            let streak = match &round.streak {
                Some((id, count)) if id == &msg.user_id => count + 1,
                _ => 1,
            };
            round.streak = Some((msg.user_id.clone(), streak));
            round.idle = 0;
            let gained = points(asked.hinted, streak);
            let name = display(msg).to_string();
            let entry = round.scores.entry(msg.user_id.clone()).or_default();
            entry.name = name.clone();
            entry.points += gained;
            entry.correct += 1;
            let key = format!(
                "{}{}",
                career_prefix(&round.server, &round.channel),
                msg.user_id
            );
            let mut career = load_career(&key)?;
            career.add(&name, gained, streak, now);
            kv_save(&key, &serde_json::to_string(&career)?)?;
            let brass = setting_i64("brass_per_answer", &round.server, Some(&round.channel), 3)
                .clamp(0, 50);
            let event = format!("trivia:{}:{}", round.id, round.number);
            if brass > 0 {
                economy(
                    &round.server,
                    &msg.user_id,
                    brass as u64,
                    &event,
                    "trivia_answer",
                )?;
            }
            let seconds = format!("{:.0}", (now - asked.asked_at).max(0));
            let streak_text = if streak >= 3 {
                themed(
                    "trivia.streak",
                    &[" · {count} in a row 🔥"],
                    &[("count", &streak.to_string())],
                )?
            } else {
                String::new()
            };
            let brass_text = if brass > 0 {
                themed(
                    "trivia.brass",
                    &[" · +{brass} brass"],
                    &[("brass", &brass.to_string())],
                )?
            } else {
                String::new()
            };
            reply(
                &round.server,
                &round.channel,
                &themed(
                    "trivia.correct",
                    &["✅ {user}: {answer}! +{points} ({seconds}s){brass}{streak}"],
                    &[
                        ("user", &name),
                        ("answer", &asked.question.answer),
                        ("points", &gained.to_string()),
                        ("seconds", &seconds),
                        ("brass", &brass_text),
                        ("streak", &streak_text),
                    ],
                )?,
            )?;
            award(
                &round.server,
                &msg.user_id,
                &name,
                &round.channel,
                "correct",
                &event,
            )?;
            if streak == HOT_STREAK {
                award(
                    &round.server,
                    &msg.user_id,
                    &name,
                    &round.channel,
                    "hot_streaks",
                    &format!("{event}:streak"),
                )?;
            }
            book(round, now + GAP_SECONDS)
        }
    }
}

/// The scheduler's knock: hint, reveal, or the next question, whichever is due.
fn step(server: &str, channel: &str, seq: u64, now: i64) -> Result<(), Error> {
    let Some(mut round) = round_in(server, channel)? else {
        return Ok(());
    };
    if round.seq != seq {
        return Ok(());
    }
    match &round.asked {
        Some(asked) if !asked.hinted => hint(&mut round, now),
        Some(_) => reveal(&mut round, now),
        None if round.number >= round.total => finish(&mut round, Ending::Complete),
        None => ask(&mut round, now),
    }
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

fn cmd_start(
    server: &str,
    msg: &MessagePayload,
    argument: &str,
    now: i64,
) -> Result<Option<String>, Error> {
    let channel = msg.target.as_str();
    if round_in(server, channel)?.is_some() {
        return Ok(Some(say(
            msg,
            "trivia.already",
            "A round is already under way, {honorific}; answer away!",
            &[],
        )?));
    }
    let default = setting_i64("round_length", server, Some(channel), 10).clamp(3, 25);
    let total = match argument.trim() {
        "" => default,
        text => match text.parse::<i64>() {
            Ok(n) if (3..=25).contains(&n) => n,
            _ => {
                return Ok(Some(say(
                    msg,
                    "trivia.usage",
                    "Start a round with !trivia, or !trivia 5 for a short one (3 to 25 questions), {honorific}.",
                    &[],
                )?))
            }
        },
    };
    let mut entropy = Entropy::default();
    let mut round = Round {
        server: server.into(),
        channel: channel.into(),
        id: format!(
            "{:08x}{:08x}",
            entropy.below(u32::MAX)?,
            entropy.below(u32::MAX)?
        ),
        starter: msg.user_id.clone(),
        total: total as u32,
        number: 0,
        asked: None,
        scores: BTreeMap::new(),
        streak: None,
        idle: 0,
        seq: 0,
    };
    reply(
        server,
        channel,
        &say(
            msg,
            "trivia.starting",
            "🧠 {user} starts a trivia round: {total} questions. Just type your answers!",
            &[("total", &total.to_string())],
        )?,
    )?;
    ask(&mut round, now)?;
    Ok(None)
}

fn cmd_stop(server: &str, msg: &MessagePayload) -> Result<Option<String>, Error> {
    let Some(mut round) = round_in(server, &msg.target)? else {
        return Ok(Some(say(
            msg,
            "trivia.none",
            "There's no round on, {honorific}.",
            &[],
        )?));
    };
    let admin = msg.role.is_some_and(|role| role.satisfies(Role::Admin));
    if round.starter != msg.user_id && !admin {
        return Ok(Some(say(
            msg,
            "trivia.stop_denied",
            "Only whoever started the round, or an admin, can stop it, {honorific}.",
            &[],
        )?));
    }
    finish(&mut round, Ending::Stopped)?;
    Ok(None)
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

fn cmd_top(server: &str, msg: &MessagePayload, argument: &str, now: i64) -> Result<String, Error> {
    let all_time = matches!(
        argument.trim().to_ascii_lowercase().as_str(),
        "all" | "ever"
    );
    let mut ranked = careers_in(server, &msg.target)?
        .into_iter()
        .map(|(_, career)| {
            let score = if all_time {
                career.points
            } else {
                career.points_this_week(now)
            };
            (career.name, score)
        })
        .filter(|(_, score)| *score > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let period = if all_time {
        themed("trivia.period_all", &["all time"], &[])?
    } else {
        themed("trivia.period_week", &["this week"], &[])?
    };
    if ranked.is_empty() {
        return say(
            msg,
            "trivia.top_empty",
            "No trivia scores in {channel} {period} yet, {honorific}.",
            &[("channel", &msg.target), ("period", &period)],
        );
    }
    let board = ranked
        .iter()
        .take(10)
        .enumerate()
        .map(|(index, (name, score))| format!("{}. {} {score}", index + 1, no_highlight(name)))
        .collect::<Vec<_>>()
        .join(" · ");
    say(
        msg,
        "trivia.top",
        "🏆 Trivia in {channel}, {period}: {board}",
        &[
            ("channel", &msg.target),
            ("period", &period),
            ("board", &board),
        ],
    )
}

fn cmd_me(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    let key = format!("{}{}", career_prefix(server, &msg.target), msg.user_id);
    let career = load_career(&key)?;
    if career.correct == 0 {
        return say(
            msg,
            "trivia.me_empty",
            "You've no trivia answers in {channel} yet, {honorific}.",
            &[("channel", &msg.target)],
        );
    }
    say(
        msg,
        "trivia.me",
        "{user} in {channel}: {points} points · answered {correct} · rounds won {rounds} · best streak {streak}",
        &[
            ("channel", &msg.target),
            ("points", &career.points.to_string()),
            ("correct", &career.correct.to_string()),
            ("rounds", &career.rounds_won.to_string()),
            ("streak", &career.best_streak.to_string()),
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
            "trivia.channel_only",
            "Trivia is played in channels, {honorific}.",
            &[],
        )?)
    } else if msg.user_id.is_empty() {
        Some(say(
            msg,
            "trivia.profile_missing",
            "I cannot place your profile just now, {honorific}; do try again shortly.",
            &[],
        )?)
    } else if setting("enabled", server, Some(&msg.target))? != "true" {
        Some(say(
            msg,
            "trivia.disabled",
            "Trivia is switched off in {channel}, {honorific}.",
            &[("channel", &msg.target)],
        )?)
    } else {
        let (sub, rest) = argument
            .split_once(char::is_whitespace)
            .map(|(sub, rest)| (sub.to_ascii_lowercase(), rest.trim()))
            .unwrap_or((argument.to_ascii_lowercase(), ""));
        match sub.as_str() {
            "stop" | "end" => cmd_stop(server, msg)?,
            "top" | "scores" => Some(cmd_top(server, msg, rest, now)?),
            "me" => Some(cmd_me(server, msg)?),
            _ => cmd_start(server, msg, argument, now)?,
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
    if command.eq_ignore_ascii_case("!trivia") {
        handle_command(server, &msg, argument, timestamp()?)?;
        return Ok(());
    }
    if msg.is_private || msg.user_id.is_empty() || text.starts_with('!') {
        return Ok(());
    }
    let Some(mut round) = round_in(server, &msg.target)? else {
        return Ok(());
    };
    answer(&mut round, &msg, timestamp()?)?;
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
    let subject = &request.subject.profile_id;
    let mut mutations = subject_careers(&request)
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect::<Vec<_>>();
    // A round in progress on their network holds their name and score too.
    let rounds = format!("round:{}:", encode(&request.subject.server));
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&rounds) && !entry.value.trim().is_empty())
    {
        let mut round: Round = serde_json::from_str(&entry.value)?;
        let before = round.scores.len();
        round.scores.remove(subject);
        let streak = round.streak.as_ref().is_some_and(|(id, _)| id == subject);
        if streak {
            round.streak = None;
        }
        let out = round.asked.as_mut().is_some_and(|asked| {
            let had = asked.out.len();
            asked.out.retain(|id| id != subject);
            had != asked.out.len()
        });
        if before != round.scores.len() || streak || out {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: Some(serde_json::to_string(&round)?),
            });
        }
    }
    // The in-memory copies are reloaded from KV after the plan is applied.
    ROUNDS.with(|rounds| *rounds.borrow_mut() = None);
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("career:{}:", encode(&request.server));
    let mut totals: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for entry in &request.entries {
        let Some(rest) = entry.key.strip_prefix(&prefix) else {
            continue;
        };
        let Some((_, profile_id)) = rest.split_once(':') else {
            continue;
        };
        if entry.value.trim().is_empty() {
            continue;
        }
        let career: Career = serde_json::from_str(&entry.value)?;
        let total = totals.entry(profile_id.to_string()).or_default();
        total.0 += career.correct;
        total.1 += career.rounds_won;
    }
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values: totals
            .into_iter()
            .flat_map(|(profile_id, (correct, rounds))| {
                [("correct", correct), ("rounds_won", rounds)]
                    .into_iter()
                    .map(move |(stat, value)| AchievementSetMax {
                        profile_id: profile_id.clone(),
                        stat: stat.into(),
                        value,
                    })
            })
            .collect(),
    })?)
}
