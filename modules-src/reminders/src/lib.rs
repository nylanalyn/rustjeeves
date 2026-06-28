//! Durable, channel-local self-reminders backed by the host scheduler.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, ScheduleCancel, ScheduleList,
    ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, ThemeReq, COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const MAX_TEXT_CHARS: usize = 300;
const DEFAULT_MAX_PENDING: i64 = 20;
const DEFAULT_MAX_HORIZON: i64 = 30 * 24 * 60 * 60;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "remind".into(),
                aliases: vec!["reminder".into()],
                description: "Set or cancel a channel-local self-reminder.".into(),
                usage: "!remind me in <duration> to <message> | !remind cancel <id>".into(),
            },
            CommandSpec {
                name: "reminders".into(),
                aliases: Vec::new(),
                description: "List your pending reminders in this channel.".into(),
                usage: "!reminders".into(),
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
                key: "max_pending".into(),
                description: "Maximum pending reminders per user in one channel.".into(),
                default: DEFAULT_MAX_PENDING.to_string(),
                kind: SettingKind::Integer { min: 1, max: 50 },
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
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
        ],
    })?)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct ReminderPayload {
    owner_id: String,
    owner_display: String,
    number: u64,
    text: String,
}

#[derive(Debug, PartialEq, Eq)]
struct NewReminder {
    seconds: i64,
    text: String,
}

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
    if msg.is_private {
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "channel_only",
                &["Reminders must be created and managed in the channel where they will be delivered."],
                &[("user", display_name(&msg))],
            )?,
        )?;
        return Ok(());
    }
    let now = timestamp()?;
    if command == "!reminders" {
        list_reminders(&env.server, &msg, now)?;
        return Ok(());
    }
    let arg = text.strip_prefix("!remind").unwrap_or("").trim();
    if let Some((action, raw_id)) = arg.split_once(char::is_whitespace) {
        if action.eq_ignore_ascii_case("cancel") {
            cancel_reminder(&env.server, &msg, raw_id.trim())?;
            return Ok(());
        }
    }
    create_reminder(&env.server, &msg, arg, now)?;
    Ok(())
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Timer {
        channel, payload, ..
    } = env.event
    else {
        return Ok(());
    };
    let reminder: ReminderPayload = serde_json::from_str(&payload)?;
    reply(
        &env.server,
        &channel,
        &themed(
            "delivery",
            &["Reminder for {user}: {message}"],
            &[
                ("user", &reminder.owner_display),
                ("message", &reminder.text),
                ("id", &reminder.number.to_string()),
            ],
        )?,
    )?;
    Ok(())
}

fn create_reminder(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    arg: &str,
    now: i64,
) -> Result<(), Error> {
    let parsed = match parse_new_reminder(arg) {
        Ok(parsed) => parsed,
        Err(ParseError::OtherTarget) => {
            return reply(
                server,
                &msg.target,
                &themed(
                    "self_only",
                    &["For now I can only remind you yourself. Try: !remind me in 10 minutes to check the oven."],
                    &[("user", display_name(msg))],
                )?,
            );
        }
        Err(ParseError::BadDuration) => {
            return reply(
                server,
                &msg.target,
                &themed(
                    "bad_duration",
                    &["I couldn't understand that duration, {user}. Try 10 minutes, 2 hours, or 1h30m."],
                    &[("user", display_name(msg))],
                )?,
            );
        }
        Err(ParseError::Usage) => return usage(server, &msg.target, display_name(msg)),
    };
    if parsed.text.is_empty() {
        return usage(server, &msg.target, display_name(msg));
    }
    if parsed.text.chars().count() > MAX_TEXT_CHARS {
        return reply(
            server,
            &msg.target,
            &themed(
                "too_long",
                &["That reminder is too long; keep it to {max} characters."],
                &[("max", &MAX_TEXT_CHARS.to_string())],
            )?,
        );
    }
    let max_horizon = setting_i64(
        "max_horizon_seconds",
        server,
        Some(&msg.target),
        DEFAULT_MAX_HORIZON,
    )?;
    if parsed.seconds > max_horizon {
        return reply(
            server,
            &msg.target,
            &themed(
                "too_far",
                &["That is too far away, {user}; the current limit is {limit}."],
                &[
                    ("user", display_name(msg)),
                    ("limit", &human_duration(max_horizon)),
                ],
            )?,
        );
    }

    let owner_id = stable_id(&msg.user_id, &msg.nick);
    let jobs = list_jobs(server, &msg.target)?;
    let owner_count = jobs
        .iter()
        .filter_map(job_payload)
        .filter(|payload| payload.owner_id == owner_id)
        .count() as i64;
    let max_pending = setting_i64(
        "max_pending",
        server,
        Some(&msg.target),
        DEFAULT_MAX_PENDING,
    )?;
    if owner_count >= max_pending {
        return reply(
            server,
            &msg.target,
            &themed(
                "queue_full",
                &["You already have {max} reminders waiting in this channel, {user}."],
                &[
                    ("max", &max_pending.to_string()),
                    ("user", display_name(msg)),
                ],
            )?,
        );
    }

    let number = next_number(server, &owner_id)?;
    let payload = ReminderPayload {
        owner_id: owner_id.clone(),
        owner_display: sanitize(display_name(msg)),
        number,
        text: sanitize(&parsed.text),
    };
    let due_at = now.saturating_add(parsed.seconds);
    let request = ScheduleSet {
        id: job_id(server, &owner_id, number),
        server: server.into(),
        channel: msg.target.clone(),
        due_at,
        payload: serde_json::to_string(&payload)?,
    };
    if unsafe { schedule_set(serde_json::to_string(&request)?) }.is_err() {
        return reply(
            server,
            &msg.target,
            &themed(
                "service_error",
                &["I couldn't save that reminder right now, {user}."],
                &[("user", display_name(msg))],
            )?,
        );
    }
    reply(
        server,
        &msg.target,
        &themed(
            "scheduled",
            &["Reminder #{id} set for {when} from now, {user}."],
            &[
                ("id", &number.to_string()),
                ("when", &human_duration(parsed.seconds)),
                ("user", display_name(msg)),
            ],
        )?,
    )
}

fn list_reminders(server: &str, msg: &jeeves_abi::MessagePayload, now: i64) -> Result<(), Error> {
    let owner_id = stable_id(&msg.user_id, &msg.nick);
    let mut reminders = list_jobs(server, &msg.target)?
        .into_iter()
        .filter_map(|job| job_payload(&job).map(|payload| (job, payload)))
        .filter(|(_, payload)| payload.owner_id == owner_id)
        .collect::<Vec<_>>();
    reminders.sort_by_key(|(job, _)| job.due_at);
    if reminders.is_empty() {
        return reply(
            server,
            &msg.target,
            &themed(
                "none",
                &["You have no reminders waiting in this channel, {user}."],
                &[("user", display_name(msg))],
            )?,
        );
    }
    let count = reminders.len();
    reply(
        server,
        &msg.target,
        &themed(
            "list_header",
            &["You have {count} reminder(s) waiting here, {user}:"],
            &[("count", &count.to_string()), ("user", display_name(msg))],
        )?,
    )?;
    for (job, payload) in reminders.iter().take(5) {
        reply(
            server,
            &msg.target,
            &themed(
                "list_item",
                &["#{id} in {when}: {message}"],
                &[
                    ("id", &payload.number.to_string()),
                    ("when", &human_duration(job.due_at.saturating_sub(now))),
                    ("message", &payload.text),
                ],
            )?,
        )?;
    }
    if count > 5 {
        reply(
            server,
            &msg.target,
            &themed(
                "list_more",
                &["…and {count} more."],
                &[("count", &(count - 5).to_string())],
            )?,
        )?;
    }
    Ok(())
}

fn cancel_reminder(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    raw_id: &str,
) -> Result<(), Error> {
    let Some(number) = raw_id.trim_start_matches('#').parse::<u64>().ok() else {
        return usage(server, &msg.target, display_name(msg));
    };
    let owner_id = stable_id(&msg.user_id, &msg.nick);
    let found = list_jobs(server, &msg.target)?
        .into_iter()
        .filter_map(|job| job_payload(&job).map(|payload| (job, payload)))
        .find(|(_, payload)| payload.owner_id == owner_id && payload.number == number);
    let Some((job, _)) = found else {
        return reply(
            server,
            &msg.target,
            &themed(
                "not_found",
                &["I couldn't find reminder #{id} for you in this channel."],
                &[("id", &number.to_string()), ("user", display_name(msg))],
            )?,
        );
    };
    let cancelled =
        unsafe { schedule_cancel(serde_json::to_string(&ScheduleCancel { id: job.id })?)? }
            == "true";
    let key = if cancelled { "cancelled" } else { "not_found" };
    let defaults: &[&str] = if cancelled {
        &["Cancelled reminder #{id}, {user}."]
    } else {
        &["I couldn't find reminder #{id} for you in this channel."]
    };
    reply(
        server,
        &msg.target,
        &themed(
            key,
            defaults,
            &[("id", &number.to_string()), ("user", display_name(msg))],
        )?,
    )
}

fn list_jobs(server: &str, channel: &str) -> Result<Vec<ScheduledJob>, Error> {
    let raw = unsafe {
        schedule_list(serde_json::to_string(&ScheduleList {
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}

fn job_payload(job: &ScheduledJob) -> Option<ReminderPayload> {
    serde_json::from_str(&job.payload).ok()
}

fn next_number(server: &str, owner_id: &str) -> Result<u64, Error> {
    let key = format!("sequence:{server}:{owner_id}");
    let raw = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.clone() })?)? };
    let number = raw.parse::<u64>().unwrap_or(0).saturating_add(1).max(1);
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key,
            value: number.to_string(),
        })?)?
    };
    Ok(number)
}

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

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?
    };
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

fn usage(server: &str, target: &str, user: &str) -> Result<(), Error> {
    reply(
        server,
        target,
        &themed(
            "usage",
            &["Usage: !remind me in <duration> to <message>; !reminders; !remind cancel <id>."],
            &[("user", user)],
        )?,
    )
}

#[derive(Debug, PartialEq, Eq)]
enum ParseError {
    Usage,
    OtherTarget,
    BadDuration,
}

fn parse_new_reminder(arg: &str) -> Result<NewReminder, ParseError> {
    let arg = arg.trim();
    let lower = arg.to_ascii_lowercase();
    let rest = if lower.starts_with("me ") {
        &arg[3..]
    } else if lower.starts_with("in ") {
        arg
    } else if arg.is_empty() {
        return Err(ParseError::Usage);
    } else {
        return Err(ParseError::OtherTarget);
    };
    if !rest
        .get(..3)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("in "))
    {
        return Err(ParseError::Usage);
    }
    let rest = &rest[3..];
    let separator = rest
        .to_ascii_lowercase()
        .find(" to ")
        .ok_or(ParseError::Usage)?;
    let duration = &rest[..separator];
    let text = &rest[separator + 4..];
    let seconds = parse_duration(duration).ok_or(ParseError::BadDuration)?;
    let text = sanitize(text);
    if text.is_empty() {
        return Err(ParseError::Usage);
    }
    Ok(NewReminder { seconds, text })
}

fn parse_duration(input: &str) -> Option<i64> {
    let chars = input
        .trim()
        .to_ascii_lowercase()
        .chars()
        .collect::<Vec<_>>();
    if chars.is_empty() {
        return None;
    }
    let mut index = 0;
    let mut total = 0i64;
    let mut parts = 0;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let number_start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if number_start == index {
            return None;
        }
        let number = chars[number_start..index]
            .iter()
            .collect::<String>()
            .parse::<i64>()
            .ok()?;
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let unit_start = index;
        while index < chars.len() && chars[index].is_ascii_alphabetic() {
            index += 1;
        }
        let unit = chars[unit_start..index].iter().collect::<String>();
        let multiplier = match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 60 * 60,
            "d" | "day" | "days" => 24 * 60 * 60,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(multiplier)?)?;
        parts += 1;
    }
    (parts > 0 && total > 0).then_some(total)
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

fn stable_id(user_id: &str, nick: &str) -> String {
    if user_id.is_empty() {
        format!("nick:{}", nick.to_ascii_lowercase())
    } else {
        user_id.into()
    }
}

fn display_name(msg: &jeeves_abi::MessagePayload) -> &str {
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
    fn parses_natural_and_compact_durations() {
        assert_eq!(parse_duration("10 minutes"), Some(600));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("2 days 3 hours"), Some(183_600));
        assert_eq!(parse_duration("0 minutes"), None);
        assert_eq!(parse_duration("tomorrow"), None);
    }

    #[test]
    fn parses_self_reminder_syntax_and_rejects_other_targets() {
        assert_eq!(
            parse_new_reminder("me in 10 minutes to check the oven"),
            Ok(NewReminder {
                seconds: 600,
                text: "check the oven".into()
            })
        );
        assert_eq!(
            parse_new_reminder("in 1h30m to stretch").unwrap().seconds,
            5_400
        );
        assert_eq!(
            parse_new_reminder("ME IN 2 Hours TO stretch")
                .unwrap()
                .seconds,
            7_200
        );
        assert_eq!(
            parse_new_reminder("Alice in 1 hour to wave"),
            Err(ParseError::OtherTarget)
        );
    }

    #[test]
    fn formats_durations_and_sanitizes_messages() {
        assert_eq!(human_duration(5_400), "1 hour 30 minutes");
        assert_eq!(sanitize(" check\n\u{0003}04  logs "), "check04 logs");
    }
}
