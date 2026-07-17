//! A tiny `!pug` command: pug.im chooses a fresh photo every time its link is opened.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, SendMessage, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
};

const PUG_URL: &str = "https://pug.im";

#[host_fn]
extern "ExtismHost" {
    fn award_stats(input: String) -> String;
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![AchievementStat {
            id: "pugs_requested".into(),
            description: "Pug photos requested".into(),
        }],
        achievements: vec![AchievementSpec {
            id: "pug_enthusiast".into(),
            name: "Pug Enthusiast".into(),
            description: "Request 10 pug photos.".into(),
            stat: "pugs_requested".into(),
            threshold: 10,
            optional: true,
            secret: false,
        }],
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&command_manifest())?)
}

fn command_manifest() -> CommandManifest {
    CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "pug".into(),
            aliases: Vec::new(),
            description: "Get a link to a random pug photo.".into(),
            usage: "!pug".into(),
        }],
    }
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).into(), (*value).into()))
                .collect(),
        })?)?
    })
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

fn award(server: &str, profile_id: &str, display_name: &str, target: &str) -> Result<(), Error> {
    if profile_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "pugs_requested".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?
    };
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    if !matches!(msg.text.trim(), "!pug") {
        return Ok(());
    }

    let target = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let caller = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let text = themed(
        "pug.link",
        &["{user}: a fresh pug photo awaits: {url}"],
        &[("user", caller), ("url", PUG_URL)],
    )?;
    reply(&env.server, target, &text)?;
    award(&env.server, &msg.user_id, caller, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_manifest_describes_pug() {
        let manifest = command_manifest();
        assert_eq!(manifest.commands.len(), 1);
        assert_eq!(manifest.commands[0].name, "pug");
        assert_eq!(manifest.commands[0].usage, "!pug");
    }
}
