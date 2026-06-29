//! YouTube search and opt-in link announcements. Network access and credentials remain host-owned.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, SendMessage, ServerQuery, SettingGet,
    SettingKind, SettingScope, SettingSpec, SettingsManifest, ThemeReq, YoutubeLookup,
    YoutubeResponse, YoutubeResult, YoutubeSearch, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn youtube_lookup(input: String) -> String;
    fn youtube_search(input: String) -> String;
    fn bot_nick(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "yt".into(),
            aliases: vec!["youtube".into()],
            description: "Search YouTube and show the top video result.".into(),
            usage: "!yt [search] <query>".into(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    let all = || {
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
                description: "Announce metadata for links posted in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "search_cooldown_seconds".into(),
                description: "Per-profile delay between YouTube searches.".into(),
                default: "20".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3_600 },
                scopes: all(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "announce_cooldown_seconds".into(),
                description: "Delay before the same video is announced here again.".into(),
                default: "30".into(),
                kind: SettingKind::DurationSeconds {
                    min: 0,
                    max: 86_400,
                },
                scopes: all(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_links_per_message".into(),
                description: "Maximum linked videos included in one announcement.".into(),
                default: "2".into(),
                kind: SettingKind::Integer { min: 1, max: 4 },
                scopes: all(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "seen_cache_size".into(),
                description: "Maximum remembered video ids per channel.".into(),
                default: "100".into(),
                kind: SettingKind::Integer { min: 10, max: 500 },
                scopes: all(),
                applies_immediately: true,
            },
            SettingSpec {
                key: "show_likes".into(),
                description: "Include like counts in search and link output.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: all(),
                applies_immediately: true,
            },
        ],
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
        })?)?
    };
    Ok(())
}

fn setting(key: &str, server: &str, channel: Option<&str>) -> Result<String, Error> {
    Ok(unsafe {
        setting_get(serde_json::to_string(&SettingGet {
            key: key.into(),
            server: Some(server.into()),
            channel: channel.map(str::to_string),
        })?)?
    })
}

fn bool_setting(key: &str, server: &str, channel: Option<&str>) -> Result<bool, Error> {
    Ok(setting(key, server, channel)? == "true")
}

fn int_setting(key: &str, server: &str, channel: Option<&str>, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn timestamp() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("")
}

fn cooldown_key(server: &str, profile_id: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(profile_id))
}

fn seen_key(server: &str, channel: &str) -> String {
    format!("seen:{}:{}", encode(server), encode(channel))
}

fn kv_read(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_write(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?
    };
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
struct SeenVideo {
    id: String,
    timestamp: i64,
}

fn valid_id(id: &str) -> bool {
    id.len() == 11
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn video_id_from_token(token: &str) -> Option<String> {
    let token = token.trim_matches(|character: char| {
        character.is_ascii_punctuation()
            && !matches!(character, ':' | '/' | '?' | '=' | '&' | '-' | '_')
    });
    let rest = token
        .strip_prefix("https://")
        .or_else(|| token.strip_prefix("http://"))?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let id = if host == "youtu.be" || host == "www.youtu.be" {
        path.split(['?', '&', '#', '/']).next()
    } else if matches!(
        host.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com"
    ) {
        if let Some(path) = path
            .strip_prefix("shorts/")
            .or_else(|| path.strip_prefix("embed/"))
        {
            path.split(['?', '&', '#', '/']).next()
        } else if let Some(query) = path.strip_prefix("watch?") {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == "v").then_some(value)
            })
        } else {
            None
        }
    } else {
        None
    }?;
    valid_id(id).then(|| id.to_string())
}

fn extract_ids(text: &str, limit: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for token in text.split_whitespace() {
        if let Some(id) = video_id_from_token(token) {
            if !ids.contains(&id) {
                ids.push(id);
                if ids.len() >= limit {
                    break;
                }
            }
        }
    }
    ids
}

fn is_addressed(text: &str, nick: &str) -> bool {
    let text = text.trim_start();
    text.get(..nick.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(nick))
        && text
            .get(nick.len()..)
            .is_some_and(|rest| rest.starts_with(',') || rest.starts_with(':'))
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let text = msg.text.trim();
    let mut command_parts = text.splitn(2, char::is_whitespace);
    let command = command_parts.next().unwrap_or("").to_ascii_lowercase();
    if matches!(command.as_str(), "!yt" | "!youtube") {
        let mut query = command_parts.next().unwrap_or("").trim();
        if query
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("search "))
        {
            query = query[7..].trim();
        }
        return handle_search(&server, &msg, query);
    }

    if msg.is_private
        || text.starts_with('!')
        || !bool_setting("enabled", &server, Some(&msg.target))?
    {
        return Ok(());
    }
    let own_nick = unsafe {
        bot_nick(serde_json::to_string(&ServerQuery {
            server: server.clone(),
        })?)?
    };
    if msg.nick.eq_ignore_ascii_case(&own_nick) || is_addressed(text, &own_nick) {
        return Ok(());
    }
    let limit =
        int_setting("max_links_per_message", &server, Some(&msg.target), 2).clamp(1, 4) as usize;
    let ids = extract_ids(text, limit);
    if ids.is_empty() {
        return Ok(());
    }
    let current = timestamp()?;
    let cooldown =
        int_setting("announce_cooldown_seconds", &server, Some(&msg.target), 30).clamp(0, 86_400);
    let cache_size =
        int_setting("seen_cache_size", &server, Some(&msg.target), 100).clamp(10, 500) as usize;
    let key = seen_key(&server, &msg.target);
    let mut seen: Vec<SeenVideo> = serde_json::from_str(&kv_read(&key)?).unwrap_or_default();
    if cooldown == 0 {
        seen.clear();
    } else {
        seen.retain(|entry| current.saturating_sub(entry.timestamp) <= cooldown);
    }
    let ids = ids
        .into_iter()
        .filter(|id| !seen.iter().any(|entry| entry.id == *id))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(());
    }
    let raw =
        unsafe { youtube_lookup(serde_json::to_string(&YoutubeLookup { ids: ids.clone() })?)? };
    let response: YoutubeResponse = serde_json::from_str(&raw)?;
    for id in ids {
        seen.retain(|entry| entry.id != id);
        seen.push(SeenVideo {
            id,
            timestamp: current,
        });
    }
    if seen.len() > cache_size {
        seen.drain(0..seen.len() - cache_size);
    }
    kv_write(&key, &serde_json::to_string(&seen)?)?;
    if response.results.is_empty() {
        return Ok(reply_error(
            &server,
            &msg.target,
            response.error.as_deref(),
        )?);
    }
    let show_likes = bool_setting("show_likes", &server, Some(&msg.target))?;
    let videos = response
        .results
        .iter()
        .map(|result| format_video(result, current, show_likes))
        .collect::<Vec<_>>()
        .join(" | ");
    Ok(reply(
        &server,
        &msg.target,
        &themed("announce", &["YouTube: {videos}"], &[("videos", &videos)])?,
    )?)
}

fn handle_search(server: &str, msg: &jeeves_abi::MessagePayload, query: &str) -> FnResult<()> {
    let destination = if msg.is_private {
        &msg.nick
    } else {
        &msg.target
    };
    let user = if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    };
    if query.is_empty() {
        return Ok(reply(
            server,
            destination,
            &themed(
                "search_empty",
                &["What should I search YouTube for, {user}?"],
                &[("user", user)],
            )?,
        )?);
    }
    if query.chars().count() > 200 {
        return Ok(reply(
            server,
            destination,
            &themed(
                "query_too_long",
                &["That YouTube search is too long, {user}."],
                &[("user", user)],
            )?,
        )?);
    }
    if msg.user_id.is_empty() {
        return Ok(reply(
            server,
            destination,
            &themed(
                "identity_unavailable",
                &["I could not verify your profile, {user}; try again shortly."],
                &[("user", user)],
            )?,
        )?);
    }
    let current = timestamp()?;
    let cooldown = int_setting(
        "search_cooldown_seconds",
        server,
        (!msg.is_private).then_some(msg.target.as_str()),
        20,
    )
    .clamp(0, 3_600);
    let key = cooldown_key(server, &msg.user_id);
    let previous = kv_read(&key)?.parse::<i64>().unwrap_or(0);
    let remaining = cooldown - current.saturating_sub(previous);
    if current > 0 && remaining > 0 && remaining <= cooldown {
        let seconds = remaining.to_string();
        return Ok(reply(
            server,
            destination,
            &themed(
                "cooldown",
                &["Please wait {seconds}s before searching again, {user}."],
                &[("seconds", &seconds), ("user", user)],
            )?,
        )?);
    }
    kv_write(&key, &current.to_string())?;
    let raw = unsafe {
        youtube_search(serde_json::to_string(&YoutubeSearch {
            query: query.into(),
        })?)?
    };
    let response: YoutubeResponse = serde_json::from_str(&raw)?;
    let Some(result) = response.results.first() else {
        if response.error.as_deref() == Some("not_found") {
            return Ok(reply(
                server,
                destination,
                &themed(
                    "search_no_results",
                    &["I found no YouTube results for {query}, {user}."],
                    &[("query", query), ("user", user)],
                )?,
            )?);
        }
        return Ok(reply_error(server, destination, response.error.as_deref())?);
    };
    let show_likes = bool_setting(
        "show_likes",
        server,
        (!msg.is_private).then_some(msg.target.as_str()),
    )?;
    let views = compact_count(result.view_count);
    let likes = result
        .like_count
        .map(compact_count)
        .unwrap_or_else(|| "—".into());
    let duration = format_duration(result.duration_seconds);
    let age = relative_age(&result.published_at, current);
    let url = format!("https://youtu.be/{}", result.video_id);
    let default = if show_likes {
        "{title} — {channel} · {views} views · {likes} likes · {duration} · {age} · {url}"
    } else {
        "{title} — {channel} · {views} views · {duration} · {age} · {url}"
    };
    Ok(reply(
        server,
        destination,
        &themed(
            "search_result",
            &[default],
            &[
                ("title", &truncate(&result.title, 80)),
                ("channel", &truncate(&result.channel, 50)),
                ("views", &views),
                ("likes", &likes),
                ("duration", &duration),
                ("age", &age),
                ("url", &url),
                ("user", user),
                ("query", query),
            ],
        )?,
    )?)
}

fn reply_error(server: &str, target: &str, error: Option<&str>) -> Result<(), Error> {
    let (key, message) = match error {
        Some("not_configured") => ("not_configured", "YouTube search needs an API key."),
        Some("quota_exceeded") => (
            "quota_exceeded",
            "YouTube's API quota is exhausted right now.",
        ),
        Some("not_found") => (
            "not_found",
            "I could not find an available YouTube video for that.",
        ),
        Some("invalid_request") => ("invalid_request", "YouTube rejected that request."),
        _ => ("unavailable", "YouTube is not answering right now."),
    };
    reply(server, target, &themed(key, &[message], &[])?)
}

fn format_video(result: &YoutubeResult, now: i64, show_likes: bool) -> String {
    let likes = if show_likes {
        result
            .like_count
            .map(|count| format!(" · {} likes", compact_count(count)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "{} — {} · {} views{} · {} · {} · https://youtu.be/{}",
        truncate(&result.title, 65),
        truncate(&result.channel, 35),
        compact_count(result.view_count),
        likes,
        format_duration(result.duration_seconds),
        relative_age(&result.published_at, now),
        result.video_id
    )
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.into()
    } else {
        let mut value = value
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>();
        value.push('…');
        value
    }
}

fn compact_count(value: u64) -> String {
    for (unit, divisor) in [("B", 1_000_000_000_u64), ("M", 1_000_000), ("k", 1_000)] {
        if value >= divisor {
            let tenths = value.saturating_mul(10) / divisor;
            return if tenths.is_multiple_of(10) {
                format!("{}{}", tenths / 10, unit)
            } else {
                format!("{}.{:01}{}", tenths / 10, tenths % 10, unit)
            };
        }
    }
    value.to_string()
}

fn format_duration(seconds: u64) -> String {
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            (seconds % 3_600) / 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

fn relative_age(published: &str, now: i64) -> String {
    let parts = published
        .get(..10)
        .unwrap_or("")
        .split('-')
        .collect::<Vec<_>>();
    let Some((year, month, day)) = parts
        .first()
        .and_then(|year| year.parse::<i64>().ok())
        .zip(parts.get(1).and_then(|month| month.parse::<i64>().ok()))
        .zip(parts.get(2).and_then(|day| day.parse::<i64>().ok()))
        .map(|((year, month), day)| (year, month, day))
    else {
        return "unknown age".into();
    };
    let days = days_from_civil(year, month, day);
    let age_days = (now.div_euclid(86_400) - days).max(0);
    if age_days >= 365 {
        plural_age(age_days / 365, "year")
    } else if age_days >= 30 {
        plural_age(age_days / 30, "month")
    } else if age_days >= 7 {
        plural_age(age_days / 7, "week")
    } else if age_days > 0 {
        format!("{age_days} days ago")
    } else {
        "today".into()
    }
}

fn plural_age(value: i64, unit: &str) -> String {
    format!("{value} {unit}{} ago", if value == 1 { "" } else { "s" })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let adjusted_month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * adjusted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[plugin_fn]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let values = request
        .entries
        .iter()
        .filter(|entry| entry.key == key)
        .map(|entry| entry.value.clone())
        .collect::<Vec<_>>();
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if values.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({"cooldown_timestamps": values})
        },
    })?)
}

#[plugin_fn]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let key = cooldown_key(&request.subject.server, &request.subject.profile_id);
    let mutations = request
        .entries
        .iter()
        .filter(|entry| entry.key == key)
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
    fn extracts_supported_urls_and_deduplicates() {
        let ids = extract_ids(
            "https://youtu.be/dQw4w9WgXcQ and https://www.youtube.com/watch?v=dQw4w9WgXcQ https://youtube.com/shorts/aqz-KE-bpKQ",
            4,
        );
        assert_eq!(ids, vec!["dQw4w9WgXcQ", "aqz-KE-bpKQ"]);
    }

    #[test]
    fn rejects_lookalike_hosts_and_bad_ids() {
        assert_eq!(
            video_id_from_token("https://evil-youtube.com/watch?v=dQw4w9WgXcQ"),
            None
        );
        assert_eq!(video_id_from_token("https://youtu.be/short"), None);
    }

    #[test]
    fn formats_counts_durations_and_age() {
        assert_eq!(compact_count(1_234_567), "1.2M");
        assert_eq!(format_duration(3_723), "1:02:03");
        assert_eq!(
            relative_age("2020-01-01T00:00:00Z", 1_609_459_200),
            "1 year ago"
        );
    }

    #[test]
    fn address_detection_requires_prefix_punctuation() {
        assert!(is_addressed("Jeeves: inspect this", "jeeves"));
        assert!(!is_addressed("I told Jeeves about it", "jeeves"));
    }
}
