//! Player blockades and their escrow.
use crate::model::{Game, PlayerBlockade};
use crate::{now_secs, player_blockade_job_id, reply, save_state, schedule, themed};
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

pub(crate) fn break_attempt(
    game: &mut Game,
    target: &str,
    crew: i64,
    now: i64,
) -> Option<(bool, i64)> {
    let blockade = game.players.get(target)?.player_blockade.as_ref()?;
    if now >= blockade.until {
        return None;
    }
    let strength = blockade.strength;
    let regular_sent = crew.min(game.players.get(target)?.home_regular());
    if crew > strength {
        settle(game, target, true);
        Some((true, 0))
    } else {
        let lost = (regular_sent.saturating_add(9) / 10).min(regular_sent);
        if let Some(player) = game.players.get_mut(target) {
            player.crew_regular -= lost;
        }
        Some((false, lost))
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
    now: i64,
) -> Result<(), Error> {
    let reply_to = &msg.nick;
    if args.len() != 2 {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: usage is !blockade <captain> <crew>."],
                &[],
            )?,
        );
    }
    let Some(crew) = args[1].parse::<i64>().ok().filter(|n| *n > 0) else {
        return reply(
            server,
            reply_to,
            &themed("pirate.error", &["Arrr: send a positive crew count."], &[])?,
        );
    };
    let uuid = msg.user_id.trim();
    let key = server.to_string();
    let game = state
        .games
        .get_mut(&key)
        .ok_or_else(|| Error::msg("game missing"))?;
    let Some(target) = crate::resolve_uuid(game, server, args[0])? else {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: that captain is not on these seas."],
                &[],
            )?,
        );
    };
    if target == uuid {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: you cannot blockade your own isle."],
                &[],
            )?,
        );
    }
    if game.players.get(uuid).is_none_or(|p| p.parked)
        || game.players.get(&target).is_none_or(|p| p.parked)
    {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: both captains must be at sea."],
                &[],
            )?,
        );
    }
    if game
        .players
        .get(&target)
        .is_some_and(|p| p.shielded(now) || p.blockaded(now) || active(p, now))
    {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: that isle is shielded or already blockaded."],
                &[],
            )?,
        );
    }
    if game
        .players
        .get(&target)
        .is_some_and(|p| p.navy_blockade_until > now)
        || game.navy_pending_target.as_deref() == Some(&target)
    {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: a Royal Navy blockade is already due or active there."],
                &[],
            )?,
        );
    }
    if game.players.values().any(|p| {
        p.player_blockade
            .as_ref()
            .is_some_and(|b| active(p, now) && b.blockader_uuid == uuid)
    }) {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: one of those captains already has a player blockade committed."],
                &[],
            )?,
        );
    }
    let player = game.players.get_mut(uuid).expect("checked");
    let available = player.home_crew(now);
    if crew > available {
        return reply(
            server,
            reply_to,
            &themed(
                "pirate.error",
                &["Arrr: you only have {crew} crew home."],
                &[("crew", &available.to_string())],
            )?,
        );
    }
    let regular = crew.min(player.home_regular());
    let loyal = crew - regular;
    player.crew_regular -= regular;
    player.crew_loyal -= loyal;
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
            &["Your crew have blockaded {target} for 24 hours. Their strength is hidden."],
            &[("target", &target_nick)],
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
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.error",
                &["Arrr: claim an isle with !signon first."],
                &[],
            )?,
        )?;
        return Ok(true);
    };
    if !crate::game_open(server, game) {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.error",
                &["The Pirate Isles are closed for now."],
                &[],
            )?,
        )?;
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
        .is_some_and(|game| settle_expired(game, now))
    {
        save_state(state)?;
    }
    if msg.user_id.trim().is_empty()
        || state
            .games
            .get(server)
            .is_none_or(|g| !g.players.contains_key(msg.user_id.trim()))
    {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.error",
                &["Arrr: you need an isle before you can issue blockade orders."],
                &[],
            )?,
        )?;
        return Ok(true);
    }
    if state.games[server].players[&msg.user_id].parked {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.error",
                &["Your ship is parked; unpark it in a channel first."],
                &[],
            )?,
        )?;
        return Ok(true);
    }
    if command.eq_ignore_ascii_case("!blockade") {
        initiate(server, &room, msg, &args, state, now)?;
        return Ok(true);
    }
    if args.len() != 1 {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.error",
                &["Arrr: use !sail <crew> in PM to break a player blockade."],
                &[],
            )?,
        )?;
        return Ok(true);
    }
    let Some(crew) = args[0].parse::<i64>().ok().filter(|n| *n > 0) else {
        reply(
            server,
            &msg.nick,
            &themed("pirate.error", &["Arrr: send a positive crew count."], &[])?,
        )?;
        return Ok(true);
    };
    let result = {
        let game = state.games.get_mut(server).expect("game checked");
        let uuid = msg.user_id.trim();
        let available = game.players[uuid].home_crew(now);
        if !active(&game.players[uuid], now) {
            reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.error",
                    &["No player blockade is active on your isle."],
                    &[],
                )?,
            )?;
            return Ok(true);
        }
        if crew > available {
            reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.error",
                    &["You only have {count} crew home."],
                    &[("count", &available.to_string())],
                )?,
            )?;
            return Ok(true);
        }
        break_attempt(game, uuid, crew, now).ok_or_else(|| Error::msg("player blockade expired"))?
    };
    if result.0 {
        crate::cancel_schedule(&player_blockade_job_id(server, msg.user_id.trim()))?;
    }
    save_state(state)?;
    if result.0 {
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
    } else {
        reply(
            server,
            &msg.nick,
            &themed(
                "pirate.player_blockade_held",
                &["🚢 The blockade held; {lost} regular crew were lost."],
                &[("lost", &result.1.to_string())],
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
        assert_eq!(break_attempt(&mut game, "target", 11, 10), Some((false, 2)));
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
}
