//! `!time` and `!until`.
//!
//! - `!time [target, target…]` — local time for yourself, a nick, a place, or a zone ("PST",
//!   "Europe/London", "UTC+5:30"); several targets are separated by commas.
//! - `!time #channel` / `!time here` — everyone present with a saved location, grouped by clock.
//! - `!time 3pm PST in London` — convert a clock time between zones, places, or people.
//! - `!time format 12|24` — your preferred clock style (stored per profile).
//! - `!until <date|event|weekday> [time]` — countdowns in your own timezone.
//!
//! Timezone rules stay host-side (`local_time`) so the WASM always sees current IANA data.

mod when;

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, Channel,
    CommandManifest, CommandSpec, Event, EventEnvelope, GeoQuery, GeoResult, KvGet, KvSet,
    LocalTimeQuery, LocalTimeResult, LocalWallTime, MessagePayload, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, Profile, ProfileKey, ProfileUpdate,
    StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION,
};
use jeeves_guest::{encode, no_highlight, reply, themed};
use std::collections::BTreeMap;
use when::{Date, MONTHS};

const MAX_TARGETS: usize = 5;
const MAX_CHANNEL_MEMBERS: usize = 150;
const MAX_NAMES_PER_CLOCK: usize = 6;

#[host_fn]
extern "ExtismHost" {
    fn profile_get(input: String) -> String;
    fn profile_set(input: String) -> String;
    fn geocode(input: String) -> String;
    fn local_time(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn channel_members(input: String) -> String;
}

// ── manifests ───────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "lookups".into(),
            description: "Successful time lookups".into(),
        }],
        achievements: [
            ("right_on_time", "Right on Time", 1),
            ("clock_watcher", "Clock Watcher", 25),
            ("master_hours", "Master of Hours", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Complete {threshold} successful time lookups."),
            stat: "lookups".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "time".into(),
                aliases: vec!["clock".into()],
                description: "Local time for you, people, places, or zones (commas for several); \
                              #channel for everyone here; convert with !time 3pm PST in London; \
                              !time format 12|24."
                    .into(),
                usage: "!time [who/where, …] | !time #channel | !time <clock> [from] in <to> | !time format 12|24".into(),
                ..Default::default()
            },
            CommandSpec {
                name: "until".into(),
                aliases: vec!["countdown".into()],
                description: "How long until a date, event, weekday, or time, in your timezone."
                    .into(),
                usage: "!until <christmas | dec 25 | 2027-01-01 | friday 8pm | 18:00>".into(),
                ..Default::default()
            },
        ],
    })?)
}

// ── host helpers ────────────────────────────────────────────────────────────

fn say(ctx: &Ctx, key: &str, default: &str, vars: &[(&str, &str)]) -> Result<(), Error> {
    let mut vars = vars.to_vec();
    vars.push(("user", ctx.caller));
    reply(ctx.server, ctx.dest, &themed(key, &[default], &vars)?)
}

fn award(ctx: &Ctx) -> Result<(), Error> {
    if ctx.msg.user_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: ctx.server.into(),
            profile_id: ctx.msg.user_id.clone(),
            display_name: ctx.caller.into(),
            target: ctx.dest.into(),
            increments: vec![StatIncrement {
                stat: "lookups".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

fn get_profile(server: &str, nick: &str) -> Result<Option<Profile>, Error> {
    let out = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
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

fn get_local_time(
    timezone: &str,
    local: Option<LocalWallTime>,
) -> Result<Option<LocalTimeResult>, Error> {
    let out = unsafe {
        local_time(serde_json::to_string(&LocalTimeQuery {
            timezone: timezone.into(),
            unix_seconds: None,
            local,
        })?)?
    };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn get_local_time_at(timezone: &str, unix_seconds: i64) -> Result<Option<LocalTimeResult>, Error> {
    let out = unsafe {
        local_time(serde_json::to_string(&LocalTimeQuery {
            timezone: timezone.into(),
            unix_seconds: Some(unix_seconds),
            local: None,
        })?)?
    };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn timezone_for_profile(server: &str, p: &Profile) -> Result<Option<String>, Error> {
    // A stored timezone is authoritative: `!location` saves it alongside the coordinates, so
    // re-geocoding here would only add an HTTP round trip to every `!time` and let geocoder
    // drift overwrite good data.
    if p.timezone.is_some() {
        return Ok(p.timezone.clone());
    }
    // Profiles saved before timezone storage existed are backfilled once, from the canonical
    // label when available (it carries the disambiguating region, e.g. Melbourne, Florida).
    let Some(location) = location_for_timezone_lookup(p) else {
        return Ok(None);
    };
    let Some(geo) = do_geocode(location)? else {
        return Ok(None);
    };
    unsafe {
        profile_set(serde_json::to_string(&ProfileUpdate {
            server: server.into(),
            nick: p.nick.clone(),
            timezone: Some(geo.timezone.clone()),
            ..Default::default()
        })?)?
    };
    Ok(Some(geo.timezone))
}

fn location_for_timezone_lookup(p: &Profile) -> Option<&str> {
    p.location_label
        .as_deref()
        .or(p.location_display.as_deref())
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

// ── clock style preference ──────────────────────────────────────────────────

fn format_key(server: &str, profile_id: &str) -> String {
    format!("format:{}:{}", encode(server), encode(profile_id))
}

/// True when the caller prefers a 24-hour clock.
fn prefers_24h(server: &str, profile_id: &str) -> Result<bool, Error> {
    if profile_id.is_empty() {
        return Ok(false);
    }
    let raw = unsafe {
        kv_get(serde_json::to_string(&KvGet {
            key: format_key(server, profile_id),
        })?)?
    };
    Ok(raw.trim() == "24")
}

fn clock_text(hour: u32, minute: u32, twenty_four: bool) -> String {
    if twenty_four {
        return format!("{hour:02}:{minute:02}");
    }
    let (hour12, period) = match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    };
    format!("{hour12}:{minute:02} {period}")
}

fn short_weekday(weekday: &str) -> String {
    weekday.chars().take(3).collect()
}

fn time_vars(local: &LocalTimeResult, twenty_four: bool) -> Vec<(&'static str, String)> {
    vec![
        ("time", clock_text(local.hour_24, local.minute, twenty_four)),
        ("time12", clock_text(local.hour_24, local.minute, false)),
        ("time24", clock_text(local.hour_24, local.minute, true)),
        ("weekday", local.weekday.clone()),
        (
            "date",
            format!("{:04}-{:02}-{:02}", local.year, local.month, local.day),
        ),
        ("zone", local.abbreviation.clone()),
        ("timezone", local.timezone.clone()),
        ("offset", local.utc_offset.clone()),
    ]
}

// ── target resolution ───────────────────────────────────────────────────────

/// Where a time is wanted: the zone to ask the host for, and how to name it.
#[derive(Clone, Debug)]
enum Place {
    Own { zone: String },
    Person { zone: String, nick: String },
    Location { zone: String, label: String },
    Zone { zone: String, label: String },
}

impl Place {
    fn zone(&self) -> &str {
        match self {
            Place::Own { zone }
            | Place::Person { zone, .. }
            | Place::Location { zone, .. }
            | Place::Zone { zone, .. } => zone,
        }
    }

    fn label(&self) -> String {
        match self {
            Place::Own { .. } => "your time".into(),
            Place::Person { nick, .. } => nick.clone(),
            Place::Location { label, .. } | Place::Zone { label, .. } => label.clone(),
        }
    }
}

enum Lookup {
    Found(Place),
    /// A known person with no saved location.
    NoLocation(String),
    NotFound(String),
}

fn resolve(ctx: &Ctx, target: &str) -> Result<Lookup, Error> {
    let target = target.trim();
    if target.is_empty() || target.eq_ignore_ascii_case("me") {
        return Ok(match get_profile(ctx.server, &ctx.msg.nick)? {
            Some(profile) => match timezone_for_profile(ctx.server, &profile)? {
                Some(zone) => Lookup::Found(Place::Own { zone }),
                None => Lookup::NoLocation(ctx.caller.into()),
            },
            None => Lookup::NoLocation(ctx.caller.into()),
        });
    }
    // A zone name, abbreviation, or offset ("PST", "Europe/London", "UTC+2").
    if let Some(local) = get_local_time(target, None)? {
        let label = if target.len() <= 5 || target.to_ascii_lowercase().starts_with("utc") {
            target.to_uppercase()
        } else {
            local.timezone.clone()
        };
        return Ok(Lookup::Found(Place::Zone {
            zone: local.timezone,
            label,
        }));
    }
    // A nick with a saved location wins; a nick without one (someone called "paris" or
    // "georgia") falls through to treating the argument as a place.
    let profile = get_profile(ctx.server, target)?;
    let profile_zone = match &profile {
        Some(profile) => timezone_for_profile(ctx.server, profile)?,
        None => None,
    };
    if let (Some(profile), Some(zone)) = (&profile, profile_zone) {
        return Ok(Lookup::Found(Place::Person {
            zone,
            nick: profile.nick.clone(),
        }));
    }
    if let Some(geo) = do_geocode(target)? {
        let label = geo_label(&geo);
        return Ok(Lookup::Found(Place::Location {
            zone: geo.timezone,
            label,
        }));
    }
    Ok(if profile.is_some() {
        Lookup::NoLocation(target.into())
    } else {
        Lookup::NotFound(target.into())
    })
}

/// Reply for a target that couldn't be resolved; returns Ok(()) after replying.
fn explain_miss(ctx: &Ctx, lookup: &Lookup) -> Result<(), Error> {
    match lookup {
        Lookup::NoLocation(who) if who == ctx.caller => say(
            ctx,
            "missing_location",
            "Set your location first, {user}: !location <place>.",
            &[],
        ),
        Lookup::NoLocation(who) => say(
            ctx,
            "user_missing_location",
            "{target} hasn't saved a location.",
            &[("target", who)],
        ),
        Lookup::NotFound(query) => say(
            ctx,
            "location_not_found",
            "I couldn't find '{query}', {user}.",
            &[("query", query)],
        ),
        Lookup::Found(_) => Ok(()),
    }
}

// ── dispatch ────────────────────────────────────────────────────────────────

struct Ctx<'a> {
    server: &'a str,
    dest: &'a str,
    caller: &'a str,
    msg: &'a MessagePayload,
    twenty_four: bool,
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, arg) = text
        .split_once(char::is_whitespace)
        .map(|(command, arg)| (command, arg.trim()))
        .unwrap_or((text, ""));
    // The host rewrites `!clock` and `!countdown` to these.
    if command != "!time" && command != "!until" {
        return Ok(());
    }
    let ctx = Ctx {
        server: &env.server,
        dest: if msg.is_private {
            &msg.nick
        } else {
            &msg.target
        },
        caller: if msg.display.is_empty() {
            &msg.nick
        } else {
            &msg.display
        },
        msg: &msg,
        twenty_four: prefers_24h(&env.server, &msg.user_id)?,
    };
    if command == "!until" {
        return Ok(until(&ctx, arg)?);
    }
    let lower = arg.to_ascii_lowercase();
    if let Some(style) = lower.strip_prefix("format") {
        return Ok(set_format(&ctx, style.trim())?);
    }
    if arg.starts_with(['#', '&']) || lower == "here" {
        let channel = if lower == "here" {
            msg.target.as_str()
        } else {
            arg
        };
        return Ok(channel_clocks(&ctx, channel)?);
    }
    if let Some((hour, minute, rest)) = when::parse_clock(arg) {
        return Ok(convert(&ctx, hour, minute, rest)?);
    }
    let targets = arg
        .split(',')
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .take(MAX_TARGETS)
        .collect::<Vec<_>>();
    if targets.len() > 1 {
        return Ok(several(&ctx, &targets)?);
    }
    Ok(single(&ctx, arg)?)
}

fn single(ctx: &Ctx, target: &str) -> Result<(), Error> {
    let lookup = resolve(ctx, target)?;
    let Lookup::Found(place) = &lookup else {
        return explain_miss(ctx, &lookup);
    };
    let Some(local) = get_local_time(place.zone(), None)? else {
        return say(
            ctx,
            "service_error",
            "I couldn't determine the local time right now, {user}.",
            &[],
        );
    };
    let vars = time_vars(&local, ctx.twenty_four);
    let label = place.label();
    let mut all = vars
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect::<Vec<_>>();
    all.push(("target", &label));
    let (key, default) = match place {
        Place::Own { .. } => (
            "own_time",
            "Your local time is {time} on {weekday}, {date} ({zone}, UTC{offset}), {user}.",
        ),
        Place::Person { .. } => (
            "user_time",
            "{target}'s local time is {time} on {weekday}, {date} ({zone}, UTC{offset}).",
        ),
        Place::Location { .. } => (
            "location_time",
            "The local time in {target} is {time} on {weekday}, {date} ({zone}, UTC{offset}).",
        ),
        Place::Zone { .. } => (
            "zone_time",
            "It's {time} on {weekday}, {date} in {target} ({zone}, UTC{offset}).",
        ),
    };
    say(ctx, key, default, &all)?;
    award(ctx)
}

fn several(ctx: &Ctx, targets: &[&str]) -> Result<(), Error> {
    let mut entries = Vec::new();
    for target in targets {
        let lookup = resolve(ctx, target)?;
        let entry = match &lookup {
            Lookup::Found(place) => match get_local_time(place.zone(), None)? {
                Some(local) => format!(
                    "{} {} {}",
                    place.label(),
                    clock_text(local.hour_24, local.minute, ctx.twenty_four),
                    short_weekday(&local.weekday)
                ),
                None => format!("{target} ?"),
            },
            Lookup::NoLocation(who) => format!("{who} (no location)"),
            Lookup::NotFound(query) => format!("{query} (unknown)"),
        };
        entries.push(entry);
    }
    say(
        ctx,
        "clock.several",
        "{user}: {times}",
        &[("times", &entries.join(" · "))],
    )?;
    award(ctx)
}

/// `!time 3pm PST in London`: a clock time at the source, shown at the target.
fn convert(ctx: &Ctx, hour: u32, minute: u32, rest: &str) -> Result<(), Error> {
    let rest = rest.trim();
    let lower = rest.to_ascii_lowercase();
    // "PST in London" / "in London" / "PST to UTC" / "PST" (then: your time).
    let split = [" in ", " to "]
        .iter()
        .filter_map(|separator| lower.rfind(separator).map(|index| (index, separator.len())))
        .max_by_key(|(index, _)| *index);
    let (source_text, target_text) = match split {
        Some((index, length)) => (rest[..index].trim(), rest[index + length..].trim()),
        None => match lower
            .strip_prefix("in ")
            .or_else(|| lower.strip_prefix("to "))
        {
            Some(_) => ("", rest[3..].trim()),
            None => (rest, ""),
        },
    };
    let source = resolve(ctx, source_text)?;
    let Lookup::Found(source) = source else {
        if source_text.is_empty() {
            return say(
                ctx,
                "clock.convert_needs_zone",
                "Tell me where that time is, {user} (e.g. !time 3pm PST in London), or set your !location.",
                &[],
            );
        }
        return explain_miss(ctx, &source);
    };
    let target = resolve(ctx, target_text)?;
    let Lookup::Found(target) = target else {
        return explain_miss(ctx, &target);
    };
    let unavailable = || {
        say(
            ctx,
            "service_error",
            "I couldn't determine the local time right now, {user}.",
            &[],
        )
    };
    // The clock time on the source's current local date.
    let Some(today) = get_local_time(source.zone(), None)? else {
        return unavailable();
    };
    let wall = LocalWallTime {
        year: today.year,
        month: today.month,
        day: today.day,
        hour,
        minute,
    };
    let Some(from) = get_local_time(source.zone(), Some(wall))? else {
        return unavailable();
    };
    let Some(to) = get_local_time_at(target.zone(), from.unix_seconds)? else {
        return unavailable();
    };
    let day_note = |local: &LocalTimeResult| short_weekday(&local.weekday);
    say(
        ctx,
        "clock.convert",
        "{from_time} {from_zone} ({from_day}) in {from} is {to_time} {to_zone} ({to_day}) in {to}.",
        &[
            (
                "from_time",
                &clock_text(from.hour_24, from.minute, ctx.twenty_four),
            ),
            ("from_zone", &from.abbreviation),
            ("from_day", &day_note(&from)),
            ("from", &source.label()),
            (
                "to_time",
                &clock_text(to.hour_24, to.minute, ctx.twenty_four),
            ),
            ("to_zone", &to.abbreviation),
            ("to_day", &day_note(&to)),
            ("to", &target.label()),
        ],
    )?;
    award(ctx)
}

fn set_format(ctx: &Ctx, style: &str) -> Result<(), Error> {
    let value = match style {
        "24" | "24h" | "24-hour" => "24",
        "12" | "12h" | "12-hour" => "12",
        _ => {
            return say(
                ctx,
                "clock.format_usage",
                "Choose !time format 12 or !time format 24, {user}.",
                &[],
            )
        }
    };
    if ctx.msg.user_id.is_empty() {
        return say(
            ctx,
            "clock.identity_unavailable",
            "I can't verify your profile right now, {user}; please try again shortly.",
            &[],
        );
    }
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: format_key(ctx.server, &ctx.msg.user_id),
            value: value.into(),
        })?)?
    };
    say(
        ctx,
        "clock.format_set",
        "Very good, {user}: {style}-hour clocks from now on.",
        &[("style", value)],
    )
}

/// A local (year, month, day, hour, minute), so clock groups sort by what they read.
type ClockKey = (i32, u32, u32, u32, u32);

/// Everyone present with a saved timezone, grouped by what their clock reads.
fn channel_clocks(ctx: &Ctx, channel: &str) -> Result<(), Error> {
    let raw = unsafe {
        channel_members(serde_json::to_string(&Channel {
            server: ctx.server.into(),
            channel: channel.into(),
        })?)?
    };
    let members: Vec<String> = serde_json::from_str(&raw)?;
    if members.is_empty() {
        return say(
            ctx,
            "clock.channel_unknown",
            "I'm not in {channel}, or I don't know who's there yet, {user}.",
            &[("channel", channel)],
        );
    }
    let mut zones: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unknown = 0;
    for nick in members.iter().take(MAX_CHANNEL_MEMBERS) {
        match get_profile(ctx.server, nick)?.and_then(|profile| profile.timezone) {
            Some(zone) => zones.entry(zone).or_default().push(nick.clone()),
            None => unknown += 1,
        }
    }
    unknown += members.len().saturating_sub(MAX_CHANNEL_MEMBERS);
    // Group zones whose clocks agree right now, ordered by local date and time.
    let mut clocks: BTreeMap<ClockKey, (String, Vec<String>)> = BTreeMap::new();
    for (zone, nicks) in zones {
        if let Some(local) = get_local_time(&zone, None)? {
            let key = (
                local.year,
                local.month,
                local.day,
                local.hour_24,
                local.minute,
            );
            let label = format!(
                "{} {}",
                clock_text(local.hour_24, local.minute, ctx.twenty_four),
                short_weekday(&local.weekday)
            );
            clocks
                .entry(key)
                .or_insert((label, Vec::new()))
                .1
                .extend(nicks);
        } else {
            unknown += nicks.len();
        }
    }
    if clocks.is_empty() {
        return say(
            ctx,
            "clock.channel_empty",
            "Nobody in {channel} has saved a location yet, {user}. !location <place> fixes that.",
            &[("channel", channel)],
        );
    }
    let groups = clocks
        .into_values()
        .map(|(label, mut nicks)| {
            nicks.sort_by_key(|nick| nick.to_lowercase());
            let shown = nicks
                .iter()
                .take(MAX_NAMES_PER_CLOCK)
                .map(|nick| no_highlight(nick))
                .collect::<Vec<_>>()
                .join(", ");
            let more = nicks.len().saturating_sub(MAX_NAMES_PER_CLOCK);
            if more > 0 {
                format!("{label} — {shown} +{more}")
            } else {
                format!("{label} — {shown}")
            }
        })
        .collect::<Vec<_>>()
        .join(" · ");
    let unknown_note = if unknown > 0 {
        format!(" (+{unknown} without a saved location)")
    } else {
        String::new()
    };
    say(
        ctx,
        "clock.channel",
        "Clocks in {channel}: {clocks}{unknown}",
        &[
            ("channel", channel),
            ("clocks", &groups),
            ("unknown", &unknown_note),
        ],
    )?;
    award(ctx)
}

fn until(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    if arg.is_empty() {
        return say(
            ctx,
            "clock.until_usage",
            "Usage: !until <christmas | dec 25 | 2027-01-01 | friday 8pm | 18:00>",
            &[],
        );
    }
    // Your own timezone when saved, otherwise UTC (and say so).
    let own_zone = match get_profile(ctx.server, &ctx.msg.nick)? {
        Some(profile) => timezone_for_profile(ctx.server, &profile)?,
        None => None,
    };
    let zone = own_zone.clone().unwrap_or_else(|| "UTC".into());
    let Some(now) = get_local_time(&zone, None)? else {
        return say(
            ctx,
            "service_error",
            "I couldn't determine the local time right now, {user}.",
            &[],
        );
    };
    let today = Date {
        year: now.year,
        month: now.month,
        day: now.day,
    };
    let Some(target) = when::parse_target(arg, today, now.hour_24 * 60 + now.minute) else {
        return say(
            ctx,
            "clock.until_unknown",
            "I couldn't work out when '{when}' is, {user}. Try !until dec 25, !until friday 8pm, or !until christmas.",
            &[("when", arg)],
        );
    };
    let wall = LocalWallTime {
        year: target.date.year,
        month: target.date.month,
        day: target.date.day,
        hour: target.hour,
        minute: target.minute,
    };
    let Some(then) = get_local_time(&zone, Some(wall))? else {
        return say(
            ctx,
            "service_error",
            "I couldn't determine the local time right now, {user}.",
            &[],
        );
    };
    let seconds = then.unix_seconds - now.unix_seconds;
    let date = format!(
        "{} {} {} {}",
        then.weekday,
        then.day,
        MONTHS[(then.month - 1) as usize],
        then.year
    );
    let when_text = if target.hour == 0 && target.minute == 0 {
        date
    } else {
        format!(
            "{date}, {}",
            clock_text(then.hour_24, then.minute, ctx.twenty_four)
        )
    };
    let what = target
        .name
        .map_or_else(|| when_text.clone(), str::to_string);
    let zone_note = if own_zone.is_some() {
        String::new()
    } else {
        " (UTC — set !location for your own time)".into()
    };
    say(
        ctx,
        "clock.until",
        "{what} ({when}) is in {duration}{zone_note}, {user}.",
        &[
            ("what", &what),
            ("when", &when_text),
            ("duration", &when::describe_duration(seconds)),
            ("zone_note", &zone_note),
        ],
    )?;
    award(ctx)
}

// ── data lifecycle (the clock-style preference) ─────────────────────────────

fn lifecycle_keys(request: &ModuleDataRequest) -> Vec<String> {
    std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| format_key(&request.subject.server, identity))
        .collect()
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    let preference = request
        .entries
        .iter()
        .find(|entry| keys.contains(&entry.key))
        .map(|entry| entry.value.clone());
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: match preference {
            Some(value) => serde_json::json!({ "clock_format": value }),
            None => serde_json::Value::Null,
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = lifecycle_keys(&request);
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations: request
            .entries
            .iter()
            .filter(|entry| keys.contains(&entry.key))
            .map(|entry| ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            })
            .collect(),
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks_render_in_both_styles() {
        assert_eq!(clock_text(0, 5, false), "12:05 AM");
        assert_eq!(clock_text(12, 5, false), "12:05 PM");
        assert_eq!(clock_text(15, 30, false), "3:30 PM");
        assert_eq!(clock_text(9, 7, true), "09:07");
    }

    #[test]
    fn canonical_location_label_wins_over_ambiguous_display() {
        let profile = Profile {
            location_display: Some("Melbourne".into()),
            location_label: Some("Melbourne, Brevard County, Florida, United States".into()),
            ..Default::default()
        };
        assert_eq!(
            location_for_timezone_lookup(&profile),
            Some("Melbourne, Brevard County, Florida, United States")
        );
    }

    #[test]
    fn names_are_unhighlighted() {
        assert_eq!(no_highlight("sir bob"), "s\u{200B}ir b\u{200B}ob");
    }
}
