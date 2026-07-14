//! Channel moderation commands backed by the host's narrow operator capability.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, Category, ChannelOperator, ChannelOperatorAction, ChannelOperatorMode,
    CommandManifest, CommandSpec, Event, EventEnvelope, Level, LogReq, Role, ScheduleCancel,
    ScheduleSet, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const MAX_BAN_SECONDS: i64 = 30 * 24 * 60 * 60;
const MAX_TOPIC_BYTES: usize = 300;
const MAX_REASON_BYTES: usize = 200;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn log(input: String) -> String;
    fn now(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn channel_operator(input: String) -> String;
}

#[derive(Debug, Serialize, Deserialize)]
struct TimedBan {
    target: String,
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: Vec::new(),
        achievements: Vec::new(),
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            command(
                "ban",
                "Temporarily ban a nick or mask (admin, channel only).",
                "!ban <nick|mask> <duration>",
            ),
            command(
                "unban",
                "Remove a ban placed by !ban (admin, channel only).",
                "!unban <nick|mask>",
            ),
            command(
                "kick",
                "Kick a nick from this channel (admin, channel only).",
                "!kick <nick> [reason]",
            ),
            command(
                "op",
                "Grant channel operator status (admin, channel only).",
                "!op <nick>",
            ),
            command(
                "deop",
                "Remove channel operator status (admin, channel only).",
                "!deop <nick>",
            ),
            command(
                "hop",
                "Grant channel half-operator status (admin, channel only).",
                "!hop <nick>",
            ),
            command(
                "dehop",
                "Remove channel half-operator status (admin, channel only).",
                "!dehop <nick>",
            ),
            command(
                "voice",
                "Grant channel voice status (admin, channel only).",
                "!voice <nick>",
            ),
            command(
                "devoice",
                "Remove channel voice status (admin, channel only).",
                "!devoice <nick>",
            ),
            command(
                "topic",
                "Set this channel's topic (admin, channel only).",
                "!topic <text>",
            ),
        ],
    })?)
}

#[plugin_fn]
pub fn init() -> FnResult<()> {
    command_log("operator module loaded")?;
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, argument) = split_command(text);
    if !is_operator_command(command) {
        return Ok(());
    }
    let user = display_name(&msg);
    if msg.is_private {
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "operator.channel_only",
                &["Operator commands must be used in the channel they affect, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
        command_log(&format!(
            "[{}] DENIED {} -> {}",
            env.server, msg.nick, command
        ))?;
        reply(
            &env.server,
            &msg.target,
            &themed(
                "operator.denied",
                &["I'm afraid I can't allow that, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    match command {
        "!ban" => ban(&env.server, &msg.target, argument, user)?,
        "!unban" => unban(&env.server, &msg.target, argument, user)?,
        "!kick" => kick(&env.server, &msg.target, argument, user)?,
        "!topic" => topic(&env.server, &msg.target, argument, user)?,
        "!op" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Op,
            true,
            command,
            user,
        )?,
        "!deop" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Op,
            false,
            command,
            user,
        )?,
        "!hop" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Halfop,
            true,
            command,
            user,
        )?,
        "!dehop" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Halfop,
            false,
            command,
            user,
        )?,
        "!voice" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Voice,
            true,
            command,
            user,
        )?,
        "!devoice" => mode(
            &env.server,
            &msg.target,
            argument,
            ChannelOperatorMode::Voice,
            false,
            command,
            user,
        )?,
        _ => {}
    };
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
    let timed: TimedBan = serde_json::from_str(&payload)?;
    operator_action(
        &env.server,
        &channel,
        ChannelOperatorAction::Mode {
            mode: ChannelOperatorMode::Ban,
            adding: false,
            target: timed.target.clone(),
        },
    )?;
    command_log(&format!(
        "[{}] expired ban {} in {}",
        env.server, timed.target, channel
    ))?;
    Ok(())
}

fn ban(server: &str, channel: &str, argument: &str, user: &str) -> Result<(), Error> {
    let Some((raw_target, raw_duration)) = argument.split_once(char::is_whitespace) else {
        return usage(server, channel, "ban", user);
    };
    let Some(target) = ban_target(raw_target) else {
        return usage(server, channel, "ban", user);
    };
    let Some(seconds) = parse_duration(raw_duration.trim()) else {
        return usage(server, channel, "ban", user);
    };
    if seconds > MAX_BAN_SECONDS {
        return reply(
            server,
            channel,
            &themed(
                "operator.ban_too_long",
                &["Timed bans may not exceed 30 days, {user}."],
                &[("user", user)],
            )?,
        );
    }
    let due_at = timestamp()?.saturating_add(seconds);
    let id = ban_job_id(channel, &target);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id,
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at,
            payload: serde_json::to_string(&TimedBan {
                target: target.clone(),
            })?,
        })?)?;
    }
    operator_action(
        server,
        channel,
        ChannelOperatorAction::Mode {
            mode: ChannelOperatorMode::Ban,
            adding: true,
            target: target.clone(),
        },
    )?;
    command_log(&format!(
        "[{server}] {user} banned {target} in {channel} for {seconds}s"
    ))?;
    reply(
        server,
        channel,
        &themed(
            "operator.ban_requested",
            &["Ban for {target} requested for {duration}, {user}."],
            &[
                ("target", &target),
                ("duration", &human_duration(seconds)),
                ("user", user),
            ],
        )?,
    )
}

fn unban(server: &str, channel: &str, argument: &str, user: &str) -> Result<(), Error> {
    let Some(target) = ban_target(argument) else {
        return usage(server, channel, "unban", user);
    };
    operator_action(
        server,
        channel,
        ChannelOperatorAction::Mode {
            mode: ChannelOperatorMode::Ban,
            adding: false,
            target: target.clone(),
        },
    )?;
    unsafe {
        schedule_cancel(serde_json::to_string(&ScheduleCancel {
            id: ban_job_id(channel, &target),
        })?)?;
    }
    command_log(&format!("[{server}] {user} unbanned {target} in {channel}"))?;
    reply(
        server,
        channel,
        &themed(
            "operator.unban_requested",
            &["Unban for {target} requested, {user}."],
            &[("target", &target), ("user", user)],
        )?,
    )
}

fn kick(server: &str, channel: &str, argument: &str, user: &str) -> Result<(), Error> {
    let (nick, reason) = argument
        .split_once(char::is_whitespace)
        .unwrap_or((argument, "Requested by an operator"));
    if !valid_token(nick) || !valid_text(reason, MAX_REASON_BYTES) {
        return usage(server, channel, "kick", user);
    }
    operator_action(
        server,
        channel,
        ChannelOperatorAction::Kick {
            nick: nick.into(),
            reason: reason.trim().into(),
        },
    )?;
    command_log(&format!("[{server}] {user} kicked {nick} from {channel}"))?;
    reply(
        server,
        channel,
        &themed(
            "operator.kick_requested",
            &["Kick for {target} requested, {user}."],
            &[("target", nick), ("user", user)],
        )?,
    )
}

fn topic(server: &str, channel: &str, argument: &str, user: &str) -> Result<(), Error> {
    if !valid_text(argument, MAX_TOPIC_BYTES) || argument.is_empty() {
        return usage(server, channel, "topic", user);
    }
    operator_action(
        server,
        channel,
        ChannelOperatorAction::Topic {
            topic: argument.into(),
        },
    )?;
    command_log(&format!("[{server}] {user} changed topic in {channel}"))?;
    reply(
        server,
        channel,
        &themed(
            "operator.topic_requested",
            &["Topic update requested, {user}."],
            &[("user", user)],
        )?,
    )
}

fn mode(
    server: &str,
    channel: &str,
    argument: &str,
    kind: ChannelOperatorMode,
    adding: bool,
    command: &str,
    user: &str,
) -> Result<(), Error> {
    if !valid_token(argument) {
        return usage(server, channel, command.trim_start_matches('!'), user);
    }
    operator_action(
        server,
        channel,
        ChannelOperatorAction::Mode {
            mode: kind,
            adding,
            target: argument.into(),
        },
    )?;
    command_log(&format!(
        "[{server}] {user} ran {command} {argument} in {channel}"
    ))?;
    reply(
        server,
        channel,
        &themed(
            "operator.mode_requested",
            &["Mode change for {target} requested, {user}."],
            &[("target", argument), ("user", user)],
        )?,
    )
}

fn operator_action(
    server: &str,
    channel: &str,
    action: ChannelOperatorAction,
) -> Result<(), Error> {
    unsafe {
        channel_operator(serde_json::to_string(&ChannelOperator {
            server: server.into(),
            channel: channel.into(),
            action,
        })?)?;
    }
    Ok(())
}

fn usage(server: &str, channel: &str, command: &str, user: &str) -> Result<(), Error> {
    let usage = match command {
        "ban" => "!ban <nick|mask> <duration>",
        "unban" => "!unban <nick|mask>",
        "kick" => "!kick <nick> [reason]",
        "topic" => "!topic <text>",
        other => {
            return reply(
                server,
                channel,
                &themed(
                    "operator.usage",
                    &["Usage: {usage}"],
                    &[("usage", &format!("!{other} <nick>")), ("user", user)],
                )?,
            )
        }
    };
    reply(
        server,
        channel,
        &themed(
            "operator.usage",
            &["Usage: {usage}"],
            &[("usage", usage), ("user", user)],
        )?,
    )
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|s| (*s).into()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect(),
        })?)?
    })
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&jeeves_abi::SendMessage {
            server: server.into(),
            target: target.into(),
            text: text.into(),
        })?)?;
    }
    Ok(())
}

fn command_log(message: &str) -> Result<(), Error> {
    unsafe {
        log(serde_json::to_string(&LogReq {
            level: Level::Info,
            category: Category::Command,
            message: message.into(),
        })?)?;
    }
    Ok(())
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse()?)
}
fn display_name(msg: &jeeves_abi::MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}
fn split_command(text: &str) -> (&str, &str) {
    text.split_once(char::is_whitespace)
        .map(|(cmd, rest)| (cmd, rest.trim()))
        .unwrap_or((text, ""))
}
fn is_operator_command(command: &str) -> bool {
    matches!(
        command,
        "!ban"
            | "!unban"
            | "!kick"
            | "!topic"
            | "!op"
            | "!deop"
            | "!hop"
            | "!dehop"
            | "!voice"
            | "!devoice"
    )
}
fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && value
            .chars()
            .all(|ch| !ch.is_control() && !ch.is_whitespace())
}
fn valid_text(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .chars()
            .all(|ch| ch != '\r' && ch != '\n' && !ch.is_control())
}
fn ban_target(value: &str) -> Option<String> {
    valid_token(value).then(|| {
        if value.contains(['!', '@', '*', '?']) {
            value.into()
        } else {
            format!("{value}!*@*")
        }
    })
}
fn ban_job_id(channel: &str, target: &str) -> String {
    format!("ban:{channel}:{target}")
}

fn parse_duration(input: &str) -> Option<i64> {
    let chars = input
        .trim()
        .to_ascii_lowercase()
        .chars()
        .collect::<Vec<_>>();
    let mut index = 0;
    let mut total = 0i64;
    while index < chars.len() {
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return None;
        }
        let number = chars[start..index]
            .iter()
            .collect::<String>()
            .parse::<i64>()
            .ok()?;
        while index < chars.len() && chars[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_alphabetic() {
            index += 1;
        }
        let unit = chars[start..index].iter().collect::<String>();
        let multiplier = match unit.as_str() {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600,
            "d" | "day" | "days" => 86_400,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(multiplier)?)?;
    }
    (total >= 60).then_some(total)
}

fn human_duration(seconds: i64) -> String {
    if seconds % 86_400 == 0 {
        format!("{} day(s)", seconds / 86_400)
    } else if seconds % 3_600 == 0 {
        format!("{} hour(s)", seconds / 3_600)
    } else if seconds % 60 == 0 {
        format!("{} minute(s)", seconds / 60)
    } else {
        format!("{seconds} seconds")
    }
}

#[cfg(test)]
mod tests {
    use super::{ban_target, parse_duration};

    #[test]
    fn parses_compound_durations() {
        assert_eq!(parse_duration("1 hour"), Some(3_600));
        assert_eq!(parse_duration("1h30m"), Some(5_400));
        assert_eq!(parse_duration("45 seconds"), None);
    }

    #[test]
    fn turns_a_nick_into_a_bounded_ban_mask() {
        assert_eq!(ban_target("trouble"), Some("trouble!*@*".into()));
        assert_eq!(
            ban_target("*!*@example.test"),
            Some("*!*@example.test".into())
        );
        assert_eq!(ban_target("bad nick"), None);
    }
}
