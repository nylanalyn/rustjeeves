//! Player blockades and their escrow.
use crate::commands::reply_error;
use crate::model::{Game, PlayerBlockade, Stragglers, MAX_STRAGGLER_GROUPS};
use crate::{
    now_secs, player_blockade_job_id, reply, save_state, schedule, themed, PirateSettings, Rng,
};
use extism_pdk::Error;

pub(crate) fn active(player: &crate::model::Player, now: i64) -> bool {
    player
        .player_blockade
        .as_ref()
        .is_some_and(|b| now < b.until)
}

pub(crate) fn settle_expired(game: &mut Game, now: i64) -> bool {
    let expired: Vec<String> = game
        .players
        .iter()
        .filter(|(_, player)| {
            player
                .player_blockade
                .as_ref()
                .is_some_and(|b| b.until <= now)
        })
        .map(|(id, _)| id.clone())
        .collect();
    let changed = !expired.is_empty();
    for target in expired {
        settle(game, &target, false);
    }
    changed
}

/// End a blockade exactly once. Breaking returns escrow; expiry pays it to the blockader.
pub(crate) fn settle(game: &mut Game, target: &str, broken: bool) -> Option<(String, i64, i64)> {
    let blockade = game.players.get_mut(target)?.player_blockade.take()?;
    let beneficiary = if broken {
        target
    } else {
        blockade.blockader_uuid.as_str()
    };
    if let Some(player) = game.players.get_mut(beneficiary) {
        player.gold = player.gold.saturating_add(blockade.escrow_gold);
        player.rum = player.rum.saturating_add(blockade.escrow_rum);
    }
    if let Some(blockader) = game.players.get_mut(&blockade.blockader_uuid) {
        blockader.crew_regular = blockader.crew_regular.saturating_add(blockade.crew_regular);
        blockader.crew_loyal = blockader.crew_loyal.saturating_add(blockade.crew_loyal);
    }
    Some((
        blockade.blockader_nick,
        blockade.escrow_gold,
        blockade.escrow_rum,
    ))
}

pub(crate) fn handle_expiry(server: &str, game_key: &str, target: &str) -> Result<(), Error> {
    let mut state = crate::load_state()?;
    let now = now_secs();
    let Some(game) = state.games.get(game_key) else {
        return Ok(());
    };
    let announce_expiry = crate::game_open(server, game);
    if announce_expiry {
        crate::voyage::resolve_overdue(
            &mut state,
            server,
            game_key,
            &crate::pirate_settings(server),
            now,
        )?;
    }
    let Some(game) = state.games.get_mut(game_key) else {
        return Ok(());
    };
    let due = game
        .players
        .get(target)
        .and_then(|p| p.player_blockade.as_ref())
        .is_some_and(|b| b.until <= now);
    if !due {
        return Ok(());
    }
    let target_nick = game
        .players
        .get(target)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_default();
    let Some((blockader, gold, rum)) = settle(game, target, false) else {
        return Ok(());
    };
    save_state(&state)?;
    if !announce_expiry {
        return Ok(());
    }
    let game = state.games.get(game_key).expect("game retained");
    crate::announce(server, game, "pirate.player_blockade_expired",
        &["The blockade of {target} ended. {blockader} seized {gold} gold and {rum} rum from escrow."],
        &[("target", &target_nick), ("blockader", &blockader), ("gold", &gold.to_string()), ("rum", &rum.to_string())])
}

pub(crate) fn create(
    game: &mut Game,
    blockader: &str,
    target: &str,
    regular: i64,
    loyal: i64,
    now: i64,
) {
    let blockader_nick = game
        .players
        .get(blockader)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_default();
    if let Some(player) = game.players.get_mut(target) {
        player.player_blockade = Some(PlayerBlockade {
            blockader_uuid: blockader.into(),
            blockader_nick,
            started_at: now,
            until: now + 24 * 3600,
            strength: regular + loyal,
            crew_regular: regular,
            crew_loyal: loyal,
            ..Default::default()
        });
        player.navy_blockade_until = 0;
        player.navy_blockade_strength = 0;
    }
}

/// What happened to a blockader's crew when their blockade was broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Scattered {
    pub(crate) blockader_uuid: String,
    pub(crate) blockader_nick: String,
    /// Regular crew lost for good.
    pub(crate) lost: i64,
    /// Regular crew straggling home, due at `returns_at`.
    pub(crate) stragglers: i64,
    pub(crate) returns_at: i64,
    /// Loyal crew always make it straight home.
    pub(crate) loyal_home: i64,
}

/// The result of a sortie against a player blockade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BreakOutcome {
    pub(crate) broken: bool,
    /// The defender's own regular crew lost in a failed attempt.
    pub(crate) crew_lost: i64,
    /// Present when the blockade broke: the fate of the blockader's crew.
    pub(crate) scattered: Option<Scattered>,
}

/// A broken blockade routs its crew. Each regular crew member is lost for good at
/// `blockade_broken_loss_pct`; the rest straggle home after `blockade_straggler_hours`. Loyal crew
/// are never lost and come straight home. Call after [`settle`] has credited the crew back, so this
/// only has to take the routed regulars away again.
fn scatter(
    game: &mut Game,
    blockader: &str,
    regular: i64,
    loyal: i64,
    now: i64,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> Option<Scattered> {
    let player = game.players.get_mut(blockader)?;
    let regular = regular.clamp(0, player.crew_regular.max(0));
    let chance = settings.blockade_broken_loss_pct.clamp(0, 100) as f64 / 100.0;
    let lost = (0..regular).filter(|_| rng.chance(chance)).count() as i64;
    let stragglers = regular - lost;
    let returns_at = now + settings.blockade_straggler_hours.max(1) * 3_600;
    player.crew_regular -= regular;
    player.career_crew_lost += lost;
    if stragglers > 0 {
        if player.stragglers.len() >= MAX_STRAGGLER_GROUPS {
            // Bounded state: fold into the latest group rather than growing the list.
            if let Some(last) = player.stragglers.last_mut() {
                last.count += stragglers;
                last.returns_at = last.returns_at.max(returns_at);
            }
        } else {
            player.stragglers.push(Stragglers {
                count: stragglers,
                returns_at,
            });
        }
    }
    Some(Scattered {
        blockader_uuid: blockader.to_string(),
        blockader_nick: player.nick_cache.clone(),
        lost,
        stragglers,
        returns_at,
        loyal_home: loyal.max(0),
    })
}

/// Welcome home every straggler group that has arrived, for every captain at sea. Parked
/// captains' stragglers wait: their timers shift on `!unpark`.
pub(crate) fn return_stragglers(game: &mut Game, now: i64) -> bool {
    let mut changed = false;
    for player in game.players.values_mut() {
        if !player.parked && player.return_stragglers(now) > 0 {
            changed = true;
        }
    }
    changed
}

pub(crate) fn break_attempt(
    game: &mut Game,
    target: &str,
    crew: i64,
    now: i64,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> Option<BreakOutcome> {
    let blockade = game.players.get(target)?.player_blockade.as_ref()?;
    if now >= blockade.until {
        return None;
    }
    let strength = blockade.strength;
    let blockader = blockade.blockader_uuid.clone();
    let (blockade_regular, blockade_loyal) = (blockade.crew_regular, blockade.crew_loyal);
    let regular_sent = crew.min(game.players.get(target)?.home_regular());
    if crew > strength {
        settle(game, target, true);
        let scattered = scatter(
            game,
            &blockader,
            blockade_regular,
            blockade_loyal,
            now,
            settings,
            rng,
        );
        Some(BreakOutcome {
            broken: true,
            crew_lost: 0,
            scattered,
        })
    } else {
        let lost = (regular_sent.saturating_add(9) / 10).min(regular_sent);
        if let Some(player) = game.players.get_mut(target) {
            player.crew_regular -= lost;
        }
        Some(BreakOutcome {
            broken: false,
            crew_lost: lost,
            scattered: None,
        })
    }
}

pub(crate) fn announce_start(server: &str, target: &str, blockader: &str) -> Result<(), Error> {
    reply(server, target, &themed("pirate.player_blockade_started",
        &["{blockader} has blockaded your isle for 24 hours. Voyages can still sail, but each returning reward has a 50% chance to lose 40–60% to escrow. Break it with !sail <crew>."],
        &[("blockader", blockader)])?)
}

pub(crate) fn initiate(
    server: &str,
    channel: &str,
    msg: &jeeves_abi::MessagePayload,
    args: &[&str],
    state: &mut crate::model::State,
    settings: &PirateSettings,
    now: i64,
) -> Result<(), Error> {
    let reply_to = &msg.nick;
    if args.len() != 2 {
        return reply_error(server, reply_to, "usage is !blockade <captain> <crew>");
    }
    let Some(crew) = args[1].parse::<i64>().ok().filter(|n| *n > 0) else {
        return reply_error(server, reply_to, "send a positive crew count");
    };
    let uuid = msg.user_id.trim();
    let key = server.to_string();
    let game = state
        .games
        .get_mut(&key)
        .ok_or_else(|| Error::msg("game missing"))?;
    let Some(target) = crate::resolve_uuid(game, server, args[0])? else {
        return reply_error(server, reply_to, "that captain is not on these seas");
    };
    if target == uuid {
        return reply_error(server, reply_to, "you cannot blockade your own isle");
    }
    if game.players.get(uuid).is_none_or(|p| p.parked)
        || game.players.get(&target).is_none_or(|p| p.parked)
    {
        return reply_error(server, reply_to, "both captains must be at sea");
    }
    if game
        .players
        .get(&target)
        .is_some_and(|p| p.shielded(now) || p.blockaded(now) || active(p, now))
    {
        return reply_error(
            server,
            reply_to,
            "that isle is shielded or already blockaded",
        );
    }
    if game
        .players
        .get(&target)
        .is_some_and(|p| p.navy_blockade_until > now)
        || game.navy_pending_target.as_deref() == Some(&target)
    {
        return reply_error(
            server,
            reply_to,
            "a Royal Navy blockade is already due or active there",
        );
    }
    if game.players.values().any(|p| {
        p.player_blockade
            .as_ref()
            .is_some_and(|b| active(p, now) && b.blockader_uuid == uuid)
    }) {
        return reply_error(
            server,
            reply_to,
            "you already have a player blockade committed",
        );
    }
    let player = game.players.get_mut(uuid).expect("checked");
    player.last_activity_at = now;
    let available = player.home_crew(now);
    if crew > available {
        return reply_error(
            server,
            reply_to,
            &format!("you only have {available} crew home"),
        );
    }
    let regular = crew.min(player.home_regular());
    let loyal = crew - regular;
    player.crew_regular -= regular;
    player.crew_loyal -= loyal;
    // A blockade is an act of aggression: it is noticed, and it ends any new-captain shield.
    player.notoriety += settings.notoriety_player_blockade;
    let dropped_shield = crate::voyage::drop_shield(player, now);
    create(game, uuid, &target, regular, loyal, now);
    let due = now + 24 * 3600;
    let target_nick = game
        .players
        .get(&target)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_else(|| args[0].to_string());
    schedule(
        &player_blockade_job_id(server, &target),
        server,
        channel,
        Some(target.clone()),
        due,
        "",
    )?;
    let snapshot = game.clone();
    save_state(state)?;
    crate::announce(
        server,
        &snapshot,
        "pirate.player_blockade_public",
        &["🏴‍☠️ {blockader} has blockaded {target}'s isle for 24 hours."],
        &[("blockader", &msg.display), ("target", &target_nick)],
    )?;
    announce_start(server, &target_nick, &msg.display)?;
    reply(
        server,
        reply_to,
        &themed(
            "pirate.player_blockade_departure",
            &["Your crew have blockaded {target} for 24 hours; their strength is hidden. Hold it and you seize the escrow — but if {target} breaks it, your crew scatter: some are lost, the rest straggle home over days.{shield}"],
            &[
                ("target", &target_nick),
                (
                    "shield",
                    if dropped_shield {
                        " Your new-captain shield is gone."
                    } else {
                        ""
                    },
                ),
            ],
        )?,
    )
}

/// Tell the routed blockader what became of their crew.
fn notify_scattered(server: &str, breaker: &str, scattered: &Scattered) -> Result<(), Error> {
    if scattered.blockader_nick.is_empty() {
        return Ok(());
    }
    let hours = ((scattered.returns_at - now_secs()).max(0) + 3_599) / 3_600;
    reply(
        server,
        &scattered.blockader_nick,
        &themed(
            "pirate.player_blockade_routed",
            &["💥 {breaker} broke your blockade and your crew scattered: {lost} lost for good, {stragglers} straggling home (due in about {hours}h). Your {loyal} loyal crew made it straight back."],
            &[
                ("breaker", breaker),
                ("lost", &scattered.lost.to_string()),
                ("stragglers", &scattered.stragglers.to_string()),
                ("hours", &hours.to_string()),
                ("loyal", &scattered.loyal_home.to_string()),
            ],
        )?,
    )
}

pub(crate) fn handle_pm_command(
    server: &str,
    msg: &jeeves_abi::MessagePayload,
    state: &mut crate::model::State,
) -> Result<bool, Error> {
    let mut parts = msg.text.split_whitespace();
    let Some(command) = parts.next() else {
        return Ok(false);
    };
    let args: Vec<&str> = parts.collect();
    if !command.eq_ignore_ascii_case("!blockade") && !command.eq_ignore_ascii_case("!sail") {
        return Ok(false);
    }
    let now = now_secs();
    let Some(game) = state.games.get(server) else {
        reply_error(server, &msg.nick, "claim an isle with !signon first")?;
        return Ok(true);
    };
    if !crate::game_open(server, game) {
        reply_error(server, &msg.nick, "the Pirate Isles are closed for now")?;
        return Ok(true);
    }
    let room = game
        .rooms
        .first()
        .map(|r| r.name.clone())
        .unwrap_or_else(|| "#pirate".into());
    crate::voyage::resolve_overdue(state, server, server, &crate::pirate_settings(server), now)?;
    if state
        .games
        .get_mut(server)
        .is_some_and(|game| settle_expired(game, now) | return_stragglers(game, now))
    {
        save_state(state)?;
    }
    if msg.user_id.trim().is_empty()
        || state
            .games
            .get(server)
            .is_none_or(|g| !g.players.contains_key(msg.user_id.trim()))
    {
        reply_error(
            server,
            &msg.nick,
            "you need an isle before you can issue blockade orders",
        )?;
        return Ok(true);
    }
    if state.games[server].players[&msg.user_id].parked {
        reply_error(
            server,
            &msg.nick,
            "your ship is parked; unpark it in a channel first",
        )?;
        return Ok(true);
    }
    let settings = crate::pirate_settings(server);
    if command.eq_ignore_ascii_case("!blockade") {
        initiate(server, &room, msg, &args, state, &settings, now)?;
        return Ok(true);
    }
    if args.len() != 1 {
        reply_error(
            server,
            &msg.nick,
            "use !sail <crew> in PM to break a player blockade",
        )?;
        return Ok(true);
    }
    let Some(crew) = args[0].parse::<i64>().ok().filter(|n| *n > 0) else {
        reply_error(server, &msg.nick, "send a positive crew count")?;
        return Ok(true);
    };
    let result = {
        let game = state.games.get_mut(server).expect("game checked");
        let uuid = msg.user_id.trim();
        game.players
            .get_mut(uuid)
            .expect("captain checked")
            .last_activity_at = now;
        let available = game.players[uuid].home_crew(now);
        if !active(&game.players[uuid], now) {
            reply_error(
                server,
                &msg.nick,
                "no player blockade is active on your isle",
            )?;
            return Ok(true);
        }
        if crew > available {
            reply_error(
                server,
                &msg.nick,
                &format!("you only have {available} crew home"),
            )?;
            return Ok(true);
        }
        break_attempt(game, uuid, crew, now, &settings, &mut crate::rng()?)
            .ok_or_else(|| Error::msg("player blockade expired"))?
    };
    if result.broken {
        crate::cancel_schedule(&player_blockade_job_id(server, msg.user_id.trim()))?;
    }
    save_state(state)?;
    if result.broken {
        let snapshot = state.games[server].clone();
        crate::announce(server, &snapshot, "pirate.player_blockade_broken_public", &["⚔️ {user} broke the player blockade. The intercepted stores are back in their hold."], &[("user", &msg.display)])?;
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.player_blockade_broken",
                &["⚔️ Your blockade is broken. All intercepted gold and rum have been returned."],
                &[],
            )?,
        )?;
        if let Some(scattered) = &result.scattered {
            crate::log_failure(
                "blockade rout notice",
                notify_scattered(server, &msg.display, scattered),
            );
        }
    } else {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.player_blockade_held",
                &["🚢 The blockade held; {lost} regular crew were lost."],
                &[("lost", &result.crew_lost.to_string())],
            )?,
        )?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Player, PlayerBlockade};

    #[test]
    fn break_returns_escrow_and_expiry_pays_blockader_once() {
        let mut game = Game::default();
        game.players.insert(
            "target".into(),
            Player {
                gold: 10,
                rum: 2,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "attacker".into(),
                    escrow_gold: 30,
                    escrow_rum: 4,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        game.players.insert(
            "attacker".into(),
            Player {
                gold: 5,
                ..Default::default()
            },
        );
        settle(&mut game, "target", true);
        assert_eq!(
            (game.players["target"].gold, game.players["target"].rum),
            (40, 6)
        );
        assert_eq!(game.players["attacker"].gold, 5);

        game.players.get_mut("target").unwrap().player_blockade = Some(PlayerBlockade {
            blockader_uuid: "attacker".into(),
            escrow_gold: 7,
            ..Default::default()
        });
        settle(&mut game, "target", false);
        assert_eq!(game.players["attacker"].gold, 12);
        assert!(game.players["target"].player_blockade.is_none());
    }

    #[test]
    fn failed_break_rounds_regular_crew_loss_up() {
        let mut game = Game::default();
        game.players.insert(
            "target".into(),
            Player {
                crew_regular: 11,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "attacker".into(),
                    until: 100,
                    strength: 100,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        game.players.insert("attacker".into(), Player::default());
        let outcome = break_attempt(
            &mut game,
            "target",
            11,
            10,
            &PirateSettings::defaults(),
            &mut Rng::new(1),
        )
        .unwrap();
        assert!(!outcome.broken);
        assert_eq!(outcome.crew_lost, 2);
        assert_eq!(game.players["target"].crew_regular, 9);
    }

    #[test]
    fn target_parking_does_not_pause_escrow_expiry() {
        let mut game = Game::default();
        game.players.insert(
            "target".into(),
            Player {
                parked: true,
                gold: 3,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "attacker".into(),
                    until: 100,
                    escrow_gold: 27,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        game.players.insert("attacker".into(), Player::default());
        assert!(settle_expired(&mut game, 100));
        assert_eq!(game.players["attacker"].gold, 27);
        assert!(game.players["target"].player_blockade.is_none());
    }

    fn broken_blockade(regular: i64, loyal: i64) -> Game {
        let mut game = Game::default();
        game.players.insert(
            "target".into(),
            Player {
                crew_regular: 20,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "blockader".into(),
                    until: 100,
                    strength: regular + loyal,
                    crew_regular: regular,
                    crew_loyal: loyal,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        game.players.insert(
            "blockader".into(),
            Player {
                nick_cache: "Bea".into(),
                ..Default::default()
            },
        );
        game
    }

    #[test]
    fn a_broken_blockade_loses_some_crew_and_strands_the_rest_for_days() {
        let settings = PirateSettings::defaults();
        let mut game = broken_blockade(10, 2);
        let outcome =
            break_attempt(&mut game, "target", 13, 10, &settings, &mut Rng::new(3)).unwrap();
        assert!(outcome.broken);
        let scattered = outcome.scattered.unwrap();
        assert_eq!(scattered.lost + scattered.stragglers, 10);
        assert_eq!(scattered.returns_at, 10 + 48 * 3_600);
        let blockader = &game.players["blockader"];
        assert_eq!(blockader.crew_regular, 0, "no regular crew are home yet");
        assert_eq!(blockader.crew_loyal, 2, "loyal crew come straight home");
        assert_eq!(blockader.career_crew_lost, scattered.lost);
        assert_eq!(blockader.stragglers_out(), scattered.stragglers);

        // Nothing arrives early; everyone left arrives on time.
        assert!(!return_stragglers(&mut game, scattered.returns_at - 1));
        assert_eq!(
            return_stragglers(&mut game, scattered.returns_at),
            scattered.stragglers > 0
        );
        assert_eq!(game.players["blockader"].crew_regular, scattered.stragglers);
    }

    #[test]
    fn the_rout_loss_rate_follows_the_setting() {
        let mut rng = Rng::new(11);
        for (pct, expect_all_lost) in [(0, false), (100, true)] {
            let settings = PirateSettings {
                blockade_broken_loss_pct: pct,
                ..PirateSettings::defaults()
            };
            let mut game = broken_blockade(6, 0);
            let scattered = break_attempt(&mut game, "target", 7, 10, &settings, &mut rng)
                .unwrap()
                .scattered
                .unwrap();
            assert_eq!(scattered.lost, if expect_all_lost { 6 } else { 0 });
        }
    }

    #[test]
    fn an_expired_blockade_still_brings_every_crew_home() {
        let mut game = broken_blockade(5, 1);
        assert!(settle_expired(&mut game, 100));
        let blockader = &game.players["blockader"];
        assert_eq!((blockader.crew_regular, blockader.crew_loyal), (5, 1));
        assert!(blockader.stragglers.is_empty());
    }
}
