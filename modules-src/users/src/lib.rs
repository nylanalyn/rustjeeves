//! User profiles module for rustjeeves.
//!
//! Builds a profile per person (created on first contact) and exposes commands to set personal
//! info: `!title`, `!birthday`, `!pronouns`, `!location`, and `!whoami` / `!profile` to read it.
//! Profiles live in the host-level profile store (shared, so a future weather module can read the
//! location). All replies go through the theme system.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, GeoQuery, GeoResult, Profile, ProfileClear,
    ProfileKey, ProfileUpdate, SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION,
};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_ensure(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn profile_set(input: String) -> String;
    fn profile_clear(input: String) -> String;
    fn geocode(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    let mut whoami = command("whoami", "Show a stored user profile.", "!whoami [nick]");
    whoami.aliases = vec!["profile".into()];
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            whoami,
            command("title", "Set or clear your title.", "!title <title|clear>"),
            command(
                "birthday",
                "Set or clear your birthday.",
                "!birthday <date|clear>",
            ),
            command(
                "pronouns",
                "Set or clear your pronouns.",
                "!pronouns <values|clear>",
            ),
            command(
                "location",
                "Set or clear your saved location.",
                "!location <place|clear>",
            ),
            command("clear", "Clear a profile field.", "!clear <field>"),
        ],
    })?)
}

fn clear_field(server: &str, nick: &str, field: &str) -> Result<(), Error> {
    let req = ProfileClear {
        server: server.into(),
        nick: nick.into(),
        field: field.into(),
    };
    unsafe { profile_clear(serde_json::to_string(&req)?)? };
    Ok(())
}

/// If `arg` is "clear", clear `field` for `nick` and reply (addressing them as `addr`); returns
/// true if handled.
fn handle_clear(
    server: &str,
    dest: &str,
    nick: &str,
    addr: &str,
    field: &str,
    arg: &str,
) -> Result<bool, Error> {
    if !arg.eq_ignore_ascii_case("clear") {
        return Ok(false);
    }
    clear_field(server, nick, field)?;
    reply(
        server,
        dest,
        &themed(
            "cleared",
            &["Cleared your {field}, {user}."],
            &[("user", addr), ("field", field)],
        )?,
    )?;
    Ok(true)
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.into(),
        target: target.into(),
        text: text.into(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

/// Fetch a themed (configurable) string. `defaults` seeds the theme file on first use.
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

fn set_profile(update: &ProfileUpdate) -> Result<(), Error> {
    unsafe { profile_set(serde_json::to_string(update)?)? };
    Ok(())
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

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    // Skeleton on first contact + last-seen update, for every message.
    let key = ProfileKey {
        server: server.clone(),
        nick: msg.nick.clone(),
    };
    unsafe { profile_ensure(serde_json::to_string(&key)?)? };

    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }
    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let mut parts = text.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let arg = parts.next().unwrap_or("").trim();
    // `nick` is identity (profile key); `addr` is how we address them ({title} {nick} if set).
    let nick = msg.nick.as_str();
    let addr = if msg.display.is_empty() {
        nick
    } else {
        msg.display.as_str()
    };

    match cmd {
        "!whoami" | "!profile" => {
            let target = if arg.is_empty() { nick } else { arg };
            match get_profile(&server, target)? {
                Some(p) => reply(&server, dest, &format_profile(&p)?)?,
                None => reply(
                    &server,
                    dest,
                    &themed(
                        "unknown",
                        &["I've no profile for {target} yet."],
                        &[("target", target)],
                    )?,
                )?,
            }
        }
        "!title" if handle_clear(&server, dest, nick, addr, "title", arg)? => {}
        "!birthday" if handle_clear(&server, dest, nick, addr, "birthday", arg)? => {}
        "!pronouns" if handle_clear(&server, dest, nick, addr, "pronouns", arg)? => {}
        "!location" if handle_clear(&server, dest, nick, addr, "location", arg)? => {}
        "!clear" => {
            let field = arg.to_lowercase();
            match field.as_str() {
                "title" | "birthday" | "pronouns" | "location" => {
                    clear_field(&server, nick, &field)?;
                    reply(
                        &server,
                        dest,
                        &themed(
                            "cleared",
                            &["Cleared your {field}, {user}."],
                            &[("user", addr), ("field", &field)],
                        )?,
                    )?;
                }
                _ => reply(
                    &server,
                    dest,
                    &themed(
                        "clear_help",
                        &["I can clear: title, birthday, pronouns, location."],
                        &[("user", addr)],
                    )?,
                )?,
            }
        }
        "!title" => {
            if arg.is_empty() {
                reply(
                    &server,
                    dest,
                    &themed(
                        "title_empty",
                        &["What title would you like, {user}? e.g. !title Captain"],
                        &[("user", addr)],
                    )?,
                )?;
            } else {
                set_profile(&ProfileUpdate {
                    server: server.clone(),
                    nick: nick.into(),
                    title: Some(arg.into()),
                    ..Default::default()
                })?;
                reply(
                    &server,
                    dest,
                    &themed(
                        "title_set",
                        &["Very good. I shall call you {title}, {user}."],
                        &[("user", addr), ("title", arg)],
                    )?,
                )?;
            }
        }
        "!birthday" => match parse_birthday(arg) {
            Some(bd) => {
                set_profile(&ProfileUpdate {
                    server: server.clone(),
                    nick: nick.into(),
                    birthday: Some(bd.clone()),
                    ..Default::default()
                })?;
                reply(
                    &server,
                    dest,
                    &themed(
                        "birthday_set",
                        &["Noted your birthday as {birthday}, {user}."],
                        &[("user", addr), ("birthday", &bd)],
                    )?,
                )?;
            }
            None => reply(
                &server,
                dest,
                &themed(
                    "birthday_bad",
                    &["I couldn't parse that date, {user}. Try MM-DD, MM-DD-YYYY, or 'March 14'."],
                    &[("user", addr)],
                )?,
            )?,
        },
        "!pronouns" => match parse_pronouns(arg) {
            Some((s, o, p)) => {
                set_profile(&ProfileUpdate {
                    server: server.clone(),
                    nick: nick.into(),
                    pronoun_subject: Some(s.clone()),
                    pronoun_object: Some(o.clone()),
                    pronoun_possessive: Some(p.clone()),
                    ..Default::default()
                })?;
                reply(
                    &server,
                    dest,
                    &themed(
                        "pronouns_set",
                        &["Noted — {subj}/{obj}/{poss}, {user}."],
                        &[("user", addr), ("subj", &s), ("obj", &o), ("poss", &p)],
                    )?,
                )?;
            }
            None => reply(
                &server,
                dest,
                &themed(
                    "pronouns_bad",
                    &["Try a preset (he/she/they) or a set like xe/xem/xyr, {user}."],
                    &[("user", addr)],
                )?,
            )?,
        },
        "!location" => {
            if arg.is_empty() {
                reply(
                    &server,
                    dest,
                    &themed(
                        "location_empty",
                        &["Where are you, {user}? e.g. !location Hackney, England"],
                        &[("user", addr)],
                    )?,
                )?;
            } else {
                match do_geocode(arg)? {
                    Some(g) => {
                        let label = geo_label(&g);
                        set_profile(&ProfileUpdate {
                            server: server.clone(),
                            nick: nick.into(),
                            location_display: Some(arg.into()),
                            location_label: Some(label.clone()),
                            lat: Some(g.lat),
                            lon: Some(g.lon),
                            ..Default::default()
                        })?;
                        reply(
                            &server,
                            dest,
                            &themed(
                                "location_set",
                                &["Noted your location as {location}, {user}. (found {label})"],
                                &[("user", addr), ("location", arg), ("label", &label)],
                            )?,
                        )?;
                    }
                    None => reply(
                        &server,
                        dest,
                        &themed(
                            "location_notfound",
                            &["I couldn't find '{query}', {user}."],
                            &[("user", addr), ("query", arg)],
                        )?,
                    )?,
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn geo_label(g: &GeoResult) -> String {
    let mut parts = vec![g.name.clone()];
    if let Some(a) = &g.admin1 {
        parts.push(a.clone());
    }
    if let Some(c) = &g.country {
        parts.push(c.clone());
    }
    parts.join(", ")
}

fn format_profile(p: &Profile) -> Result<String, Error> {
    let title = p.title.clone().unwrap_or_else(|| "—".into());
    let pronouns = match (&p.pronoun_subject, &p.pronoun_object, &p.pronoun_possessive) {
        (Some(s), Some(o), Some(pp)) => format!("{s}/{o}/{pp}"),
        _ => "—".into(),
    };
    let birthday = p.birthday.clone().unwrap_or_else(|| "—".into());
    let location = p.location_display.clone().unwrap_or_else(|| "—".into());
    let firstseen = ymd(p.created);
    themed(
        "profile",
        &["{user} — title: {title}; pronouns: {pronouns}; birthday: {birthday}; location: {location}; first seen {firstseen}."],
        &[
            ("user", &p.nick),
            ("title", &title),
            ("pronouns", &pronouns),
            ("birthday", &birthday),
            ("location", &location),
            ("firstseen", &firstseen),
        ],
    )
}

// ---- Pure parsing helpers (unit-tested) ----

/// Parse a birthday into normalized `MM-DD` or `MM-DD-YYYY`. Requires at least month + day.
fn parse_birthday(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Numeric: MM-DD[-YYYY] with '-', '/', or '.' separators.
    let nums: Vec<&str> = s
        .split(['-', '/', '.'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    if nums.len() >= 2
        && nums[0].chars().all(|c| c.is_ascii_digit())
        && nums[1].chars().all(|c| c.is_ascii_digit())
    {
        let mo: u32 = nums[0].parse().ok()?;
        let dy: u32 = nums[1].parse().ok()?;
        if !(1..=12).contains(&mo) || !(1..=31).contains(&dy) {
            return None;
        }
        if nums.len() >= 3 {
            if let Ok(yr) = nums[2].parse::<i32>() {
                return Some(format!("{mo:02}-{dy:02}-{yr}"));
            }
        }
        return Some(format!("{mo:02}-{dy:02}"));
    }
    // Month-name form: "March 14", "Mar 14 1990", "14 March".
    let mut mo = None;
    let mut dy = None;
    let mut yr = None;
    for tok in s
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
    {
        if let Some(m) = month_num(&tok.to_lowercase()) {
            mo = Some(m);
        } else if let Ok(n) = tok.parse::<u32>() {
            if n >= 1000 {
                yr = Some(n as i32);
            } else if (1..=31).contains(&n) && dy.is_none() {
                dy = Some(n);
            }
        }
    }
    let (mo, dy) = (mo?, dy?);
    match yr {
        Some(y) => Some(format!("{mo:02}-{dy:02}-{y}")),
        None => Some(format!("{mo:02}-{dy:02}")),
    }
}

fn month_num(s: &str) -> Option<u32> {
    let months = [
        "january",
        "february",
        "march",
        "april",
        "may",
        "june",
        "july",
        "august",
        "september",
        "october",
        "november",
        "december",
    ];
    if s.len() < 3 {
        return None;
    }
    months
        .iter()
        .position(|m| m.starts_with(s) || s.starts_with(&m[..3]))
        .map(|i| i as u32 + 1)
}

/// Parse pronouns: a preset word (he/she/they/it/xe/ze/fae) or a slash-form set (`xe/xem/xyr`).
/// Returns (subject, object, possessive).
fn parse_pronouns(s: &str) -> Option<(String, String, String)> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if s.contains('/') {
        let p: Vec<&str> = s
            .split('/')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .collect();
        return match p.len() {
            0 => None,
            1 => preset(p[0]).or_else(|| Some((p[0].into(), p[0].into(), p[0].into()))),
            2 => Some((p[0].into(), p[1].into(), p[1].into())),
            _ => Some((p[0].into(), p[1].into(), p[2].into())),
        };
    }
    preset(&s)
}

fn preset(w: &str) -> Option<(String, String, String)> {
    let t = match w {
        "he" => ("he", "him", "his"),
        "she" => ("she", "her", "her"),
        "they" => ("they", "them", "their"),
        "it" => ("it", "it", "its"),
        "xe" => ("xe", "xem", "xyr"),
        "ze" => ("ze", "zir", "zir"),
        "fae" => ("fae", "faer", "faer"),
        _ => return None,
    };
    Some((t.0.into(), t.1.into(), t.2.into()))
}

/// Convert a Unix timestamp (seconds) to a `YYYY-MM-DD` date (UTC). Hinnant's civil-from-days.
fn ymd(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn birthdays() {
        assert_eq!(parse_birthday("03-14"), Some("03-14".into()));
        assert_eq!(parse_birthday("3/14"), Some("03-14".into()));
        assert_eq!(parse_birthday("03-14-1990"), Some("03-14-1990".into()));
        assert_eq!(parse_birthday("March 14"), Some("03-14".into()));
        assert_eq!(parse_birthday("Mar 14 1990"), Some("03-14-1990".into()));
        assert_eq!(parse_birthday("14 March"), Some("03-14".into()));
        assert_eq!(parse_birthday("13-40"), None); // bad month/day
        assert_eq!(parse_birthday(""), None);
        assert_eq!(parse_birthday("hello"), None);
    }

    #[test]
    fn pronouns() {
        assert_eq!(
            parse_pronouns("she"),
            Some(("she".into(), "her".into(), "her".into()))
        );
        assert_eq!(
            parse_pronouns("they"),
            Some(("they".into(), "them".into(), "their".into()))
        );
        assert_eq!(
            parse_pronouns("xe/xem/xyr"),
            Some(("xe".into(), "xem".into(), "xyr".into()))
        );
        assert_eq!(
            parse_pronouns("ne/nem"),
            Some(("ne".into(), "nem".into(), "nem".into()))
        );
        assert_eq!(parse_pronouns(""), None);
    }

    #[test]
    fn dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_700_000_000), "2023-11-14");
    }
}
