//! Oddities: achievements that mean nothing and arrive for no reason.
//!
//! Where `enabled` is on (off by default, per channel), the network keeps one hidden due time. Once
//! it passes, every ordinary channel line has a `claim_odds` chance of being the one, and its
//! speaker unlocks a random oddity they don't hold yet ("BEEEEEEES", "A Dark Shadow Behind Their
//! Head"); the host announces it like any other unlock. The next due time is then drawn at random
//! so that about `per_week` turn up a week across the whole network. Because each line is a ticket,
//! people who talk more are likelier to be picked, and since the draw waits for the room to be
//! talking, nobody wins by being first in after a quiet night.
//!
//! The only stored state is each network's due time, so there are no data hooks; the unlocks are
//! host-owned achievement rows. Oddities are secret and optional: masked until found, and never
//! required for catalogue completion.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementProfileSummary, AchievementSpec, AchievementStat,
    AchievementsGetRequest, AwardStatsRequest, AwardStatsResponse, Event, EventEnvelope,
    MessagePayload, SettingKind, SettingScope, SettingSpec, SettingsManifest, StatIncrement,
    ACHIEVEMENT_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{display, encode, kv_load, kv_save, setting_i64, timestamp, Entropy};

const MODULE: &str = "oddities";
const HOUR: i64 = 3_600;
const WEEK: i64 = 7 * 24 * HOUR;
const DEFAULT_PER_WEEK: i64 = 2;
const DEFAULT_CLAIM_ODDS: i64 = 15;
/// No two oddities on a network land closer together than this.
const MIN_GAP: i64 = 6 * HOUR;
/// Cuts off the exponential's long tail so a network never waits more than this many average gaps.
const MAX_GAP_MEANS: f64 = 3.0;

/// (id, name, description). The id is also the stat; each unlocks at 1. Names are announced as
/// "{user} unlocked {name}.", so they carry no closing punctuation of their own.
const ODDITIES: &[(&str, &str, &str)] = &[
    ("bees", "BEEEEEEES", "The bees have chosen."),
    (
        "shadow",
        "A Dark Shadow Behind Their Head",
        "Don't turn around.",
    ),
    ("soup", "The Concept of Soup", "Not soup itself. The idea."),
    ("tooth", "A Tooth (Not Theirs)", "Whose, then?"),
    (
        "pigeons",
        "Seventeen Pigeons in a Trenchcoat",
        "They would like a job.",
    ),
    ("colours", "The Ability to Hear Colours", "Teal is so loud."),
    (
        "door",
        "A Door That Wasn't There Yesterday",
        "It is slightly ajar.",
    ),
    ("moist", "Moist", "Simply that."),
    (
        "socks",
        "Every Lost Sock, At Once",
        "They were all together.",
    ),
    (
        "hum",
        "A Small, Persistent Hum",
        "It gets louder when you listen.",
    ),
    ("moon", "Eye Contact with the Moon", "It looked away first."),
    ("lasagna", "Forbidden Lasagna", "Do not reheat it."),
    (
        "ghost",
        "A Ghost Who Is Mostly Fine About It",
        "Some days are harder than others.",
    ),
    ("cat", "One (1) Wet Cat", "Furious, but present."),
    ("thursday", "The Last Thursday", "There will be no more."),
    ("walls", "Something in the Walls", "It knows your name now."),
    (
        "ducks",
        "An Unsettling Number of Ducks",
        "Count them. Again.",
    ),
    (
        "letter",
        "A Strongly Worded Letter from the Sea",
        "It wants its shells back.",
    ),
    ("spoon", "Knowledge of the Spoon", "You cannot unknow it."),
    (
        "hat",
        "The Hat That Watches",
        "It fits perfectly. That's the problem.",
    ),
    (
        "crabs",
        "A Silent Crab Rave",
        "The crabs dance without music.",
    ),
    (
        "reflection",
        "A Reflection That Blinks Late",
        "Just a little late.",
    ),
    (
        "basement",
        "The Rights to the Basement",
        "You don't have a basement.",
    ),
    (
        "ladder",
        "A Very Long Ladder Going Down",
        "Nobody has seen the bottom.",
    ),
    (
        "voice",
        "Grandma's Other Voice",
        "The one she uses at night.",
    ),
    ("toast", "Toast, Briefly Sentient", "It had so much to say."),
];

#[host_fn]
extern "ExtismHost" {
    fn award_stats(input: String) -> String;
    fn achievements_get(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Let lines here occasionally unlock a random oddity.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "per_week".into(),
                description: "Average oddities unlocked a week across a network.".into(),
                default: DEFAULT_PER_WEEK.to_string(),
                kind: SettingKind::Integer { min: 1, max: 28 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: false,
            },
            SettingSpec {
                key: "claim_odds".into(),
                description: "Once one is due, each line has a 1-in-this chance of claiming it."
                    .into(),
                default: DEFAULT_CLAIM_ODDS.to_string(),
                kind: SettingKind::Integer { min: 1, max: 1_000 },
                scopes: vec![SettingScope::Global, SettingScope::Network],
                applies_immediately: true,
            },
        ],
    })?)
}

#[plugin_fn]
pub fn achievements(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        catalog_version: 1,
        stats: ODDITIES
            .iter()
            // Named vaguely, since stats can show where the secret achievement names don't.
            .map(|(id, _, _)| AchievementStat {
                id: (*id).into(),
                description: "Something odd happened".into(),
            })
            .collect(),
        // Pure chance, so none is ever required, and all of them stay masked until they happen.
        achievements: ODDITIES
            .iter()
            .map(|(id, name, description)| AchievementSpec {
                id: (*id).into(),
                name: (*name).into(),
                description: (*description).into(),
                stat: (*id).into(),
                threshold: 1,
                optional: true,
                secret: true,
            })
            .collect(),
        prestige: Vec::new(),
    })?)
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    // The host only delivers ambient lines where `enabled` is on.
    if msg.is_private || msg.user_id.is_empty() || msg.text.trim_start().starts_with('!') {
        return Ok(());
    }
    let server = env.server.as_str();
    let now = timestamp()?;
    let key = due_key(server);
    let mut entropy = Entropy::default();
    let Some(due) = kv_load(&key)?.trim().parse::<i64>().ok() else {
        // First line seen here (or unreadable state): start the clock rather than firing.
        return Ok(kv_save(
            &key,
            &next_due(server, now, &mut entropy)?.to_string(),
        )?);
    };
    if now < due {
        return Ok(());
    }
    let odds = setting_i64("claim_odds", server, None, DEFAULT_CLAIM_ODDS).max(1);
    if entropy.below(odds as u32)? != 0 {
        return Ok(());
    }
    let held = held(server, &msg.user_id)?;
    let unheld = ODDITIES
        .iter()
        .map(|(id, _, _)| *id)
        .filter(|id| !held.iter().any(|held| held == id))
        .collect::<Vec<_>>();
    // Someone with the whole set just talks; the drop waits for someone else.
    if unheld.is_empty() {
        return Ok(());
    }
    let id = unheld[entropy.below(unheld.len() as u32)? as usize];
    // Only a real unlock spends the drop: an opted-out profile gets nothing and leaves it armed.
    if award(server, &msg, id)? {
        kv_save(&key, &next_due(server, now, &mut entropy)?.to_string())?;
    }
    Ok(())
}

fn due_key(server: &str) -> String {
    format!("due:{}", encode(server))
}

fn next_due(server: &str, now: i64, entropy: &mut Entropy) -> Result<i64, Error> {
    let per_week = setting_i64("per_week", server, None, DEFAULT_PER_WEEK).clamp(1, 28);
    // Uniform in (0, 1], never 0, so the logarithm is finite.
    let uniform = (f64::from(entropy.below(1 << 24)?) + 1.0) / f64::from(1u32 << 24);
    Ok(now + gap(WEEK / per_week, uniform))
}

/// Seconds until the next oddity is due: `MIN_GAP` plus an exponential draw, so the gaps average
/// `mean` and show no rhythm. `uniform` is in (0, 1].
fn gap(mean: i64, uniform: f64) -> i64 {
    let rest = (mean - MIN_GAP).max(HOUR) as f64;
    let draw = (-uniform.ln() * rest).min(rest * MAX_GAP_MEANS);
    MIN_GAP + draw.round() as i64
}

/// Oddity ids this profile already holds.
fn held(server: &str, profile_id: &str) -> Result<Vec<String>, Error> {
    let request = AchievementsGetRequest::Catalog {
        server: server.into(),
        profile_id: Some(profile_id.into()),
        module: Some(MODULE.into()),
    };
    let summary: AchievementProfileSummary =
        serde_json::from_str(&unsafe { achievements_get(serde_json::to_string(&request)?)? })?;
    Ok(summary
        .modules
        .into_iter()
        .filter(|module| module.module == MODULE)
        .flat_map(|module| module.achievements)
        .filter(|achievement| achievement.earned)
        .map(|achievement| achievement.id)
        .collect())
}

/// Grants `id`; true when it was newly unlocked (and so announced by the host).
fn award(server: &str, msg: &MessagePayload, id: &str) -> Result<bool, Error> {
    let raw = unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: display(msg).into(),
            target: msg.target.clone(),
            increments: vec![StatIncrement {
                stat: id.into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?
    };
    let response: AwardStatsResponse = serde_json::from_str(&raw)?;
    Ok(!response.unlocked.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oddities_are_distinct_and_announce_cleanly() {
        assert!(ODDITIES.len() >= 20);
        for (index, (id, name, description)) in ODDITIES.iter().enumerate() {
            assert!(!id.is_empty() && !name.is_empty() && !description.is_empty());
            assert!(
                !name.ends_with(['.', '!', '?']),
                "{name} would end up with doubled punctuation"
            );
            assert!(
                ODDITIES[index + 1..]
                    .iter()
                    .all(|(other, _, _)| other != id),
                "duplicate id {id}"
            );
        }
    }

    #[test]
    fn gaps_respect_the_floor_and_the_cap() {
        let mean = WEEK / DEFAULT_PER_WEEK;
        let rest = (mean - MIN_GAP) as f64;
        assert_eq!(gap(mean, 1.0), MIN_GAP);
        let longest = gap(mean, 1.0 / f64::from(1u32 << 24));
        assert_eq!(longest, MIN_GAP + (rest * MAX_GAP_MEANS).round() as i64);
        // The median exponential draw is ln 2 of its mean.
        let median = gap(mean, 0.5);
        assert_eq!(
            median,
            MIN_GAP + (rest * std::f64::consts::LN_2).round() as i64
        );
    }

    #[test]
    fn the_average_gap_lands_near_the_weekly_rate() {
        let mean = WEEK / DEFAULT_PER_WEEK;
        let steps = 100_000;
        let total: i64 = (1..=steps)
            .map(|step| gap(mean, step as f64 / steps as f64))
            .sum();
        let average = total / steps;
        // The cap trims the tail a little (about 5%), so expect slightly under the mean.
        assert!(
            average > mean * 9 / 10 && average <= mean,
            "{average} vs {mean}"
        );
    }

    #[test]
    fn a_high_rate_still_keeps_the_floor() {
        assert!(gap(WEEK / 28, 1.0) >= MIN_GAP);
        assert!(gap(WEEK / 28, 0.01) > MIN_GAP);
    }
}
