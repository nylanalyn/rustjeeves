//! Spontaneous animal hunt game for rustjeeves.
//!
//! At random intervals the bot releases a wild animal into an enabled channel.
//! The first person to !hunt or !hug it claims it. Scores are tracked per channel.
//!
//! IMPORTANT: must be explicitly enabled per channel via the `enabled` setting.
//!
//! Commands: !hunt  !hug  !hunt score [nick]  !hunt top
//!
//! Theme keys (all under "hunt.*"):
//!   release, caught, hugged, escaped, nothing,
//!   score, no_score, top, top_empty

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, RandomBytesRequest,
    RandomBytesResponse, ScheduleCancel, ScheduleList, ScheduleSet, ScheduledJob, SendMessage,
    SettingGet, SettingKind, SettingScope, SettingSpec, SettingsManifest, ThemeReq,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const ANIMALS: &[&str] = &[
    "cat", "kitten", "puppy", "duck", "rabbit", "squirrel", "hedgehog",
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
}

// ── command manifest ──────────────────────────────────────────────────────────

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let c = |name: &str, desc: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: desc.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            c(
                "hunt",
                "Catch or check scores in the channel animal hunt. Use !hunt, !hunt score [nick], or !hunt top.",
                "!hunt [score [nick] | top]",
            ),
            c("hug", "Hug the animal instead of catching it.", "!hug"),
        ],
    })?)
}

// ── settings manifest ─────────────────────────────────────────────────────────

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![
            SettingSpec {
                key: "enabled".into(),
                description: "Whether to release animals spontaneously in this channel.".into(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel, SettingScope::Network, SettingScope::Global],
                applies_immediately: true,
            },
            SettingSpec {
                key: "min_interval_mins".into(),
                description: "Minimum minutes between animal appearances.".into(),
                default: "60".into(),
                kind: SettingKind::Integer { min: 5, max: 1440 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "max_interval_mins".into(),
                description: "Maximum minutes between animal appearances.".into(),
                default: "180".into(),
                kind: SettingKind::Integer { min: 5, max: 2880 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "expire_mins".into(),
                description: "Minutes before an unclaimed animal wanders away.".into(),
                default: "10".into(),
                kind: SettingKind::Integer { min: 1, max: 60 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── state structs ─────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct ActiveEvent {
    animal: String,
    released_at: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct BoardEntry {
    /// Stable profile UUID — may be empty for users without a profile.
    user_id: String,
    nick: String,
    hunted: u32,
    hugged: u32,
}

// ── job ID helpers (encoded per server+channel to avoid cross-channel cancel) ─

fn next_job_id(server: &str, channel: &str) -> String {
    format!("next:{server}:{channel}")
}

fn expire_job_id(server: &str, channel: &str) -> String {
    format!("expire:{server}:{channel}")
}

// ── KV helpers ────────────────────────────────────────────────────────────────

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

fn active_key(server: &str, channel: &str) -> String {
    format!("active:{server}:{channel}")
}

fn board_key(server: &str, channel: &str) -> String {
    format!("board:{server}:{channel}")
}

fn load_active(server: &str, channel: &str) -> Result<Option<ActiveEvent>, Error> {
    let raw = kv_load(&active_key(server, channel))?;
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(serde_json::from_str(&raw).ok())
}

fn clear_active(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&active_key(server, channel), "")
}

fn load_board(server: &str, channel: &str) -> Result<Vec<BoardEntry>, Error> {
    let raw = kv_load(&board_key(server, channel))?;
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_board(server: &str, channel: &str, board: &[BoardEntry]) -> Result<(), Error> {
    kv_save(&board_key(server, channel), &serde_json::to_string(board)?)
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
    let raw =
        unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

fn has_pending_job(server: &str, channel: &str, id: &str) -> bool {
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
    jobs.iter().any(|j| j.id == id)
}

// ── scheduling ────────────────────────────────────────────────────────────────

fn schedule_next(server: &str, channel: &str) -> Result<(), Error> {
    let min_mins = read_setting_i64("min_interval_mins", server, channel, 60);
    let max_mins = read_setting_i64("max_interval_mins", server, channel, 180).max(min_mins + 1);

    let bytes = get_random_bytes(4)?;
    let range = ((max_mins - min_mins) * 60).max(1) as u64;
    let r = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as u64;
    let delay = min_mins * 60 + (r % range) as i64;

    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: next_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            due_at: now_secs() + delay,
            payload: String::new(),
        })?)?;
    }
    Ok(())
}

/// Ensure a "next" job is queued for this channel if none is pending and no animal is active.
/// Called lazily on every message in enabled channels so the module bootstraps itself.
fn ensure_scheduled(server: &str, channel: &str) -> Result<(), Error> {
    let nid = next_job_id(server, channel);
    let eid = expire_job_id(server, channel);
    if has_pending_job(server, channel, &nid) || has_pending_job(server, channel, &eid) {
        return Ok(());
    }
    if load_active(server, channel)?.is_some() {
        return Ok(());
    }
    schedule_next(server, channel)
}

fn cancel_expire(server: &str, channel: &str) {
    let _ = unsafe {
        schedule_cancel(
            serde_json::to_string(&ScheduleCancel {
                id: expire_job_id(server, channel),
            })
            .unwrap_or_default(),
        )
    };
}

// ── timer handlers ────────────────────────────────────────────────────────────

fn handle_next(server: &str, channel: &str) -> Result<(), Error> {
    if !read_setting_bool("enabled", server, channel, false) {
        return Ok(());
    }

    let bytes = get_random_bytes(5)?;
    let animal = ANIMALS[bytes[0] as usize % ANIMALS.len()];

    let active = ActiveEvent {
        animal: animal.to_string(),
        released_at: now_secs(),
    };
    kv_save(&active_key(server, channel), &serde_json::to_string(&active)?)?;

    let expire_mins = read_setting_i64("expire_mins", server, channel, 10);
    unsafe {
        schedule_set(serde_json::to_string(&ScheduleSet {
            id: expire_job_id(server, channel),
            server: server.into(),
            channel: channel.into(),
            due_at: now_secs() + expire_mins * 60,
            payload: animal.to_string(),
        })?)?;
    }

    reply(
        server,
        channel,
        &themed(
            "hunt.release",
            &["A wild {animal} appears! Type !hunt to catch it or !hug to befriend it."],
            &[("animal", animal)],
        )?,
    )?;
    Ok(())
}

fn handle_expire(server: &str, channel: &str) -> Result<(), Error> {
    if let Some(event) = load_active(server, channel)? {
        clear_active(server, channel)?;
        reply(
            server,
            channel,
            &themed(
                "hunt.escaped",
                &["The {animal} wandered away..."],
                &[("animal", &event.animal)],
            )?,
        )?;
    }

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next(server, channel)?;
    }
    Ok(())
}

// ── command handlers ──────────────────────────────────────────────────────────

enum ClaimType {
    Hunt,
    Hug,
}

fn cmd_claim(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    user_id: &str,
    claim_type: ClaimType,
) -> Result<(), Error> {
    let Some(event) = load_active(server, channel)? else {
        reply(
            server,
            channel,
            &themed(
                "hunt.nothing",
                &["There's nothing here right now. Wait for an animal to appear."],
                &[],
            )?,
        )?;
        return Ok(());
    };

    let animal = event.animal.clone();
    cancel_expire(server, channel);
    clear_active(server, channel)?;

    // Update channel board
    let mut board = load_board(server, channel)?;
    let idx = if !user_id.is_empty() {
        board
            .iter()
            .position(|e| e.user_id == user_id)
            .or_else(|| board.iter().position(|e| e.nick.eq_ignore_ascii_case(nick)))
    } else {
        board.iter().position(|e| e.nick.eq_ignore_ascii_case(nick))
    };

    match idx {
        Some(i) => {
            board[i].nick = nick.to_string();
            if !user_id.is_empty() {
                board[i].user_id = user_id.to_string();
            }
            match &claim_type {
                ClaimType::Hunt => board[i].hunted += 1,
                ClaimType::Hug => board[i].hugged += 1,
            }
        }
        None => {
            board.push(BoardEntry {
                user_id: user_id.to_string(),
                nick: nick.to_string(),
                hunted: matches!(claim_type, ClaimType::Hunt) as u32,
                hugged: matches!(claim_type, ClaimType::Hug) as u32,
            });
        }
    }
    save_board(server, channel, &board)?;

    if read_setting_bool("enabled", server, channel, false) {
        schedule_next(server, channel)?;
    }

    match claim_type {
        ClaimType::Hunt => reply(
            server,
            channel,
            &themed(
                "hunt.caught",
                &["{nick} caught the {animal}!"],
                &[("nick", display), ("animal", &animal)],
            )?,
        )?,
        ClaimType::Hug => reply(
            server,
            channel,
            &themed(
                "hunt.hugged",
                &["{nick} hugged the {animal}!"],
                &[("nick", display), ("animal", &animal)],
            )?,
        )?,
    }
    Ok(())
}

fn cmd_score(server: &str, channel: &str, target_nick: &str, target_display: &str) -> Result<(), Error> {
    let board = load_board(server, channel)?;
    match board
        .iter()
        .find(|e| e.nick.eq_ignore_ascii_case(target_nick))
    {
        Some(e) => reply(
            server,
            channel,
            &themed(
                "hunt.score",
                &["{nick}: {hunted} caught, {hugged} hugged"],
                &[
                    ("nick", target_display),
                    ("hunted", &e.hunted.to_string()),
                    ("hugged", &e.hugged.to_string()),
                ],
            )?,
        )?,
        None => reply(
            server,
            channel,
            &themed(
                "hunt.no_score",
                &["{nick} hasn't caught or hugged anything yet."],
                &[("nick", target_display)],
            )?,
        )?,
    }
    Ok(())
}

fn cmd_top(server: &str, channel: &str) -> Result<(), Error> {
    let mut board = load_board(server, channel)?;

    if board.is_empty() {
        reply(
            server,
            channel,
            &themed(
                "hunt.top_empty",
                &["Nobody has caught or hugged anything yet. Watch for animals!"],
                &[],
            )?,
        )?;
        return Ok(());
    }

    board.sort_by(|a, b| {
        (b.hunted + b.hugged)
            .cmp(&(a.hunted + a.hugged))
            .then(b.hunted.cmp(&a.hunted))
    });

    let entries: Vec<String> = board
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, e)| {
            format!(
                "{}. {} ({} caught, {} hugged)",
                i + 1,
                e.nick,
                e.hunted,
                e.hugged
            )
        })
        .collect();

    reply(
        server,
        channel,
        &themed(
            "hunt.top",
            &["Hunt board: {board}"],
            &[("board", &entries.join(" | "))],
        )?,
    )?;
    Ok(())
}

// ── exports ───────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_event(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Timer { id, channel, .. } = env.event else {
        return Ok(());
    };

    if id.starts_with("next:") {
        handle_next(&server, &channel)?;
    } else if id.starts_with("expire:") {
        handle_expire(&server, &channel)?;
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

    if msg.is_private {
        return Ok(());
    }

    let channel = &msg.target;
    let enabled = read_setting_bool("enabled", &server, channel, false);

    if enabled {
        ensure_scheduled(&server, channel)?;
    }

    let text = msg.text.trim();
    let lower = text.to_ascii_lowercase();

    if !lower.starts_with("!hunt") && !lower.starts_with("!hug") {
        return Ok(());
    }

    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };
    let user_id = &msg.user_id;

    if lower == "!hug" || lower.starts_with("!hug ") {
        cmd_claim(&server, channel, nick, display, user_id, ClaimType::Hug)?;
        return Ok(());
    }

    // !hunt [score [nick] | top]
    let rest = text[5..].trim(); // after "!hunt"
    let sub = rest.split_whitespace().next().unwrap_or("");

    match sub {
        "" => cmd_claim(&server, channel, nick, display, user_id, ClaimType::Hunt)?,
        "score" => {
            let target = rest["score".len()..].trim();
            let (tnick, tdisp) = if target.is_empty() {
                (nick.as_str(), display)
            } else {
                (target, target)
            };
            cmd_score(&server, channel, tnick, tdisp)?;
        }
        "top" => cmd_top(&server, channel)?,
        _ => {}
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_channel_scoped() {
        assert_ne!(
            next_job_id("libera", "#general"),
            next_job_id("libera", "#other"),
        );
        assert_ne!(
            expire_job_id("net1", "#chan"),
            expire_job_id("net2", "#chan"),
        );
    }

    #[test]
    fn animal_selection_is_bounded() {
        for b in 0u8..=255 {
            let idx = b as usize % ANIMALS.len();
            assert!(idx < ANIMALS.len());
        }
    }

    #[test]
    fn random_delay_stays_in_range() {
        let min_mins: i64 = 60;
        let max_mins: i64 = 180;
        let range = ((max_mins - min_mins) * 60).max(1) as u64;
        // Test a few representative byte patterns
        for bytes in [[0u8, 0, 0, 0], [255, 255, 255, 255], [1, 2, 3, 4]] {
            let r = u32::from_le_bytes(bytes) as u64;
            let delay = min_mins * 60 + (r % range) as i64;
            assert!(delay >= min_mins * 60);
            assert!(delay < max_mins * 60);
        }
    }

    #[test]
    fn board_sort_order() {
        let mut board = vec![
            BoardEntry { user_id: String::new(), nick: "alice".into(), hunted: 1, hugged: 0 },
            BoardEntry { user_id: String::new(), nick: "bob".into(), hunted: 5, hugged: 3 },
            BoardEntry { user_id: String::new(), nick: "carol".into(), hunted: 2, hugged: 2 },
        ];
        board.sort_by(|a, b| {
            (b.hunted + b.hugged)
                .cmp(&(a.hunted + a.hugged))
                .then(b.hunted.cmp(&a.hunted))
        });
        assert_eq!(board[0].nick, "bob");   // 8 total
        assert_eq!(board[1].nick, "carol"); // 4 total
        assert_eq!(board[2].nick, "alice"); // 1 total
    }
}
