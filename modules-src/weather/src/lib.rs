//! Weather module for rustjeeves.
//!
//! - `!weather [place|nick]` — current conditions (wind direction and gusts, UV when it matters,
//!   optional AQI) plus significant US alerts from the NWS. With no argument it uses the caller's
//!   saved location; a nick uses that person's; anything else is geocoded, and the reply names
//!   the place actually found.
//! - `!forecast [place|nick]` — a compact three-day forecast with sunrise and sunset.
//! - `!weather units metric|imperial|both`, `!weather aqi on|off` — per-person preferences.
//! - `!weather daily <HH:MM|off>` — a morning forecast by private message, scheduled in the
//!   person's own timezone and owned by their profile (erasure cancels it).

mod format;

use extism_pdk::*;
use format::{wmo_text, Units};
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, GeoQuery, GeoResult, KvGet, KvSet, LocalTimeQuery,
    LocalTimeResult, LocalWallTime, MessagePayload, ModuleDataDeletePlan, ModuleDataRequest,
    ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, ScheduleCancel, ScheduleList,
    ScheduleSet, ScheduledJob, SendMessage, StatIncrement, ThemeReq, WeatherAlert,
    WeatherAlertsResult, WeatherQuery, WeatherResult, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
};
use serde::{Deserialize, Serialize};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn geocode(input: String) -> String;
    fn weather(input: String) -> String;
    fn weather_alerts(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn local_time(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("weather_eye", "A Weather Eye", 1),
        ("prepared_anything", "Prepared for Anything", 25),
        ("resident_meteorologist", "Resident Meteorologist", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Complete {threshold} successful weather lookups."),
        stat: "lookups".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "weather_ducks".into(),
        name: "Lovely Weather for Ducks".into(),
        description: "Check the weather during severe rain or a storm.".into(),
        stat: "severe_weather".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "lookups".into(),
                description: "Successful weather lookups".into(),
            },
            AchievementStat {
                id: "severe_weather".into(),
                description: "Severe rain or storm lookups".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display_name: &str,
    target: &str,
    severe: bool,
) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    let mut increments = vec![StatIncrement {
        stat: "lookups".into(),
        amount: 1,
    }];
    if severe {
        increments.push(StatIncrement {
            stat: "severe_weather".into(),
            amount: 1,
        });
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
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
                name: "weather".into(),
                aliases: vec!["w".into()],
                description:
                    "Current weather for you, a person, or a place, with optional AQI and \
                              US alerts; set units, AQI, or a daily morning forecast by PM."
                        .into(),
                usage: "!weather [place|nick] | !weather units <metric|imperial|both> | \
                        !weather aqi <on|off> | !weather daily <HH:MM|off>"
                    .into(),
                ..Default::default()
            },
            CommandSpec {
                name: "forecast".into(),
                aliases: vec!["fc".into()],
                description: "A compact three-day forecast with sunrise and sunset.".into(),
                usage: "!forecast [place|nick]".into(),
                ..Default::default()
            },
        ],
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.into(),
        target: target.into(),
        text: text.into(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let key = ProfileKey {
        server: server.into(),
        nick: nick.into(),
    };
    let out = unsafe { profile_get(serde_json::to_string(&key)?)? };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn do_geocode(query: &str) -> Result<Option<GeoResult>, Error> {
    let out = unsafe {
        geocode(serde_json::to_string(&GeoQuery {
            query: query.into(),
        })?)?
    };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn get_weather(lat: f64, lon: f64) -> Result<Option<WeatherResult>, Error> {
    let out = unsafe { weather(serde_json::to_string(&WeatherQuery { lat, lon })?)? };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn get_weather_alerts(lat: f64, lon: f64) -> Result<WeatherAlertsResult, Error> {
    let req = serde_json::to_string(&WeatherQuery { lat, lon })?;
    let raw = unsafe { weather_alerts(req)? };
    if raw.is_empty() {
        return Ok(WeatherAlertsResult::default());
    }
    Ok(serde_json::from_str(&raw)?)
}

fn get_local_time(query: LocalTimeQuery) -> Result<Option<LocalTimeResult>, Error> {
    let out = unsafe { local_time(serde_json::to_string(&query)?)? };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn geo_label(g: &GeoResult) -> String {
    let mut parts = vec![g.name.clone()];
    if let Some(admin1) = &g.admin1 {
        parts.push(admin1.clone());
    }
    if let Some(country) = &g.country {
        parts.push(country.clone());
    }
    parts.join(", ")
}

// ── preferences ─────────────────────────────────────────────────────────────

fn units_key(server: &str, profile_id: &str) -> String {
    format!("units:{}:{}", encode(server), encode(profile_id))
}

fn daily_job_id(server: &str, profile_id: &str) -> String {
    format!("daily:{}:{}", encode(server), encode(profile_id))
}

fn units_for(server: &str, profile_id: &str) -> Result<Units, Error> {
    if profile_id.is_empty() {
        return Ok(Units::Both);
    }
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: units_key(server, profile_id),
        })?)?
    };
    Ok(Units::parse(&raw).unwrap_or(Units::Both))
}

// ── locations ───────────────────────────────────────────────────────────────

struct Spot {
    label: String,
    lat: f64,
    lon: f64,
}

enum Lookup {
    Found(Spot),
    /// A known person with no saved location (the caller, when `own` is true).
    NoLocation {
        who: String,
        own: bool,
    },
    NotFound(String),
}

fn profile_spot(profile: &Profile) -> Option<Spot> {
    Some(Spot {
        label: profile
            .location_label
            .clone()
            .or_else(|| profile.location_display.clone())
            .unwrap_or_else(|| "your location".into()),
        lat: profile.lat?,
        lon: profile.lon?,
    })
}

fn resolve(server: &str, msg: &MessagePayload, arg: &str) -> Result<Lookup, Error> {
    if arg.is_empty() {
        return Ok(
            match get_profile(server, &msg.nick)?
                .as_ref()
                .and_then(profile_spot)
            {
                Some(spot) => Lookup::Found(spot),
                None => Lookup::NoLocation {
                    who: msg.nick.clone(),
                    own: true,
                },
            },
        );
    }
    // A nick with a saved location wins; a nick without one (someone called "paris") falls
    // through to treating the argument as a place.
    let profile = get_profile(server, arg)?;
    if let Some(spot) = profile.as_ref().and_then(profile_spot) {
        return Ok(Lookup::Found(spot));
    }
    if let Some(geo) = do_geocode(arg)? {
        // Name what the geocoder actually found, so "paris" → Paris, Texas is visible.
        return Ok(Lookup::Found(Spot {
            label: geo_label(&geo),
            lat: geo.lat,
            lon: geo.lon,
        }));
    }
    Ok(match profile {
        Some(profile) => Lookup::NoLocation {
            who: profile.nick,
            own: false,
        },
        None => Lookup::NotFound(arg.into()),
    })
}

fn explain_miss(server: &str, dest: &str, addr: &str, lookup: &Lookup) -> Result<(), Error> {
    let text = match lookup {
        Lookup::NoLocation { own: true, .. } => themed(
            "weather_noloc",
            &["Set your location first, {user}: !location <place>."],
            &[("user", addr)],
        )?,
        Lookup::NoLocation { who, .. } => themed(
            "weather.user_noloc",
            &["{target} hasn't saved a location, {user}."],
            &[("target", who), ("user", addr)],
        )?,
        Lookup::NotFound(query) => themed(
            "weather_notfound",
            &["I couldn't find '{query}', {user}."],
            &[("user", addr), ("query", query)],
        )?,
        Lookup::Found(_) => return Ok(()),
    };
    reply(server, dest, &text)
}

// ── reports ─────────────────────────────────────────────────────────────────

/// The current-conditions line, built from themed parts so unit preferences and optional extras
/// don't each need a whole-line template.
fn current_report(
    location: &str,
    w: &WeatherResult,
    units: Units,
    show_aqi: bool,
) -> Result<String, Error> {
    let mut text = themed(
        "weather.now",
        &["Weather for {location}: {desc}, {temp} (feels {feels}), humidity {humidity}%, wind {wind}."],
        &[
            ("location", location),
            ("desc", wmo_text(w.code)),
            ("temp", &format::temperature(w.temp_c, units)),
            ("feels", &format::temperature(w.apparent_c, units)),
            ("humidity", &format!("{:.0}", w.humidity)),
            (
                "wind",
                &format::wind(w.wind_kmh, w.wind_direction_deg, w.gusts_kmh, units),
            ),
        ],
    )?;
    if let Some(mm) = w.forecast_rain_mm.filter(|mm| *mm >= 0.1) {
        text.push_str(&themed(
            "weather.rain_today",
            &[" Rain today: {rain}."],
            &[("rain", &format::rain(mm, units))],
        )?);
    }
    if let Some(uv) = w.uv_index.filter(|uv| w.is_day && *uv >= 3.0) {
        text.push_str(&themed(
            "weather.uv",
            &[" UV {uv}."],
            &[("uv", &format!("{uv:.0}"))],
        )?);
    }
    if let Some(aqi) = w.us_aqi.filter(|_| show_aqi) {
        text.push_str(&themed(
            "weather.aqi",
            &[" Air quality: {aqi} US AQI ({aqi_category})."],
            &[
                ("aqi", &format!("{aqi:.0}")),
                ("aqi_category", aqi_category(aqi)),
            ],
        )?);
    }
    Ok(text)
}

fn forecast_report(location: &str, w: &WeatherResult, units: Units) -> Result<String, Error> {
    let days = w
        .daily
        .iter()
        .map(|day| format::forecast_day(day, units))
        .collect::<Vec<_>>()
        .join(" · ");
    let today = w.daily.first();
    let sun = match (
        today.and_then(|day| day.sunrise.as_deref()),
        today.and_then(|day| day.sunset.as_deref()),
    ) {
        (Some(rise), Some(set)) => format!(" · ☀ {rise}–{set}"),
        _ => String::new(),
    };
    themed(
        "weather.forecast",
        &["Forecast for {location}: {days}{sun}"],
        &[("location", location), ("days", &days), ("sun", &sun)],
    )
}

fn send_alerts(server: &str, dest: &str, location: &str, lat: f64, lon: f64) -> Result<(), Error> {
    let alert_events = significant_alert_events(&get_weather_alerts(lat, lon)?.alerts);
    if alert_events.is_empty() {
        return Ok(());
    }
    reply(
        server,
        dest,
        &themed(
            "weather.alerts",
            &["⚠ NWS alerts for {location}: {alerts}."],
            &[
                ("location", location),
                ("alerts", &format_alert_events(&alert_events)),
            ],
        )?,
    )
}

// ── daily morning forecast ──────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct DailyPlan {
    /// Nick when the forecast was set; resolved through profile aliases at delivery.
    nick: String,
    hour: u32,
    minute: u32,
}

/// "8", "8am", "08:00", "7:30pm" → (hour, minute).
fn parse_daily_time(text: &str) -> Option<(u32, u32)> {
    let lower = text.trim().to_ascii_lowercase();
    let (body, pm) = if let Some(body) = lower.strip_suffix("pm") {
        (body.trim(), Some(true))
    } else if let Some(body) = lower.strip_suffix("am") {
        (body.trim(), Some(false))
    } else {
        (lower.as_str(), None)
    };
    let (hour, minute) = match body.split_once(':') {
        Some((hour, minute)) if minute.len() == 2 => (hour.parse().ok()?, minute.parse().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
        _ => return None,
    };
    let hour = match pm {
        Some(pm) if (1..=12).contains(&hour) => match (hour, pm) {
            (12, false) => 0,
            (12, true) => 12,
            (hour, true) => hour + 12,
            (hour, false) => hour,
        },
        Some(_) => return None,
        None => hour,
    };
    (hour <= 23 && minute <= 59).then_some((hour, minute))
}

/// The next instant at `hour:minute` in `timezone`, strictly after now.
fn next_delivery(timezone: &str, hour: u32, minute: u32) -> Result<Option<i64>, Error> {
    let Some(now) = get_local_time(LocalTimeQuery {
        timezone: timezone.into(),
        unix_seconds: None,
        local: None,
    })?
    else {
        return Ok(None);
    };
    for day_offset in [0i64, 1, 2] {
        // Each day's date from the zone itself, so month ends and DST are the host's problem.
        let Some(day) = get_local_time(LocalTimeQuery {
            timezone: timezone.into(),
            unix_seconds: Some(now.unix_seconds + day_offset * 86_400),
            local: None,
        })?
        else {
            return Ok(None);
        };
        let Some(at) = get_local_time(LocalTimeQuery {
            timezone: timezone.into(),
            unix_seconds: None,
            local: Some(LocalWallTime {
                year: day.year,
                month: day.month,
                day: day.day,
                hour,
                minute,
            }),
        })?
        else {
            continue;
        };
        if at.unix_seconds > now.unix_seconds + 30 {
            return Ok(Some(at.unix_seconds));
        }
    }
    Ok(None)
}

fn schedule_daily(
    server: &str,
    profile_id: &str,
    channel: &str,
    plan: &DailyPlan,
    due_at: i64,
) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: daily_job_id(server, profile_id),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: Some(profile_id.into()),
            due_at,
            payload: serde_json::to_string(plan)?,
        })?)?;
    }
    Ok(())
}

fn handle_daily(server: &str, msg: &MessagePayload, addr: &str, arg: &str) -> Result<(), Error> {
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
    if msg.user_id.is_empty() {
        return say(
            "weather.identity_unavailable",
            "I can't verify your profile right now, {user}; please try again shortly.",
            &[],
        );
    }
    let job_id = daily_job_id(server, &msg.user_id);
    if arg.is_empty() {
        let raw = unsafe { schedule_list(serde_json::to_string(&ScheduleList::default())?)? };
        let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
        return match jobs
            .iter()
            .find(|job| job.id == job_id)
            .and_then(|job| serde_json::from_str::<DailyPlan>(&job.payload).ok())
        {
            Some(plan) => say(
                "weather.daily_status",
                "Your morning forecast arrives by private message at {time}, {user}. !weather daily off stops it.",
                &[("time", &format!("{:02}:{:02}", plan.hour, plan.minute))],
            ),
            None => say(
                "weather.daily_none",
                "You have no morning forecast set, {user}. Try !weather daily 08:00.",
                &[],
            ),
        };
    }
    if matches!(arg.to_ascii_lowercase().as_str(), "off" | "stop" | "cancel") {
        unsafe {
            schedule_cancel(serde_json::to_string(&ScheduleCancel { id: job_id })?)?;
        }
        return say(
            "weather.daily_off",
            "Very good, {user}; no more morning forecasts.",
            &[],
        );
    }
    let Some((hour, minute)) = parse_daily_time(arg) else {
        return say(
            "weather.daily_usage",
            "Use !weather daily 08:00 (your local time) or !weather daily off, {user}.",
            &[],
        );
    };
    let profile = get_profile(server, &msg.nick)?;
    let (Some(_), Some(timezone)) = (
        profile.as_ref().and_then(profile_spot),
        profile
            .as_ref()
            .and_then(|profile| profile.timezone.clone()),
    ) else {
        return say(
            "weather_noloc",
            "Set your location first, {user}: !location <place>.",
            &[],
        );
    };
    let Some(due_at) = next_delivery(&timezone, hour, minute)? else {
        return say(
            "weather.daily_unavailable",
            "I couldn't work out your local time just now, {user}.",
            &[],
        );
    };
    let plan = DailyPlan {
        nick: msg.nick.clone(),
        hour,
        minute,
    };
    schedule_daily(server, &msg.user_id, dest, &plan, due_at)?;
    say(
        "weather.daily_set",
        "Very good, {user}: a forecast by private message every day at {time} your time.",
        &[("time", &format!("{hour:02}:{minute:02}"))],
    )
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
    if !id.starts_with("daily:") {
        return Ok(());
    }
    let server = env.server;
    let plan: DailyPlan = serde_json::from_str(&payload)?;
    // The nick resolves through profile aliases, so a renamed user still gets their forecast.
    let Some(profile) = get_profile(&server, &plan.nick)? else {
        return Ok(());
    };
    let (Some(spot), Some(timezone)) = (profile_spot(&profile), profile.timezone.clone()) else {
        // No location any more: stop quietly rather than PM an error every morning.
        return Ok(());
    };
    let units = units_for(&server, &profile.id)?;
    if let Some(w) = get_weather(spot.lat, spot.lon)? {
        let today = w
            .daily
            .first()
            .map(|day| format::forecast_day(day, units))
            .unwrap_or_default();
        let text = themed(
            "weather.daily_message",
            &["Good morning, {user}. {location} today: {today}. Right now: {desc}, {temp}."],
            &[
                ("user", &profile.nick),
                ("location", &spot.label),
                ("today", &today),
                ("desc", wmo_text(w.code)),
                ("temp", &format::temperature(w.temp_c, units)),
            ],
        )?;
        reply(&server, &profile.nick, &text)?;
        send_alerts(&server, &profile.nick, &spot.label, spot.lat, spot.lon)?;
    }
    if let Some(due_at) = next_delivery(&timezone, plan.hour, plan.minute)? {
        schedule_daily(&server, &profile.id, &channel, &plan, due_at)?;
    }
    Ok(())
}

// ── dispatch ────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, arg) = text
        .split_once(char::is_whitespace)
        .map(|(command, arg)| (command, arg.trim()))
        .unwrap_or((text, ""));
    // The host rewrites `!w` and `!fc` to these.
    if command != "!weather" && command != "!forecast" {
        return Ok(());
    }
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let addr = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    if command == "!weather" {
        let lower = arg.to_ascii_lowercase();
        let (sub, rest) = lower
            .split_once(char::is_whitespace)
            .map(|(sub, rest)| (sub, rest.trim()))
            .unwrap_or((lower.as_str(), ""));
        match sub {
            "aqi" => return Ok(handle_aqi(&server, &msg, dest, addr, rest)?),
            "units" | "unit" => return Ok(handle_units(&server, &msg, dest, addr, rest)?),
            "daily" | "morning" => {
                let original_rest = arg
                    .split_once(char::is_whitespace)
                    .map(|(_, rest)| rest.trim())
                    .unwrap_or("");
                return Ok(handle_daily(&server, &msg, addr, original_rest)?);
            }
            _ => {}
        }
    }
    let lookup = resolve(&server, &msg, arg)?;
    let Lookup::Found(spot) = &lookup else {
        return Ok(explain_miss(&server, dest, addr, &lookup)?);
    };
    let Some(w) = get_weather(spot.lat, spot.lon)? else {
        reply(
            &server,
            dest,
            &themed(
                "weather_error",
                &["The weather service isn't answering right now, {user}."],
                &[("user", addr)],
            )?,
        )?;
        return Ok(());
    };
    let units = units_for(&server, &msg.user_id)?;
    if command == "!forecast" {
        reply(&server, dest, &forecast_report(&spot.label, &w, units)?)?;
    } else {
        let show_aqi = aqi_enabled(&server, &msg.user_id)?;
        reply(
            &server,
            dest,
            &current_report(&spot.label, &w, units, show_aqi)?,
        )?;
        send_alerts(&server, dest, &spot.label, spot.lat, spot.lon)?;
    }
    award(
        &server,
        &msg.user_id,
        addr,
        dest,
        matches!(w.code, 65..=67 | 80..=82 | 95..=99),
    )?;
    Ok(())
}

fn handle_aqi(
    server: &str,
    msg: &MessagePayload,
    dest: &str,
    addr: &str,
    value: &str,
) -> Result<(), Error> {
    if value.is_empty() {
        let state = if aqi_enabled(server, &msg.user_id)? {
            "on"
        } else {
            "off"
        };
        return reply(
            server,
            dest,
            &themed(
                "weather.aqi_status",
                &["AQI is {state} for your weather reports, {user}. Use !weather aqi on|off."],
                &[("state", state), ("user", addr)],
            )?,
        );
    }
    let enabled = match value {
        "on" => true,
        "off" => false,
        _ => return reply(
            server,
            dest,
            &themed(
                "weather.aqi_usage",
                &["Choose whether AQI appears with !weather aqi on or !weather aqi off, {user}."],
                &[("user", addr)],
            )?,
        ),
    };
    if msg.user_id.is_empty() {
        return reply(
            server,
            dest,
            &themed(
                "weather.aqi_profile_error",
                &["I couldn't save that AQI preference right now, {user}."],
                &[("user", addr)],
            )?,
        );
    }
    set_aqi_enabled(server, &msg.user_id, enabled)?;
    reply(
        server,
        dest,
        &themed(
            "weather.aqi_saved",
            &["AQI is now {state} for your weather reports, {user}."],
            &[
                ("state", if enabled { "on" } else { "off" }),
                ("user", addr),
            ],
        )?,
    )
}

fn handle_units(
    server: &str,
    msg: &MessagePayload,
    dest: &str,
    addr: &str,
    value: &str,
) -> Result<(), Error> {
    if value.is_empty() {
        let units = units_for(server, &msg.user_id)?;
        return reply(
            server,
            dest,
            &themed(
                "weather.units_status",
                &["Your weather units are {units}, {user}. Use !weather units metric|imperial|both."],
                &[("units", units.name()), ("user", addr)],
            )?,
        );
    }
    let Some(units) = Units::parse(value) else {
        return reply(
            server,
            dest,
            &themed(
                "weather.units_usage",
                &["Choose !weather units metric, imperial, or both, {user}."],
                &[("user", addr)],
            )?,
        );
    };
    if msg.user_id.is_empty() {
        return reply(
            server,
            dest,
            &themed(
                "weather.identity_unavailable",
                &["I can't verify your profile right now, {user}; please try again shortly."],
                &[("user", addr)],
            )?,
        );
    }
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: units_key(server, &msg.user_id),
            value: units.name().into(),
        })?)?
    };
    reply(
        server,
        dest,
        &themed(
            "weather.units_saved",
            &["Weather now comes in {units} units for you, {user}."],
            &[("units", units.name()), ("user", addr)],
        )?,
    )
}

// ── data lifecycle ──────────────────────────────────────────────────────────

fn lifecycle_keys(request: &ModuleDataRequest) -> Vec<(String, &'static str)> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .flat_map(|identity| {
            [
                (aqi_key(&request.subject.server, identity), "aqi_preference"),
                (
                    units_key(&request.subject.server, identity),
                    "units_preference",
                ),
                (
                    retired_local_cooldown_key(&request.subject.server, identity),
                    "retired_cooldown",
                ),
            ]
        })
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let mut data = serde_json::Map::new();
    for entry in &request.entries {
        if let Some((_, name)) = keys.iter().find(|(key, _)| *key == entry.key) {
            if *name != "retired_cooldown" {
                data.insert((*name).into(), entry.value.clone().into());
            }
        }
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
    let keys = lifecycle_keys(&request);
    let mutations = request
        .entries
        .iter()
        .filter(|entry| keys.iter().any(|(key, _)| *key == entry.key))
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

fn significant_alert_events(alerts: &[WeatherAlert]) -> Vec<String> {
    let mut events = Vec::new();
    for alert in alerts {
        let event = alert.event.trim();
        let normalized_event = event.to_ascii_lowercase();
        let significant = normalized_event.ends_with("warning")
            || normalized_event.ends_with("watch")
            || normalized_event.contains("emergency")
            || matches!(alert.severity.as_str(), "Severe" | "Extreme");
        if significant
            && !event.is_empty()
            && !events
                .iter()
                .any(|existing: &String| existing.eq_ignore_ascii_case(event))
        {
            events.push(event.to_string());
        }
    }
    events
}

fn format_alert_events(events: &[String]) -> String {
    const DISPLAY_LIMIT: usize = 3;
    let mut output = events
        .iter()
        .take(DISPLAY_LIMIT)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    let remaining = events.len().saturating_sub(DISPLAY_LIMIT);
    if remaining > 0 {
        output.push_str(&format!("; +{remaining} more"));
    }
    output
}

fn encode(value: &str) -> String {
    value.bytes().map(|byte| format!("{byte:02x}")).collect()
}

fn aqi_key(server: &str, profile_id: &str) -> String {
    format!("aqi:{}:{}", encode(server), encode(profile_id))
}

fn retired_local_cooldown_key(server: &str, profile_id: &str) -> String {
    format!("local-cooldown:{}:{}", encode(server), encode(profile_id))
}

fn aqi_enabled(server: &str, profile_id: &str) -> Result<bool, Error> {
    if profile_id.is_empty() {
        return Ok(true);
    }
    let value = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: aqi_key(server, profile_id),
        })?)?
    };
    Ok(value != "off")
}

fn set_aqi_enabled(server: &str, profile_id: &str, enabled: bool) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: aqi_key(server, profile_id),
            value: if enabled { "on" } else { "off" }.into(),
        })?)?
    };
    Ok(())
}

fn aqi_category(aqi: f64) -> &'static str {
    match aqi.round() as i64 {
        ..=50 => "Good",
        51..=100 => "Moderate",
        101..=150 => "Unhealthy for sensitive groups",
        151..=200 => "Unhealthy",
        201..=300 => "Very unhealthy",
        _ => "Hazardous",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aqi_categories() {
        assert_eq!(aqi_category(50.0), "Good");
        assert_eq!(aqi_category(101.0), "Unhealthy for sensitive groups");
        assert_eq!(aqi_category(301.0), "Hazardous");
    }

    #[test]
    fn daily_times_parse() {
        assert_eq!(parse_daily_time("08:00"), Some((8, 0)));
        assert_eq!(parse_daily_time("8"), Some((8, 0)));
        assert_eq!(parse_daily_time("7:30pm"), Some((19, 30)));
        assert_eq!(parse_daily_time("12am"), Some((0, 0)));
        assert_eq!(parse_daily_time("25:00"), None);
        assert_eq!(parse_daily_time("13pm"), None);
        assert_eq!(parse_daily_time("soon"), None);
    }

    #[test]
    fn selects_and_bounds_significant_nws_alerts() {
        let alerts = vec![
            WeatherAlert {
                event: "Tornado Warning".into(),
                severity: "Extreme".into(),
            },
            WeatherAlert {
                event: "Hazardous Weather Outlook".into(),
                severity: "Unknown".into(),
            },
            WeatherAlert {
                event: "Freeze Warning".into(),
                severity: "Moderate".into(),
            },
            WeatherAlert {
                event: "Hurricane Watch".into(),
                severity: "Severe".into(),
            },
            WeatherAlert {
                event: "Civil Emergency Message".into(),
                severity: "Severe".into(),
            },
            WeatherAlert {
                event: "tornado warning".into(),
                severity: "Extreme".into(),
            },
        ];

        let events = significant_alert_events(&alerts);
        assert_eq!(
            events,
            vec![
                "Tornado Warning",
                "Freeze Warning",
                "Hurricane Watch",
                "Civil Emergency Message"
            ]
        );
        assert_eq!(
            format_alert_events(&events),
            "Tornado Warning; Freeze Warning; Hurricane Watch; +1 more"
        );
    }
}
