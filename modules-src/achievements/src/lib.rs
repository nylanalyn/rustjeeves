use extism_pdk::*;
use jeeves_abi::{
    AchievementProfileSummary, AchievementsGetRequest, CommandManifest, CommandSpec, Event,
    EventEnvelope, Profile, ProfileKey, SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION,
};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn achievements_get(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "achievements".into(),
            aliases: vec!["ach".into()],
            description: "Show achievement collections and progress.".into(),
            usage: "!achievements [nick] | !achievements list [module]".into(),
        }],
    })?)
}

fn themed(key: &str, default: &str, vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: vec![default.into()],
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn reply(server: &str, target: &str, text: String) -> Result<(), Error> {
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: target.into(),
            text,
        })?)?;
    }
    Ok(())
}

fn prestige_name(name: &str, rank: u64) -> String {
    if rank <= 1 {
        return name.into();
    }
    let mut value = rank.min(3_999);
    let mut numeral = String::new();
    for (number, text) in [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ] {
        while value >= number {
            value -= number;
            numeral.push_str(text);
        }
    }
    format!("{name} {numeral}")
}

fn chunks(parts: Vec<String>, max_chars: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for part in parts {
        let separator = if current.is_empty() { "" } else { "; " };
        if !current.is_empty()
            && current.chars().count() + separator.len() + part.chars().count() > max_chars
        {
            lines.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("; ");
        }
        current.extend(part.chars().take(max_chars));
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut words = msg.text.split_whitespace();
    if !matches!(words.next(), Some("!achievements" | "!ach")) {
        return Ok(());
    }
    let dest = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let first = words.next();
    let module = if first == Some("list") {
        words.next().map(str::to_string)
    } else {
        None
    };
    let requested_nick = if first.is_some() && first != Some("list") {
        first.unwrap()
    } else {
        &msg.nick
    };
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: env.server.clone(),
            nick: requested_nick.into(),
        })?)?
    };
    if raw.is_empty() {
        reply(
            &env.server,
            dest,
            themed(
                "achievements.unknown",
                "No profile is known for {user}.",
                &[("user", requested_nick)],
            )?,
        )?;
        return Ok(());
    }
    let profile: Profile = serde_json::from_str(&raw)?;
    let request = if first == Some("list") {
        AchievementsGetRequest::Catalog {
            server: env.server.clone(),
            profile_id: Some(profile.id),
            module: module.clone(),
        }
    } else {
        AchievementsGetRequest::Profile {
            server: env.server.clone(),
            profile_id: profile.id,
        }
    };
    let summary: AchievementProfileSummary =
        serde_json::from_str(&unsafe { achievements_get(serde_json::to_string(&request)?)? })?;
    if first == Some("list") {
        let parts = if let Some(selected) = module.as_deref() {
            match summary
                .modules
                .iter()
                .find(|entry| entry.module == selected)
            {
                Some(entry) => std::iter::once(format!(
                    "{} {}/{}",
                    entry.module, entry.earned, entry.available
                ))
                .chain(
                    entry
                        .achievements
                        .iter()
                        .map(|item| {
                            if item.earned {
                                format!("✓ {}", item.name)
                            } else if item.secret {
                                "? Undiscovered secret".into()
                            } else {
                                format!("· {} {}/{}", item.name, item.current, item.threshold)
                            }
                        })
                        .chain(
                            entry
                                .prestige
                                .iter()
                                .map(|rank| format!("★ {}", prestige_name(&rank.name, rank.rank))),
                        ),
                )
                .collect::<Vec<_>>(),
                None => vec![format!("Unknown achievement module: {selected}")],
            }
        } else {
            summary
                .modules
                .iter()
                .map(|m| format!("{} {}/{}", m.module, m.earned, m.available))
                .collect::<Vec<_>>()
        };
        for modules in chunks(parts, 330) {
            reply(
                &env.server,
                dest,
                themed(
                    "achievements.list",
                    "{user}: {modules}",
                    &[("user", requested_nick), ("modules", &modules)],
                )?,
            )?;
        }
        return Ok(());
    }
    let recent = summary
        .recent
        .iter()
        .take(3)
        .map(|u| u.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let closest = summary
        .closest
        .iter()
        .take(3)
        .map(|p| format!("{} {}/{}", p.name, p.current, p.threshold))
        .collect::<Vec<_>>()
        .join("; ");
    let prestige = summary
        .modules
        .iter()
        .flat_map(|module| module.prestige.iter())
        .take(3)
        .map(|rank| prestige_name(&rank.name, rank.rank))
        .collect::<Vec<_>>()
        .join(", ");
    let closest = if prestige.is_empty() {
        closest
    } else if closest.is_empty() {
        format!("Prestige: {prestige}")
    } else {
        format!("{closest}; Prestige: {prestige}")
    };
    reply(
        &env.server,
        dest,
        themed(
            "achievements.summary",
            "{user}: {earned}/{available} collected. Recent: {recent}. Closest: {closest}.",
            &[
                ("user", requested_nick),
                ("earned", &summary.earned.to_string()),
                ("available", &summary.available.to_string()),
                ("recent", &recent),
                ("closest", &closest),
            ],
        )?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_chunks_are_bounded_without_dropping_entries() {
        let lines = chunks(vec!["one".into(), "two".into(), "three".into()], 8);
        assert_eq!(lines, ["one; two", "three"]);
        assert!(lines.iter().all(|line| line.chars().count() <= 8));
    }

    #[test]
    fn prestige_one_omits_the_numeral() {
        assert_eq!(prestige_name("Master", 1), "Master");
        assert_eq!(prestige_name("Master", 4), "Master IV");
    }
}
