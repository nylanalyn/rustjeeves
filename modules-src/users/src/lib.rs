//! User profiles module for rustjeeves.
//!
//! Exposes commands to set personal
//! info: `!title`, `!birthday`, `!pronouns`, `!location`, and `!whoami` / `!profile` to read it.
//! Profiles live in the host-level profile store (shared, so a future weather module can read the
//! location). All replies go through the theme system.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandShortcut, CommandSpec, CosmeticsWornRequest, Event, EventEnvelope, GeoQuery, GeoResult,
    Profile, ProfileClear, ProfileKey, ProfileUpdate, StatIncrement, WornCosmetics,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
};
use jeeves_guest::{reply, themed};

#[host_fn]
extern "ExtismHost" {
    fn profile_get(input: String) -> String;
    fn profile_set(input: String) -> String;
    fn profile_clear(input: String) -> String;
    fn geocode(input: String) -> String;
    fn award_stats(input: String) -> String;
    fn cosmetics_worn(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let specs = [
        ("known_house", "Known to the House", "own_views", false),
        (
            "properly_addressed",
            "Properly Addressed",
            "title_set",
            true,
        ),
        (
            "introductions_made",
            "Introductions Made",
            "pronouns_set",
            true,
        ),
        ("on_the_map", "On the Map", "location_set", true),
        ("happy_returns", "Many Happy Returns", "birthday_set", true),
        (
            "dossier_complete",
            "Dossier Complete",
            "complete_profile",
            true,
        ),
    ];
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: [
            "own_views",
            "title_set",
            "pronouns_set",
            "location_set",
            "birthday_set",
            "complete_profile",
        ]
        .into_iter()
        .map(|id| AchievementStat {
            id: id.into(),
            description: id.replace('_', " "),
        })
        .collect(),
        achievements: specs
            .into_iter()
            .map(|(id, name, stat, optional)| AchievementSpec {
                id: id.into(),
                name: name.into(),
                description: match stat {
                    "own_views" => "View your own saved profile.".into(),
                    "title_set" => "Save a courtesy title.".into(),
                    "pronouns_set" => "Save your pronouns.".into(),
                    "location_set" => "Save a location.".into(),
                    "birthday_set" => "Save a birthday.".into(),
                    _ => "Save a title, pronouns, location, and birthday.".into(),
                },
                stat: stat.into(),
                threshold: 1,
                optional,
                secret: false,
            })
            .collect(),
        prestige: Vec::new(),
    })?)
}

fn award(
    server: &str,
    profile_id: &str,
    display_name: &str,
    target: &str,
    stats: &[&str],
) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: stats
                .iter()
                .map(|stat| StatIncrement {
                    stat: (*stat).into(),
                    amount: 1,
                })
                .collect(),
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    let mut whoami = command(
        "whoami",
        "Show a stored user profile, or clear one of your fields.",
        "!whoami [nick] | !whoami clear <field>",
    );
    whoami.aliases = vec!["profile".into()];
    whoami.shortcuts = vec![CommandShortcut::new("clear", "clear").described(
        "Clear a profile field: title, birthday, pronouns, or location.",
        "!clear <field>",
    )];
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

fn detail_stats(
    server: &str,
    nick: &str,
    changed: &'static str,
) -> Result<Vec<&'static str>, Error> {
    let mut stats = vec![changed];
    if get_profile(server, nick)?.is_some_and(|p| {
        p.title.is_some()
            && p.birthday.is_some()
            && p.pronoun_subject.is_some()
            && p.location_display.is_some()
    }) {
        stats.push("complete_profile");
    }
    Ok(stats)
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

    // Profiles are created and `last_seen` refreshed by the host's resolver before dispatch.
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

    // `!clear <field>` is a host shortcut for `!whoami clear <field>` (and `!profile` an alias of
    // `!whoami`); a raw `!clear` only arrives when an operator has freed that name.
    if cmd == "!clear" {
        return Ok(());
    }
    let (cmd, arg) = match arg.split_once(char::is_whitespace) {
        Some((word, rest)) if cmd == "!whoami" && word.eq_ignore_ascii_case("clear") => {
            ("!clear", rest.trim())
        }
        None if cmd == "!whoami" && arg.eq_ignore_ascii_case("clear") => ("!clear", ""),
        _ => (cmd, arg),
    };

    match cmd {
        "!whoami" => {
            let target = if arg.is_empty() { nick } else { arg };
            match get_profile(&server, target)? {
                Some(p) => {
                    let badge = worn_badge(&server, &p.id)?;
                    reply(&server, dest, &format_profile(&p, badge.as_deref())?)?;
                    if arg.is_empty() {
                        award(&server, &msg.user_id, addr, dest, &["own_views"])?;
                    }
                }
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
                let stats = detail_stats(&server, nick, "title_set")?;
                award(&server, &msg.user_id, addr, dest, &stats)?;
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
                        &[("user", addr), ("birthday", &pretty_birthday(&bd))],
                    )?,
                )?;
                let stats = detail_stats(&server, nick, "birthday_set")?;
                award(&server, &msg.user_id, addr, dest, &stats)?;
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
                let stats = detail_stats(&server, nick, "pronouns_set")?;
                award(&server, &msg.user_id, addr, dest, &stats)?;
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
                            timezone: Some(g.timezone.clone()),
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
                        let stats = detail_stats(&server, nick, "location_set")?;
                        award(&server, &msg.user_id, addr, dest, &stats)?;
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

/// The badge this profile wears, if any (cosmetics come from gacha eggs).
fn worn_badge(server: &str, profile_id: &str) -> Result<Option<String>, Error> {
    let raw = unsafe {
        cosmetics_worn(serde_json::to_string(&CosmeticsWornRequest {
            server: server.into(),
            profile_ids: vec![profile_id.into()],
        })?)?
    };
    let worn: Vec<WornCosmetics> = serde_json::from_str(&raw)?;
    Ok(worn.into_iter().next().and_then(|worn| worn.badge))
}

fn format_profile(p: &Profile, badge: Option<&str>) -> Result<String, Error> {
    let title = p.title.clone().unwrap_or_else(|| "—".into());
    let pronouns = match (&p.pronoun_subject, &p.pronoun_object, &p.pronoun_possessive) {
        (Some(s), Some(o), Some(pp)) => format!("{s}/{o}/{pp}"),
        _ => "—".into(),
    };
    let birthday = p
        .birthday
        .as_deref()
        .map(pretty_birthday)
        .unwrap_or_else(|| "—".into());
    let location = p.location_display.clone().unwrap_or_else(|| "—".into());
    let firstseen = ymd(p.created);
    themed(
        "profile",
        &["{user} — title: {title}; pronouns: {pronouns}; birthday: {birthday}; location: {location}; first seen {firstseen}."],
        &[
            // `{user}` carries the worn badge ("🦉 alice"); `{name}` and `{badge}` are separate.
            (
                "user",
                &badge.map_or_else(|| p.nick.clone(), |badge| format!("{badge} {}", p.nick)),
            ),
            ("name", &p.nick),
            ("badge", badge.unwrap_or("")),
            ("title", &title),
            ("pronouns", &pronouns),
            ("birthday", &birthday),
            ("location", &location),
            ("firstseen", &firstseen),
        ],
    )
}

// ---- Pure parsing helpers (unit-tested) ----

/// Parse a birthday into normalized `MM-DD` or `MM-DD-YYYY`. Requires at least month + day and
/// rejects dates that don't exist (the host would refuse them anyway).
fn parse_birthday(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Numeric: MM-DD[-YYYY] or ISO YYYY-MM-DD, with '-', '/', or '.' separators.
    let nums: Vec<&str> = s
        .split(['-', '/', '.'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    if nums.len() >= 2 && nums.iter().all(|x| x.chars().all(|c| c.is_ascii_digit())) {
        let (mo, dy, yr) = if nums[0].len() == 4 {
            // ISO order: YYYY-MM-DD.
            if nums.len() != 3 {
                return None;
            }
            (
                nums[1].parse().ok()?,
                nums[2].parse().ok()?,
                nums[0].parse().ok(),
            )
        } else {
            let yr = match nums.get(2) {
                Some(yr) if yr.len() == 4 => Some(yr.parse().ok()?),
                Some(_) => return None,
                None => None,
            };
            (nums[0].parse().ok()?, nums[1].parse().ok()?, yr)
        };
        return format_birthday(mo, dy, yr);
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
            if (1000..=9999).contains(&n) {
                yr = Some(n as i32);
            } else if (1..=31).contains(&n) && dy.is_none() {
                dy = Some(n);
            }
        }
    }
    format_birthday(mo?, dy?, yr)
}

/// Validate a calendar date and render the stored `MM-DD[-YYYY]` form. Without a year, Feb 29
/// is allowed.
fn format_birthday(mo: u32, dy: u32, yr: Option<i32>) -> Option<String> {
    let leap = yr.is_none_or(|y| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0);
    let days = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days).contains(&dy) || yr.is_some_and(|y| !(1000..=9999).contains(&y)) {
        return None;
    }
    Some(match yr {
        Some(y) => format!("{mo:02}-{dy:02}-{y}"),
        None => format!("{mo:02}-{dy:02}"),
    })
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// Render a stored `MM-DD[-YYYY]` birthday unambiguously ("March 5" / "March 5, 1990"), so a
/// day-first typist notices when their date was read month-first.
fn pretty_birthday(stored: &str) -> String {
    let parts: Vec<&str> = stored.split('-').collect();
    let month = parts
        .first()
        .and_then(|m| m.parse::<usize>().ok())
        .and_then(|m| MONTHS.get(m.wrapping_sub(1)));
    let day = parts.get(1).and_then(|d| d.parse::<u32>().ok());
    match (month, day, parts.get(2)) {
        (Some(month), Some(day), Some(year)) => format!("{month} {day}, {year}"),
        (Some(month), Some(day), None) => format!("{month} {day}"),
        _ => stored.to_string(),
    }
}

/// Match a full or abbreviated (3+ letter) month name, e.g. "mar", "sept", "december".
fn month_num(s: &str) -> Option<u32> {
    if s.len() < 3 {
        return None;
    }
    MONTHS
        .iter()
        .position(|m| m.to_lowercase().starts_with(s))
        .map(|i| i as u32 + 1)
}

/// Parse pronouns: a preset word (he/she/they/it/xe/ze/fae) or a slash-form set (`xe/xem/xyr`).
/// Returns (subject, object, possessive).
///
/// The common short forms `he/him`, `she/her`, `they/them` name a preset, so the missing
/// possessive comes from it. Mixed sets such as `she/they` start with a preset subject too; the
/// profile holds one grammatical set, so the first one is used for sentences about the person.
fn parse_pronouns(s: &str) -> Option<(String, String, String)> {
    let s = s.trim().to_lowercase();
    if s.is_empty() {
        return None;
    }
    if !s.contains('/') {
        return preset(&s);
    }
    let p: Vec<&str> = s
        .split('/')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .collect();
    match p.as_slice() {
        [] => None,
        [one] => preset(one).or_else(|| Some((one.to_string(), one.to_string(), one.to_string()))),
        // A known subject with a short form (he/him, they/them, she/they): trust the preset.
        [subject, _] if preset(subject).is_some() => preset(subject),
        [subject, object] => Some((subject.to_string(), object.to_string(), object.to_string())),
        [subject, object, possessive, ..] => Some((
            subject.to_string(),
            object.to_string(),
            possessive.to_string(),
        )),
    }
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
        assert_eq!(parse_birthday("1990-03-14"), Some("03-14-1990".into()));
        assert_eq!(parse_birthday("02-31"), None);
        assert_eq!(parse_birthday("02-29"), Some("02-29".into()));
        assert_eq!(parse_birthday("02-29-1991"), None);
        assert_eq!(parse_birthday("02-29-1992"), Some("02-29-1992".into()));
        assert_eq!(parse_birthday("03-14-90"), None);
        assert_eq!(parse_birthday("marvel 14"), None);
        assert_eq!(parse_birthday("Sept 3"), Some("09-03".into()));
        assert_eq!(pretty_birthday("03-05"), "March 5");
        assert_eq!(pretty_birthday("03-05-1990"), "March 5, 1990");
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
        assert_eq!(
            parse_pronouns("he/him"),
            Some(("he".into(), "him".into(), "his".into()))
        );
        assert_eq!(
            parse_pronouns("They/Them"),
            Some(("they".into(), "them".into(), "their".into()))
        );
        assert_eq!(
            parse_pronouns("she/they"),
            Some(("she".into(), "her".into(), "her".into()))
        );
        assert_eq!(parse_pronouns(""), None);
    }

    #[test]
    fn dates() {
        assert_eq!(ymd(0), "1970-01-01");
        assert_eq!(ymd(1_700_000_000), "2023-11-14");
    }
}
