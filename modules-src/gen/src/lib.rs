//! AI Horde-backed `!gen <prompt>`. The host owns the provider (submit + poll host functions);
//! this module validates the prompt, queues the generation, and drives polling through the
//! durable scheduler so the image link lands in the channel when the horde finishes.

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, ImageGenState, ImageGenStatusRequest,
    ImageGenStatusResponse, ImageGenSubmitRequest, ImageGenSubmitResponse, ModuleDataDeletePlan,
    ModuleDataRequest, ModuleDataResponse, ModuleKvMutation, RandomBytesRequest,
    RandomBytesResponse, ScheduleSet, SettingGet, SettingKind, SettingScope, SettingSpec,
    SettingsManifest, StatIncrement, ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION,
    DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use jeeves_guest::{cooldown_check, cooldown_start, encode, reply, themed, timestamp, Cooldown};
use serde::{Deserialize, Serialize};

const MIN_PROMPT_CHARS: usize = 2;
const MAX_PROMPT_CHARS: usize = 300;
const MAX_IMAGE_URL_BYTES: usize = 2_048;

#[host_fn]
extern "ExtismHost" {
    fn image_gen_submit(input: String) -> String;
    fn image_gen_status(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn award_stats(input: String) -> String;
}

/// Payload carried by the polling timer. Kept small: the scheduler bounds payload size.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct GenJob {
    job_id: String,
    prompt: String,
    user_id: String,
    display: String,
    attempts: u32,
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "gen".into(),
            aliases: Vec::new(),
            description: "Generate an AI image from a text prompt and post its link here.".into(),
            usage: "!gen <prompt>".into(),
            ..Default::default()
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
                description: "Seconds a user must wait between !gen requests.".into(),
                kind: SettingKind::DurationSeconds { min: 0, max: 3600 },
                default: "60".into(),
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "poll_interval_seconds".into(),
                description: "Seconds between polls of a queued image generation.".into(),
                kind: SettingKind::DurationSeconds { min: 5, max: 60 },
                default: "15".into(),
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_poll_attempts".into(),
                description: "How many times to poll before giving up on a generation.".into(),
                kind: SettingKind::Integer { min: 1, max: 40 },
                default: "12".into(),
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
            id: "images_generated".into(),
            description: "AI images successfully generated and posted".into(),
        }],
        achievements: [
            ("first_sketch", "First Sketch", 1),
            ("gallery", "Gallery", 25),
            ("muralist", "Muralist", 100),
        ]
        .into_iter()
        .map(|(id, name, threshold)| AchievementSpec {
            id: id.into(),
            name: name.into(),
            description: format!("Post {threshold} AI-generated images."),
            stat: "images_generated".into(),
            threshold,
            optional: false,
            secret: false,
        })
        .collect(),
        prestige: Vec::new(),
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

fn int_setting(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    setting(key, server, channel)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(fallback)
}

fn cooldown_key(server: &str, profile_id: &str) -> String {
    format!("cooldown:{}:{}", encode(server), encode(profile_id))
}

fn random_hex(bytes: usize) -> Result<String, Error> {
    let raw =
        unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count: bytes })?)? };
    let response: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(response
        .bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn job_id(server: &str, profile_id: &str) -> Result<String, Error> {
    Ok(format!(
        "gen:{}:{}:{}",
        encode(server),
        encode(profile_id),
        random_hex(8)?
    ))
}

fn schedule(server: &str, id: &str, channel: &str, job: &GenJob, due_at: i64) -> Result<(), Error> {
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: id.into(),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: Some(job.user_id.clone()),
            due_at,
            payload: serde_json::to_string(job)?,
        })?)?;
    }
    Ok(())
}

fn award(server: &str, job: &GenJob, target: &str) -> Result<(), Error> {
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: job.user_id.clone(),
            display_name: job.display.clone(),
            target: target.into(),
            increments: vec![StatIncrement {
                stat: "images_generated".into(),
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
        .is_some_and(|command| command.eq_ignore_ascii_case("!gen"))
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
                "gen.channel_only",
                &["Image generation is channel-only, {user}."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }
    let prompt = parts.next().unwrap_or("").trim();
    let prompt_chars = prompt.chars().count();
    if !(MIN_PROMPT_CHARS..=MAX_PROMPT_CHARS).contains(&prompt_chars) {
        reply(
            &server,
            &msg.target,
            &themed(
                "gen.invalid_prompt",
                &["Usage: !gen <2-300 character prompt>"],
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
                "gen.identity_unavailable",
                &["I could not verify your stable profile, {user}; please try again shortly."],
                &[("user", user)],
            )?,
        )?;
        return Ok(());
    }

    let current = timestamp()?;
    let cooldown = int_setting("cooldown_seconds", &server, &msg.target, 60).clamp(0, 3600);
    let key = cooldown_key(&server, &msg.user_id);
    match cooldown_check(&key, current, cooldown)? {
        Cooldown::Ready => {}
        Cooldown::Quiet => return Ok(()),
        Cooldown::Warn(remaining) => {
            reply(
                &server,
                &msg.target,
                &themed(
                    "gen.cooldown",
                    &["Please wait {seconds}s before generating another image, {user}."],
                    &[("seconds", &remaining.to_string()), ("user", user)],
                )?,
            )?;
            return Ok(());
        }
    }
    cooldown_start(&key, current)?;

    let raw = unsafe {
        image_gen_submit(serde_json::to_string(&ImageGenSubmitRequest {
            prompt: prompt.into(),
        })?)?
    };
    let response: ImageGenSubmitResponse = serde_json::from_str(&raw)?;
    if response.error.is_some() || response.job_id.is_empty() {
        reply_submit_error(&server, &msg.target, user, response.error.as_deref())?;
        return Ok(());
    }

    let job = GenJob {
        job_id: response.job_id,
        prompt: prompt.into(),
        user_id: msg.user_id.clone(),
        display: user.into(),
        attempts: 0,
    };
    let id = job_id(&server, &msg.user_id)?;
    let poll_interval = int_setting("poll_interval_seconds", &server, &msg.target, 15).clamp(5, 60);
    schedule(&server, &id, &msg.target, &job, current + poll_interval)?;
    reply(
        &server,
        &msg.target,
        &themed(
            "gen.queued",
            &["{user}: queued \"{prompt}\" — I'll post the image here when it's ready (usually under a minute)."],
            &[("user", user), ("prompt", &job.prompt)],
        )?,
    )?;
    Ok(())
}

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer {
        id,
        channel,
        payload,
        ..
    } = env.event
    else {
        return Ok(());
    };
    if !id.starts_with("gen:") {
        return Ok(());
    }
    let job: GenJob = match serde_json::from_str(&payload) {
        Ok(job) => job,
        Err(_) => return Ok(()),
    };

    let raw = unsafe {
        image_gen_status(serde_json::to_string(&ImageGenStatusRequest {
            job_id: job.job_id.clone(),
        })?)?
    };
    let response: ImageGenStatusResponse = serde_json::from_str(&raw)?;
    match response.state {
        ImageGenState::Pending => {
            let max_attempts =
                int_setting("max_poll_attempts", &server, &channel, 12).clamp(1, 40) as u32;
            if job.attempts + 1 >= max_attempts {
                reply(
                    &server,
                    &channel,
                    &themed(
                        "gen.timeout",
                        &["{user}, the image service is still busy with \"{prompt}\"; try again later."],
                        &[("user", &job.display), ("prompt", &job.prompt)],
                    )?,
                )?;
                return Ok(());
            }
            let poll_interval =
                int_setting("poll_interval_seconds", &server, &channel, 15).clamp(5, 60);
            let next = GenJob {
                attempts: job.attempts + 1,
                ..job
            };
            schedule(&server, &id, &channel, &next, timestamp()? + poll_interval)?;
        }
        ImageGenState::Done => {
            if response.url.is_empty() || response.url.len() > MAX_IMAGE_URL_BYTES {
                reply_status_error(&server, &channel, &job, Some("unavailable"))?;
                return Ok(());
            }
            reply(
                &server,
                &channel,
                &themed(
                    "gen.result",
                    &["{user} asked for \"{prompt}\": {url}"],
                    &[
                        ("user", &job.display),
                        ("prompt", &job.prompt),
                        ("url", &response.url),
                    ],
                )?,
            )?;
            award(&server, &job, &channel)?;
        }
        ImageGenState::Error => {
            reply_status_error(&server, &channel, &job, response.error.as_deref())?;
        }
    }
    Ok(())
}

fn reply_submit_error(
    server: &str,
    target: &str,
    user: &str,
    error: Option<&str>,
) -> Result<(), Error> {
    let (key, default) = match error {
        Some("rate_limited") => (
            "gen.rate_limited",
            "The image service is busy right now; please try again in a moment, {user}.",
        ),
        Some("authentication") => (
            "gen.authentication",
            "The image provider rejected its credentials.",
        ),
        _ => (
            "gen.unavailable",
            "The image service is unavailable right now, {user}.",
        ),
    };
    reply(server, target, &themed(key, &[default], &[("user", user)])?)
}

fn reply_status_error(
    server: &str,
    channel: &str,
    job: &GenJob,
    error: Option<&str>,
) -> Result<(), Error> {
    let (key, default) = match error {
        Some("censored") => (
            "gen.censored",
            "That prompt was filtered by the image service, {user}.",
        ),
        Some("rate_limited") => (
            "gen.rate_limited",
            "The image service is busy right now; please try again in a moment, {user}.",
        ),
        _ => (
            "gen.unavailable",
            "The image service gave up on \"{prompt}\", {user}.",
        ),
    };
    reply(
        server,
        channel,
        &themed(
            key,
            &[default],
            &[("user", &job.display), ("prompt", &job.prompt)],
        )?,
    )
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
    fn prompt_bounds_are_character_based() {
        assert!((MIN_PROMPT_CHARS..=MAX_PROMPT_CHARS)
            .contains(&"flying chickens on the moon".chars().count()));
        assert!(!(MIN_PROMPT_CHARS..=MAX_PROMPT_CHARS).contains(&"x".chars().count()));
        assert!(!(MIN_PROMPT_CHARS..=MAX_PROMPT_CHARS)
            .contains(&"y".repeat(MAX_PROMPT_CHARS + 1).chars().count()));
    }

    #[test]
    fn gen_job_payload_round_trips() {
        let job = GenJob {
            job_id: "abc-123".into(),
            prompt: "flying chickens on the moon".into(),
            user_id: "profile-1".into(),
            display: "sir aureate".into(),
            attempts: 3,
        };
        let encoded = serde_json::to_string(&job).unwrap();
        assert_eq!(serde_json::from_str::<GenJob>(&encoded).unwrap(), job);
    }
}
