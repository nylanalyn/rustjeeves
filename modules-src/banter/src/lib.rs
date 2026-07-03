//! Opt-in channel banter for two small IRC rituals.
//!
//! - A whole-word `sail` from the configured sailor nick gets a sailing response.
//! - A whole-word `caw` or `kaw` from anyone gets a piece of crow lore.
//!
//! At most one response is sent per message, with sailing taking precedence. The module is
//! disabled by default and has separate per-channel cooldowns for the two triggers.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, Event, EventEnvelope,
    KvGet, KvSet, SendMessage, ServerQuery, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    SETTINGS_MANIFEST_VERSION,
};

const SAILING_LINES: &[&str] = &[
    "A touch of weather helm is conversation, {user}; an armful is merely drag.",
    "The leeward telltales have rendered their verdict, {user}: ease a fraction and let them fly.",
    "Reef while it is still a tactical decision, {user}, not an athletic emergency.",
    "Velocity made good is the honest measure, {user}; pointing prettily is not the same as arriving.",
    "In the gust, traveler down before mainsheet out, {user}; preserve the leech before surrendering shape.",
    "The apparent wind always creeps forward as the boat accelerates, {user}. Trim for the wind you have made.",
    "A clean bottom and a quiet helm win arguments long before the start gun, {user}.",
    "Keep the slot breathing, {user}; a strangled leeward side helps neither main nor headsail.",
    "The sea has accepted your sail plan, {user}, subject to the usual amendments in wind and judgment.",
    "A fair lead, a fair line, and no unnecessary turns around the winch, {user}. Civilization endures.",
    "Tension the halyard for the luff you need, {user}; wrinkles are instruments, not decorations.",
    "Downwind, sail the pressure rather than the compass, {user}; the shortest line is often the slow one.",
    "The vang is attending to twist, {user}; one trusts the boom will now behave like a gentleman.",
    "Current is a moving racecourse, {user}. Laylines drawn on the land are works of fiction.",
    "When in doubt, {user}, make the boat fast before attempting to make it clever.",
    "The favored tack is temporary, {user}; pressure and shift remain the more durable acquaintances.",
    "A smooth tack begins before the helm moves, {user}: speed first, turn second, trim throughout.",
    "One hand for the vessel and one for yourself, {user}; the sea is unimpressed by misplaced confidence.",
    "The compass says header, the water says current, and the telltales say trim, {user}. Hear all three.",
    "There is no shame in easing six millimetres, {user}. There is considerable shame in stalling the foil.",
];

const CROW_LINES: &[&str] = &[
    "The murder hears you, {user}. The murder hears, and requests shiny things.",
    "A black feather has been placed beside your name in the ledger, {user}. This is probably favorable.",
    "The crows acknowledge your call, {user}. Their reply is delayed by committee.",
    "Three crows have convened on the eastern wire, {user}. None will disclose the agenda.",
    "Your message has entered the rookery, {user}. Expect judgment at dusk.",
    "The eldest crow tilts its head, {user}. You have either impressed it or incurred a small debt.",
    "A distant wingbeat answers, {user}. The murder is awake now.",
    "The crows repeat your name softly, {user}, testing how it sounds in prophecy.",
    "One crow brings a button, another a warning, {user}. You may choose only one.",
    "The rooftop parliament recognizes the delegate from {user}.",
    "A crow has added your call to the old songs, {user}. The rhyme is ominously good.",
    "The murder approves, {user}, though the minutes will record several tasteful objections.",
    "Something black-winged has carried your words beyond the treeline, {user}.",
    "The crows know what you meant, {user}. Regrettably, they also know what you did not say.",
    "A walnut has been left at the threshold for you, {user}. Crow diplomacy proceeds apace.",
    "The western murder answers in kind, {user}. The eastern murder claims prior art.",
    "Seven bright eyes turn toward you, {user}. The eighth is watching something behind you.",
    "Your call was acceptable, {user}. The crows will permit the sun to set on schedule.",
    "The rookery stirs, {user}. Somewhere, a small and ceremonial key has changed hands.",
    "The murder has heard your petition, {user}. Tribute may be paid in peanuts or secrets.",
];

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn bot_nick(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let mut achievements = [
        ("murder_acquaintance", "Murder Acquaintance", 1),
        ("rookery_regular", "Rookery Regular", 25),
        ("corvid_attache", "Corvid Attaché", 100),
    ]
    .into_iter()
    .map(|(id, name, threshold)| AchievementSpec {
        id: id.into(),
        name: name.into(),
        description: format!("Receive {threshold} crow responses."),
        stat: "crow_responses".into(),
        threshold,
        optional: false,
        secret: false,
    })
    .collect::<Vec<_>>();
    achievements.push(AchievementSpec {
        id: "abaft_banter".into(),
        name: "Abaft the Banter".into(),
        description: "Trigger the configured sailing ritual.".into(),
        stat: "sailing_ritual".into(),
        threshold: 1,
        optional: true,
        secret: true,
    });
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: vec![
            AchievementStat {
                id: "crow_responses".into(),
                description: "Crow responses".into(),
            },
            AchievementStat {
                id: "sailing_ritual".into(),
                description: "Sailing ritual responses".into(),
            },
        ],
        achievements,
        prestige: Vec::new(),
    })?)
}

fn award(server: &str, message: &jeeves_abi::MessagePayload, stat: &str) -> Result<(), Error> {
    if message.user_id.is_empty() {
        return Ok(());
    }
    let display = if message.display.is_empty() {
        &message.nick
    } else {
        &message.display
    };
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: message.user_id.clone(),
            display_name: display.clone(),
            target: message.target.clone(),
            increments: vec![StatIncrement {
                stat: stat.into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let scopes = || {
        vec![
            SettingScope::Global,
            SettingScope::Network,
            SettingScope::Channel,
        ]
    };
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether sailing and crow call-and-response is active here.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "sailor_nick".into(),
                description: "Nickname whose whole-word 'sail' triggers sailing banter.".into(),
                default: "witeshark2".into(),
                kind: SettingKind::String { max_len: 32 },
                scopes: scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "sailing_cooldown_seconds".into(),
                description: "Minimum delay between sailing responses in one channel.".into(),
                default: "15".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: scopes(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "crow_cooldown_seconds".into(),
                description: "Minimum delay between crow responses in one channel.".into(),
                default: "8".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: scopes(),
                applies_immediately: true,
            },
        ],
    })?)
}

fn setting(key: &str, server: &str, channel: &str) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: Some(channel.into()),
        })?)?
    })
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

fn contains_word(text: &str, expected: &[&str]) -> bool {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            expected
                .iter()
                .any(|expected| word.eq_ignore_ascii_case(expected))
        })
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()
}

fn cooldown_key(kind: &str, server: &str, channel: &str) -> String {
    format!("cooldown:{kind}:{}:{}", encode(server), encode(channel))
}

fn cooldown_ready(
    kind: &str,
    setting_key: &str,
    server: &str,
    channel: &str,
) -> Result<bool, Error> {
    let cooldown = setting(setting_key, server, channel)?
        .parse::<i64>()
        .unwrap_or(0)
        .clamp(0, 3_600);
    if cooldown == 0 {
        return Ok(true);
    }
    let current = unsafe { now(String::new())? }.parse::<i64>().unwrap_or(0);
    let key = cooldown_key(kind, server, channel);
    let previous = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.clone() })?)? }
        .parse::<i64>()
        .unwrap_or(0);
    if current <= 0 || current.saturating_sub(previous) < cooldown {
        return Ok(false);
    }
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key,
            value: current.to_string(),
        })?)?
    };
    Ok(true)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let envelope: EventEnvelope = serde_json::from_str(&input)?;
    let server = envelope.server;
    let Event::Message(message) = envelope.event else {
        return Ok(());
    };
    if message.is_private || setting("enabled", &server, &message.target)? != "true" {
        return Ok(());
    }
    let sailing = if contains_word(&message.text, &["sail"]) {
        let sailor = setting("sailor_nick", &server, &message.target)?;
        message.nick.eq_ignore_ascii_case(sailor.trim())
    } else {
        false
    };
    let crow = contains_word(&message.text, &["caw", "kaw"]);
    if !sailing && !crow {
        return Ok(());
    }
    let own_nick = unsafe {
        bot_nick(serde_json::to_string(&ServerQuery {
            server: server.clone(),
        })?)?
    };
    if message.nick.eq_ignore_ascii_case(&own_nick) {
        return Ok(());
    }
    let user = if message.display.is_empty() {
        message.nick.as_str()
    } else {
        message.display.as_str()
    };

    if sailing {
        if cooldown_ready(
            "sailing",
            "sailing_cooldown_seconds",
            &server,
            &message.target,
        )? {
            let response = themed("sailing", SAILING_LINES, &[("user", user)])?;
            reply(&server, &message.target, &response)?;
            award(&server, &message, "sailing_ritual")?;
        }
        return Ok(());
    }

    if crow && cooldown_ready("crow", "crow_cooldown_seconds", &server, &message.target)? {
        let response = themed("crow", CROW_LINES, &[("user", user)])?;
        reply(&server, &message.target, &response)?;
        award(&server, &message, "crow_responses")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_trigger_words_anywhere_and_ignores_substrings() {
        assert!(contains_word("we should SAIL! now", &["sail"]));
        assert!(contains_word("well... cAw, then", &["caw", "kaw"]));
        assert!(contains_word("KAW!", &["caw", "kaw"]));
        assert!(!contains_word("sailing is pleasant", &["sail"]));
        assert!(!contains_word("because awkward", &["caw", "kaw"]));
    }

    #[test]
    fn cooldown_keys_are_partitioned_by_kind_network_and_channel() {
        assert_ne!(
            cooldown_key("crow", "net", "#one"),
            cooldown_key("sailing", "net", "#one")
        );
        assert_ne!(
            cooldown_key("crow", "net", "#one"),
            cooldown_key("crow", "net", "#two")
        );
        assert_ne!(
            cooldown_key("crow", "net-a", "#one"),
            cooldown_key("crow", "net-b", "#one")
        );
    }

    #[test]
    fn response_pools_are_bounded_and_have_twenty_variants() {
        assert_eq!(SAILING_LINES.len(), 20);
        assert_eq!(CROW_LINES.len(), 20);
        assert!(SAILING_LINES.iter().all(|line| line.contains("{user}")));
        assert!(CROW_LINES.iter().all(|line| line.contains("{user}")));
        assert!(SAILING_LINES.iter().all(|line| line.chars().count() <= 220));
        assert!(CROW_LINES.iter().all(|line| line.chars().count() <= 220));
    }
}
