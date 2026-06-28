//! 301 darts game for rustjeeves.
//!
//! One game per channel; players join implicitly on their first throw.
//! Score counts down from 301 to exactly 0 — no double-out required.
//! Lifetime wins are tracked per stable profile UUID.
//!
//! Commands: !darts [1|2|3]  !darts score [nick]  !darts board  !dartsstats [!dstats]
//!
//! Theme keys (all under "darts.*"):
//!   channel_only, cooldown, throw_one, throw_many, bust, win,
//!   score, not_in_game, board, board_empty, stats, stats_none

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, Profile, ProfileKey,
    RandomBytesRequest, RandomBytesResponse, SendMessage, SettingGet, SettingKind, SettingScope,
    SettingSpec, SettingsManifest, ThemeReq, COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const STARTING_SCORE: u32 = 301;

// ── host function imports ─────────────────────────────────────────────────────

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn theme(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
    fn profile_ensure(input: String) -> String;
    fn profile_get(input: String) -> String;
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
    let mut dstats = c(
        "dartsstats",
        "Show your lifetime darts win stats.",
        "!dartsstats",
    );
    dstats.aliases = vec!["dstats".into()];
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            c(
                "darts",
                "Throw darts in a channel 301 game. Use !darts [1|2|3], !darts score [nick], or !darts board.",
                "!darts [1|2|3 | score [nick] | board]",
            ),
            dstats,
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
                key: "cooldown_secs".into(),
                description: "Seconds a player must wait between throws.".into(),
                default: "30".into(),
                kind: SettingKind::Integer { min: 0, max: 300 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
            SettingSpec {
                key: "stale_days".into(),
                description: "Days of inactivity before pruning a player from the board.".into(),
                default: "7".into(),
                kind: SettingKind::Integer { min: 1, max: 30 },
                scopes: vec![SettingScope::Global, SettingScope::Channel],
                applies_immediately: true,
            },
        ],
    })?)
}

// ── game state structs ────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, Clone)]
struct PlayerState {
    nick: String,
    score: u32,
    last_throw: i64,
    darts_thrown: u32,
    joined_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
struct GameState {
    players: Vec<PlayerState>,
}

#[derive(Serialize, Deserialize, Default)]
struct PlayerStats {
    wins: u32,
    total_darts: u64,
    best_darts: u32,
}

// ── dart probability model ────────────────────────────────────────────────────

struct DartResult {
    label: String,
    value: u32,
}

/// Derive one dart throw from 3 random bytes.
///
/// b_type % 128: 0 → DBull(50), 1-4 → Bull(25), 5-10 → Miss(0), 11-127 → numbered segment.
/// For numbered: number = (b_num % 20) + 1; multiplier from b_mult % 10: 0-5 → ×1, 6-8 → ×2, 9 → ×3.
fn throw_dart(b_type: u8, b_num: u8, b_mult: u8) -> DartResult {
    match b_type % 128 {
        0 => DartResult {
            label: "DBull".into(),
            value: 50,
        },
        1..=4 => DartResult {
            label: "Bull".into(),
            value: 25,
        },
        5..=10 => DartResult {
            label: "Miss".into(),
            value: 0,
        },
        _ => {
            let num = (b_num % 20) as u32 + 1;
            let (mult, prefix): (u32, &str) = match b_mult % 10 {
                0..=5 => (1, ""),
                6..=8 => (2, "D"),
                _ => (3, "T"),
            };
            DartResult {
                label: if prefix.is_empty() {
                    num.to_string()
                } else {
                    format!("{prefix}{num}")
                },
                value: num * mult,
            }
        }
    }
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

fn read_setting_i64(key: &str, server: &str, channel: &str, default: i64) -> i64 {
    (|| -> Option<i64> {
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
        raw.trim().parse().ok()
    })()
    .unwrap_or(default)
}

fn get_random_bytes(count: usize) -> Result<Vec<u8>, Error> {
    let raw =
        unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    let resp: RandomBytesResponse = serde_json::from_str(&raw)?;
    Ok(resp.bytes)
}

fn get_profile_uuid(server: &str, nick: &str) -> Result<Option<String>, Error> {
    unsafe {
        profile_ensure(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?;
    }
    let raw = unsafe {
        profile_get(serde_json::to_string(&ProfileKey {
            server: server.into(),
            nick: nick.into(),
        })?)?
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let p: Profile = serde_json::from_str(&raw)?;
    Ok(if p.id.is_empty() { None } else { Some(p.id) })
}

// ── KV helpers ────────────────────────────────────────────────────────────────

fn game_key(server: &str, channel: &str) -> String {
    format!("game:{server}:{channel}")
}

fn stats_key(uuid: &str) -> String {
    format!("stats:{uuid}")
}

fn load_game(server: &str, channel: &str) -> Result<GameState, Error> {
    let raw = kv_load(&game_key(server, channel))?;
    if raw.is_empty() {
        return Ok(GameState::default());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_game(server: &str, channel: &str, state: &GameState) -> Result<(), Error> {
    kv_save(&game_key(server, channel), &serde_json::to_string(state)?)
}

fn load_stats(uuid: &str) -> Result<PlayerStats, Error> {
    let raw = kv_load(&stats_key(uuid))?;
    if raw.is_empty() {
        return Ok(PlayerStats::default());
    }
    Ok(serde_json::from_str(&raw).unwrap_or_default())
}

fn save_stats(uuid: &str, stats: &PlayerStats) -> Result<(), Error> {
    kv_save(&stats_key(uuid), &serde_json::to_string(stats)?)
}

// ── command handlers ──────────────────────────────────────────────────────────

fn cmd_throw(
    server: &str,
    channel: &str,
    nick: &str,
    display: &str,
    num_darts: usize,
) -> Result<(), Error> {
    let now = now_secs();
    let cooldown = read_setting_i64("cooldown_secs", server, channel, 30);
    let stale_secs = read_setting_i64("stale_days", server, channel, 7) * 86400;

    let mut game = load_game(server, channel)?;
    game.players.retain(|p| now - p.last_throw < stale_secs);

    // Cooldown check for existing players
    let player_idx = game
        .players
        .iter()
        .position(|p| p.nick.eq_ignore_ascii_case(nick));
    if let Some(i) = player_idx {
        let elapsed = now - game.players[i].last_throw;
        if cooldown > 0 && elapsed < cooldown {
            let wait = (cooldown - elapsed).to_string();
            reply(
                server,
                channel,
                &themed(
                    "darts.cooldown",
                    &["{nick}, wait {secs}s before throwing again."],
                    &[("nick", display), ("secs", &wait)],
                )?,
            )?;
            return Ok(());
        }
    }

    // Throw darts
    let bytes = get_random_bytes(num_darts * 3)?;
    let darts: Vec<DartResult> = (0..num_darts)
        .map(|i| throw_dart(bytes[i * 3], bytes[i * 3 + 1], bytes[i * 3 + 2]))
        .collect();

    let current_score = player_idx
        .map(|i| game.players[i].score)
        .unwrap_or(STARTING_SCORE);
    let turn_total: u32 = darts.iter().map(|d| d.value).sum();
    let bust = turn_total > current_score;
    let new_score = if bust {
        current_score
    } else {
        current_score - turn_total
    };

    // Update player entry
    let darts_now = if let Some(i) = player_idx {
        if !bust {
            game.players[i].score = new_score;
        }
        game.players[i].last_throw = now;
        game.players[i].darts_thrown += num_darts as u32;
        game.players[i].darts_thrown
    } else {
        game.players.push(PlayerState {
            nick: nick.to_string(),
            score: new_score,
            last_throw: now,
            darts_thrown: num_darts as u32,
            joined_at: now,
        });
        num_darts as u32
    };

    let darts_str = darts
        .iter()
        .map(|d| format!("{} ({})", d.label, d.value))
        .collect::<Vec<_>>()
        .join(", ");
    let turn_str = turn_total.to_string();
    let score_str = current_score.to_string();
    let new_score_str = new_score.to_string();
    let count_str = num_darts.to_string();
    let darts_now_str = darts_now.to_string();

    if bust {
        save_game(server, channel, &game)?;
        reply(
            server,
            channel,
            &themed(
                "darts.bust",
                &["{nick} throws {darts} → BUST! ({turn} pts, needed ≤{score}) Score holds."],
                &[
                    ("nick", display),
                    ("darts", &darts_str),
                    ("turn", &turn_str),
                    ("score", &score_str),
                ],
            )?,
        )?;
        return Ok(());
    }

    if new_score == 0 {
        game.players.retain(|p| !p.nick.eq_ignore_ascii_case(nick));
        save_game(server, channel, &game)?;

        if let Ok(Some(uuid)) = get_profile_uuid(server, nick) {
            if let Ok(mut stats) = load_stats(&uuid) {
                stats.wins += 1;
                stats.total_darts += darts_now as u64;
                if stats.best_darts == 0 || darts_now < stats.best_darts {
                    stats.best_darts = darts_now;
                }
                let _ = save_stats(&uuid, &stats);
            }
        }

        reply(
            server,
            channel,
            &themed(
                "darts.win",
                &["{nick} throws {darts} → CHECKOUT! {nick} wins 301 in {count} darts!"],
                &[
                    ("nick", display),
                    ("darts", &darts_str),
                    ("count", &darts_now_str),
                ],
            )?,
        )?;
        return Ok(());
    }

    save_game(server, channel, &game)?;

    if num_darts == 1 {
        reply(
            server,
            channel,
            &themed(
                "darts.throw_one",
                &["{nick} throws: {dart} — {score} remaining"],
                &[("nick", display), ("dart", &darts_str), ("score", &new_score_str)],
            )?,
        )?;
    } else {
        reply(
            server,
            channel,
            &themed(
                "darts.throw_many",
                &["{nick} throws {count}: {darts} → {turn} pts — {score} remaining"],
                &[
                    ("nick", display),
                    ("count", &count_str),
                    ("darts", &darts_str),
                    ("turn", &turn_str),
                    ("score", &new_score_str),
                ],
            )?,
        )?;
    }

    Ok(())
}

fn cmd_board(server: &str, channel: &str) -> Result<(), Error> {
    let now = now_secs();
    let stale_secs = read_setting_i64("stale_days", server, channel, 7) * 86400;
    let mut game = load_game(server, channel)?;
    game.players.retain(|p| now - p.last_throw < stale_secs);

    if game.players.is_empty() {
        reply(
            server,
            channel,
            &themed(
                "darts.board_empty",
                &["No active players. Throw to join: !darts"],
                &[],
            )?,
        )?;
        return Ok(());
    }

    let mut sorted = game.players.clone();
    sorted.sort_by_key(|p| p.score);

    let entries: Vec<String> = sorted
        .iter()
        .enumerate()
        .map(|(i, p)| format!("{}. {} ({} left, {} darts)", i + 1, p.nick, p.score, p.darts_thrown))
        .collect();
    let board_str = entries.join(" | ");

    reply(
        server,
        channel,
        &themed("darts.board", &["Darts board: {board}"], &[("board", &board_str)])?,
    )?;
    Ok(())
}

fn cmd_score(
    server: &str,
    channel: &str,
    display: &str,
    target_nick: &str,
) -> Result<(), Error> {
    let now = now_secs();
    let stale_secs = read_setting_i64("stale_days", server, channel, 7) * 86400;
    let mut game = load_game(server, channel)?;
    game.players.retain(|p| now - p.last_throw < stale_secs);

    match game
        .players
        .iter()
        .find(|p| p.nick.eq_ignore_ascii_case(target_nick))
    {
        Some(p) => {
            let score_str = p.score.to_string();
            let darts_str = p.darts_thrown.to_string();
            reply(
                server,
                channel,
                &themed(
                    "darts.score",
                    &["{nick}: {score} remaining, {darts} darts thrown"],
                    &[("nick", display), ("score", &score_str), ("darts", &darts_str)],
                )?,
            )?;
        }
        None => {
            reply(
                server,
                channel,
                &themed(
                    "darts.not_in_game",
                    &["{nick} is not in the current game. Throw to join: !darts"],
                    &[("nick", display)],
                )?,
            )?;
        }
    }
    Ok(())
}

fn cmd_stats(server: &str, channel: &str, nick: &str, display: &str) -> Result<(), Error> {
    let uuid = match get_profile_uuid(server, nick)? {
        Some(u) => u,
        None => {
            reply(
                server,
                channel,
                &themed(
                    "darts.stats_none",
                    &["{nick} has no darts stats yet. Throw with !darts"],
                    &[("nick", display)],
                )?,
            )?;
            return Ok(());
        }
    };

    let stats = load_stats(&uuid)?;
    if stats.wins == 0 {
        reply(
            server,
            channel,
            &themed(
                "darts.stats_none",
                &["{nick} has no wins yet. Throw with !darts"],
                &[("nick", display)],
            )?,
        )?;
        return Ok(());
    }

    let wins_str = stats.wins.to_string();
    let avg = format!(
        "{:.1}",
        stats.total_darts as f64 / stats.wins as f64
    );
    let best_str = if stats.best_darts > 0 {
        stats.best_darts.to_string()
    } else {
        "—".into()
    };

    reply(
        server,
        channel,
        &themed(
            "darts.stats",
            &["{nick}: {wins} win(s) | avg {avg} darts/win | best {best} darts"],
            &[
                ("nick", display),
                ("wins", &wins_str),
                ("avg", &avg),
                ("best", &best_str),
            ],
        )?,
    )?;
    Ok(())
}

// ── dispatch ──────────────────────────────────────────────────────────────────

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    let text = msg.text.trim();
    let lower = text.to_ascii_lowercase();

    if !lower.starts_with("!darts") && !lower.starts_with("!dstats") {
        return Ok(());
    }

    if msg.is_private {
        reply(
            &server,
            &msg.nick,
            &themed(
                "darts.channel_only",
                &["Darts must be played in a channel, not in private."],
                &[],
            )?,
        )?;
        return Ok(());
    }

    let channel = &msg.target;
    let nick = &msg.nick;
    let display = if msg.display.is_empty() {
        nick.as_str()
    } else {
        msg.display.as_str()
    };

    // !dartsstats / !dstats
    if lower.starts_with("!dartsstats") || lower.starts_with("!dstats") {
        cmd_stats(&server, channel, nick, display)?;
        return Ok(());
    }

    // !darts [subcommand]
    let rest = text[6..].trim(); // text after "!darts"
    let sub = rest.split_whitespace().next().unwrap_or("");

    match sub {
        "" | "1" | "2" | "3" => {
            let n = sub.parse::<usize>().unwrap_or(1).clamp(1, 3);
            cmd_throw(&server, channel, nick, display, n)?;
        }
        "board" => cmd_board(&server, channel)?,
        "score" => {
            let target = rest["score".len()..].trim();
            if target.is_empty() {
                cmd_score(&server, channel, display, nick)?;
            } else {
                cmd_score(&server, channel, target, target)?;
            }
        }
        _ => {}
    }

    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn double_bull() {
        let d = throw_dart(0, 0, 0);
        assert_eq!(d.label, "DBull");
        assert_eq!(d.value, 50);
    }

    #[test]
    fn single_bull() {
        let d = throw_dart(3, 0, 0);
        assert_eq!(d.label, "Bull");
        assert_eq!(d.value, 25);
    }

    #[test]
    fn miss() {
        let d = throw_dart(8, 0, 0);
        assert_eq!(d.label, "Miss");
        assert_eq!(d.value, 0);
    }

    #[test]
    fn single_segment() {
        // b_type % 128 = 20 → numbered; b_num % 20 = 5 → 6; b_mult % 10 = 3 → ×1
        let d = throw_dart(20, 5, 3);
        assert_eq!(d.label, "6");
        assert_eq!(d.value, 6);
    }

    #[test]
    fn double_segment() {
        // b_type % 128 = 20 → numbered; b_num % 20 = 19 → 20; b_mult % 10 = 7 → ×2
        let d = throw_dart(20, 19, 7);
        assert_eq!(d.label, "D20");
        assert_eq!(d.value, 40);
    }

    #[test]
    fn triple_segment() {
        // b_type % 128 = 20 → numbered; b_num % 20 = 9 → 10; b_mult % 10 = 9 → ×3
        let d = throw_dart(20, 9, 9);
        assert_eq!(d.label, "T10");
        assert_eq!(d.value, 30);
    }

    #[test]
    fn bust_when_over() {
        let score: u32 = 20;
        let turn: u32 = 21;
        assert!(turn > score);
    }

    #[test]
    fn win_when_exact() {
        let score: u32 = 20;
        let turn: u32 = 20;
        assert!(!(turn > score)); // not a bust
        assert_eq!(score - turn, 0);
    }

    #[test]
    fn max_single_dart() {
        // T20 is maximum single dart (60)
        let d = throw_dart(20, 19, 9); // 20 × 3 = 60
        assert_eq!(d.label, "T20");
        assert_eq!(d.value, 60);
    }
}
