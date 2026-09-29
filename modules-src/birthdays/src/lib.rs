//! Birthday greetings.
//!
//! Where the `enabled` setting is on (off by default, per channel), someone who has saved a
//! birthday with `!birthday` is wished a happy birthday the first time they speak on the day, in
//! their saved timezone (UTC otherwise). Greeting them where they are, rather than announcing at
//! midnight, means nobody is congratulated to an empty room. Each person is greeted once per
//! network per year, and `birthday_brass` (default 25) is a gift from the house. A 29 February
//! birthday is kept on the 28th in other years. Clearing the birthday stops the greetings.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest,
    EconomyTransactionRequest, Event, EventEnvelope, KvGet, KvSet, LocalTimeQuery, LocalTimeResult,
    MessagePayload, ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    Profile, ProfileKey, SendMessage, SettingKind, SettingScope, SettingSpec, SettingsManifest,
    StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{setting_i64, themed};
use std::cell::RefCell;
use std::collections::HashMap;

const DEFAULT_BRASS: i64 = 25;
const DAY: i64 = 86_400;
/// People remembered at once; the cache is only an optimisation, so it is simply emptied when full.
const MAX_CACHED: usize = 2_000;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn now(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn profile_get(input: String) -> String;
    fn local_time(input: String) -> String;
    fn economy_award(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Wish people here a happy birthday the first time they speak on it."
                    .into(),
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
                key: "birthday_brass".into(),
                description: "Brass given with a birthday greeting (0 for none).".into(),
                default: DEFAULT_BRASS.to_string(),
                kind: SettingKind::Integer { min: 0, max: 1_000 },
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
        stats: vec![AchievementStat {
            id: "greetings".into(),
            description: "Birthdays celebrated".into(),
        }],
        // It depends on sharing a birthday and on the channel's setting, so it's optional.
        achievements: vec![AchievementSpec {
            id: "another_year".into(),
            name: "Another Year Wiser".into(),
            description: "Be wished a happy birthday.".into(),
            stat: "greetings".into(),
            threshold: 1,
            optional: true,
            secret: false,
        }],
        prestige: Vec::new(),
    })?)
}

fn greeted_key(server: &str, profile_id: &str) -> String {
    format!("greeted:{server}:{profile_id}")
}

/// What we know about one person today (UTC), so their profile is read once a day.
#[derive(Clone)]
struct Known {
    utc_day: i64,
    birthday: Option<(u32, u32)>,
    timezone: Option<String>,
    /// Greeted (or found already greeted) this year.
    done_year: Option<i32>,
}

thread_local! {
    static KNOWN: RefCell<HashMap<(String, String), Known>> = RefCell::new(HashMap::new());
}

/// Month and day from a stored `MM-DD` or `MM-DD-YYYY` birthday.
fn month_day(stored: &str) -> Option<(u32, u32)> {
    let mut parts = stored.split('-');
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    ((1..=12).contains(&month) && (1..=31).contains(&day)).then_some((month, day))
}

fn leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Whether `(month, day)` of `year` is this birthday; 29 February falls on the 28th otherwise.
fn is_birthday(birthday: (u32, u32), year: i32, month: u32, day: u32) -> bool {
    birthday == (month, day) || (birthday == (2, 29) && !leap(year) && (month, day) == (2, 28))
}

fn lookup(server: &str, nick: &str, profile_id: &str, utc_day: i64) -> Result<Known, Error> {
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    let profile = if raw.trim().is_empty() {
        None
    } else {
        Some(serde_json::from_str::<Profile>(&raw)?)
    };
    let profile = profile.filter(|profile| profile.id == profile_id);
    Ok(Known {
        utc_day,
        birthday: profile
            .as_ref()
            .and_then(|profile| profile.birthday.as_deref())
            .and_then(month_day),
        timezone: profile.and_then(|profile| profile.timezone),
        done_year: None,
    })
}

fn today(timezone: Option<&str>) -> Result<Option<LocalTimeResult>, Error> {
    let query = |zone: &str| -> Result<String, Error> {
        Ok(unsafe {
            local_time(serde_json::to_string(&LocalTimeQuery {
                timezone: zone.into(),
                unix_seconds: None,
                local: None,
            })?)?
        })
    };
    let mut raw = query(timezone.unwrap_or("UTC"))?;
    if raw.is_empty() && timezone.is_some() {
        raw = query("UTC")?;
    }
    Ok(if raw.is_empty() {
        None
    } else {
        Some(serde_json::from_str(&raw)?)
    })
}

/// Gives the brass (idempotent per person and year), records the year, then greets. A host
/// failure aborts the whole call, so anything before the record is simply retried on the next
/// line, and the greeting only goes out once the record is written.
fn greet(server: &str, msg: &MessagePayload, record: String, year: i32) -> Result<(), Error> {
    let name = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    let brass = setting_i64("birthday_brass", server, Some(&msg.target), DEFAULT_BRASS)
        .clamp(0, 1_000) as u64;
    let event_id = format!("birthdays:{}:{year}", msg.user_id);
    if brass > 0 {
        unsafe {
            economy_award(serde_json::to_string(&EconomyTransactionRequest {
                server: server.into(),
                profile_id: msg.user_id.clone(),
                amount: brass,
                event_id: event_id.clone(),
                reason: "birthday".into(),
            })?)?
        };
    }
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: record,
            value: year.to_string(),
        })?)?
    };
    let text = if brass > 0 {
        themed(
            "birthdays.greeting_brass",
            &[
                "🎂 Happy birthday, {user}! Many happy returns of the day — and {brass} brass, with the compliments of the house.",
                "🎂 Many happy returns, {user}! The house begs you accept {brass} brass on the occasion.",
            ],
            &[("user", name), ("brass", &brass.to_string())],
        )?
    } else {
        themed(
            "birthdays.greeting",
            &[
                "🎂 Happy birthday, {user}! Many happy returns of the day.",
                "🎂 Many happy returns, {user}! I trust the day finds you well.",
            ],
            &[("user", name)],
        )?
    };
    unsafe {
        send_message(serde_json::to_string(&SendMessage {
            server: server.into(),
            target: msg.target.clone(),
            text,
        })?)?;
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: msg.user_id.clone(),
            display_name: name.into(),
            target: msg.target.clone(),
            increments: vec![StatIncrement {
                stat: "greetings".into(),
                amount: 1,
            }],
            deduplication_id: Some(event_id),
        })?)?;
    }
    Ok(())
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
    let now: i64 = unsafe { now(String::new())? }.parse().unwrap_or(0);
    let utc_day = now.div_euclid(DAY);
    let key = (server.to_string(), msg.user_id.clone());
    let cached = KNOWN.with(|known| {
        known
            .borrow()
            .get(&key)
            .filter(|known| known.utc_day == utc_day)
            .cloned()
    });
    let mut known = match cached {
        Some(known) => known,
        None => lookup(server, &msg.nick, &msg.user_id, utc_day)?,
    };
    if let Some(birthday) = known.birthday {
        if let Some(local) = today(known.timezone.as_deref())? {
            if known.done_year != Some(local.year)
                && is_birthday(birthday, local.year, local.month, local.day)
            {
                let record = greeted_key(server, &msg.user_id);
                let greeted = unsafe {
                    kv_get(serde_json::to_string(&KvGet {
                        key: record.clone(),
                    })?)?
                };
                if greeted.trim() != local.year.to_string() {
                    greet(server, &msg, record, local.year)?;
                }
                known.done_year = Some(local.year);
            }
        }
    }
    KNOWN.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= MAX_CACHED && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, known);
    });
    Ok(())
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = greeted_key(&request.subject.server, &request.subject.profile_id);
    let year = request
        .entries
        .iter()
        .find(|entry| entry.key == key)
        .map(|entry| entry.value.clone());
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: match year {
            Some(year) => serde_json::json!({ "last_greeted_year": year }),
            None => serde_json::Value::Null,
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = greeted_key(&request.subject.server, &request.subject.profile_id);
    KNOWN.with(|known| {
        known.borrow_mut().remove(&(
            request.subject.server.clone(),
            request.subject.profile_id.clone(),
        ))
    });
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations: request
            .entries
            .iter()
            .filter(|entry| entry.key == key)
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
    fn reads_stored_birthdays() {
        assert_eq!(month_day("03-14"), Some((3, 14)));
        assert_eq!(month_day("12-01-1990"), Some((12, 1)));
        assert_eq!(month_day("13-01"), None);
        assert_eq!(month_day("nonsense"), None);
    }

    #[test]
    fn leap_day_birthdays_fall_on_the_28th_otherwise() {
        assert!(is_birthday((3, 14), 2026, 3, 14));
        assert!(!is_birthday((3, 14), 2026, 3, 15));
        assert!(is_birthday((2, 29), 2028, 2, 29));
        assert!(!is_birthday((2, 29), 2028, 2, 28));
        assert!(is_birthday((2, 29), 2026, 2, 28));
        assert!(!is_birthday((2, 28), 2026, 2, 29));
        assert!(!leap(1900) && leap(2000));
    }
}
