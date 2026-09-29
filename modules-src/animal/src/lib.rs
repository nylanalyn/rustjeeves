//! `!animal [kind]` — a fresh picture of a capybara, fox, pug, axolotl, or ~150 other animals.
//!
//! The host owns the catalogue and every image source (`animal_image`); this module only names
//! an animal, applies a per-user cooldown, and themes the reply. The old pug module lives on as
//! the `!pug` shortcut, and its achievement carries over.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AnimalImageRequest, AnimalImageResponse,
    AwardStatsRequest, CommandManifest, CommandShortcut, CommandSpec, Event, EventEnvelope,
    MessagePayload, SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{display, reply, themed, timestamp};
use std::cell::RefCell;
use std::collections::BTreeMap;

const DEFAULT_COOLDOWN_SECONDS: i64 = 5;
const MAX_KIND_CHARS: usize = 40;
const MAX_COOLDOWN_ENTRIES: usize = 2_000;

/// Top-level shortcuts: (name, animal). Everything else is `!animal <kind>`.
const SHORTCUTS: &[(&str, &str)] = &[
    ("pug", "pug"),
    ("cat", "cat"),
    ("dog", "dog"),
    ("fox", "fox"),
    ("capy", "capybara"),
    ("capybara", "capybara"),
    ("duck", "duck"),
    ("bunny", "bunny"),
];

#[host_fn]
extern "ExtismHost" {
    fn setting_get(input: String) -> String;
    fn animal_image(input: String) -> String;
    fn award_stats(input: String) -> String;
}

thread_local! {
    /// Last request per (server, profile). Memory only: a reload simply forgets cooldowns.
    static LAST_REQUEST: RefCell<BTreeMap<(String, String), i64>> = const { RefCell::new(BTreeMap::new()) };
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "animal".into(),
            aliases: vec!["animals".into()],
            description:
                "A fresh picture of an animal: capybara, fox, pug, axolotl, and many more.".into(),
            usage: "!animal [kind | list]".into(),
            shortcuts: SHORTCUTS
                .iter()
                .map(|(name, kind)| {
                    CommandShortcut::new(name, kind)
                        .described(&format!("A fresh {kind} picture."), &format!("!{name}"))
                })
                .collect(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "cooldown_seconds".into(),
            description: "Seconds a person waits between animal pictures.".into(),
            default: DEFAULT_COOLDOWN_SECONDS.to_string(),
            kind: SettingKind::DurationSeconds { min: 0, max: 300 },
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    let spec =
        |id: &str, name: &str, description: &str, stat: &str, threshold, optional, secret| {
            AchievementSpec {
                id: id.into(),
                name: name.into(),
                description: description.into(),
                stat: stat.into(),
                threshold,
                optional,
                secret,
            }
        };
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: [
            ("pictures", "Animal pictures requested"),
            ("pugs_requested", "Pug photos requested"),
            ("distinct_animals", "Different animals requested"),
            ("capybaras", "Capybara pictures requested"),
        ]
        .into_iter()
        .map(|(id, description)| AchievementStat {
            id: id.into(),
            description: description.into(),
        })
        .collect(),
        achievements: vec![
            spec(
                "a_friendly_face",
                "A Friendly Face",
                "Request an animal picture.",
                "pictures",
                1,
                false,
                false,
            ),
            spec(
                "menagerie_keeper",
                "Menagerie Keeper",
                "Request 25 animal pictures.",
                "pictures",
                25,
                false,
                false,
            ),
            spec(
                "ark_warden",
                "Ark Warden",
                "Request 100 animal pictures.",
                "pictures",
                100,
                false,
                false,
            ),
            spec(
                "noahs_understudy",
                "Noah's Understudy",
                "Request pictures of 10 different animals.",
                "distinct_animals",
                10,
                false,
                false,
            ),
            spec(
                "field_guide",
                "Field Guide",
                "Request pictures of 50 different animals.",
                "distinct_animals",
                50,
                true,
                false,
            ),
            // Carried over from the retired pug module (same id, same stat).
            spec(
                "pug_enthusiast",
                "Pug Enthusiast",
                "Request 10 pug photos.",
                "pugs_requested",
                10,
                true,
                false,
            ),
            spec(
                "capybara_diplomacy",
                "Capybara Diplomacy",
                "Summon a capybara.",
                "capybaras",
                1,
                true,
                true,
            ),
        ],
        prestige: Vec::new(),
    })?)
}

fn say(
    server: &str,
    target: &str,
    key: &str,
    default: &str,
    vars: &[(&str, &str)],
) -> Result<(), Error> {
    reply(server, target, &themed(key, &[default], vars)?)
}

fn cooldown_seconds(server: &str, channel: Option<&str>) -> Result<i64, Error> {
    let raw = unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: "cooldown_seconds".into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    };
    Ok(raw
        .trim()
        .parse()
        .unwrap_or(DEFAULT_COOLDOWN_SECONDS)
        .clamp(0, 300))
}

/// Seconds still to wait, recording this request when none remain.
fn cooldown_remaining(server: &str, profile_id: &str, now: i64, window: i64) -> i64 {
    let key = (server.to_string(), profile_id.to_string());
    LAST_REQUEST.with(|last| {
        let mut last = last.borrow_mut();
        let remaining = last
            .get(&key)
            .map_or(0, |at| window - now.saturating_sub(*at));
        if remaining <= 0 {
            if last.len() >= MAX_COOLDOWN_ENTRIES {
                last.retain(|_, at| now.saturating_sub(*at) < window);
            }
            last.insert(key, now);
        }
        remaining.max(0)
    })
}

/// "a capybara", "an axolotl", "an otter".
fn with_article(kind: &str) -> String {
    let article = if kind
        .chars()
        .next()
        .is_some_and(|first| "aeiou".contains(first.to_ascii_lowercase()))
    {
        "an"
    } else {
        "a"
    };
    format!("{article} {kind}")
}

fn request(kind: &str, list: bool) -> Result<AnimalImageResponse, Error> {
    let raw = unsafe {
        animal_image(serde_json::to_string(&AnimalImageRequest {
            kind: kind.into(),
            list,
        })?)?
    };
    Ok(serde_json::from_str(&raw)?)
}

fn award(
    server: &str,
    msg: &MessagePayload,
    dest: &str,
    increments: Vec<(&str, u64)>,
    dedup: Option<String>,
) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: dest.into(),
            increments: increments
                .into_iter()
                .map(|(stat, amount)| StatIncrement {
                    stat: stat.into(),
                    amount,
                })
                .collect(),
            deduplication_id: dedup,
        })?)?;
    }
    Ok(())
}

fn handle(server: &str, msg: &MessagePayload, argument: &str) -> Result<(), Error> {
    let dest = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let user = display(msg);
    let kind = argument.split_whitespace().collect::<Vec<_>>().join(" ");
    if kind.eq_ignore_ascii_case("list") {
        // ~150 names: send them privately rather than filling the channel.
        let kinds = request("", true)?.kinds;
        say(
            server,
            &msg.nick,
            "animal.list",
            "I can fetch {count} animals: {animals}",
            &[
                ("count", &kinds.len().to_string()),
                ("animals", &kinds.join(", ")),
            ],
        )?;
        if !msg.is_private {
            say(
                server,
                dest,
                "animal.list_sent",
                "I've sent you the list of animals, {user}.",
                &[("user", user)],
            )?;
        }
        return Ok(());
    }
    if kind.chars().count() > MAX_KIND_CHARS {
        return say(
            server,
            dest,
            "animal.unknown",
            "I don't keep {kind} on the premises, {user}. Try !animal list.",
            &[("kind", "that"), ("user", user)],
        );
    }
    if msg.user_id.is_empty() {
        return say(
            server,
            dest,
            "animal.identity_unavailable",
            "I can't verify your profile right now, {user}; please try again shortly.",
            &[("user", user)],
        );
    }
    let current = timestamp()?;
    let window = cooldown_seconds(server, (!msg.is_private).then_some(msg.target.as_str()))?;
    let remaining = cooldown_remaining(server, &msg.user_id, current, window);
    if remaining > 0 {
        return say(
            server,
            dest,
            "animal.cooldown",
            "The animals need {seconds}s to regroup, {user}.",
            &[("seconds", &remaining.to_string()), ("user", user)],
        );
    }
    let response = request(&kind, false)?;
    let (Some(found), Some(url)) = (response.kind.as_deref(), response.url.as_deref()) else {
        return match response.error.as_deref() {
            Some("unknown") => say(
                server,
                dest,
                "animal.unknown",
                "I don't keep {kind} on the premises, {user}. Try !animal list.",
                &[("kind", &with_article(&kind)), ("user", user)],
            ),
            Some("rate_limited") => say(
                server,
                dest,
                "animal.rate_limited",
                "The animals are rather in demand; do try again in a moment, {user}.",
                &[("user", user)],
            ),
            _ => say(
                server,
                dest,
                "animal.unavailable",
                "The {kind} declined to be photographed just now, {user}.",
                &[
                    ("kind", if kind.is_empty() { "animal" } else { &kind }),
                    ("user", user),
                ],
            ),
        };
    };
    let emoji = response.emoji.as_deref().unwrap_or("🐾");
    say(
        server,
        dest,
        "animal.picture",
        "{user}: {emoji} {a_kind}! {url}",
        &[
            ("user", user),
            ("emoji", emoji),
            ("kind", found),
            ("a_kind", &with_article(found)),
            ("url", url),
        ],
    )?;
    let mut increments = vec![("pictures", 1)];
    match found {
        "pug" => increments.push(("pugs_requested", 1)),
        "capybara" => increments.push(("capybaras", 1)),
        _ => {}
    }
    award(server, msg, dest, increments, None)?;
    award(
        server,
        msg,
        dest,
        vec![("distinct_animals", 1)],
        Some(format!("distinct:{found}")),
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let (command, argument) = text
        .split_once(char::is_whitespace)
        .map(|(command, argument)| (command, argument.trim()))
        .unwrap_or((text, ""));
    // The host rewrites `!animals` and the shortcuts (`!capy` → `!animal capybara`).
    if command == "!animal" {
        handle(&env.server, &msg, argument)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn articles_follow_the_first_letter() {
        assert_eq!(with_article("capybara"), "a capybara");
        assert_eq!(with_article("axolotl"), "an axolotl");
        assert_eq!(with_article("otter"), "an otter");
    }

    #[test]
    fn cooldowns_hold_per_person() {
        assert_eq!(cooldown_remaining("net", "a", 100, 5), 0);
        assert_eq!(cooldown_remaining("net", "a", 102, 5), 3);
        assert_eq!(cooldown_remaining("net", "b", 102, 5), 0);
        assert_eq!(cooldown_remaining("net", "a", 105, 5), 0);
    }

    #[test]
    fn shortcuts_are_unique() {
        let mut names = SHORTCUTS.iter().map(|(name, _)| *name).collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), SHORTCUTS.len());
    }
}
