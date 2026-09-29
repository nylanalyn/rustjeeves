//! Durable reminders backed by the host scheduler.
//!
//! `!remind` understands forgiving phrasing (see [`phrase`]) in the owner's saved timezone:
//! "me to X in 10 minutes", "me at 5:30 next tuesday to X", "me every weekday at 9 to X". Set in
//! a channel, a reminder is delivered there (or by PM if the owner has left); set by PM, it's
//! delivered by PM. `!snooze` re-arms the last delivered reminder, and reminders for someone else
//! (`!remind sally at 10 to eat cheese`) wait for them to `!remind accept` within the hour.

mod phrase;
#[allow(dead_code)]
#[path = "../../clock/src/when.rs"]
mod when;

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, Channel,
    CommandManifest, CommandShortcut, CommandSpec, Event, EventEnvelope, KvGet, KvSet,
    LocalTimeQuery, LocalTimeResult, LocalWallTime, MessagePayload, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, ScheduleCancel,
    ScheduleList, ScheduleSet, ScheduledJob, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{encode, reply, themed, timestamp};
use phrase::{LocalNow, ParseError, RecurDays, Recurrence, When, Who};
use serde::{Deserialize, Serialize};

const MAX_TEXT_CHARS: usize = 300;
const DEFAULT_MAX_PENDING: i64 = 20;
const DEFAULT_MAX_HORIZON: i64 = 30 * 24 * 60 * 60;
const DEFAULT_MAX_RECURRING: i64 = 3;
const DEFAULT_SNOOZE: i64 = 10 * 60;
/// How long after delivery a reminder can still be snoozed.
const SNOOZE_WINDOW: i64 = 60 * 60;
/// How long a reminder request waits for its recipient's answer.
const REQUEST_TTL: i64 = 60 * 60;
const MAX_REQUESTS: usize = 200;
const MAX_REQUESTS_PER_RECIPIENT: usize = 3;
const LIST_SIZE: usize = 5;

#[host_fn]
extern "ExtismHost" {
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn local_time(input: String) -> String;
    fn channel_members(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("gentle_nudge", "A Gentle Nudge", 1),
        ("well_reminded", "Well Reminded", 25),
        ("nothing_escapes", "Nothing Escapes Me", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Receive {threshold} delivered reminders."),
        stat: "deliveries".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "long_memory".into(),
        name: "A Long Memory".into(),
        description: "Schedule a reminder at least 30 days ahead.".into(),
        stat: "long_memory".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "deliveries".into(),
                description: "Delivered reminders".into(),
            },
            AchievementStat {
                id: "long_memory".into(),
                description: "Reminders scheduled 30 days ahead".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display: &str,
    target: &str,
    stat: &str,
) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display.into(),
            target: target.into(),
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
                name: "remind".into(),
                aliases: vec!["reminder".into()],
                description: "Set a reminder in plain words: me in 10 minutes to…, me at 5pm tomorrow to…, me every monday at 9 to…; ask someone else and they accept.".into(),
                usage: "!remind [me|nick] <when> to <what> | !remind cancel <id> | !remind snooze [time] | !remind accept|decline".into(),
                shortcuts: vec![CommandShortcut::new("snooze", "snooze").described(
                    "Be reminded again of the reminder you just got (10 minutes unless you say).",
                    "!snooze [time]",
                )],
            },
            CommandSpec {
                name: "reminders".into(),
                aliases: Vec::new(),
                description: "List your pending reminders here.".into(),
                usage: "!reminders".into(),
                ..Default::default()
            },
        ],
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
                key: "max_pending".into(),
                description: "Maximum pending reminders per user in one channel (or by PM).".into(),
                default: DEFAULT_MAX_PENDING.to_string(),
                kind: SettingKind::Integer { min: 1, max: 50 },
                scopes: all.clone(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_horizon_seconds".into(),
                description: "Furthest into the future a reminder may be scheduled.".into(),
                default: DEFAULT_MAX_HORIZON.to_string(),
                kind: SettingKind::DurationSeconds {
                    min: 60,
                    max: 365 * 24 * 60 * 60,
                },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_recurring".into(),
                description: "Maximum recurring reminders per user on a network.".into(),
                default: DEFAULT_MAX_RECURRING.to_string(),
                kind: SettingKind::Integer { min: 0, max: 20 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── data model ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct RecurrenceSpec {
    /// "daily", "weekdays", or "weekly".
    days: String,
    #[serde(default)]
    weekday: u32,
    hour: u32,
    minute: u32,
    timezone: String,
}

impl RecurrenceSpec {
    fn from(recurrence: Recurrence, timezone: &str) -> Self {
        let (days, weekday) = match recurrence.days {
            RecurDays::Daily => ("daily", 0),
            RecurDays::Weekdays => ("weekdays", 0),
            RecurDays::Weekly(day) => ("weekly", day),
        };
        RecurrenceSpec {
            days: days.into(),
            weekday,
            hour: recurrence.hour,
            minute: recurrence.minute,
            timezone: timezone.into(),
        }
    }

    fn recurrence(&self) -> Recurrence {
        Recurrence {
            days: match self.days.as_str() {
                "weekdays" => RecurDays::Weekdays,
                "weekly" => RecurDays::Weekly(self.weekday.min(6)),
                _ => RecurDays::Daily,
            },
            hour: self.hour,
            minute: self.minute,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ReminderPayload {
    owner_id: String,
    owner_display: String,
    number: u64,
    text: String,
    /// Current nick at creation, for PM delivery and the "still here?" check.
    #[serde(default)]
    owner_nick: String,
    /// Set by private message: deliver by private message.
    #[serde(default)]
    private: bool,
    /// Who asked for this reminder, when it was someone else.
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    recurrence: Option<RecurrenceSpec>,
}

/// The last reminder delivered to someone, for `!snooze`.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Delivered {
    payload: ReminderPayload,
    channel: String,
    at: i64,
}

/// A reminder offered to someone else, waiting for their answer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Request {
    id: u64,
    channel: String,
    from_id: String,
    from_display: String,
    to_id: String,
    to_nick: String,
    due_at: i64,
    text: String,
    created_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Requests {
    next_id: u64,
    pending: Vec<Request>,
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let command = text.split_whitespace().next().unwrap_or("");
    if !matches!(command, "!remind" | "!reminders") {
        return Ok(());
    }
    let server = env.server.as_str();
    let dest = destination(&msg);
    if msg.user_id.is_empty() {
        // Never key state on a nick: without a stable profile the command waits.
        return Ok(reply(
            server,
            dest,
            &themed(
                "identity_unavailable",
                &["I can't verify your profile right now, {user}; please try again shortly."],
                &[("user", display_name(&msg))],
            )?,
        )?);
    }
    let now = timestamp()?;
    if command == "!reminders" {
        return Ok(list_reminders(server, &msg, now)?);
    }
    let arg = text.strip_prefix("!remind").unwrap_or("").trim();
    let (action, rest) = arg
        .split_once(char::is_whitespace)
        .map(|(action, rest)| (action, rest.trim()))
        .unwrap_or((arg, ""));
    match action.to_ascii_lowercase().as_str() {
        "cancel" | "delete" => cancel_reminder(server, &msg, rest)?,
        "snooze" => snooze(server, &msg, rest, now)?,
        "accept" => answer_request(server, &msg, true, now)?,
        "decline" | "refuse" => answer_request(server, &msg, false, now)?,
        _ => create_reminder(server, &msg, arg, now)?,
    }
    Ok(())
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Timer {
        id,
        channel,
        payload,
        ..
    } = env.event
    else {
        return Ok(());
    };
    let server = env.server.as_str();
    let reminder: ReminderPayload = serde_json::from_str(&payload)?;
    // A channel reminder whose owner has left goes to them privately instead.
    let absent = !reminder.private
        && !reminder.owner_nick.is_empty()
        && !present(server, &channel, &reminder.owner_nick)?;
    let (target, key, default) = match (reminder.private || absent, &reminder.from) {
        (false, None) => (
            channel.as_str(),
            "delivery",
            "Reminder for {user}: {message}",
        ),
        (false, Some(_)) => (
            channel.as_str(),
            "reminders.delivery_from",
            "Reminder for {user} (from {from}): {message}",
        ),
        (true, None) if absent => (
            reminder.owner_nick.as_str(),
            "reminders.delivery_elsewhere",
            "Reminder from {channel}, {user}: {message}",
        ),
        (true, Some(_)) if absent => (
            reminder.owner_nick.as_str(),
            "reminders.delivery_elsewhere_from",
            "Reminder from {channel} (from {from}), {user}: {message}",
        ),
        (true, None) => (
            reminder.owner_nick.as_str(),
            "reminders.delivery_private",
            "Reminder, {user}: {message}",
        ),
        (true, Some(_)) => (
            reminder.owner_nick.as_str(),
            "reminders.delivery_private_from",
            "Reminder (from {from}), {user}: {message}",
        ),
    };
    reply(
        server,
        target,
        &themed(
            key,
            &[default],
            &[
                ("user", &reminder.owner_display),
                ("message", &reminder.text),
                ("id", &reminder.number.to_string()),
                ("from", reminder.from.as_deref().unwrap_or("")),
                ("channel", &channel),
            ],
        )?,
    )?;
    let now = timestamp()?;
    kv_put(
        &last_key(server, &reminder.owner_id),
        &serde_json::to_string(&Delivered {
            payload: reminder.clone(),
            channel: channel.clone(),
            at: now,
        })?,
    )?;
    award(
        server,
        &reminder.owner_id,
        &reminder.owner_display,
        target,
        "deliveries",
    )?;
    if let Some(spec) = &reminder.recurrence {
        let local = local_now(&spec.timezone, now)?;
        let (date, hour, minute) = phrase::next_occurrence(spec.recurrence(), local);
        if let Some(due_at) = wall_to_instant(&spec.timezone, date, hour, minute)? {
            schedule(server, &id, &channel, &reminder, due_at)?;
        }
    }
    Ok(())
}

// ── creating ────────────────────────────────────────────────────────────────

fn create_reminder(server: &str, msg: &MessagePayload, arg: &str, now: i64) -> Result<(), Error> {
    let dest = destination(msg);
    let user = display_name(msg);
    let timezone = own_timezone(server, &msg.nick)?;
    let zone = timezone.as_deref().unwrap_or("UTC");
    let local = local_now(zone, now)?;
    let parsed = match phrase::parse(arg, local) {
        Ok(parsed) => parsed,
        Err(ParseError::Usage) => return usage(server, dest, user),
        Err(ParseError::NoTime) => {
            return reply(
                server,
                dest,
                &themed(
                    "reminders.no_time",
                    &["I couldn't tell when, {user}. Try: in 10 minutes, at 5:30pm, tomorrow at 9, next tuesday at 18:00, or every day at 8."],
                    &[("user", user)],
                )?,
            )
        }
    };
    let text = sanitize(&parsed.text);
    if text.chars().count() > MAX_TEXT_CHARS {
        return reply(
            server,
            dest,
            &themed(
                "too_long",
                &["That reminder is too long; keep it to {max} characters."],
                &[("max", &MAX_TEXT_CHARS.to_string())],
            )?,
        );
    }
    let due_at = match parsed.when {
        When::In(seconds) => Some(now.saturating_add(seconds)),
        When::At { date, hour, minute } => wall_to_instant(zone, date, hour, minute)?,
        When::Every(recurrence) => {
            let (date, hour, minute) = phrase::next_occurrence(recurrence, local);
            wall_to_instant(zone, date, hour, minute)?
        }
    };
    let Some(due_at) = due_at.filter(|due_at| *due_at > now) else {
        return reply(
            server,
            dest,
            &themed(
                "reminders.past",
                &["That time has already passed, {user}."],
                &[("user", user)],
            )?,
        );
    };
    let max_horizon = setting_i64("max_horizon_seconds", server, None, DEFAULT_MAX_HORIZON)?;
    if due_at - now > max_horizon {
        return reply(
            server,
            dest,
            &themed(
                "too_far",
                &["That is too far away, {user}; the current limit is {limit}."],
                &[("user", user), ("limit", &human_duration(max_horizon))],
            )?,
        );
    }
    if let Who::Nick(nick) = &parsed.who {
        if !same_person(server, msg, nick)? {
            if matches!(parsed.when, When::Every(_)) {
                return reply(
                    server,
                    dest,
                    &themed(
                        "reminders.recurring_self_only",
                        &["Recurring reminders are only for yourself, {user}."],
                        &[("user", user)],
                    )?,
                );
            }
            return request_for_other(server, msg, nick, due_at, &text, now);
        }
    }
    let recurrence = match parsed.when {
        When::Every(recurrence) => {
            let max = setting_i64("max_recurring", server, None, DEFAULT_MAX_RECURRING)?;
            let existing = list_jobs(server, None)?
                .iter()
                .filter_map(job_payload)
                .filter(|payload| payload.owner_id == msg.user_id && payload.recurrence.is_some())
                .count() as i64;
            if existing >= max {
                return reply(
                    server,
                    dest,
                    &themed(
                        "reminders.recurring_full",
                        &["You already have {max} recurring reminders, {user}; cancel one first."],
                        &[("max", &max.to_string()), ("user", user)],
                    )?,
                );
            }
            Some(RecurrenceSpec::from(recurrence, zone))
        }
        _ => None,
    };
    let payload = ReminderPayload {
        owner_id: msg.user_id.clone(),
        owner_display: sanitize(user),
        number: 0,
        text,
        owner_nick: msg.nick.clone(),
        private: msg.is_private,
        from: None,
        recurrence,
    };
    let Some(number) = schedule_new(server, dest, payload.clone(), due_at, user)? else {
        return Ok(());
    };
    let when = match &payload.recurrence {
        Some(spec) => format!(
            "{} (first {})",
            phrase::describe_recurrence(spec.recurrence()),
            describe_due(due_at, zone, now)?
        ),
        // "in 10 minutes" says it best; a clock time would only add a timezone to puzzle over.
        None if matches!(parsed.when, When::In(_)) => {
            format!("in {}", short_duration(due_at - now))
        }
        None => describe_due(due_at, zone, now)?,
    };
    let key = if timezone.is_none() && !matches!(parsed.when, When::In(_)) {
        "reminders.scheduled_utc"
    } else {
        "reminders.scheduled"
    };
    let default = if key == "reminders.scheduled_utc" {
        "Reminder #{id} set for {when}, {user}. (That's UTC; !location <place> sets your own time.)"
    } else {
        "Reminder #{id} set for {when}, {user}."
    };
    reply(
        server,
        dest,
        &themed(
            key,
            &[default],
            &[("id", &number.to_string()), ("when", &when), ("user", user)],
        )?,
    )?;
    if due_at - now >= 30 * 24 * 60 * 60 {
        award(server, &msg.user_id, user, dest, "long_memory")?;
    }
    Ok(())
}

/// Check the owner's queue, number the reminder, and schedule it. None when refused (and said so).
fn schedule_new(
    server: &str,
    dest: &str,
    mut payload: ReminderPayload,
    due_at: i64,
    user: &str,
) -> Result<Option<u64>, Error> {
    let max_pending = setting_i64("max_pending", server, Some(dest), DEFAULT_MAX_PENDING)?;
    let owned = list_jobs(server, Some(dest))?
        .iter()
        .filter_map(job_payload)
        .filter(|existing| existing.owner_id == payload.owner_id)
        .count() as i64;
    if owned >= max_pending {
        reply(
            server,
            dest,
            &themed(
                "queue_full",
                &["You already have {max} reminders waiting in this channel, {user}."],
                &[("max", &max_pending.to_string()), ("user", user)],
            )?,
        )?;
        return Ok(None);
    }
    payload.number = next_number(server, &payload.owner_id)?;
    let id = job_id(server, &payload.owner_id, payload.number);
    if schedule(server, &id, dest, &payload, due_at).is_err() {
        reply(
            server,
            dest,
            &themed(
                "service_error",
                &["I couldn't save that reminder right now, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(None);
    }
    Ok(Some(payload.number))
}

fn schedule(
    server: &str,
    id: &str,
    channel: &str,
    payload: &ReminderPayload,
    due_at: i64,
) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: id.into(),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: Some(payload.owner_id.clone()),
            due_at,
            payload: serde_json::to_string(payload)?,
        })?)?;
    }
    Ok(())
}

// ── reminders for other people ──────────────────────────────────────────────

fn requests_key(server: &str) -> String {
    format!("requests:{}", encode(server))
}

fn load_requests(server: &str, now: i64) -> Result<Requests, Error> {
    let raw = kv_read(&requests_key(server))?;
    let mut requests: Requests = if raw.is_empty() {
        Requests::default()
    } else {
        serde_json::from_str(&raw)?
    };
    requests
        .pending
        .retain(|request| now - request.created_at < REQUEST_TTL);
    Ok(requests)
}

fn save_requests(server: &str, requests: &Requests) -> Result<(), Error> {
    kv_put(
        &requests_key(server),
        &if requests.pending.is_empty() {
            String::new()
        } else {
            serde_json::to_string(requests)?
        },
    )
}

fn request_for_other(
    server: &str,
    msg: &MessagePayload,
    nick: &str,
    due_at: i64,
    text: &str,
    now: i64,
) -> Result<(), Error> {
    let dest = destination(msg);
    let user = display_name(msg);
    if msg.is_private {
        return reply(
            server,
            dest,
            &themed(
                "reminders.others_in_channel",
                &["Reminders for someone else are asked in a channel they're in, {user}."],
                &[("user", user)],
            )?,
        );
    }
    let Some(target) = profile(server, nick)? else {
        return reply(
            server,
            dest,
            &themed(
                "reminders.unknown_person",
                &["I don't know anyone called {target}, {user}."],
                &[("target", nick), ("user", user)],
            )?,
        );
    };
    let mut requests = load_requests(server, now)?;
    if requests
        .pending
        .iter()
        .any(|request| request.from_id == msg.user_id && request.to_id == target.id)
    {
        return reply(
            server,
            dest,
            &themed(
                "reminders.request_waiting",
                &["You've already asked {target}, {user}; let them answer first."],
                &[("target", &target.nick), ("user", user)],
            )?,
        );
    }
    if requests
        .pending
        .iter()
        .filter(|request| request.to_id == target.id)
        .count()
        >= MAX_REQUESTS_PER_RECIPIENT
        || requests.pending.len() >= MAX_REQUESTS
    {
        return reply(
            server,
            dest,
            &themed(
                "reminders.request_full",
                &["{target} has enough reminder requests waiting, {user}; try again later."],
                &[("target", &target.nick), ("user", user)],
            )?,
        );
    }
    requests.next_id = requests.next_id.max(1);
    let request = Request {
        id: requests.next_id,
        channel: msg.target.clone(),
        from_id: msg.user_id.clone(),
        from_display: sanitize(user),
        to_id: target.id.clone(),
        to_nick: target.nick.clone(),
        due_at,
        text: text.into(),
        created_at: now,
    };
    requests.next_id += 1;
    requests.pending.push(request);
    save_requests(server, &requests)?;
    // Show the time in the recipient's own zone when they've saved one.
    let zone = target.timezone.clone().unwrap_or_else(|| "UTC".into());
    let when = describe_due(due_at, &zone, now)?;
    reply(
        server,
        dest,
        &themed(
            "reminders.request",
            &["{target}, {from} would like to remind you at {when} to {message}. !remind accept or !remind decline (within the hour)."],
            &[
                ("target", &target.nick),
                ("from", user),
                ("when", &when),
                ("message", text),
            ],
        )?,
    )
}

fn answer_request(server: &str, msg: &MessagePayload, accept: bool, now: i64) -> Result<(), Error> {
    let dest = destination(msg);
    let user = display_name(msg);
    let mut requests = load_requests(server, now)?;
    let Some(index) = requests.pending.iter().position(|request| {
        request.to_id == msg.user_id && (msg.is_private || request.channel == msg.target)
    }) else {
        return reply(
            server,
            dest,
            &themed(
                "reminders.no_request",
                &["Nobody has asked to remind you of anything here, {user}."],
                &[("user", user)],
            )?,
        );
    };
    let request = requests.pending.remove(index);
    save_requests(server, &requests)?;
    if !accept {
        return reply(
            server,
            dest,
            &themed(
                "reminders.request_declined",
                &["Very good, {user}; I'll let {from}'s reminder go."],
                &[("from", &request.from_display), ("user", user)],
            )?,
        );
    }
    if request.due_at <= now {
        return reply(
            server,
            dest,
            &themed(
                "reminders.past",
                &["That time has already passed, {user}."],
                &[("user", user)],
            )?,
        );
    }
    let payload = ReminderPayload {
        owner_id: msg.user_id.clone(),
        owner_display: sanitize(user),
        number: 0,
        text: request.text.clone(),
        owner_nick: msg.nick.clone(),
        private: false,
        from: Some(request.from_display.clone()),
        recurrence: None,
    };
    let Some(number) = schedule_new(server, &request.channel, payload, request.due_at, user)?
    else {
        return Ok(());
    };
    let zone = own_timezone(server, &msg.nick)?.unwrap_or_else(|| "UTC".into());
    reply(
        server,
        &request.channel,
        &themed(
            "reminders.request_accepted",
            &["Very good, {user}: reminder #{id} from {from}, at {when}."],
            &[
                ("id", &number.to_string()),
                ("from", &request.from_display),
                ("when", &describe_due(request.due_at, &zone, now)?),
                ("user", user),
            ],
        )?,
    )
}

// ── snooze, list, cancel ────────────────────────────────────────────────────

fn last_key(server: &str, owner_id: &str) -> String {
    format!("last:{}:{}", encode(server), encode(owner_id))
}

fn snooze(server: &str, msg: &MessagePayload, rest: &str, now: i64) -> Result<(), Error> {
    let dest = destination(msg);
    let user = display_name(msg);
    let raw = kv_read(&last_key(server, &msg.user_id))?;
    let last = serde_json::from_str::<Delivered>(&raw)
        .ok()
        .filter(|last| now - last.at <= SNOOZE_WINDOW);
    let Some(last) = last else {
        return reply(
            server,
            dest,
            &themed(
                "reminders.nothing_to_snooze",
                &["There's no recent reminder to snooze, {user}."],
                &[("user", user)],
            )?,
        );
    };
    let seconds = if rest.is_empty() {
        DEFAULT_SNOOZE
    } else {
        match phrase::parse_duration(rest.strip_prefix("for ").unwrap_or(rest)) {
            Some(seconds) => seconds,
            None => {
                return reply(
                    server,
                    dest,
                    &themed(
                        "bad_duration",
                        &["I couldn't understand that duration, {user}. Try 10 minutes, 2 hours, or 1h30m."],
                        &[("user", user)],
                    )?,
                )
            }
        }
    };
    let payload = ReminderPayload {
        number: 0,
        recurrence: None,
        ..last.payload
    };
    let Some(number) = schedule_new(server, &last.channel, payload, now + seconds, user)? else {
        return Ok(());
    };
    reply(
        server,
        dest,
        &themed(
            "reminders.snoozed",
            &["Snoozed, {user}: I'll remind you again {when} (#{id})."],
            &[
                ("when", &format!("in {}", human_duration(seconds))),
                ("id", &number.to_string()),
                ("user", user),
            ],
        )?,
    )
}

fn list_reminders(server: &str, msg: &MessagePayload, now: i64) -> Result<(), Error> {
    let dest = destination(msg);
    let user = display_name(msg);
    let mut reminders = list_jobs(server, Some(dest))?
        .into_iter()
        .filter_map(|job| job_payload(&job).map(|payload| (job, payload)))
        .filter(|(_, payload)| payload.owner_id == msg.user_id)
        .collect::<Vec<_>>();
    reminders.sort_by_key(|(job, _)| job.due_at);
    if reminders.is_empty() {
        return reply(
            server,
            dest,
            &themed(
                "none",
                &["You have no reminders waiting in this channel, {user}."],
                &[("user", user)],
            )?,
        );
    }
    let items = compact_list(
        &reminders
            .iter()
            .map(|(job, payload)| (job.due_at, payload))
            .collect::<Vec<_>>(),
        now,
    );
    reply(
        server,
        dest,
        &themed(
            "reminders.list",
            &["Your reminders here, {user}: {items}"],
            &[("items", &items), ("user", user)],
        )?,
    )
}

/// "#3 in 2 hours: oven · #4 every Monday at 18:00: bins · +2 more"
fn compact_list(reminders: &[(i64, &ReminderPayload)], now: i64) -> String {
    let mut parts = reminders
        .iter()
        .take(LIST_SIZE)
        .map(|(due_at, payload)| {
            let when = match &payload.recurrence {
                Some(spec) => phrase::describe_recurrence(spec.recurrence()),
                None => format!("in {}", short_duration(due_at - now)),
            };
            let text = payload.text.chars().take(40).collect::<String>();
            let more = if payload.text.chars().count() > 40 {
                "…"
            } else {
                ""
            };
            format!("#{} {when}: {text}{more}", payload.number)
        })
        .collect::<Vec<_>>();
    if reminders.len() > LIST_SIZE {
        parts.push(format!("+{} more", reminders.len() - LIST_SIZE));
    }
    parts.join(" · ")
}

fn cancel_reminder(server: &str, msg: &MessagePayload, raw_id: &str) -> Result<(), Error> {
    let dest = destination(msg);
    let Some(number) = raw_id.trim_start_matches('#').parse::<u64>().ok() else {
        return usage(server, dest, display_name(msg));
    };
    let found = list_jobs(server, None)?
        .into_iter()
        .filter_map(|job| job_payload(&job).map(|payload| (job, payload)))
        .find(|(_, payload)| payload.owner_id == msg.user_id && payload.number == number);
    let cancelled = match found {
        Some((job, _)) => {
            let raw =
                unsafe { schedule_cancel(serde_json::to_string(&ScheduleCancel { id: job.id })?)? };
            raw == "true"
        }
        None => false,
    };
    let (key, default) = if cancelled {
        ("cancelled", "Cancelled reminder #{id}, {user}.")
    } else {
        (
            "reminders.not_found",
            "I couldn't find reminder #{id} for you, {user}.",
        )
    };
    reply(
        server,
        dest,
        &themed(
            key,
            &[default],
            &[("id", &number.to_string()), ("user", display_name(msg))],
        )?,
    )
}

// ── time ────────────────────────────────────────────────────────────────────

fn get_local_time(query: LocalTimeQuery) -> Result<Option<LocalTimeResult>, Error> {
    let raw = unsafe { local_time(serde_json::to_string(&query)?)? };
    Ok(if raw.is_empty() {
        None
    } else {
        serde_json::from_str(&raw).ok()
    })
}

fn local_now(zone: &str, now: i64) -> Result<LocalNow, Error> {
    let local = get_local_time(LocalTimeQuery {
        timezone: zone.into(),
        unix_seconds: Some(now),
        local: None,
    })?;
    Ok(match local {
        Some(local) => LocalNow {
            today: when::Date {
                year: local.year,
                month: local.month,
                day: local.day,
            },
            minutes: local.hour_24 * 60 + local.minute,
        },
        None => {
            let date = when::civil_from_days(now.div_euclid(86_400));
            LocalNow {
                today: date,
                minutes: (now.rem_euclid(86_400) / 60) as u32,
            }
        }
    })
}

fn wall_to_instant(
    zone: &str,
    date: when::Date,
    hour: u32,
    minute: u32,
) -> Result<Option<i64>, Error> {
    Ok(get_local_time(LocalTimeQuery {
        timezone: zone.into(),
        unix_seconds: None,
        local: Some(LocalWallTime {
            year: date.year,
            month: date.month,
            day: date.day,
            hour,
            minute,
        }),
    })?
    .map(|result| result.unix_seconds))
}

/// "17:30 CEST (in 3 hours)", "Tue 09:00 BST (in 19 hours)", or "in 10 minutes".
fn describe_due(due_at: i64, zone: &str, now: i64) -> Result<String, Error> {
    let at = |unix| {
        get_local_time(LocalTimeQuery {
            timezone: zone.into(),
            unix_seconds: Some(unix),
            local: None,
        })
    };
    let relative = format!("in {}", short_duration(due_at - now));
    let (Some(due), Some(today)) = (at(due_at)?, at(now)?) else {
        return Ok(relative);
    };
    let day = if (due.year, due.month, due.day) == (today.year, today.month, today.day) {
        String::new()
    } else if due_at - now < 7 * 86_400 {
        format!("{} ", due.weekday.chars().take(3).collect::<String>())
    } else {
        format!(
            "{} {} ",
            due.day,
            when::MONTHS[(due.month as usize).saturating_sub(1).min(11)]
                .chars()
                .take(3)
                .collect::<String>()
        )
    };
    Ok(format!(
        "{day}{:02}:{:02} {} ({relative})",
        due.hour_24, due.minute, due.abbreviation
    ))
}

/// Durations of a minute or more round to the nearest minute ("in 10 minutes", never "in 9
/// minutes 59 seconds" a moment after setting it).
fn short_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    if seconds >= 60 {
        human_duration((seconds + 30) / 60 * 60)
    } else {
        human_duration(seconds)
    }
}

// ── host helpers ────────────────────────────────────────────────────────────

fn destination(msg: &MessagePayload) -> &str {
    if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    }
}

fn profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    Ok(if raw.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&raw)?)
    })
}

fn own_timezone(server: &str, nick: &str) -> Result<Option<String>, Error> {
    Ok(profile(server, nick)?.and_then(|profile| profile.timezone))
}

fn same_person(server: &str, msg: &MessagePayload, nick: &str) -> Result<bool, Error> {
    Ok(nick.eq_ignore_ascii_case(&msg.nick)
        || profile(server, nick)?.is_some_and(|profile| profile.id == msg.user_id))
}

fn present(server: &str, channel: &str, nick: &str) -> Result<bool, Error> {
    let raw = unsafe {
        channel_members(serde_json::to_string(&Channel {
            server: server.into(),
            channel: channel.into(),
        })?)?
    };
    let members: Vec<String> = serde_json::from_str(&raw).unwrap_or_default();
    // An empty list means the host doesn't know the room yet; don't redirect then.
    Ok(members.is_empty()
        || members
            .iter()
            .any(|member| member.eq_ignore_ascii_case(nick)))
}

fn list_jobs(server: &str, channel: Option<&str>) -> Result<Vec<ScheduledJob>, Error> {
    let raw = unsafe {
        schedule_list(serde_json::to_string(&ScheduleList {
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}

fn job_payload(job: &ScheduledJob) -> Option<ReminderPayload> {
    serde_json::from_str(&job.payload).ok()
}

fn kv_read(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_put(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?
    };
    Ok(())
}

fn next_number(server: &str, owner_id: &str) -> Result<u64, Error> {
    let key = sequence_key(server, owner_id);
    let number = kv_read(&key)?
        .parse::<u64>()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    kv_put(&key, &number.to_string())?;
    Ok(number)
}

fn sequence_key(server: &str, owner_id: &str) -> String {
    format!("sequence:{server}:{owner_id}")
}

// ── data lifecycle ──────────────────────────────────────────────────────────

fn identities(request: &ModuleDataRequest) -> Vec<&str> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let ids = identities(&request);
    let server = &request.subject.server;
    let sequence_keys = ids
        .iter()
        .map(|id| sequence_key(server, id))
        .collect::<Vec<_>>();
    let last_keys = ids
        .iter()
        .map(|id| last_key(server, id))
        .collect::<Vec<_>>();
    let mut data = serde_json::Map::new();
    let mut sequences = Vec::new();
    for entry in &request.entries {
        if sequence_keys.contains(&entry.key) {
            sequences.push(serde_json::json!({ "key": entry.key, "last_sequence": entry.value }));
        } else if last_keys.contains(&entry.key) && !entry.value.is_empty() {
            data.insert(
                "last_delivered".into(),
                serde_json::from_str::<serde_json::Value>(&entry.value)?,
            );
        } else if entry.key == requests_key(server) && !entry.value.is_empty() {
            let requests: Requests = serde_json::from_str(&entry.value)?;
            let mine = requests
                .pending
                .iter()
                .filter(|pending| {
                    ids.contains(&pending.from_id.as_str()) || ids.contains(&pending.to_id.as_str())
                })
                .collect::<Vec<_>>();
            if !mine.is_empty() {
                data.insert("pending_requests".into(), serde_json::to_value(mine)?);
            }
        }
    }
    if !sequences.is_empty() {
        data.insert("sequences".into(), serde_json::Value::Array(sequences));
    }
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if data.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(data)
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let ids = identities(&request);
    let server = &request.subject.server;
    let owned = ids
        .iter()
        .flat_map(|id| [sequence_key(server, id), last_key(server, id)])
        .collect::<Vec<_>>();
    let mut mutations = Vec::new();
    for entry in &request.entries {
        if owned.contains(&entry.key) {
            mutations.push(ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            });
        } else if entry.key == requests_key(server) && !entry.value.is_empty() {
            let mut requests: Requests = serde_json::from_str(&entry.value)?;
            let before = requests.pending.len();
            requests.pending.retain(|pending| {
                !ids.contains(&pending.from_id.as_str()) && !ids.contains(&pending.to_id.as_str())
            });
            if requests.pending.len() != before {
                mutations.push(ModuleKvMutation {
                    key: entry.key.clone(),
                    value: if requests.pending.is_empty() {
                        None
                    } else {
                        Some(serde_json::to_string(&requests)?)
                    },
                });
            }
        }
    }
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

// ── small helpers ───────────────────────────────────────────────────────────

fn setting_i64(
    key: &str,
    server: &str,
    channel: Option<&str>,
    fallback: i64,
) -> Result<i64, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    };
    Ok(raw.parse().unwrap_or(fallback))
}

fn usage(server: &str, target: &str, user: &str) -> Result<(), Error> {
    reply(
        server,
        target,
        &themed(
            "reminders.usage",
            &["Try !remind me in 10 minutes to check the oven, !remind me at 5pm tomorrow to call mum, or !remind me every monday at 9 to take the bins out. Also: !reminders, !remind cancel <id>, !snooze."],
            &[("user", user)],
        )?,
    )
}

fn human_duration(seconds: i64) -> String {
    let mut remaining = seconds.max(0);
    let units = [
        (86_400, "day"),
        (3_600, "hour"),
        (60, "minute"),
        (1, "second"),
    ];
    let mut parts = Vec::new();
    for (size, name) in units {
        let value = remaining / size;
        if value > 0 {
            parts.push(format!(
                "{value} {name}{}",
                if value == 1 { "" } else { "s" }
            ));
            remaining %= size;
        }
        if parts.len() == 2 {
            break;
        }
    }
    if parts.is_empty() {
        "less than a second".into()
    } else {
        parts.join(" ")
    }
}

fn job_id(server: &str, owner_id: &str, number: u64) -> String {
    format!("{server}:{owner_id}:{number}")
}

fn display_name(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn sanitize(input: &str) -> String {
    input
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_compactly_with_recurrences() {
        let payload = |number, text: &str, recurrence: Option<RecurrenceSpec>| ReminderPayload {
            owner_id: "me".into(),
            owner_display: "me".into(),
            number,
            text: text.into(),
            owner_nick: "me".into(),
            private: false,
            from: None,
            recurrence,
        };
        let oven = payload(3, "check the oven", None);
        let bins = payload(
            4,
            "take the bins out",
            Some(RecurrenceSpec {
                days: "weekly".into(),
                weekday: 0,
                hour: 18,
                minute: 0,
                timezone: "UTC".into(),
            }),
        );
        let list = compact_list(&[(7_200, &oven), (100_000, &bins)], 0);
        assert_eq!(
            list,
            "#3 in 2 hours: check the oven · #4 every Monday at 18:00: take the bins out"
        );
    }

    #[test]
    fn old_payloads_still_load() {
        let old = r#"{"owner_id":"a","owner_display":"A","number":1,"text":"hi"}"#;
        let payload: ReminderPayload = serde_json::from_str(old).unwrap();
        assert!(!payload.private && payload.recurrence.is_none() && payload.from.is_none());
    }

    #[test]
    fn formats_durations_and_sanitizes_messages() {
        assert_eq!(human_duration(5_400), "1 hour 30 minutes");
        assert_eq!(short_duration(599), "10 minutes");
        assert_eq!(short_duration(45), "45 seconds");
        assert_eq!(sanitize(" check\n\u{0003}04  logs "), "check04 logs");
    }
}
