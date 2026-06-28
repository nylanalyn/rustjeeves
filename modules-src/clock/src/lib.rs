//! `!time`, using saved profile locations or an ad-hoc geocoded location.
//! Timezone conversion remains host-side so the WASM receives current IANA timezone rules.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, GeoQuery, GeoResult, LocalTimeQuery,
    LocalTimeResult, Profile, ProfileKey, ProfileUpdate, SendMessage, ThemeReq,
    COMMAND_MANIFEST_VERSION,
};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn profile_set(input: String) -> String;
    fn geocode(input: String) -> String;
    fn local_time(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "time".into(),
            aliases: vec!["clock".into()],
            description: "Show local time for a user or location.".into(),
            usage: "!time [user|location]".into(),
        }],
    })?)
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
    let req = ThemeReq {
        key: key.into(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
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

fn get_local_time(timezone: &str) -> Result<Option<LocalTimeResult>, Error> {
    let out = unsafe {
        local_time(serde_json::to_string(&LocalTimeQuery {
            timezone: timezone.into(),
            unix_seconds: None,
        })?)?
    };
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_str(&out)?))
    }
}

fn timezone_for_profile(server: &str, p: &Profile) -> Result<Option<String>, Error> {
    if let Some(timezone) = &p.timezone {
        return Ok(Some(timezone.clone()));
    }
    // Backfill profiles saved before timezone storage was introduced.
    let Some(location) = p.location_display.as_deref() else {
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

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut parts = msg.text.trim().splitn(2, char::is_whitespace);
    if parts.next() != Some("!time") {
        return Ok(());
    }

    let arg = parts.next().unwrap_or("").trim();
    let dest = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let caller: &str = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };

    let (timezone, subject, is_location) = if arg.is_empty() {
        let Some(profile) = get_profile(&env.server, &msg.nick)? else {
            reply(
                &env.server,
                dest,
                &themed(
                    "missing_location",
                    &["Set your location first, {user}: !location <place>."],
                    &[("user", caller)],
                )?,
            )?;
            return Ok(());
        };
        let Some(timezone) = timezone_for_profile(&env.server, &profile)? else {
            reply(
                &env.server,
                dest,
                &themed(
                    "missing_location",
                    &["Set your location first, {user}: !location <place>."],
                    &[("user", caller)],
                )?,
            )?;
            return Ok(());
        };
        (timezone, "your".to_string(), false)
    } else if let Some(profile) = get_profile(&env.server, arg)? {
        let Some(timezone) = timezone_for_profile(&env.server, &profile)? else {
            reply(
                &env.server,
                dest,
                &themed(
                    "user_missing_location",
                    &["{target} hasn't saved a location."],
                    &[("target", arg), ("user", caller)],
                )?,
            )?;
            return Ok(());
        };
        (timezone, profile.nick, false)
    } else {
        let Some(geo) = do_geocode(arg)? else {
            reply(
                &env.server,
                dest,
                &themed(
                    "location_not_found",
                    &["I couldn't find '{query}', {user}."],
                    &[("query", arg), ("user", caller)],
                )?,
            )?;
            return Ok(());
        };
        let label = geo_label(&geo);
        (geo.timezone, label, true)
    };

    let Some(local) = get_local_time(&timezone)? else {
        reply(
            &env.server,
            dest,
            &themed(
                "service_error",
                &["I couldn't determine the local time right now, {user}."],
                &[("user", caller)],
            )?,
        )?;
        return Ok(());
    };
    let vars = time_vars(&local);
    let var_refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut all_vars = vec![("user", caller), ("target", subject.as_str())];
    all_vars.extend(var_refs);
    let output = if is_location {
        themed(
            "location_time",
            &["The local time in {target} is {time} on {weekday}, {date} ({zone}, UTC{offset})."],
            &all_vars,
        )?
    } else if arg.is_empty() {
        themed(
            "own_time",
            &["Your local time is {time} on {weekday}, {date} ({zone}, UTC{offset}), {user}."],
            &all_vars,
        )?
    } else {
        themed(
            "user_time",
            &["{target}'s local time is {time} on {weekday}, {date} ({zone}, UTC{offset})."],
            &all_vars,
        )?
    };
    reply(&env.server, dest, &output)?;
    Ok(())
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

fn time_vars(local: &LocalTimeResult) -> Vec<(&'static str, String)> {
    let (hour, period) = match local.hour_24 {
        0 => (12, "AM"),
        1..=11 => (local.hour_24, "AM"),
        12 => (12, "PM"),
        _ => (local.hour_24 - 12, "PM"),
    };
    vec![
        ("time", format!("{hour}:{:02} {period}", local.minute)),
        (
            "time24",
            format!("{:02}:{:02}", local.hour_24, local.minute),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_midnight_noon_and_quarter_hour_zone() {
        let mut local = LocalTimeResult {
            timezone: "Asia/Kathmandu".into(),
            abbreviation: "+0545".into(),
            utc_offset: "+05:45".into(),
            year: 2026,
            month: 6,
            day: 28,
            weekday: "Sunday".into(),
            hour_24: 0,
            minute: 5,
        };
        let vars = time_vars(&local);
        assert!(vars.contains(&("time", "12:05 AM".into())));
        assert!(vars.contains(&("offset", "+05:45".into())));
        local.hour_24 = 12;
        assert!(time_vars(&local).contains(&("time", "12:05 PM".into())));
    }
}
