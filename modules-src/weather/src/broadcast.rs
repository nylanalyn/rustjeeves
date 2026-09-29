//! Severe-weather broadcasts: `!weather alerts on` (admin) makes a channel watch the saved
//! locations of its current members, posting each new official warning once and a short line
//! when it ends. Members opt out with `!weather alerts me off`.
//!
//! Broadcasts name the warned area, never the person. The channel's record of posted warnings
//! keeps, per warning, only one-way hashes of the ~1 km cells that reported it, so it can tell
//! "the warning ended" (a watched cell stopped reporting it) from "the person left" (no watched
//! cell any more) without storing anyone's coordinates.

use super::*;
use jeeves_guest::timestamp;
use std::collections::{BTreeMap, BTreeSet};

const TICK_SECONDS: i64 = 10 * 60;
const MAX_MEMBERS: usize = 300;
const MAX_CELLS: usize = 40;
const MAX_POSTS_PER_TICK: usize = 4;
const MAX_TRACKED: usize = 100;
pub(crate) const LEVELS: [&str; 3] = ["yellow", "orange", "red"];

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct Posted {
    title: String,
    area: String,
    level: u8,
    expires: i64,
    /// Hashes of the cells that last reported this warning.
    cells: Vec<String>,
}

type PostedMap = BTreeMap<String, Posted>;

pub(crate) fn job_id(server: &str, channel: &str) -> String {
    format!(
        "alerts:{}:{}",
        encode(server),
        encode(&channel.to_ascii_lowercase())
    )
}

fn state_key(server: &str, channel: &str) -> String {
    format!(
        "alerts-posted:{}:{}",
        encode(server),
        encode(&channel.to_ascii_lowercase())
    )
}

pub(crate) fn optout_key(server: &str, profile_id: &str) -> String {
    format!("alerts-optout:{}:{}", encode(server), encode(profile_id))
}

/// FNV-1a over the rounded cell: stable, but not a stored coordinate.
fn cell_hash(lat: f64, lon: f64) -> String {
    let cell = format!("{:.2},{:.2}", lat, lon);
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in cell.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// "Tornado Warning", "Yellow rain warning", or "Orange warning: gale-force gusts".
pub(crate) fn alert_title(alert: &WeatherAlert) -> String {
    if alert.source != "MeteoAlarm" {
        return alert.event.clone();
    }
    let lower = alert.event.to_lowercase();
    if lower
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|word| matches!(word, "yellow" | "amber" | "orange" | "red"))
    {
        let mut chars = alert.event.chars();
        return chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
    }
    let colour = match alert.level {
        3 => "Red",
        2 => "Orange",
        1 => "Yellow",
        _ => "Weather",
    };
    format!("{colour} warning: {}", alert.event)
}

/// "23-7" → Some((23, 7)); empty or malformed → None (no quiet hours).
fn parse_quiet_hours(text: &str) -> Option<(u32, u32)> {
    let (start, end) = text.trim().split_once('-')?;
    let (start, end) = (start.trim().parse().ok()?, end.trim().parse().ok()?);
    (start < 24 && end < 24 && start != end).then_some((start, end))
}

fn in_quiet_hours(now: i64, quiet: Option<(u32, u32)>) -> bool {
    let Some((start, end)) = quiet else {
        return false;
    };
    let hour = (now.rem_euclid(86_400) / 3_600) as u32;
    if start < end {
        (start..end).contains(&hour)
    } else {
        hour >= start || hour < end
    }
}

fn threshold(server: &str, channel: &str) -> Result<u8, Error> {
    let value = setting(server, channel, "alert_level")?;
    Ok(LEVELS
        .iter()
        .position(|level| *level == value)
        .map_or(2, |index| index as u8 + 1))
}

fn setting(server: &str, channel: &str, key: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    })
}

fn opted_out(server: &str, profile_id: &str) -> Result<bool, Error> {
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: optout_key(server, profile_id),
        })?)?
    };
    Ok(raw == "1")
}

fn load_state(server: &str, channel: &str) -> Result<PostedMap, Error> {
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: state_key(server, channel),
        })?)?
    };
    Ok(if raw.trim().is_empty() {
        PostedMap::new()
    } else {
        serde_json::from_str(&raw)?
    })
}

fn save_state(server: &str, channel: &str, state: &PostedMap) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: state_key(server, channel),
            value: if state.is_empty() {
                String::new()
            } else {
                serde_json::to_string(state)?
            },
        })?)?
    };
    Ok(())
}

fn schedule_tick(server: &str, channel: &str, due_at: i64) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at,
            payload: "{}".into(),
        })?)?;
    }
    Ok(())
}

/// A warning seen this tick.
struct Current {
    alert: WeatherAlert,
    cells: Vec<String>,
    timezone: Option<String>,
}

/// What a tick should do, decided without side effects.
#[derive(Debug, Default, PartialEq)]
struct Plan {
    announce: Vec<String>,
    lift: Vec<String>,
    forget: Vec<String>,
}

fn plan(
    state: &PostedMap,
    current: &BTreeMap<String, Vec<String>>,
    checked: &BTreeSet<String>,
    incomplete: &BTreeSet<String>,
    min_level: u8,
    now: i64,
) -> Plan {
    let mut result = Plan {
        announce: current
            .keys()
            .filter(|key| !state.contains_key(*key))
            .cloned()
            .collect(),
        ..Plan::default()
    };
    for (key, posted) in state {
        if current.contains_key(key) {
            continue;
        }
        let watched = posted
            .cells
            .iter()
            .filter(|cell| checked.contains(*cell))
            .collect::<Vec<_>>();
        if posted.level < min_level || watched.is_empty() {
            // Below a raised threshold, or nobody there is watched any more: say nothing.
            result.forget.push(key.clone());
        } else if watched.iter().any(|cell| incomplete.contains(*cell)) {
            // A provider was unreachable for that area; the warning may still stand.
            if posted.expires > 0 && posted.expires < now - 6 * 3_600 {
                result.forget.push(key.clone());
            }
        } else {
            result.lift.push(key.clone());
        }
    }
    result
}

/// "until 22:00 CEST", "until Tue 06:00 BST", or "until further notice".
fn until(expires: i64, timezone: Option<&str>, now: i64) -> Result<String, Error> {
    if expires <= 0 {
        return Ok("until further notice".into());
    }
    let zone = timezone.unwrap_or("UTC");
    let at = |unix: i64| {
        get_local_time(LocalTimeQuery {
            timezone: zone.into(),
            unix_seconds: Some(unix),
            local: None,
        })
    };
    let (Some(end), Some(today)) = (at(expires)?, at(now)?) else {
        return Ok("until further notice".into());
    };
    let day = if (end.year, end.month, end.day) == (today.year, today.month, today.day) {
        String::new()
    } else {
        format!("{} ", end.weekday.chars().take(3).collect::<String>())
    };
    Ok(format!(
        "until {day}{:02}:{:02} {}",
        end.hour_24, end.minute, end.abbreviation
    ))
}

pub(crate) fn tick(server: &str, channel: &str) -> Result<(), Error> {
    let now = timestamp()?;
    // Keep ticking whatever happens below; a failed tick must not end the broadcasts.
    schedule_tick(server, channel, now + TICK_SECONDS)?;
    let quiet = parse_quiet_hours(&setting(server, channel, "alert_quiet_hours")?);
    if in_quiet_hours(now, quiet) {
        return Ok(());
    }
    let min_level = threshold(server, channel)?;
    let members: Vec<String> = serde_json::from_str(&unsafe {
        channel_members(serde_json::to_string(&jeeves_abi::Channel {
            server: server.into(),
            channel: channel.into(),
        })?)?
    })?;
    // cell hash → (lat, lon, a timezone there)
    let mut cells = BTreeMap::<String, (f64, f64, Option<String>)>::new();
    for nick in members.iter().take(MAX_MEMBERS) {
        let Some(profile) = get_profile(server, nick)? else {
            continue;
        };
        let (Some(lat), Some(lon)) = (profile.lat, profile.lon) else {
            continue;
        };
        if cells.len() >= MAX_CELLS || opted_out(server, &profile.id)? {
            continue;
        }
        cells
            .entry(cell_hash(lat, lon))
            .or_insert((lat, lon, profile.timezone.clone()));
    }
    let checked = cells.keys().cloned().collect::<BTreeSet<_>>();
    let mut incomplete = BTreeSet::new();
    let mut current = BTreeMap::<String, Current>::new();
    for (hash, (lat, lon, timezone)) in &cells {
        let result = get_weather_alerts(*lat, *lon)?;
        if result.incomplete {
            incomplete.insert(hash.clone());
        }
        for alert in result.alerts {
            if alert.level < min_level || (alert.expires > 0 && alert.expires <= now) {
                continue;
            }
            let entry = current.entry(alert.key.clone()).or_insert_with(|| Current {
                alert: alert.clone(),
                cells: Vec::new(),
                timezone: timezone.clone(),
            });
            entry.cells.push(hash.clone());
        }
    }
    let mut state = load_state(server, channel)?;
    let decisions = plan(
        &state,
        &current
            .iter()
            .map(|(key, seen)| (key.clone(), seen.cells.clone()))
            .collect(),
        &checked,
        &incomplete,
        min_level,
        now,
    );
    // Most severe first, so a busy day leads with what matters.
    let mut announce = decisions
        .announce
        .iter()
        .filter_map(|key| current.get(key).map(|seen| (key, seen)))
        .collect::<Vec<_>>();
    announce.sort_by_key(|(_, seen)| std::cmp::Reverse(seen.alert.level));
    for (index, (key, seen)) in announce.iter().enumerate() {
        let title = alert_title(&seen.alert);
        if index < MAX_POSTS_PER_TICK {
            let text = themed(
                "weather.alert_broadcast",
                &["⚠ {title}: {area}, {until}."],
                &[
                    ("title", &title),
                    ("area", &seen.alert.area),
                    (
                        "until",
                        &until(seen.alert.expires, seen.timezone.as_deref(), now)?,
                    ),
                ],
            )?;
            reply(server, channel, &text)?;
        }
        state.insert(
            (*key).clone(),
            Posted {
                title,
                area: seen.alert.area.clone(),
                level: seen.alert.level,
                expires: seen.alert.expires,
                cells: seen.cells.clone(),
            },
        );
    }
    if announce.len() > MAX_POSTS_PER_TICK {
        let more = (announce.len() - MAX_POSTS_PER_TICK).to_string();
        reply(
            server,
            channel,
            &themed(
                "weather.alert_more",
                &["…and {count} more warning(s) in effect for people here."],
                &[("count", &more)],
            )?,
        )?;
    }
    for key in &decisions.lift {
        if let Some(posted) = state.remove(key) {
            reply(
                server,
                channel,
                &themed(
                    "weather.alert_lifted",
                    &["✓ {title} for {area} has ended."],
                    &[("title", &posted.title), ("area", &posted.area)],
                )?,
            )?;
        }
    }
    for key in &decisions.forget {
        state.remove(key);
    }
    // Refresh what's still in force: cells and expiry may move with updates.
    for (key, seen) in &current {
        if let Some(posted) = state.get_mut(key) {
            posted.cells = seen.cells.clone();
            posted.expires = seen.alert.expires;
        }
    }
    while state.len() > MAX_TRACKED {
        let oldest = state
            .iter()
            .min_by_key(|(_, posted)| posted.expires)
            .map(|(key, _)| key.clone());
        match oldest {
            Some(key) => state.remove(&key),
            None => break,
        };
    }
    save_state(server, channel, &state)
}

pub(crate) fn command(
    server: &str,
    msg: &MessagePayload,
    addr: &str,
    rest: &str,
) -> Result<(), Error> {
    let dest = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let say = |key: &str, default: &str, vars: &[(&str, &str)]| -> Result<(), Error> {
        let mut vars = vars.to_vec();
        vars.push(("user", addr));
        reply(server, dest, &themed(key, &[default], &vars)?)
    };
    let words = rest.split_whitespace().collect::<Vec<_>>();
    match words.as_slice() {
        ["me", choice @ ("on" | "off")] => {
            if msg.user_id.is_empty() {
                return say(
                    "weather.identity_unavailable",
                    "I can't verify your profile right now, {user}; please try again shortly.",
                    &[],
                );
            }
            unsafe {
                kv_set(serde_json::to_string(&KvSet {
                    key: optout_key(server, &msg.user_id),
                    value: if *choice == "off" { "1".into() } else { String::new() },
                })?)?
            };
            if *choice == "off" {
                say(
                    "weather.alerts_me_off",
                    "Very good, {user}; your location won't be watched for channel weather warnings.",
                    &[],
                )
            } else {
                say(
                    "weather.alerts_me_on",
                    "Very good, {user}; channels with weather warnings on will watch your saved location again.",
                    &[],
                )
            }
        }
        _ if msg.is_private => say(
            "weather.alerts_channel_only",
            "Weather warnings are switched on in a channel, {user}. Here you can use !weather alerts me on|off.",
            &[],
        ),
        [choice @ ("on" | "off")] => {
            if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
                return say(
                    "weather.alerts_admin_only",
                    "Only an admin may switch channel weather warnings, {user}.",
                    &[],
                );
            }
            if *choice == "off" {
                unsafe {
                    schedule_cancel(serde_json::to_string(&ScheduleCancel {
                        id: job_id(server, &msg.target),
                    })?)?;
                }
                save_state(server, &msg.target, &PostedMap::new())?;
                return say(
                    "weather.alerts_off",
                    "Very good, {user}; no more weather warnings here.",
                    &[],
                );
            }
            schedule_tick(server, &msg.target, timestamp()? + 5)?;
            let level = LEVELS[(threshold(server, &msg.target)? - 1) as usize];
            say(
                "weather.alerts_on",
                "Very good, {user}: I'll post official {level}-or-worse weather warnings covering people here, by area only. Anyone can opt out with !weather alerts me off.",
                &[("level", level)],
            )
        }
        [] => {
            let raw = unsafe {
                schedule_list(serde_json::to_string(&ScheduleList {
                    server: Some(server.into()),
                    channel: None,
                })?)?
            };
            let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
            let on = jobs.iter().any(|job| job.id == job_id(server, &msg.target));
            let personal = if msg.user_id.is_empty() || !opted_out(server, &msg.user_id)? {
                "watched"
            } else {
                "not watched (opted out)"
            };
            let level = LEVELS[(threshold(server, &msg.target)? - 1) as usize];
            if on {
                say(
                    "weather.alerts_status_on",
                    "Weather warnings ({level} or worse) are on here; your location is {personal}, {user}.",
                    &[("level", level), ("personal", personal)],
                )
            } else {
                say(
                    "weather.alerts_status_off",
                    "Weather warnings are off here, {user}. An admin can start them with !weather alerts on.",
                    &[],
                )
            }
        }
        _ => say(
            "weather.alerts_usage",
            "Use !weather alerts [on|off] (admins) or !weather alerts me on|off, {user}.",
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(event: &str, level: u8, source: &str) -> WeatherAlert {
        WeatherAlert {
            event: event.into(),
            level,
            source: source.into(),
            ..WeatherAlert::default()
        }
    }

    #[test]
    fn titles_read_naturally_for_each_source() {
        assert_eq!(
            alert_title(&alert("Tornado Warning", 3, "NWS")),
            "Tornado Warning"
        );
        assert_eq!(
            alert_title(&alert("Yellow rain warning", 1, "MeteoAlarm")),
            "Yellow rain warning"
        );
        assert_eq!(
            alert_title(&alert("gale-force gusts", 2, "MeteoAlarm")),
            "Orange warning: gale-force gusts"
        );
        assert_eq!(
            alert_title(&alert("orange warning for wind", 2, "MeteoAlarm")),
            "Orange warning for wind"
        );
        assert_eq!(
            alert_title(&alert("scattered thunderstorms", 1, "MeteoAlarm")),
            "Yellow warning: scattered thunderstorms"
        );
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let at = |hour: i64| hour * 3_600;
        let quiet = parse_quiet_hours("23-7");
        assert_eq!(quiet, Some((23, 7)));
        assert!(in_quiet_hours(at(23), quiet));
        assert!(in_quiet_hours(at(3), quiet));
        assert!(!in_quiet_hours(at(7), quiet));
        assert!(!in_quiet_hours(at(12), quiet));
        assert!(in_quiet_hours(at(13), parse_quiet_hours("12-14")));
        assert_eq!(parse_quiet_hours(""), None);
        assert_eq!(parse_quiet_hours("5-5"), None);
        assert_eq!(parse_quiet_hours("25-3"), None);
    }

    #[test]
    fn cells_hash_stably_without_storing_coordinates() {
        assert_eq!(cell_hash(50.551, 9.679), cell_hash(50.549, 9.681));
        assert_ne!(cell_hash(50.55, 9.68), cell_hash(50.56, 9.68));
        assert!(!cell_hash(50.55, 9.68).contains("50"));
    }

    #[test]
    fn plans_announce_new_lift_ended_and_forget_departed() {
        let posted = |level: u8, cells: &[&str]| Posted {
            level,
            expires: 1_000,
            cells: cells.iter().map(|cell| cell.to_string()).collect(),
            ..Posted::default()
        };
        let state = PostedMap::from([
            ("still".to_string(), posted(2, &["a"])),
            ("ended".to_string(), posted(2, &["a"])),
            ("departed".to_string(), posted(2, &["gone"])),
            ("unsure".to_string(), posted(2, &["b"])),
            ("demoted".to_string(), posted(1, &["a"])),
        ]);
        let current = BTreeMap::from([
            ("still".to_string(), vec!["a".to_string()]),
            ("fresh".to_string(), vec!["a".to_string()]),
        ]);
        let checked = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let incomplete = BTreeSet::from(["b".to_string()]);
        assert_eq!(
            plan(&state, &current, &checked, &incomplete, 2, 500),
            Plan {
                announce: vec!["fresh".into()],
                lift: vec!["ended".into()],
                forget: vec!["demoted".into(), "departed".into()],
            }
        );
    }
}
