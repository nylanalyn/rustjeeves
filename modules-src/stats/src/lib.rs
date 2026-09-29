//! Channel stats: who talks, when, and how.
//!
//! In channels an operator opts in (`enabled`, off by default), every line and `/me` is counted
//! per person: lines, words, questions, exclamations, shouting, links, actions, the hour it came
//! in, the day (in the channel's `timezone`, default America/New_York), and streaks. Nothing
//! anyone said is kept, only the numbers. `!stats private` stops counting someone and wipes them.
//!
//! Counting every line straight to the database would cost a write per line, so counts gather in
//! memory and are saved about once a minute (a scheduled flush catches quiet channels). A restart
//! loses at most that minute.
//!
//! `!stats` gives today in the channel, `!stats top [today|week|month|all]` (`!top`) the top ten,
//! `!stats me` / `!stats <nick>` a person, and `!stats hours` the channel's day as a sparkline.

use extism_pdk::*;
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandShortcut, CommandSpec, Event, EventEnvelope, LocalTimeQuery, LocalTimeResult,
    MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvEntry,
    ModuleKvMutation, Profile, ProfileKey, PublicChannelStats, Role, RunCommandRequest,
    RunCommandResponse, ScheduleSet, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{
    display, encode, honorific, kv_list_prefix, kv_load, kv_save, no_highlight, reply, setting,
    themed, timestamp,
};
use model::{
    awards, grouped, new_faces, next_digest_at, peak, short_date, sparkline, week_counts, week_of,
    weekday_name, zone_label, Award, AwardKind, Channel, LocalTime, Period, Person,
};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

mod model;

const DEFAULT_TIMEZONE: &str = "America/New_York";
const FLUSH_AFTER_SECONDS: i64 = 60;
const FLUSH_AFTER_LINES: usize = 50;
/// How long a timezone's UTC offset is trusted before asking the host again.
const OFFSET_TTL_SECONDS: i64 = 300;
const BOARD_SIZE: usize = 10;

#[host_fn]
extern "ExtismHost" {
    fn local_time(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn run_command(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "stats".into(),
            aliases: Vec::new(),
            description: "Who talks here, when, and how much: today, the top ten, the week's awards, a person, or the channel's day by hour. Counts only; !stats private opts you out.".into(),
            usage: "!stats [top [today|week|month|all] | awards [last] | me | <nick> | hours | private | public]"
                .into(),
            shortcuts: vec![CommandShortcut::new("top", "top").described(
                "The channel's top ten talkers: today, this week, this month, or ever.",
                "!top [today|week|month|all]",
            )],
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
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Count who talks in this channel (numbers only, never text).".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "digest".into(),
                description: "Post a weekly digest here on Monday mornings (needs stats enabled)."
                    .into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "public_page".into(),
                description: "Show this channel's stats on the public achievements website (people are named only if their achievements are public).".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "timezone".into(),
                description: "Timezone for this channel's days and hours (IANA name).".into(),
                default: DEFAULT_TIMEZONE.into(),
                kind: SettingKind::String { max_len: 64 },
                scopes: all,
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let stats = [
        ("lines", "Lines said in counted channels"),
        ("week_streaks", "Seven-day streaks of talking"),
        ("night_lines", "Lines between midnight and five"),
        (
            "morning_lines",
            "Lines between five and nine in the morning",
        ),
    ]
    .into_iter()
    .map(|(id, description)| AchievementStat {
        id: id.into(),
        description: description.into(),
    })
    .collect();
    // Night and morning depend on the channel's timezone and the person's habits: optional.
    let achievements = [
        (
            "chatterbox",
            "Chatterbox",
            "Say 1,000 lines.",
            "lines",
            1_000,
            false,
        ),
        (
            "pillar",
            "Pillar of the Community",
            "Say 10,000 lines.",
            "lines",
            10_000,
            false,
        ),
        (
            "regular",
            "A Regular",
            "Talk seven days running.",
            "week_streaks",
            1,
            false,
        ),
        (
            "night_owl",
            "Night Owl",
            "Say 100 lines between midnight and five.",
            "night_lines",
            100,
            true,
        ),
        (
            "early_bird",
            "Early Bird",
            "Say 100 lines between five and nine in the morning.",
            "morning_lines",
            100,
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

// ── storage ─────────────────────────────────────────────────────────────────

fn person_prefix(server: &str, channel: &str) -> String {
    format!("person:{}:{}:", encode(server), encode(channel))
}

fn person_key(server: &str, channel: &str, profile_id: &str) -> String {
    format!("{}{profile_id}", person_prefix(server, channel))
}

fn channel_key(server: &str, channel: &str) -> String {
    format!("channel:{}:{}", encode(server), encode(channel))
}

fn private_key(server: &str, profile_id: &str) -> String {
    format!("private:{}:{profile_id}", encode(server))
}

/// A stored record, or the default when absent (an empty value is a deleted one). A malformed
/// record fails loudly rather than being counted over.
fn load<T: serde::de::DeserializeOwned + Default>(key: &str) -> Result<T, Error> {
    let raw = kv_load(key)?;
    if raw.trim().is_empty() {
        Ok(T::default())
    } else {
        Ok(serde_json::from_str(&raw)?)
    }
}

/// Achievement progress gathered since the last flush, per (server, profile).
#[derive(Default)]
struct Progress {
    name: String,
    target: String,
    lines: u64,
    night: u64,
    morning: u64,
    /// Local days on which a seven-day streak was reached, for deduplication.
    streak_days: Vec<(String, i64)>,
}

/// Counts not yet saved. Records are loaded on first touch and written back at the flush.
#[derive(Default)]
struct Pending {
    people: HashMap<String, Person>,
    channels: HashMap<String, Channel>,
    progress: BTreeMap<(String, String), Progress>,
    /// (server, channel) pairs with new counts, whose public snapshot may need refreshing.
    touched: HashSet<(String, String)>,
    since: i64,
    lines: usize,
}

thread_local! {
    static PENDING: RefCell<Pending> = RefCell::new(Pending::default());
    /// (server, profile) pairs who opted out; loaded once from KV.
    static PRIVATE: RefCell<Option<HashSet<(String, String)>>> = const { RefCell::new(None) };
    /// Timezone → (UTC offset in seconds, when it was asked).
    static OFFSETS: RefCell<HashMap<String, (i64, i64)>> = RefCell::new(HashMap::new());
}

fn is_private(server: &str, profile_id: &str) -> Result<bool, Error> {
    let loaded = PRIVATE.with(|private| private.borrow().is_some());
    if !loaded {
        let mut set = HashSet::new();
        for entry in kv_list_prefix("private:")? {
            if entry.value.trim().is_empty() {
                continue;
            }
            let mut parts = entry.key.splitn(3, ':').skip(1);
            if let (Some(server), Some(profile)) = (parts.next(), parts.next()) {
                set.insert((decode(server), profile.to_string()));
            }
        }
        PRIVATE.with(|private| *private.borrow_mut() = Some(set));
    }
    Ok(PRIVATE.with(|private| {
        private
            .borrow()
            .as_ref()
            .is_some_and(|set| set.contains(&(server.to_string(), profile_id.to_string())))
    }))
}

fn decode(hex: &str) -> String {
    let bytes = (0..hex.len() / 2)
        .filter_map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok())
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The channel's timezone offset from UTC now, cached for a few minutes.
fn offset_for(zone: &str, now: i64) -> Result<i64, Error> {
    if let Some((offset, asked)) = OFFSETS.with(|offsets| offsets.borrow().get(zone).copied()) {
        if now - asked < OFFSET_TTL_SECONDS {
            return Ok(offset);
        }
    }
    let raw = unsafe {
        local_time(serde_json::to_string(&LocalTimeQuery {
            timezone: zone.into(),
            unix_seconds: Some(now),
            local: None,
        })?)?
    };
    // An unknown zone reads as UTC rather than failing every line.
    let offset = if raw.trim().is_empty() {
        0
    } else {
        let local: LocalTimeResult = serde_json::from_str(&raw)?;
        let wall = model::days_from_civil(
            i64::from(local.year),
            i64::from(local.month),
            i64::from(local.day),
        ) * model::DAY
            + i64::from(local.hour_24) * 3_600
            + i64::from(local.minute) * 60;
        wall - (now - now.rem_euclid(60))
    };
    OFFSETS.with(|offsets| offsets.borrow_mut().insert(zone.into(), (offset, now)));
    Ok(offset)
}

fn local_now(server: &str, channel: &str, now: i64) -> Result<(LocalTime, String), Error> {
    let zone = setting("timezone", server, Some(channel))?;
    let zone = if zone.trim().is_empty() {
        DEFAULT_TIMEZONE.to_string()
    } else {
        zone.trim().to_string()
    };
    Ok((LocalTime::at(now, offset_for(&zone, now)?), zone))
}

fn count_line(server: &str, msg: &MessagePayload, now: i64) -> Result<(), Error> {
    let channel = msg.target.as_str();
    let (at, _) = local_now(server, channel, now)?;
    let person_key = person_key(server, channel, &msg.user_id);
    let channel_key = channel_key(server, channel);
    let loaded_person = if PENDING.with(|p| p.borrow().people.contains_key(&person_key)) {
        None
    } else {
        Some(load::<Person>(&person_key)?)
    };
    let loaded_channel = if PENDING.with(|p| p.borrow().channels.contains_key(&channel_key)) {
        None
    } else {
        Some(load::<Channel>(&channel_key)?)
    };
    let flush_due = PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        let person = pending
            .people
            .entry(person_key.clone())
            .or_insert_with(|| loaded_person.unwrap_or_default());
        person.nick = msg.nick.clone();
        let outcome = person.record(display(msg), &msg.text, msg.is_action, at);
        pending
            .touched
            .insert((server.to_string(), channel.to_string()));
        pending
            .channels
            .entry(channel_key)
            .or_insert_with(|| loaded_channel.unwrap_or_default())
            .record(at);
        let progress = pending
            .progress
            .entry((server.to_string(), msg.user_id.clone()))
            .or_default();
        progress.name = display(msg).to_string();
        progress.target = channel.to_string();
        progress.lines += 1;
        progress.night += u64::from(outcome.night);
        progress.morning += u64::from(outcome.morning);
        if outcome.week_streak {
            progress.streak_days.push((encode(channel), at.day));
        }
        let first = pending.since == 0;
        if first {
            pending.since = now;
        }
        pending.lines += 1;
        (
            first,
            now - pending.since >= FLUSH_AFTER_SECONDS || pending.lines >= FLUSH_AFTER_LINES,
        )
    });
    ensure_digest(server, channel, now)?;
    let (first, due) = flush_due;
    if due {
        flush()?;
    } else if first {
        // Catches a channel that goes quiet before the next line would flush.
        unsafe {
            schedule_set(serde_json::to_string(&ScheduleSet {
                id: "flush".into(),
                server: server.into(),
                channel: channel.into(),
                owner_profile_id: None,
                due_at: now + FLUSH_AFTER_SECONDS + 5,
                payload: String::new(),
            })?)?
        };
    }
    Ok(())
}

/// Saves everything pending, then awards what it earned (awards only after the write), then
/// refreshes public snapshots.
fn flush() -> Result<(), Error> {
    let pending = PENDING.with(|pending| std::mem::take(&mut *pending.borrow_mut()));
    let touched = pending.touched.clone();
    for (key, person) in &pending.people {
        kv_save(key, &serde_json::to_string(person)?)?;
    }
    for (key, channel) in &pending.channels {
        kv_save(key, &serde_json::to_string(channel)?)?;
    }
    for ((server, profile_id), progress) in pending.progress {
        let mut increments = vec![
            ("lines", progress.lines),
            ("night_lines", progress.night),
            ("morning_lines", progress.morning),
        ];
        increments.retain(|(_, amount)| *amount > 0);
        let award = |increments: Vec<StatIncrement>, id: Option<String>| -> Result<(), Error> {
            unsafe {
                award_stats(serde_json::to_string(&AwardStatsRequest {
                    server: server.clone(),
                    profile_id: profile_id.clone(),
                    display_name: progress.name.clone(),
                    target: progress.target.clone(),
                    increments,
                    deduplication_id: id,
                })?)?
            };
            Ok(())
        };
        if !increments.is_empty() {
            award(
                increments
                    .into_iter()
                    .map(|(stat, amount)| StatIncrement {
                        stat: stat.into(),
                        amount,
                    })
                    .collect(),
                None,
            )?;
        }
        for (channel, day) in &progress.streak_days {
            award(
                vec![StatIncrement {
                    stat: "week_streaks".into(),
                    amount: 1,
                }],
                Some(format!("stats:streak:{channel}:{profile_id}:{day}")),
            )?;
        }
    }
    if !touched.is_empty() {
        let now = timestamp()?;
        for (server, channel) in touched {
            maybe_publish(&server, &channel, now)?;
        }
    }
    Ok(())
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

/// Everyone counted in a channel, opted-out people left out.
fn people_in(server: &str, channel: &str) -> Result<Vec<(String, Person)>, Error> {
    let prefix = person_prefix(server, channel);
    let mut people = Vec::new();
    for ModuleKvEntry { key, value } in kv_list_prefix(&prefix)? {
        if value.trim().is_empty() {
            continue;
        }
        let profile_id = key[prefix.len()..].to_string();
        if is_private(server, &profile_id)? {
            continue;
        }
        people.push((profile_id, serde_json::from_str::<Person>(&value)?));
    }
    Ok(people)
}

fn ranked(people: &[(String, Person)], period: Period, today: i64) -> Vec<(&str, &Person, u64)> {
    let mut ranked = people
        .iter()
        .map(|(id, person)| (id.as_str(), person, person.lines_in(period, today)))
        .filter(|(_, _, lines)| *lines > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.name.cmp(&b.1.name)));
    ranked
}

fn hour_label(hour: usize) -> String {
    format!("{hour:02}:00")
}

fn cmd_overview(server: &str, msg: &MessagePayload, today: i64) -> Result<String, Error> {
    let channel: Channel = load(&channel_key(server, &msg.target))?;
    let people = people_in(server, &msg.target)?;
    let lines_today = channel.lines_on(today);
    if channel.lines == 0 {
        return say(
            msg,
            "stats.overview_empty",
            "No figures for {channel} yet, {honorific}; I count from the first line after being switched on.",
            &[("channel", &msg.target)],
        );
    }
    let top = ranked(&people, Period::Today, today)
        .into_iter()
        .take(3)
        .map(|(_, person, lines)| format!("{} {}", no_highlight(&person.name), grouped(lines)))
        .collect::<Vec<_>>();
    let busiest = peak(&channel.hours()).map(hour_label).unwrap_or_default();
    let record = if channel.is_record_day(today) {
        themed("stats.record_today", &[" · a record day!"], &[])?
    } else {
        String::new()
    };
    say(
        msg,
        "stats.overview",
        "📊 {channel} today: {lines} lines from {people} people · busiest {hour} · top: {top} · counting since {since}{record}",
        &[
            ("channel", &msg.target),
            ("lines", &grouped(lines_today)),
            ("people", &ranked(&people, Period::Today, today).len().to_string()),
            ("hour", &busiest),
            ("top", &if top.is_empty() { "nobody yet".into() } else { top.join(", ") }),
            ("since", &short_date(channel.since_day)),
            ("record", &record),
        ],
    )
}

fn cmd_top(
    server: &str,
    msg: &MessagePayload,
    argument: &str,
    today: i64,
) -> Result<String, Error> {
    let Some(period) = Period::parse(argument) else {
        return say(
            msg,
            "stats.top_usage",
            "Which stretch, {honorific}? !top today, week, month, or all.",
            &[],
        );
    };
    let people = people_in(server, &msg.target)?;
    let board = ranked(&people, period, today)
        .into_iter()
        .take(BOARD_SIZE)
        .enumerate()
        .map(|(index, (_, person, lines))| {
            format!(
                "{}. {} {}",
                index + 1,
                no_highlight(&person.name),
                grouped(lines)
            )
        })
        .collect::<Vec<_>>();
    let (period_key, period_default) = match period {
        Period::Today => ("stats.period_today", "today"),
        Period::Week => ("stats.period_week", "this week"),
        Period::Month => ("stats.period_month", "this month"),
        Period::All => ("stats.period_all", "all time"),
    };
    let period_name = themed(period_key, &[period_default], &[])?;
    if board.is_empty() {
        return say(
            msg,
            "stats.top_empty",
            "Nobody has said a word in {channel} {period}, {honorific}.",
            &[("channel", &msg.target), ("period", &period_name)],
        );
    }
    say(
        msg,
        "stats.top",
        "🏆 {channel} {period}: {board}",
        &[
            ("channel", &msg.target),
            ("period", &period_name),
            ("board", &board.join(" · ")),
        ],
    )
}

fn cmd_person(
    server: &str,
    msg: &MessagePayload,
    who: Option<&str>,
    today: i64,
) -> Result<String, Error> {
    let profile_id = match who {
        None => msg.user_id.clone(),
        Some(nick) => {
            let raw = unsafe {
                profile_get(serde_json::to_string(&ProfileKey {
                    server: server.into(),
                    nick: nick.into(),
                })?)?
            };
            match (!raw.trim().is_empty())
                .then(|| serde_json::from_str::<Profile>(&raw))
                .transpose()?
            {
                Some(profile) => profile.id,
                None => String::new(),
            }
        }
    };
    let asked = who.unwrap_or(display(msg));
    let people = people_in(server, &msg.target)?;
    let Some(person) = people
        .iter()
        .find(|(id, _)| !profile_id.is_empty() && *id == profile_id)
        .map(|(_, person)| person)
    else {
        return say(
            msg,
            "stats.person_unknown",
            "I've no figures for {who} in {channel}, {honorific}.",
            &[("who", asked), ("channel", &msg.target)],
        );
    };
    let channel: Channel = load(&channel_key(server, &msg.target))?;
    let rank = ranked(&people, Period::All, today)
        .iter()
        .position(|(id, _, _)| *id == profile_id)
        .map_or(0, |index| index + 1);
    let share = (person.total.lines * 100 + channel.lines / 2)
        .checked_div(channel.lines)
        .unwrap_or(0);
    let words = if person.total.lines == 0 {
        0.0
    } else {
        person.total.words as f64 / person.total.lines as f64
    };
    let (key, default) = if person.total.lines == 1 {
        (
            "stats.person_one",
            "{who} in {channel}: {lines} line (#{rank}, {share}% of the room) · {words} words a line · liveliest at {hour} · {streak}-day streak (best {best}) · since {since}",
        )
    } else {
        (
            "stats.person",
            "{who} in {channel}: {lines} lines (#{rank}, {share}% of the room) · {words} words a line · liveliest at {hour} · {streak}-day streak (best {best}) · since {since}",
        )
    };
    say(
        msg,
        key,
        default,
        &[
            ("who", &person.name),
            ("channel", &msg.target),
            ("lines", &grouped(person.total.lines)),
            ("rank", &rank.to_string()),
            ("share", &share.to_string()),
            ("words", &format!("{words:.1}")),
            (
                "hour",
                &peak(&person.hours).map(hour_label).unwrap_or_default(),
            ),
            ("streak", &person.current_streak(today).to_string()),
            ("best", &person.best_streak.to_string()),
            ("since", &short_date(person.first_day)),
        ],
    )
}

fn cmd_hours(server: &str, msg: &MessagePayload, zone: &str) -> Result<String, Error> {
    let channel: Channel = load(&channel_key(server, &msg.target))?;
    let hours = channel.hours();
    let Some(busiest) = peak(&hours) else {
        return say(
            msg,
            "stats.hours_empty",
            "No hours to show for {channel} yet, {honorific}.",
            &[("channel", &msg.target)],
        );
    };
    let quietest = hours
        .iter()
        .enumerate()
        .min_by_key(|(_, lines)| **lines)
        .map_or(0, |(hour, _)| hour);
    say(
        msg,
        "stats.hours",
        "🕐 {channel} by hour ({zone}): {sparkline} busiest {peak}, quietest {quiet}",
        &[
            ("channel", &msg.target),
            ("zone", &zone_label(zone)),
            ("sparkline", &sparkline(&hours)),
            ("peak", &hour_label(busiest)),
            ("quiet", &hour_label(quietest)),
        ],
    )
}

/// Opts the caller out on this network: no more counting, and their figures are wiped.
fn cmd_private(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    kv_save(&private_key(server, &msg.user_id), "1")?;
    is_private(server, &msg.user_id)?;
    PRIVATE.with(|private| {
        if let Some(set) = private.borrow_mut().as_mut() {
            set.insert((server.to_string(), msg.user_id.clone()));
        }
    });
    forget(server, &msg.user_id)?;
    say(
        msg,
        "stats.private",
        "Very good, {honorific}: I've stopped counting you and wiped your figures. !stats public to be counted again.",
        &[],
    )
}

fn cmd_public(server: &str, msg: &MessagePayload) -> Result<String, Error> {
    kv_save(&private_key(server, &msg.user_id), "")?;
    PRIVATE.with(|private| {
        if let Some(set) = private.borrow_mut().as_mut() {
            set.remove(&(server.to_string(), msg.user_id.clone()));
        }
    });
    say(
        msg,
        "stats.public",
        "Noted, {honorific}; I'll count you from here on.",
        &[],
    )
}

/// Drops someone's pending counts and stored figures on a network.
fn forget(server: &str, profile_id: &str) -> Result<(), Error> {
    let suffix = format!(":{profile_id}");
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        pending.people.retain(|key, _| !key.ends_with(&suffix));
        pending
            .progress
            .remove(&(server.to_string(), profile_id.to_string()));
    });
    for entry in kv_list_prefix(&format!("person:{}:", encode(server)))? {
        if entry.key.ends_with(&suffix) && !entry.value.is_empty() {
            kv_save(&entry.key, "")?;
        }
    }
    for entry in kv_list_prefix(&format!("public:{}:", encode(server)))? {
        if entry.value.trim().is_empty() {
            continue;
        }
        let mut snapshot: PublicChannelStats = serde_json::from_str(&entry.value)?;
        if model::scrub_public(&mut snapshot, profile_id) {
            kv_save(&entry.key, &serde_json::to_string(&snapshot)?)?;
        }
    }
    Ok(())
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
    if msg.user_id.is_empty() {
        let text = say(
            msg,
            "stats.profile_missing",
            "I cannot place your profile just now, {honorific}; do try again shortly.",
            &[],
        )?;
        return reply(server, destination, &text);
    }
    let (sub, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(sub, rest)| (sub.to_ascii_lowercase(), rest.trim()))
        .unwrap_or((argument.to_ascii_lowercase(), ""));
    // Opting out or in works anywhere; the figures belong to a channel.
    let text = match sub.as_str() {
        "private" | "optout" => cmd_private(server, msg)?,
        "public" | "optin" => cmd_public(server, msg)?,
        _ if msg.is_private => say(
            msg,
            "stats.channel_only",
            "Stats belong to a channel, {honorific}; ask me there.",
            &[],
        )?,
        _ if setting("enabled", server, Some(&msg.target))? != "true" => say(
            msg,
            "stats.not_counting",
            "I'm not keeping figures in {channel}, {honorific}; an operator can switch them on.",
            &[("channel", &msg.target)],
        )?,
        _ => {
            // Read what was just said, too.
            flush()?;
            let (at, zone) = local_now(server, &msg.target, now)?;
            match sub.as_str() {
                "" => cmd_overview(server, msg, at.day)?,
                "top" => cmd_top(server, msg, rest, at.day)?,
                "me" => cmd_person(server, msg, None, at.day)?,
                "hours" | "hour" => cmd_hours(server, msg, &zone)?,
                "awards" | "award" => cmd_awards(server, msg, rest, at.day)?,
                "digest" => {
                    // A preview of this week so far, for operators setting it up.
                    if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
                        say(
                            msg,
                            "stats.digest_admin_only",
                            "Previewing the digest is for admins, {honorific}.",
                            &[],
                        )?
                    } else {
                        let channel: Channel = load(&channel_key(server, &msg.target))?;
                        let week = week_of(at.day);
                        if channel.week_summary(week).lines == 0 {
                            say(
                                msg,
                                "stats.digest_empty",
                                "Nothing to digest in {channel} this week yet, {honorific}.",
                                &[("channel", &msg.target)],
                            )?
                        } else {
                            let lines = digest_lines(server, &msg.target, &channel, week, true)?;
                            for line in &lines[..lines.len() - 1] {
                                reply(server, &msg.target, line)?;
                            }
                            lines[lines.len() - 1].clone()
                        }
                    }
                }
                // `!stats week` reads as the week's board.
                period if Period::parse(period).is_some() => cmd_top(server, msg, period, at.day)?,
                _ => cmd_person(server, msg, Some(argument.trim()), at.day)?,
            }
        }
    };
    reply(server, destination, &text)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let server = env.server.as_str();
    let text = msg.text.trim();
    let now = timestamp()?;
    if !msg.is_action {
        let (command, argument) = text
            .split_once(char::is_whitespace)
            .map(|(command, argument)| (command, argument.trim()))
            .unwrap_or((text, ""));
        if command.eq_ignore_ascii_case("!stats") {
            handle_command(server, &msg, argument, now)?;
            return Ok(());
        }
        // Other modules' commands aren't talk.
        if text.starts_with('!') {
            return Ok(());
        }
    }
    // The host only delivers ambient lines (and `/me`, via `action_events`) where `enabled` is on.
    if msg.is_private || msg.user_id.is_empty() || text.is_empty() {
        return Ok(());
    }
    if is_private(server, &msg.user_id)? {
        return Ok(());
    }
    count_line(server, &msg, now)?;
    Ok(())
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Timer { id, channel, .. } = &env.event else {
        return Ok(());
    };
    if id == "flush" {
        flush()?;
    } else if id.starts_with("digest:") {
        post_digest(&env.server, channel, timestamp()?)?;
    }
    Ok(())
}

// ── the public page ─────────────────────────────────────────────────────────

/// How often a channel's public snapshot is rebuilt, at most.
const PUBLISH_EVERY_SECONDS: i64 = 600;

thread_local! {
    /// (server, channel) → when its public snapshot was last considered.
    static PUBLISHED: RefCell<HashMap<(String, String), i64>> = RefCell::new(HashMap::new());
}

fn public_key(server: &str, channel: &str) -> String {
    format!("public:{}:{}", encode(server), encode(channel))
}

/// Whether a profile, looked up by nick, has made its achievements public.
fn public_name(server: &str, profile_id: &str, nick: &str) -> Result<Option<String>, Error> {
    if nick.is_empty() {
        return Ok(None);
    }
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let profile: Profile = serde_json::from_str(&raw)?;
    let public = profile.id == profile_id
        && profile.achievements_public == Some(true)
        && profile.achievements_opt_out != Some(true);
    Ok(public.then_some(profile.nick))
}

/// Rebuilds a channel's public snapshot if it's due, or removes it once the page is switched off.
fn maybe_publish(server: &str, channel_name: &str, now: i64) -> Result<(), Error> {
    let key = (server.to_string(), channel_name.to_string());
    let last = PUBLISHED.with(|published| published.borrow().get(&key).copied());
    if last.is_some_and(|at| now - at < PUBLISH_EVERY_SECONDS) {
        return Ok(());
    }
    PUBLISHED.with(|published| published.borrow_mut().insert(key, now));
    let snapshot_key = public_key(server, channel_name);
    if setting("public_page", server, Some(channel_name))? != "true" {
        if !kv_load(&snapshot_key)?.trim().is_empty() {
            kv_save(&snapshot_key, "")?;
        }
        return Ok(());
    }
    let (at, zone) = local_now(server, channel_name, now)?;
    let channel: Channel = load(&channel_key(server, channel_name))?;
    let mut people = people_in(server, channel_name)?;
    // Name lookups only for the most talkative; the rest can only be "someone" anyway.
    people.sort_by_key(|(_, person)| std::cmp::Reverse(person.total.lines));
    let mut names = HashMap::new();
    for (id, person) in people.iter().take(100) {
        if let Some(name) = public_name(server, id, &person.nick)? {
            names.insert(id.clone(), name);
        }
    }
    let place = model::Place {
        server,
        channel: channel_name,
        zone: &zone,
        now,
        today: at.day,
    };
    let snapshot = model::public_snapshot(&place, &channel, &people, &names);
    kv_save(&snapshot_key, &serde_json::to_string(&snapshot)?)
}

// ── awards and the weekly digest ────────────────────────────────────────────

fn award_text(award: &Award) -> Result<String, Error> {
    let name = no_highlight(&award.name);
    let count = grouped(award.value);
    let (key, default) = match award.kind {
        AwardKind::Chatterbox => (
            "stats.award_chatterbox",
            "Chatterbox, {name} ({count} lines)",
        ),
        AwardKind::Inquisitor => (
            "stats.award_inquisitor",
            "The Inquisitor, {name} ({count} questions)",
        ),
        AwardKind::Excitable => (
            "stats.award_excitable",
            "Most Excitable, {name} ({count} exclamations)",
        ),
        AwardKind::CapsLock => (
            "stats.award_caps",
            "Caps Lock Champion, {name} ({count} shouted lines)",
        ),
        AwardKind::Librarian => (
            "stats.award_librarian",
            "Link Librarian, {name} ({count} links)",
        ),
        AwardKind::NightOwl => (
            "stats.award_night_owl",
            "Night Owl, {name} ({count} lines after midnight)",
        ),
        AwardKind::Theatrical => (
            "stats.award_theatrical",
            "Most Theatrical, {name} ({count} actions)",
        ),
        AwardKind::Wordsmith => (
            "stats.award_wordsmith",
            "Wordsmith, {name} ({words} words a line)",
        ),
    };
    let words = format!("{}.{}", award.value / 10, award.value % 10);
    themed(
        key,
        &[default],
        &[("name", &name), ("count", &count), ("words", &words)],
    )
}

fn awards_line(people: &[(String, Person)], week: i64) -> Result<Vec<String>, Error> {
    awards(people, week).iter().map(award_text).collect()
}

fn cmd_awards(
    server: &str,
    msg: &MessagePayload,
    argument: &str,
    today: i64,
) -> Result<String, Error> {
    let last = matches!(argument.to_ascii_lowercase().as_str(), "last" | "lastweek");
    let week = week_of(today) - i64::from(last);
    let people = people_in(server, &msg.target)?;
    let won = awards_line(&people, week)?;
    let (period_key, period_default) = if last {
        ("stats.period_last_week", "last week")
    } else {
        ("stats.period_week", "this week")
    };
    let period = themed(period_key, &[period_default], &[])?;
    if won.is_empty() {
        return say(
            msg,
            "stats.awards_empty",
            "No awards for {channel} {period} yet, {honorific}.",
            &[("channel", &msg.target), ("period", &period)],
        );
    }
    say(
        msg,
        "stats.awards",
        "🎖 {channel} {period}: {awards}",
        &[
            ("channel", &msg.target),
            ("period", &period),
            ("awards", &won.join(" · ")),
        ],
    )
}

thread_local! {
    /// (server, channel) → when its next digest was last booked.
    static DIGEST_CHECKED: RefCell<HashMap<(String, String), i64>> = RefCell::new(HashMap::new());
}

fn digest_id(server: &str, channel: &str) -> String {
    format!("digest:{}:{}", encode(server), encode(channel))
}

fn schedule_digest(server: &str, channel: &str, now: i64) -> Result<(), Error> {
    let zone = setting("timezone", server, Some(channel))?;
    let zone = if zone.trim().is_empty() {
        DEFAULT_TIMEZONE
    } else {
        zone.trim()
    };
    let due_at = next_digest_at(now, offset_for(zone, now)?);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: digest_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at,
            payload: String::new(),
        })?)?
    };
    Ok(())
}

/// Makes sure a channel with the digest on has its next one booked. Once booked it is left alone
/// for an hour; the setting itself is an in-memory read, so switching it on takes effect at once.
fn ensure_digest(server: &str, channel: &str, now: i64) -> Result<(), Error> {
    let key = (server.to_string(), channel.to_string());
    let booked = DIGEST_CHECKED.with(|checked| checked.borrow().get(&key).copied());
    if booked.is_some_and(|at| now - at < 3_600) {
        return Ok(());
    }
    if setting("digest", server, Some(channel))? == "true" {
        schedule_digest(server, channel, now)?;
        DIGEST_CHECKED.with(|checked| checked.borrow_mut().insert(key, now));
    }
    Ok(())
}

/// Monday morning: last week in the channel, its awards, and something from the quote book.
fn post_digest(server: &str, channel_name: &str, now: i64) -> Result<(), Error> {
    if setting("enabled", server, Some(channel_name))? != "true"
        || setting("digest", server, Some(channel_name))? != "true"
    {
        // Switched off: no digest, and none booked until it's switched back on.
        DIGEST_CHECKED.with(|checked| {
            checked
                .borrow_mut()
                .remove(&(server.to_string(), channel_name.to_string()))
        });
        return Ok(());
    }
    flush()?;
    let (at, _) = local_now(server, channel_name, now)?;
    let week = week_of(at.day) - 1;
    let key = channel_key(server, channel_name);
    let mut channel: Channel = load(&key)?;
    // A redelivered timer, or a digest already posted this week.
    if channel.digest_week < week {
        channel.digest_week = week;
        kv_save(&key, &serde_json::to_string(&channel)?)?;
        let summary = channel.week_summary(week);
        if summary.lines > 0 {
            for line in digest_lines(server, channel_name, &channel, week, false)? {
                reply(server, channel_name, &line)?;
            }
        }
    }
    schedule_digest(server, channel_name, now + 60)
}

fn digest_lines(
    server: &str,
    channel_name: &str,
    channel: &Channel,
    week: i64,
    preview: bool,
) -> Result<Vec<String>, Error> {
    let summary = channel.week_summary(week);
    let people = people_in(server, channel_name)?;
    let change = if summary.previous == 0 {
        String::new()
    } else {
        let percent = (summary.lines as i64 * 100 / summary.previous as i64) - 100;
        let magnitude = percent.unsigned_abs().to_string();
        match percent.signum() {
            1 => themed(
                "stats.digest_up",
                &[" (up {percent}% on the week before)"],
                &[("percent", &magnitude)],
            )?,
            -1 => themed(
                "stats.digest_down",
                &[" (down {percent}% on the week before)"],
                &[("percent", &magnitude)],
            )?,
            _ => themed("stats.digest_same", &[" (much as the week before)"], &[])?,
        }
    };
    let (busiest_day, busiest_lines) = summary.busiest.unwrap_or((model::week_start(week), 0));
    let mut top = people
        .iter()
        .filter_map(|(_, person)| {
            week_counts(person, week).map(|w| (person.name.as_str(), w.counts.lines))
        })
        .collect::<Vec<_>>();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top = top
        .iter()
        .take(3)
        .map(|(name, lines)| format!("{} {}", no_highlight(name), grouped(*lines)))
        .collect::<Vec<_>>()
        .join(", ");
    let record = match summary.record_day {
        Some(day) => themed(
            "stats.digest_record",
            &[" · {day} was a record day!"],
            &[("day", weekday_name(day))],
        )?,
        None => String::new(),
    };
    let faces = new_faces(&people, week);
    let faces = if faces.is_empty() {
        String::new()
    } else {
        themed(
            "stats.digest_new_faces",
            &[" · new faces: {names}"],
            &[(
                "names",
                &faces
                    .iter()
                    .map(|name| no_highlight(name))
                    .collect::<Vec<_>>()
                    .join(", "),
            )],
        )?
    };
    let period = if preview {
        themed("stats.digest_period_so_far", &["This week so far"], &[])?
    } else {
        themed("stats.digest_period_last", &["Last week"], &[])?
    };
    let mut lines = vec![themed(
        "stats.digest",
        &["📰 {period} in {channel}: {lines} lines{change} · busiest {day} ({day_lines}) · top: {top}{record}{new_faces}"],
        &[
            ("period", &period),
            ("channel", channel_name),
            ("lines", &grouped(summary.lines)),
            ("change", &change),
            ("day", weekday_name(busiest_day)),
            ("day_lines", &grouped(busiest_lines)),
            ("top", &top),
            ("record", &record),
            ("new_faces", &faces),
        ],
    )?];
    let won = awards_line(&people, week)?;
    if !won.is_empty() {
        lines.push(themed(
            "stats.digest_awards",
            &["🎖 {awards}"],
            &[("awards", &won.join(" · "))],
        )?);
    }
    if let Some(quote) = random_quote(server, channel_name)? {
        lines.push(themed(
            "stats.digest_quote",
            &["📜 Remember this? {quote}"],
            &[("quote", &quote)],
        )?);
    }
    Ok(lines)
}

/// One line from the channel's quote book, via history's `!quote`; none if the book is empty or
/// history isn't loaded.
fn random_quote(server: &str, channel: &str) -> Result<Option<String>, Error> {
    let raw = unsafe {
        run_command(serde_json::to_string(&RunCommandRequest {
            server: server.into(),
            channel: Some(channel.into()),
            text: "!quote".into(),
            user_id: String::new(),
            nick: String::new(),
            display: String::new(),
            allowed: vec!["quote".into()],
        })?)?
    };
    let response: RunCommandResponse = serde_json::from_str(&raw)?;
    Ok(response
        .error
        .is_none()
        .then(|| response.lines.into_iter().next())
        .flatten()
        .filter(|line| !line.trim().is_empty()))
}

// ── lifecycle ───────────────────────────────────────────────────────────────

/// A subject's own entries on their network: per-channel figures and the opt-out flag.
fn subject_entries(request: &ModuleDataRequest) -> Vec<&ModuleKvEntry> {
    let server = encode(&request.subject.server);
    let person = format!("person:{server}:");
    let suffix = format!(":{}", request.subject.profile_id);
    let private = private_key(&request.subject.server, &request.subject.profile_id);
    request
        .entries
        .iter()
        .filter(|entry| {
            !entry.value.trim().is_empty()
                && ((entry.key.starts_with(&person) && entry.key.ends_with(&suffix))
                    || entry.key == private)
        })
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let prefix = format!("person:{}:", encode(&request.subject.server));
    let mut channels = serde_json::Map::new();
    let mut opted_out = false;
    for entry in subject_entries(&request) {
        match entry.key.strip_prefix(&prefix) {
            Some(rest) => {
                let channel = decode(rest.split(':').next().unwrap_or(""));
                let person: Person = serde_json::from_str(&entry.value)?;
                channels.insert(channel, serde_json::to_value(person)?);
            }
            None => opted_out = true,
        }
    }
    let data = if channels.is_empty() && !opted_out {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "channels": channels, "opted_out": opted_out })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    // Pending counts aren't KV yet, so they're dropped here rather than planned.
    let suffix = format!(":{}", request.subject.profile_id);
    PENDING.with(|pending| {
        let mut pending = pending.borrow_mut();
        pending.people.retain(|key, _| !key.ends_with(&suffix));
        pending.progress.remove(&(
            request.subject.server.clone(),
            request.subject.profile_id.clone(),
        ));
    });
    PRIVATE.with(|private| *private.borrow_mut() = None);
    let mut mutations = subject_entries(&request)
        .into_iter()
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect::<Vec<_>>();
    // Published snapshots hold other people too: rewrite them without the subject.
    let public = format!("public:{}:", encode(&request.subject.server));
    for entry in request
        .entries
        .iter()
        .filter(|entry| entry.key.starts_with(&public) && !entry.value.trim().is_empty())
    {
        let mut snapshot: PublicChannelStats = serde_json::from_str(&entry.value)?;
        if model::scrub_public(&mut snapshot, &request.subject.profile_id) {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: Some(serde_json::to_string(&snapshot)?),
            });
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

#[plugin_fn]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&backfill(&request)?)?)
}

/// Lifetime lines across the network's channels, and whether a seven-day streak was ever reached.
fn backfill(request: &AchievementBackfillRequest) -> Result<AchievementBackfillResponse, Error> {
    let prefix = format!("person:{}:", encode(&request.server));
    let mut totals: BTreeMap<String, (u64, bool)> = BTreeMap::new();
    for entry in &request.entries {
        let Some(rest) = entry.key.strip_prefix(&prefix) else {
            continue;
        };
        if entry.value.trim().is_empty() {
            continue;
        }
        let Some((_, profile_id)) = rest.split_once(':') else {
            continue;
        };
        let person: Person = serde_json::from_str(&entry.value)?;
        let total = totals.entry(profile_id.to_string()).or_default();
        total.0 += person.total.lines;
        total.1 |= person.best_streak >= 7;
    }
    Ok(AchievementBackfillResponse {
        values: totals
            .into_iter()
            .flat_map(|(profile_id, (lines, streak))| {
                [
                    AchievementSetMax {
                        profile_id: profile_id.clone(),
                        stat: "lines".into(),
                        value: lines,
                    },
                    AchievementSetMax {
                        profile_id,
                        stat: "week_streaks".into(),
                        value: u64::from(streak),
                    },
                ]
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_backfill_sums_channels() {
        assert_eq!(decode(&encode("#Games ü")), "#Games ü");
        let person = |lines: u64, best_streak: u32| {
            serde_json::to_string(&Person {
                total: model::Counts {
                    lines,
                    ..Default::default()
                },
                best_streak,
                ..Default::default()
            })
            .unwrap()
        };
        let request = AchievementBackfillRequest {
            server: "net".into(),
            entries: vec![
                ModuleKvEntry {
                    key: person_key("net", "#a", "p1"),
                    value: person(10, 3),
                },
                ModuleKvEntry {
                    key: person_key("net", "#b", "p1"),
                    value: person(5, 8),
                },
                ModuleKvEntry {
                    key: person_key("other", "#a", "p1"),
                    value: person(99, 9),
                },
                ModuleKvEntry {
                    key: person_key("net", "#a", "p2"),
                    value: String::new(),
                },
            ],
            previous_version: 0,
            catalog_version: 1,
        };
        let values = backfill(&request).unwrap().values;
        assert_eq!(
            values,
            [
                AchievementSetMax {
                    profile_id: "p1".into(),
                    stat: "lines".into(),
                    value: 15
                },
                AchievementSetMax {
                    profile_id: "p1".into(),
                    stat: "week_streaks".into(),
                    value: 1
                },
            ]
        );
    }
}
