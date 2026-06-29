//! Asynchronous channel-local 301 darts, modelled after the original Jeeves game.

use extism_pdk::*;
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, KvGet, KvSet, MessagePayload,
    RandomBytesRequest, RandomBytesResponse, Role, SendMessage, SettingGet, SettingKind,
    SettingScope, SettingSpec, SettingsManifest, ThemeReq, COMMAND_MANIFEST_VERSION,
    SETTINGS_MANIFEST_VERSION,
};
use serde::{Deserialize, Serialize};

const STARTING_SCORE: u32 = 301;
const MAX_DARTS_PER_TURN: u8 = 3;
const DEFAULT_COOLDOWN_SECS: i64 = 30 * 60;
const MAX_PLAYERS: usize = 100;

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn theme(input: String) -> String;
    fn kv_get(input: String) -> String;
    fn kv_set(input: String) -> String;
    fn now(input: String) -> String;
    fn setting_get(input: String) -> String;
    fn random_bytes(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            CommandSpec {
                name: "darts".into(),
                aliases: Vec::new(),
                description: "Play the channel's asynchronous 301 darts match.".into(),
                usage: "!darts [1|2|3 | score | reset]".into(),
            },
            CommandSpec {
                name: "dartsstats".into(),
                aliases: vec!["dstats".into()],
                description: "Show your lifetime darts wins.".into(),
                usage: "!dartsstats".into(),
            },
        ],
    })?)
}

#[plugin_fn]
pub fn settings(_: String) -> FnResult<String> {
    Ok(serde_json::to_string(&SettingsManifest {
        version: SETTINGS_MANIFEST_VERSION,
        settings: vec![SettingSpec {
            key: "cooldown_secs".into(),
            description: "Rest after a player's third dart; another player's throw ends it.".into(),
            default: DEFAULT_COOLDOWN_SECS.to_string(),
            kind: SettingKind::DurationSeconds {
                min: 0,
                max: 24 * 60 * 60,
            },
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }],
    })?)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Player {
    user_id: String,
    nick: String,
    display: String,
    remaining: u32,
    joined_at: i64,
    #[serde(default)]
    turn_darts: u8,
    #[serde(default)]
    cooldown_until: i64,
    #[serde(default)]
    match_darts: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Game {
    players: Vec<Player>,
    created_at: i64,
}

#[derive(Default, Serialize, Deserialize)]
struct Stats {
    wins: u32,
    total_darts: u64,
    best_darts: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Dart {
    label: String,
    points: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Normal,
    Miss,
    Bust,
    Win,
}

fn game_key(server: &str, channel: &str) -> String {
    format!("game:{server}:{channel}")
}

fn stats_key(server: &str, user_id: &str) -> String {
    format!("stats:{server}:{user_id}")
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

fn load_game(server: &str, channel: &str) -> Result<Game, Error> {
    let raw = kv_load(&game_key(server, channel))?;
    let mut game: Game = serde_json::from_str(&raw).unwrap_or_default();
    // Do not allow legacy nick-only entries to be claimed by a new owner of that nick.
    game.players.retain(|player| !player.user_id.is_empty());
    Ok(game)
}

fn save_game(server: &str, channel: &str, game: &Game) -> Result<(), Error> {
    kv_save(&game_key(server, channel), &serde_json::to_string(game)?)
}

fn clear_game(server: &str, channel: &str) -> Result<(), Error> {
    kv_save(&game_key(server, channel), "")
}

fn load_stats(server: &str, user_id: &str) -> Result<Stats, Error> {
    Ok(serde_json::from_str(&kv_load(&stats_key(server, user_id))?).unwrap_or_default())
}

fn save_stats(server: &str, user_id: &str, stats: &Stats) -> Result<(), Error> {
    kv_save(&stats_key(server, user_id), &serde_json::to_string(stats)?)
}

fn now_secs() -> Result<i64, Error> {
    Ok(unsafe { now(String::new())? }.parse().unwrap_or(0))
}

fn setting_i64(key: &str, server: &str, channel: &str, fallback: i64) -> i64 {
    (|| -> Option<i64> {
        unsafe {
            setting_get(
                serde_json::to_string(&SettingGet {
                    key: key.into(),
                    server: Some(server.into()),
                    channel: Some(channel.into()),
                })
                .ok()?,
            )
            .ok()?
            .parse()
            .ok()
        }
    })()
    .unwrap_or(fallback)
}

fn host_random(count: usize) -> Result<Vec<u8>, Error> {
    let raw = unsafe { random_bytes(serde_json::to_string(&RandomBytesRequest { count })?)? };
    Ok(serde_json::from_str::<RandomBytesResponse>(&raw)?.bytes)
}

/// Weighted board from the original: singles 4, doubles 2, triples 1, outer bull 2,
/// bullseye 1, miss 2. Total weight: 145.
fn dart_from_roll(roll: u16) -> Dart {
    let roll = roll % 145;
    match roll {
        0..=79 => {
            let number = (roll / 4) as u32 + 1;
            Dart {
                label: number.to_string(),
                points: number,
            }
        }
        80..=119 => {
            let number = ((roll - 80) / 2) as u32 + 1;
            Dart {
                label: format!("double {number}"),
                points: number * 2,
            }
        }
        120..=139 => {
            let number = (roll - 120) as u32 + 1;
            Dart {
                label: format!("triple {number}"),
                points: number * 3,
            }
        }
        140..=141 => Dart {
            label: "outer bull".into(),
            points: 25,
        },
        142 => Dart {
            label: "bullseye".into(),
            points: 50,
        },
        _ => Dart {
            label: "miss".into(),
            points: 0,
        },
    }
}

fn apply_dart(remaining: &mut u32, dart: &Dart) -> Outcome {
    if dart.points == 0 {
        return Outcome::Miss;
    }
    if dart.points > *remaining {
        return Outcome::Bust;
    }
    *remaining -= dart.points;
    if *remaining == 0 {
        Outcome::Win
    } else {
        Outcome::Normal
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
            default: defaults.iter().map(|value| (*value).into()).collect(),
            vars: vars
                .iter()
                .map(|(key, value)| ((*key).into(), (*value).into()))
                .collect(),
        })?)?
    })
}

fn identity(msg: &MessagePayload) -> String {
    if msg.user_id.is_empty() {
        format!("nick:{}", msg.nick.to_ascii_lowercase())
    } else {
        msg.user_id.clone()
    }
}

fn display(msg: &MessagePayload) -> &str {
    if msg.display.is_empty() {
        &msg.nick
    } else {
        &msg.display
    }
}

fn throw(server: &str, msg: &MessagePayload, requested: u8) -> Result<(), Error> {
    let channel = &msg.target;
    let now = now_secs()?;
    let cooldown_secs = setting_i64("cooldown_secs", server, channel, DEFAULT_COOLDOWN_SECS);
    let user_id = identity(msg);
    let mut game = load_game(server, channel)?;
    if game.created_at == 0 {
        game.created_at = now;
    }

    let existing = game
        .players
        .iter()
        .position(|player| player.user_id == user_id);
    if existing.is_none() && game.players.len() >= MAX_PLAYERS {
        return reply(
            server,
            channel,
            &themed("darts.full", &["The darts match is full."], &[])?,
        );
    }
    if existing.is_none() {
        game.players.push(Player {
            user_id: user_id.clone(),
            nick: msg.nick.clone(),
            display: display(msg).into(),
            remaining: STARTING_SCORE,
            joined_at: now,
            ..Default::default()
        });
    }
    let index = game
        .players
        .iter()
        .position(|player| player.user_id == user_id)
        .unwrap();
    if game.players[index].cooldown_until > now {
        let minutes = (game.players[index].cooldown_until - now + 59) / 60;
        return reply(
            server,
            channel,
            &themed(
                "darts.cooldown",
                &["{user}'s throwing arm needs a rest: about {minutes} minute(s) remain. Another player throwing will end it."],
                &[("user", display(msg)), ("minutes", &minutes.to_string())],
            )?,
        );
    }

    let released = game
        .players
        .iter()
        .any(|player| player.user_id != user_id && player.cooldown_until > now);
    if released {
        for player in &mut game.players {
            if player.user_id != user_id && player.cooldown_until > now {
                player.cooldown_until = 0;
                player.turn_darts = 0;
            }
        }
    }

    let available = MAX_DARTS_PER_TURN - game.players[index].turn_darts;
    if requested > available {
        return reply(
            server,
            channel,
            &themed(
                "darts.turn_limit",
                &["You have only {count} dart(s) left this turn, {user}."],
                &[("count", &available.to_string()), ("user", display(msg))],
            )?,
        );
    }

    let bytes = host_random(requested as usize * 2)?;
    let mut results = Vec::new();
    let mut won = false;
    for pair in bytes.chunks_exact(2) {
        let dart = dart_from_roll(u16::from_le_bytes([pair[0], pair[1]]));
        let outcome = apply_dart(&mut game.players[index].remaining, &dart);
        game.players[index].turn_darts += 1;
        game.players[index].match_darts += 1;
        results.push((dart, outcome));
        if matches!(outcome, Outcome::Miss | Outcome::Bust | Outcome::Win) {
            won = outcome == Outcome::Win;
            break;
        }
    }
    game.players[index].nick = msg.nick.clone();
    game.players[index].display = display(msg).into();

    let details = results
        .iter()
        .map(|(dart, outcome)| match outcome {
            Outcome::Normal => format!("{} ({} pts)", dart.label, dart.points),
            Outcome::Miss => "miss (turn ends)".into(),
            Outcome::Bust => format!("{} ({} pts) — bust", dart.label, dart.points),
            Outcome::Win => format!("{} ({} pts) — exactly zero", dart.label, dart.points),
        })
        .collect::<Vec<_>>()
        .join(" · ");

    if won {
        let darts = game.players[index].match_darts;
        let mut stats = load_stats(server, &user_id)?;
        stats.wins += 1;
        stats.total_darts += darts as u64;
        if stats.best_darts == 0 || darts < stats.best_darts {
            stats.best_darts = darts;
        }
        save_stats(server, &user_id, &stats)?;
        clear_game(server, channel)?;
        return reply(
            server,
            channel,
            &themed(
                "darts.win",
                &["{user} throws {throws}. Magnificent — exactly zero in {count} darts! The match is complete."],
                &[("user", display(msg)), ("throws", &details), ("count", &darts.to_string())],
            )?,
        );
    }

    let remaining = game.players[index].remaining;
    if game.players[index].turn_darts >= MAX_DARTS_PER_TURN {
        game.players[index].turn_darts = 0;
        game.players[index].cooldown_until = now.saturating_add(cooldown_secs);
    }
    let resting = game.players[index].cooldown_until > now;
    save_game(server, channel, &game)?;
    reply(
        server,
        channel,
        &themed(
            if resting {
                "darts.throw_rest"
            } else {
                "darts.throw"
            },
            if resting {
                &["{user} throws: {throws}. {remaining} remain. Three darts complete; the throwing arm rests until another player steps up."]
            } else {
                &["{user} throws: {throws}. {remaining} remain."]
            },
            &[
                ("user", display(msg)),
                ("throws", &details),
                ("remaining", &remaining.to_string()),
            ],
        )?,
    )
}

fn score(server: &str, channel: &str) -> Result<(), Error> {
    let mut game = load_game(server, channel)?;
    if game.players.is_empty() {
        return reply(
            server,
            channel,
            &themed(
                "darts.empty",
                &["No active darts match. Use !darts to begin."],
                &[],
            )?,
        );
    }
    game.players.sort_by_key(|player| player.remaining);
    let board = game
        .players
        .iter()
        .take(10)
        .map(|player| format!("{}: {}", player.display, player.remaining))
        .collect::<Vec<_>>()
        .join(" | ");
    reply(
        server,
        channel,
        &themed("darts.score", &["{board}"], &[("board", &board)])?,
    )
}

fn stats(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let stats = load_stats(server, &identity(msg))?;
    let average = if stats.wins == 0 {
        "—".into()
    } else {
        format!("{:.1}", stats.total_darts as f64 / stats.wins as f64)
    };
    reply(
        server,
        &msg.target,
        &themed(
            "darts.stats",
            &["{user}: {wins} win(s), average {average} darts, best {best}."],
            &[
                ("user", display(msg)),
                ("wins", &stats.wins.to_string()),
                ("average", &average),
                ("best", &stats.best_darts.to_string()),
            ],
        )?,
    )
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };
    let token = msg
        .text
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(token.as_str(), "!darts" | "!dartsstats" | "!dstats") {
        return Ok(());
    }
    if msg.is_private {
        reply(
            &env.server,
            &msg.nick,
            &themed(
                "darts.channel_only",
                &["Darts is played in a channel."],
                &[],
            )?,
        )?;
        return Ok(());
    }
    if matches!(token.as_str(), "!dartsstats" | "!dstats") {
        stats(&env.server, &msg)?;
        return Ok(());
    }
    let rest = msg.text[token.len()..].trim().to_ascii_lowercase();
    match rest.as_str() {
        "" => throw(&env.server, &msg, 1)?,
        "1" | "2" | "3" => throw(&env.server, &msg, rest.parse().unwrap_or(1))?,
        "score" | "board" => score(&env.server, &msg.target)?,
        "reset" if msg.role.is_some_and(|role| role.satisfies(Role::Admin)) => {
            clear_game(&env.server, &msg.target)?;
            reply(
                &env.server,
                &msg.target,
                &themed("darts.reset", &["The darts match has been reset."], &[])?,
            )?;
        }
        "reset" => reply(
            &env.server,
            &msg.target,
            &themed(
                "darts.reset_denied",
                &["Only an administrator may reset the darts match."],
                &[],
            )?,
        )?,
        _ => reply(
            &env.server,
            &msg.target,
            &themed(
                "darts.usage",
                &["Usage: !darts [1|2|3 | score | reset]"],
                &[],
            )?,
        )?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weighted_board_boundaries() {
        assert_eq!(
            dart_from_roll(0),
            Dart {
                label: "1".into(),
                points: 1
            }
        );
        assert_eq!(dart_from_roll(79).points, 20);
        assert_eq!(dart_from_roll(80).points, 2);
        assert_eq!(dart_from_roll(119).points, 40);
        assert_eq!(dart_from_roll(139).points, 60);
        assert_eq!(dart_from_roll(142).points, 50);
        assert_eq!(dart_from_roll(144).points, 0);
    }

    #[test]
    fn darts_are_applied_sequentially() {
        let mut remaining = 20;
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "5".into(),
                    points: 5
                }
            ),
            Outcome::Normal
        );
        assert_eq!(remaining, 15);
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "20".into(),
                    points: 20
                }
            ),
            Outcome::Bust
        );
        assert_eq!(remaining, 15);
    }

    #[test]
    fn exact_dart_wins_immediately() {
        let mut remaining = 20;
        assert_eq!(
            apply_dart(
                &mut remaining,
                &Dart {
                    label: "double 10".into(),
                    points: 20
                }
            ),
            Outcome::Win
        );
        assert_eq!(remaining, 0);
    }
}
