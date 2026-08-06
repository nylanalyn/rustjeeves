//! Periodic decorated `*pop*` for rustjeeves.
//!
//! Every few minutes Jeeves emits a `*pop*` into an opted-in channel, dressed up with a random
//! kaomoji/emoji flourish and (optionally) mIRC colour codes. It does nothing else. It exists
//! because someone asked for Jeeves to "pop" more.
//!
//! IMPORTANT: popping is off by default. An administrator turns it on in-channel with `!pop on`,
//!            or the operator flips the `enabled` setting for the channel. The in-channel toggle
//!            is a per-channel override stored in module KV and always wins over `enabled`.
//!
//! Commands: !pop [on | off | status]
//!
//! Theme keys (all under "pop.*"):
//!   shout (list — the pop flourishes themselves; operators edit this in theme.toml),
//!   turned_on (admin enabled popping; vars: nick),
//!   turned_off (admin disabled popping; vars: nick),
//!   already (toggle was already in that position; vars: nick, state),
//!   status (current state report; vars: state, mins),
//!   denied (non-admin tried to toggle; vars: nick)

use extism_pdk::*;
use jeeves_abi::{
    AchievementManifest, AchievementSpec, AchievementStat, AwardStatsRequest, CommandManifest,
    CommandSpec, Event, EventEnvelope, KvGet, KvSet, RandomBytesRequest, RandomBytesResponse, Role,
    ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob, SendMessage, SettingGet, SettingKind,
    SettingScope, SettingSpec, SettingsManifest, StatIncrement, ThemeReq,
    ACHIEVEMENT_MANIFEST_VERSION, COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};

// ── tuning constants ──────────────────────────────────────────────────────────

/// Longest base flourish we will decorate. Colour codes multiply length, so the base stays short.
const MAX_BASE_CHARS: usize = 64;
/// Decoration is dropped entirely if it would push the line past this many bytes. The host
/// truncates at 450; staying well under keeps a mangled half-escape off the wire.
const MAX_DECORATED_BYTES: usize = 380;
/// Floor on the scheduled delay, so a tiny interval plus negative jitter can't become a flood.
const MIN_DELAY_SECS: i64 = 30;

/// mIRC colour indices, chosen for legibility on both light and dark clients.
const PALETTE: &[u8] = &[4, 7, 8, 9, 11, 12, 13, 6];

/// The pop flourishes. Seeded into `theme.toml` on first use; operators replace them there.
/// Deliberately brace-free — `{`/`}` would read as theme placeholders.
const DEFAULT_POPS: &[&str] = &[
    "*pop*",
    "*p o p*",
    "*POP*",
    "( ꙭ ) *pop*",
    "ヽ(°〇°)ﾉ *POP*",
    "(っ˘ω˘ς ) *pop*",
    "⁽⁽ଘ( ˊᵕˋ )ଓ⁾⁾ *pop*",
    "( ºΔº )━ *pop*",
    "٩(◕‿◕)۶ *pop!*",
    "(ﾉ◕ヮ◕)ﾉ*:･ﾟ✧ *pop*",
    "◝(⑅•ᴗ•⑅)◜ *pop*",
    "(￣▽￣)ノ *pop*",
    "🫧 *pop* 🫧",
    "🎈 *POP* 💥",
    "🍾 *pop*",
    "✨ *pop* ✨",
    "🧋 *pop*",
    "🔮 *pop* 🔮",
    "🎉 *p-p-pop* 🎉",
    "🪩 *pop* 🪩",
    "🐡 *pop*",
    "*pop* — pardon me, sir.",
    "*pop* (that one was free)",
    "*pop*, and again: *pop*",
    "*ＰＯＰ*",
    "*ρ σ ρ*",
];

// ── host function imports ─────────────────────────────────────────────────────

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn schedule_set(input: String) -> String;
    fn schedule_cancel(input: String) -> String;
    fn schedule_list(input: String) -> String;
    fn award_stats(input: String) -> String;
}

// ── manifests ─────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![CommandSpec {
            name: "pop".into(),
            description: "Turn Jeeves's periodic *pop* on or off in this channel, or report it."
                .into(),
            usage: "!pop [on | off | status]".into(),
            aliases: Vec::new(),
        }],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether Jeeves pops in this channel by default. An in-channel \
                              `!pop on`/`!pop off` overrides this for that channel."
                    .into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "interval_mins".into(),
                description: "Average minutes between pops.".into(),
                default: "5".into(),
                kind: SettingKind::Integer { min: 1, max: 1440 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "jitter_secs".into(),
                description:
                    "Random spread either side of the interval, so pops aren't metronomic.".into(),
                default: "90".into(),
                kind: SettingKind::Integer { min: 0, max: 600 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "style".into(),
                description: "How loudly a pop is dressed: plain text, one colour, a rainbow \
                              gradient, or full chaos."
                    .into(),
                default: "chaos".into(),
                kind: SettingKind::Choice {
                    options: vec![
                        "plain".into(),
                        "color".into(),
                        "rainbow".into(),
                        "chaos".into(),
                    ],
                },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
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
            id: "enabled".into(),
            description: "Times you turned the pop on in a channel.".into(),
        }],
        achievements: vec![AchievementSpec {
            id: "master_of_ceremonies".into(),
            name: "Master of Ceremonies".into(),
            description: "Turn the pop on.".into(),
            stat: "enabled".into(),
            threshold: 1,
            // Admin-only and configuration-dependent, so it must not gate catalog completion.
            optional: true,
            secret: true,
        }],
        prestige: Vec::new(),
    })?)
}

// ── host helpers ──────────────────────────────────────────────────────────────

fn now_secs() -> i64 {
    unsafe {
        now(String::new())
            .unwrap_or_default()
            .trim()
            .parse()
            .unwrap_or(0)
    }
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

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    Ok(unsafe {
        theme(serde_json::to_string(&ThemeReq {
            key: key.into(),
            default: defaults.iter().map(|s| s.to_string()).collect(),
            vars: vars
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        })?)?
    })
}

fn kv_load(key: &str) -> Result<String, Error> {
    Ok(unsafe { kv_get(serde_json::to_string(&KvGet { key: key.into() })?)? })
}

fn kv_save(key: &str, value: &str) -> Result<(), Error> {
    unsafe {
        kv_set(serde_json::to_string(&KvSet {
            key: key.into(),
            value: value.into(),
        })?)?;
    }
    Ok(())
}

fn read_setting_raw(key: &str, server: &str, channel: &str) -> Option<String> {
    let raw = unsafe {
        setting_get(
            serde_json::to_string(&SettingGet {
                key: key.into(),
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .ok()?,
        )
        .ok()?
    };
    Some(raw)
}

fn read_setting_bool(key: &str, server: &str, channel: &str, default: bool) -> bool {
    read_setting_raw(key, server, channel)
        .and_then(|s| match s.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn read_setting_i64(key: &str, server: &str, channel: &str, default: i64) -> i64 {
    read_setting_raw(key, server, channel)
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(default)
}

fn get_random_bytes(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

// ── state ─────────────────────────────────────────────────────────────────────

fn toggle_key(server: &str, channel: &str) -> String {
    format!("toggle:{server}:{channel}")
}

fn job_id(server: &str, channel: &str) -> String {
    format!("pop:{server}:{channel}")
}

/// The in-channel override, if an admin has ever set one for this channel.
fn toggle_override(server: &str, channel: &str) -> Option<bool> {
    match kv_load(&toggle_key(server, channel))
        .unwrap_or_default()
        .trim()
    {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// Whether Jeeves should be popping here: the in-channel override, else the operator setting.
fn popping(server: &str, channel: &str) -> bool {
    toggle_override(server, channel)
        .unwrap_or_else(|| read_setting_bool("enabled", server, channel, false))
}

fn has_pending_job(server: &str, channel: &str) -> bool {
    let raw = unsafe {
        schedule_list(
            serde_json::to_string(&ScheduleList {
                server: Some(server.into()),
                channel: Some(channel.into()),
            })
            .unwrap_or_default(),
        )
        .unwrap_or_default()
    };
    let jobs: Vec<ScheduledJob> = serde_json::from_str(&raw).unwrap_or_default();
    let id = job_id(server, channel);
    jobs.iter().any(|job| job.id == id)
}

fn cancel_pop(server: &str, channel: &str) {
    let _ = unsafe {
        schedule_cancel(
            serde_json::to_string(&ScheduleCancel {
                id: job_id(server, channel),
            })
            .unwrap_or_default(),
        )
    };
}

fn schedule_pop(server: &str, channel: &str) -> Result<(), Error> {
    let interval = read_setting_i64("interval_mins", server, channel, 5).max(1) * 60;
    let jitter = read_setting_i64("jitter_secs", server, channel, 90).clamp(0, 600);
    let offset = if jitter == 0 {
        0
    } else {
        let bytes = get_random_bytes(4)?;
        let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as i64;
        r % (jitter * 2 + 1) - jitter
    };
    let delay = (interval + offset).max(MIN_DELAY_SECS);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            owner_profile_id: None,
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

/// Schedule the next pop unless one is already pending. Cheap enough to call per message; it is
/// what restarts the cycle after a reload without waiting for a manual `!pop on`.
fn ensure_scheduled(server: &str, channel: &str) -> Result<(), Error> {
    if !has_pending_job(server, channel) {
        schedule_pop(server, channel)?;
    }
    Ok(())
}

// ── decoration ────────────────────────────────────────────────────────────────

/// Pick a `u8` in `0..len` from a byte, without modulo bias mattering at these sizes.
fn pick(byte: u8, len: usize) -> usize {
    byte as usize % len.max(1)
}

/// Wrap the whole line in one random palette colour.
fn style_color(text: &str, rng: &[u8]) -> String {
    let color = PALETTE[pick(rng[0], PALETTE.len())];
    format!("\u{3}{color:02}{text}\u{f}")
}

/// Walk the palette one character at a time, starting at a random offset.
fn style_rainbow(text: &str, rng: &[u8]) -> String {
    let start = pick(rng[0], PALETTE.len());
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if ch == ' ' {
            out.push(ch);
            continue;
        }
        let color = PALETTE[(start + i) % PALETTE.len()];
        out.push_str(&format!("\u{3}{color:02}"));
        out.push(ch);
    }
    out.push('\u{f}');
    out
}

/// Random colour per character plus occasional bold/italic/reverse. This is the wild one.
fn style_chaos(text: &str, rng: &[u8]) -> String {
    let mut out = String::new();
    for (i, ch) in text.chars().enumerate() {
        if ch == ' ' {
            out.push(ch);
            continue;
        }
        let seed = rng[i % rng.len()] ^ (i as u8).wrapping_mul(31);
        let color = PALETTE[pick(seed, PALETTE.len())];
        out.push_str(&format!("\u{3}{color:02}"));
        match seed % 7 {
            0 => out.push('\u{2}'),  // bold
            1 => out.push('\u{1d}'), // italic
            2 => out.push('\u{16}'), // reverse
            _ => {}
        }
        out.push(ch);
    }
    out.push('\u{f}');
    out
}

/// Dress `base` according to `style`, falling back to plain text if the escapes would blow the
/// line budget (a truncated colour escape looks like garbage in every client).
fn decorate(base: &str, style: &str, rng: &[u8]) -> String {
    let base: String = base.chars().take(MAX_BASE_CHARS).collect();
    if rng.is_empty() {
        return base;
    }
    let decorated = match style {
        "color" => style_color(&base, rng),
        "rainbow" => style_rainbow(&base, rng),
        "chaos" => style_chaos(&base, rng),
        _ => return base,
    };
    if decorated.len() > MAX_DECORATED_BYTES {
        base
    } else {
        decorated
    }
}

fn emit_pop(server: &str, channel: &str) -> Result<(), Error> {
    let base = themed("pop.shout", DEFAULT_POPS, &[])?;
    if base.trim().is_empty() {
        return Ok(());
    }
    let style = read_setting_raw("style", server, channel)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "chaos".into());
    // One byte per character is plenty of entropy for per-character styling.
    let rng = get_random_bytes(MAX_BASE_CHARS)?;
    reply(server, channel, &decorate(&base, &style, &rng))
}

// ── commands ──────────────────────────────────────────────────────────────────

#[derive(Debug, PartialEq, Eq)]
enum PopCommand {
    On,
    Off,
    Status,
}

fn parse_command(text: &str) -> Option<PopCommand> {
    let mut parts = text.split_whitespace();
    let head = parts.next()?;
    if !head.eq_ignore_ascii_case("!pop") {
        return None;
    }
    match parts
        .next()
        .unwrap_or("status")
        .to_ascii_lowercase()
        .as_str()
    {
        "on" | "start" | "yes" => Some(PopCommand::On),
        "off" | "stop" | "no" => Some(PopCommand::Off),
        _ => Some(PopCommand::Status),
    }
}

fn award_enabled(server: &str, channel: &str, user_id: &str, display: &str) -> Result<(), Error> {
    if user_id.is_empty() {
        return Ok(());
    }
    unsafe {
        award_stats(serde_json::to_string(&AwardStatsRequest {
            server: server.into(),
            profile_id: user_id.into(),
            display_name: display.into(),
            target: channel.into(),
            increments: vec![StatIncrement {
                stat: "enabled".into(),
                amount: 1,
            }],
            deduplication_id: None,
        })?)?;
    }
    Ok(())
}

fn cmd_on(server: &str, channel: &str, display: &str, user_id: &str) -> Result<(), Error> {
    if popping(server, channel) {
        return reply(
            server,
            channel,
            &themed(
                "pop.already",
                &["I am already popping, {nick}."],
                &[("nick", display), ("state", "on")],
            )?,
        );
    }
    kv_save(&toggle_key(server, channel), "on")?;
    schedule_pop(server, channel)?;
    reply(
        server,
        channel,
        &themed(
            "pop.turned_on",
            &["Very good, {nick}. I shall pop."],
            &[("nick", display)],
        )?,
    )?;
    award_enabled(server, channel, user_id, display)
}

fn cmd_off(server: &str, channel: &str, display: &str) -> Result<(), Error> {
    let was_on = popping(server, channel);
    kv_save(&toggle_key(server, channel), "off")?;
    cancel_pop(server, channel);
    let (key, default) = if was_on {
        ("pop.turned_off", "As you wish, {nick}. Popping ceases.")
    } else {
        ("pop.already", "I was not popping to begin with, {nick}.")
    };
    reply(
        server,
        channel,
        &themed(key, &[default], &[("nick", display), ("state", "off")])?,
    )
}

fn cmd_status(server: &str, channel: &str) -> Result<(), Error> {
    let state = if popping(server, channel) {
        "on"
    } else {
        "off"
    };
    let mins = read_setting_i64("interval_mins", server, channel, 5).max(1);
    reply(
        server,
        channel,
        &themed(
            "pop.status",
            &["Popping is {state}, roughly every {mins} minutes."],
            &[("state", state), ("mins", &mins.to_string())],
        )?,
    )
}

// ── hooks ─────────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer { id, channel, .. } = env.event else {
        return Ok(());
    };
    if !id.starts_with("pop:") {
        return Ok(());
    }
    // Re-check on every firing: an operator can flip `enabled` off between pops, and the timer
    // only re-arms itself while popping is still wanted.
    if !popping(&server, &channel) {
        return Ok(());
    }
    emit_pop(&server, &channel)?;
    schedule_pop(&server, &channel)?;
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    if msg.is_private {
        return Ok(());
    }
    let channel = &msg.target;

    if popping(&server, channel) {
        ensure_scheduled(&server, channel)?;
    }

    let Some(command) = parse_command(msg.text.trim()) else {
        return Ok(());
    };
    let display = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };

    if command == PopCommand::Status {
        return Ok(cmd_status(&server, channel)?);
    }
    if !msg.role.is_some_and(|role| role.satisfies(Role::Admin)) {
        return Ok(reply(
            &server,
            channel,
            &themed(
                "pop.denied",
                &["Only administrators may direct my popping, {nick}."],
                &[("nick", display)],
            )?,
        )?);
    }
    match command {
        PopCommand::On => cmd_on(&server, channel, display, &msg.user_id)?,
        _ => cmd_off(&server, channel, display)?,
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_toggle_words_and_defaults_to_status() {
        assert_eq!(parse_command("!pop on"), Some(PopCommand::On));
        assert_eq!(parse_command("!POP OFF"), Some(PopCommand::Off));
        assert_eq!(parse_command("!pop"), Some(PopCommand::Status));
        assert_eq!(parse_command("!pop wharrgarbl"), Some(PopCommand::Status));
        assert_eq!(parse_command("popcorn"), None);
        assert_eq!(parse_command("!popcorn on"), None);
    }

    #[test]
    fn job_ids_are_channel_and_network_scoped() {
        assert_ne!(job_id("net", "#a"), job_id("net", "#b"));
        assert_ne!(job_id("net1", "#x"), job_id("net2", "#x"));
        assert!(job_id("net", "#x").starts_with("pop:"));
    }

    #[test]
    fn plain_style_leaves_text_untouched() {
        assert_eq!(decorate("*pop*", "plain", &[1, 2, 3, 4]), "*pop*");
        assert_eq!(decorate("*pop*", "nonsense", &[1, 2, 3, 4]), "*pop*");
    }

    /// Remove mIRC formatting so a decorated line can be compared against its source text.
    fn strip_formatting(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            match ch {
                '\u{3}' => {
                    for _ in 0..2 {
                        if chars.peek().is_some_and(|c| c.is_ascii_digit()) {
                            chars.next();
                        }
                    }
                }
                '\u{f}' | '\u{2}' | '\u{1d}' | '\u{16}' => {}
                _ => out.push(ch),
            }
        }
        out
    }

    #[test]
    fn decorated_styles_wrap_the_text_and_reset() {
        for style in ["color", "rainbow", "chaos"] {
            let out = decorate("*pop*", style, &[3, 9, 17, 200, 41, 7]);
            assert!(out.starts_with('\u{3}'), "{style} must open with a colour");
            assert!(out.ends_with('\u{f}'), "{style} must reset at the end");
            assert_eq!(
                strip_formatting(&out),
                "*pop*",
                "{style} must keep the text"
            );
        }
    }

    #[test]
    fn decoration_preserves_multibyte_flourishes() {
        let base = "ヽ(°〇°)ﾉ *POP* 🫧";
        let out = decorate(base, "rainbow", &[5, 11, 2, 8]);
        assert_eq!(strip_formatting(&out), base);
    }

    #[test]
    fn oversized_decoration_falls_back_to_plain() {
        // Multibyte flourishes are where escapes actually blow the budget: 4 bytes of emoji plus
        // a 3-byte colour escape per character.
        let long = "🎈".repeat(MAX_BASE_CHARS);
        let out = decorate(&long, "chaos", &[7]);
        assert_eq!(out, long);
    }

    #[test]
    fn empty_entropy_degrades_to_plain_rather_than_panicking() {
        assert_eq!(decorate("*pop*", "chaos", &[]), "*pop*");
    }

    #[test]
    fn base_text_is_bounded() {
        let long = "p".repeat(MAX_BASE_CHARS * 4);
        assert_eq!(
            decorate(&long, "plain", &[7]).chars().count(),
            MAX_BASE_CHARS
        );
    }

    #[test]
    fn default_pops_avoid_theme_placeholder_braces() {
        for pop in DEFAULT_POPS {
            assert!(!pop.contains('{') && !pop.contains('}'), "{pop} has braces");
        }
    }
}
