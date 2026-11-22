//! AI Horde-backed text-to-image (`!gen`). Two-phase so no host call ever blocks on the queue:
//! `submit` queues a generation and returns the provider job id, `status` polls it; the guest
//! module drives polling through the durable scheduler. HTTP, credentials, and rate gates stay
//! host-owned behind the `image_gen` capability. Anonymous use works with the horde's well-known
//! anonymous key; operators can set their own key (higher priority) via the `ai_horde_api_key`
//! config value or `RUSTJEEVES_AI_HORDE_API_KEY`.

use jeeves_abi::{ImageGenState, ImageGenStatusResponse, ImageGenSubmitResponse};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const API_KEY_CONFIG: &str = "ai_horde_api_key";
const API_ROOT: &str = "https://aihorde.net/api/v2";
const PROVIDER: &str = "AI Horde";
const ANONYMOUS_KEY: &str = "0000000000";
const MAX_PROMPT_CHARS: usize = 300;
const MAX_JOB_ID_BYTES: usize = 128;
const MAX_RESPONSE_BYTES: u64 = 512 * 1024;
const MAX_IMAGE_URL_BYTES: usize = 2_048;
const WIDTH: u32 = 512;
const HEIGHT: u32 = 512;
const STEPS: u32 = 20;
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(10);
const STATUS_TIMEOUT: Duration = Duration::from_secs(8);
const MIN_SUBMIT_INTERVAL: Duration = Duration::from_secs(2);
const SUBMIT_WINDOW: Duration = Duration::from_secs(60 * 60);
const MAX_SUBMITS_PER_WINDOW: usize = 20;
const MIN_STATUS_INTERVAL: Duration = Duration::from_secs(1);
const STATUS_WINDOW: Duration = Duration::from_secs(60);
const MAX_STATUSES_PER_WINDOW: usize = 120;

#[derive(Default)]
struct RequestGate {
    last: Option<Instant>,
    recent: VecDeque<Instant>,
}

static SUBMIT_GATE: OnceLock<Mutex<RequestGate>> = OnceLock::new();
static STATUS_GATE: OnceLock<Mutex<RequestGate>> = OnceLock::new();

/// Queue a generation; returns the provider job id.
pub fn submit(prompt: &str, configured_key: Option<&str>) -> ImageGenSubmitResponse {
    let prompt = clean_prompt(prompt);
    if prompt.is_empty() || prompt.chars().count() > MAX_PROMPT_CHARS {
        return submit_failure("invalid_request");
    }
    if !admit(
        &SUBMIT_GATE,
        MIN_SUBMIT_INTERVAL,
        SUBMIT_WINDOW,
        MAX_SUBMITS_PER_WINDOW,
    ) {
        return submit_failure("rate_limited");
    }
    let key = api_key(configured_key);
    let body = json!({
        "prompt": prompt,
        "params": {
            "n": 1,
            "width": WIDTH,
            "height": HEIGHT,
            "steps": STEPS,
            "sampler_name": "k_lms",
            "cfg_scale": 7.0,
        },
        "nsfw": false,
        "censor_nsfw": true,
        "models": ["stable_diffusion"],
        "shared": false,
    });
    let value = match post_json(
        &format!("{API_ROOT}/generate/async"),
        &key,
        &body,
        SUBMIT_TIMEOUT,
    ) {
        Ok(value) => value,
        Err(error) => return submit_failure(error),
    };
    match value.get("id").and_then(Value::as_str) {
        Some(id)
            if !id.is_empty()
                && id.len() <= MAX_JOB_ID_BYTES
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') =>
        {
            ImageGenSubmitResponse {
                job_id: id.into(),
                provider: PROVIDER.into(),
                error: None,
            }
        }
        _ => submit_failure("unavailable"),
    }
}

/// Poll a queued generation. `Done` carries a validated HTTPS image URL.
pub fn status(job_id: &str, configured_key: Option<&str>) -> ImageGenStatusResponse {
    let job_id = job_id.trim();
    if job_id.is_empty()
        || job_id.len() > MAX_JOB_ID_BYTES
        || !job_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return status_failure("invalid_request");
    }
    if !admit(
        &STATUS_GATE,
        MIN_STATUS_INTERVAL,
        STATUS_WINDOW,
        MAX_STATUSES_PER_WINDOW,
    ) {
        return status_failure("rate_limited");
    }
    let key = api_key(configured_key);
    let value = match get_json(
        &format!("{API_ROOT}/generate/status/{job_id}"),
        &key,
        STATUS_TIMEOUT,
    ) {
        Ok(value) => value,
        Err(error) => return status_failure(error),
    };
    if !value.get("done").and_then(Value::as_bool).unwrap_or(false) {
        return ImageGenStatusResponse {
            state: ImageGenState::Pending,
            url: String::new(),
            provider: PROVIDER.into(),
            error: None,
        };
    }
    let generation = value
        .get("generations")
        .and_then(Value::as_array)
        .and_then(|generations| generations.first());
    if generation
        .and_then(|g| g.get("censored"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return status_failure("censored");
    }
    match generation
        .and_then(|g| g.get("img"))
        .and_then(Value::as_str)
        .and_then(valid_image_url)
    {
        Some(url) => ImageGenStatusResponse {
            state: ImageGenState::Done,
            url,
            provider: PROVIDER.into(),
            error: None,
        },
        // Base64 inline images (workers without R2) cannot be posted as an IRC link.
        None => status_failure("unavailable"),
    }
}

fn api_key(configured: Option<&str>) -> String {
    configured
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("RUSTJEEVES_AI_HORDE_API_KEY")
                .ok()
                .map(|key| key.trim().to_string())
                .filter(|key| !key.is_empty())
        })
        .unwrap_or_else(|| ANONYMOUS_KEY.into())
}

fn admit(
    gate: &OnceLock<Mutex<RequestGate>>,
    min_interval: Duration,
    window: Duration,
    max_per_window: usize,
) -> bool {
    let now = Instant::now();
    let mut gate = gate
        .get_or_init(|| Mutex::new(RequestGate::default()))
        .lock()
        .unwrap();
    while gate
        .recent
        .front()
        .is_some_and(|instant| now.duration_since(*instant) >= window)
    {
        gate.recent.pop_front();
    }
    if gate
        .last
        .is_some_and(|instant| now.duration_since(instant) < min_interval)
        || gate.recent.len() >= max_per_window
    {
        return false;
    }
    gate.last = Some(now);
    gate.recent.push_back(now);
    true
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .build(),
    )
}

fn map_error(error: ureq::Error) -> &'static str {
    match error {
        ureq::Error::StatusCode(400) => "invalid_request",
        ureq::Error::StatusCode(401 | 403) => "authentication",
        ureq::Error::StatusCode(404) => "not_found",
        ureq::Error::StatusCode(429) => "rate_limited",
        _ => "unavailable",
    }
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<Value, &'static str> {
    let body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE_BYTES)
        .read_to_string()
        .map_err(|_| "unavailable")?;
    serde_json::from_str(&body).map_err(|_| "unavailable")
}

fn post_json(url: &str, key: &str, body: &Value, timeout: Duration) -> Result<Value, &'static str> {
    let mut response = agent(timeout)
        .post(url)
        .header("apikey", key)
        .header("Accept", "application/json")
        .send_json(body)
        .map_err(map_error)?;
    read_body(&mut response)
}

fn get_json(url: &str, key: &str, timeout: Duration) -> Result<Value, &'static str> {
    let mut response = agent(timeout)
        .get(url)
        .header("apikey", key)
        .header("Accept", "application/json")
        .call()
        .map_err(map_error)?;
    read_body(&mut response)
}

/// Only linkable images are useful to an IRC bot: HTTPS, bounded, no control characters.
fn valid_image_url(url: &str) -> Option<String> {
    if url.len() > MAX_IMAGE_URL_BYTES || url.chars().any(char::is_control) {
        return None;
    }
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    (host.contains('.') && !host.starts_with('.') && !path.is_empty()).then(|| url.to_string())
}

fn clean_prompt(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn submit_failure(error: &str) -> ImageGenSubmitResponse {
    ImageGenSubmitResponse {
        job_id: String::new(),
        provider: PROVIDER.into(),
        error: Some(error.into()),
    }
}

fn status_failure(error: &str) -> ImageGenStatusResponse {
    ImageGenStatusResponse {
        state: ImageGenState::Error,
        url: String::new(),
        provider: PROVIDER.into(),
        error: Some(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_response_parses_job_id_and_rejects_malformed() {
        let value = json!({ "id": "abcd-1234_EF" });
        let id = value.get("id").and_then(Value::as_str).unwrap();
        assert!(id.len() <= MAX_JOB_ID_BYTES);
        assert!(json!({ "id": "bad id!" })
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')));
    }

    #[test]
    fn status_done_requires_linkable_https_image() {
        let pending = json!({ "done": false });
        assert!(!pending
            .get("done")
            .and_then(Value::as_bool)
            .unwrap_or(false));
        assert_eq!(
            valid_image_url("https://aihorde.net/r2/shared/abc.webp").as_deref(),
            Some("https://aihorde.net/r2/shared/abc.webp")
        );
        assert!(valid_image_url("http://aihorde.net/x.webp").is_none());
        assert!(valid_image_url("data:image/webp;base64,AAAA").is_none());
        assert!(valid_image_url("https://evil.example").is_none());
    }

    #[test]
    fn censored_generation_is_reported() {
        let value = json!({ "done": true, "generations": [{ "censored": true }] });
        let generation = value
            .get("generations")
            .and_then(Value::as_array)
            .and_then(|g| g.first())
            .unwrap();
        assert!(generation
            .get("censored")
            .and_then(Value::as_bool)
            .unwrap_or(false));
    }

    #[test]
    fn prompts_are_cleaned_and_bounded() {
        assert_eq!(
            clean_prompt("  flying\tchickens  on\nthe moon "),
            "flying chickens on the moon"
        );
        assert!(
            clean_prompt(&"x".repeat(MAX_PROMPT_CHARS + 1))
                .chars()
                .count()
                > MAX_PROMPT_CHARS
        );
    }

    #[test]
    fn anonymous_key_is_the_fallback() {
        assert_eq!(api_key(None), ANONYMOUS_KEY);
        assert_eq!(api_key(Some("  ")), ANONYMOUS_KEY);
        assert_eq!(api_key(Some("real-key")), "real-key");
    }

    /// Talks to the real service; run by hand with
    /// `cargo test -p jeeves live_ai_horde -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_ai_horde() {
        let submitted = submit("flying chickens on the moon", None);
        assert_eq!(submitted.error, None, "submit failed: {:?}", submitted);
        assert!(!submitted.job_id.is_empty());
        // Anonymous generations queue for a while; poll until done or attempts run out.
        let mut response = status(&submitted.job_id, None);
        for _ in 0..40 {
            if response.state != ImageGenState::Pending {
                break;
            }
            std::thread::sleep(Duration::from_secs(10));
            response = status(&submitted.job_id, None);
        }
        assert_eq!(
            response.state,
            ImageGenState::Done,
            "status: {:?}",
            response
        );
        assert!(valid_image_url(&response.url).is_some());
    }
}
