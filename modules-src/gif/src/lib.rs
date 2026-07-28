//! KLIPY-backed `!gif` search. HTTP and credentials remain in the host.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, GifSearchRequest, GifSearchResponse, KvGet, KvSet,
    ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    RandomBytesRequest, RandomBytesResponse, SendMessage, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, StatIncrement, ThemeReq, ACHIEVEMENT_MANIFEST_VERSION,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};

const MIN_QUERY_CHARS: usize = 2;
const MAX_QUERY_CHARS: usize = 80;
const MAX_RESULT_POOL: i64 = 12;
const MAX_MEDIA_URL_BYTES: usize = 320;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn gif_search(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn award_stats(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "gif".into(),
            aliases: Vec::new(),
            description: "Search for a relevant GIF and post its link to the channel.".into(),
            usage: "!gif <search terms>".into(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "cooldown_seconds".into(),
                description: "Seconds a user must wait between GIF searches.".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 300 },
                default: "10".into(),
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "result_pool".into(),
                description: "Number of top search results from which one GIF is chosen randomly."
                    .into(),
                kind: SettingKind::Integer {
                    min: 1,
                    max: MAX_RESULT_POOL,
                },
                default: "6".into(),
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
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
            id: "gifs_posted".into(),
            description: "GIF searches successfully posted".into(),
        }],
        achievements: [
            ("moving_picture", "Moving Picture", 1),
            ("reactionary", "Reactionary", 25),
            ("gif_oracle", "GIF Oracle", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Post {threshold} GIF search results."),
            stat: "gifs_posted".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
    })?)
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
        })?)?;
    }
    Ok(())
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

fn int_setting(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn cooldown_key(server: &str, profile_id: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(profile_id))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn cooldown_get(key: &str) -> Result<(i64, bool), Error> {
    let value = unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? };
    let value = value.parse::<i64>().unwrap_or(0);
    Ok((value.saturating_abs(), value < 0))
}

fn cooldown_set(key: &str, value: i64) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.to_string(),
        })?)?;
    }
    Ok(())
}

fn random_index(len: usize) -> Result<usize, Error> {
    if len <= 1 {
        return Ok(0);
    }
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: 4 })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    let bytes: [u8; 4] = response
        .bytes
        .get(..4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| Error::msg("host returned insufficient randomness"))?;
    Ok(u32::from_le_bytes(bytes) as usize % len)
}

fn award(server: &str, profile_id: &str, display_name: &str, target: &str) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: profile_id.into(),
            display_name: display_name.into(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "gifs_posted".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let mut parts = msg.text.splitn(2, char::is_whitespace);
    if !parts
        .next()
        .is_some_and(|command| command.eq_ignore_ascii_case("!gif"))
    {
        return Ok(());
    }
    let user = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    if msg.is_private {
        reply(
            &server,
            &msg.nick,
            &themed(
                "gif.channel_only",
                &["GIF searches are channel-only, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    let query = parts.next().unwrap_or("").trim();
    let query_chars = query.chars().count();
    if !(MIN_QUERY_CHARS..=MAX_QUERY_CHARS).contains(&query_chars) {
        reply(
            &server,
            &msg.target,
            &themed(
                "gif.invalid_query",
                &["Usage: !gif <2-80 character search>"],
                &[],
            )?,
        )?;
        return Ok(());
    }
    if msg.user_id.is_empty() {
        reply(
            &server,
            &msg.target,
            &themed(
                "gif.identity_unavailable",
                &["I could not verify your stable profile, {user}; please try again shortly."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    let current = timestamp()?;
    let cooldown = int_setting("cooldown_seconds", &server, &msg.target, 10).clamp(0, 300);
    let key = cooldown_key(&server, &msg.user_id);
    let (last_used, warned) = cooldown_get(&key)?;
    let remaining = cooldown - current.saturating_sub(last_used);
    if current > 0 && remaining > 0 && remaining <= cooldown {
        if !warned {
            cooldown_set(&key, -last_used)?;
            let seconds = remaining.to_string();
            reply(
                &server,
                &msg.target,
                &themed(
                    "gif.cooldown",
                    &["Please wait {seconds}s before searching for another GIF, {user}."],
                    &[("seconds", &seconds), ("user", user)],
                )?,
            )?;
        }
        return Ok(());
    }
    cooldown_set(&key, current)?;

    let limit = int_setting("result_pool", &server, &msg.target, 6).clamp(1, MAX_RESULT_POOL);
    let raw = unsafe {
        gif_search(serde_json::to_string(&GifSearchRequest {
            query: query.into(),
            limit: limit as u32,
        })?)?
    };
    let response: GifSearchResponse = serde_json::from_str(&raw)?;
    if response.results.is_empty() {
        reply_search_error(&server, &msg.target, user, response.error.as_deref())?;
        return Ok(());
    }
    let result = &response.results[random_index(response.results.len())?];
    if result.url.len() > MAX_MEDIA_URL_BYTES {
        reply_search_error(&server, &msg.target, user, Some("unavailable"))?;
        return Ok(());
    }
    let provider = if response.provider.is_empty() {
        "GIF provider"
    } else {
        response.provider.as_str()
    };
    reply(
        &server,
        &msg.target,
        &themed(
            "gif.result",
            &["{url} (via {provider})"],
            &[("url", &result.url), ("provider", provider)],
        )?,
    )?;
    award(&server, &msg.user_id, user, &msg.target)?;
    Ok(())
}

fn reply_search_error(
    server: &str,
    target: &str,
    user: &str,
    error: Option<&str>,
) -> Result<(), Error> {
    let (key, default) = match error {
        Some("not_configured") => (
            "gif.not_configured",
            "GIF search has not been configured by the operator yet.",
        ),
        Some("not_found") => (
            "gif.not_found",
            "I could not find a GIF for that search, {user}.",
        ),
        Some("rate_limited") => (
            "gif.rate_limited",
            "The GIF shelves are busy; please try again in a moment, {user}.",
        ),
        Some("authentication") => (
            "gif.authentication",
            "The GIF provider rejected its credentials.",
        ),
        _ => (
            "gif.unavailable",
            "The GIF provider is unavailable right now, {user}.",
        ),
    };
    reply(server, target, &themed(key, &[default], &[("user", user)])?)
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| cooldown_key(&request.subject.server, identity))
        .collect::<Vec<_>>();
    let values: Vec<String> = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| entry.value.clone())
        .collect();
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if values.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "cooldown_timestamps": values })
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let keys = std::iter::once(request.subject.profile_id.as_str())
        .chain(request.aliases.iter().map(String::as_str))
        .map(|identity| cooldown_key(&request.subject.server, identity))
        .collect::<Vec<_>>();
    let mutations = request
        .entries
        .iter()
        .filter(|entry| keys.contains(&entry.key))
        .map(|entry| ModuleKvMutation {
            key: entry.key.clone(),
            value: None,
        })
        .collect();
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooldown_keys_use_stable_profile_identity() {
        assert_ne!(
            cooldown_key("network-a", "profile-1"),
            cooldown_key("network-a", "profile-2")
        );
        assert_ne!(
            cooldown_key("network-a", "profile-1"),
            cooldown_key("network-b", "profile-1")
        );
    }

    #[test]
    fn query_bounds_preserve_punctuation() {
        let query = "danger, danger";
        assert!((MIN_QUERY_CHARS..=MAX_QUERY_CHARS).contains(&query.chars().count()));
    }
}
