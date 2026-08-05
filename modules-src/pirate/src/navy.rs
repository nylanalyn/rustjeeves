//! The Royal Navy: periodic blockades of the most notorious captain (PLAN §11). Job scheduling
//! lives here; effects are lazy timestamps on the target player.

use crate::model::Game;
use crate::{reply, setting_enabled, themed, PirateSettings, Rng};

/// Pick the blockade target: highest notoriety; ties broken by nick for determinism.
/// Returns (uuid, nick). No players → None.
pub(crate) fn pick_target(game: &Game) -> Option<(String, String)> {
    game.players
        .iter()
        .max_by(|(a_uuid, a), (b_uuid, b)| {
            a.notoriety
                .cmp(&b.notoriety)
                .then_with(|| b.nick_cache.cmp(&a.nick_cache))
                .then_with(|| b_uuid.cmp(a_uuid))
        })
        .map(|(uuid, player)| (uuid.clone(), player.nick_cache.clone()))
}

/// Next navy-announcement due time: now + a random interval in [min, max] days.
pub(crate) fn next_announce(now: i64, settings: &PirateSettings, rng: &mut Rng) -> i64 {
    let min = settings.navy_interval_days_min.max(1);
    let max = settings.navy_interval_days_max.max(min);
    now + rng.between(min, max) * 86_400
}

pub(crate) fn next_navy_due(settings: &PirateSettings, now: i64, rng: &mut Rng) -> i64 {
    next_announce(now, settings, rng)
}

/// Apply the blockade: 24h of no launches and half gold income.
pub(crate) fn apply_blockade(game: &mut Game, target: &str, now: i64) -> Option<String> {
    let player = game.players.get_mut(target)?;
    player.navy_blockade_until = now + 24 * 3600;
    game.navy_pending_target = None;
    game.navy_pending_hit_at = 0;
    Some(player.nick_cache.clone())
}

pub(crate) fn handle_navy_announce(
    server: &str,
    channel: &str,
    game_key: &str,
) -> Result<(), extism_pdk::Error> {
    let now = crate::now_secs();
    // No sightings while the game is off — the 24h warning would never be read, and the blockade
    // that follows would land on a captain who was given no chance to answer it.
    if !setting_enabled(server, channel) {
        let settings = crate::pirate_settings(server, channel);
        crate::schedule(
            &crate::navy_job_id(server, channel),
            server,
            channel,
            None,
            next_announce(now, &settings, &mut crate::rng()?),
            "",
        )?;
        return Ok(());
    }
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get_mut(game_key) else {
        return Ok(());
    };
    let Some((target_uuid, target_nick)) = pick_target(game) else {
        crate::save_state(&state)?;
        return Ok(());
    };
    let hit_at = now + 24 * 3600;
    game.navy_pending_target = Some(target_uuid.clone());
    game.navy_pending_hit_at = hit_at;
    crate::save_state(&state)?;
    let payload = serde_json::to_string(&serde_json::json!({"target_uuid": target_uuid}))?;
    crate::schedule(
        &crate::navy_hit_job_id(server, channel),
        server,
        channel,
        None,
        hit_at,
        &payload,
    )?;
    reply(
        server,
        channel,
        &themed(
            "pirate.navy_sighting",
            &["The Royal Navy has sighted {target}! In 24 hours, the blockade falls."],
            &[("target", &target_nick)],
        )?,
    )?;
    Ok(())
}

pub(crate) fn handle_navy_hit(
    server: &str,
    channel: &str,
    game_key: &str,
    payload: &str,
) -> Result<(), extism_pdk::Error> {
    let now = crate::now_secs();
    let settings = crate::pirate_settings(server, channel);
    // The blockade is a 24h punishment. Landing it on a disabled game would sit out the whole
    // downtime unannounced and unanswerable, so it is dropped — but the patrol still gets its
    // next sighting scheduled, or the Navy would never sail again.
    if !setting_enabled(server, channel) {
        crate::schedule(
            &crate::navy_job_id(server, channel),
            server,
            channel,
            None,
            next_announce(now, &settings, &mut crate::rng()?),
            "",
        )?;
        return Ok(());
    }
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get_mut(game_key) else {
        return Ok(());
    };
    let target = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("target_uuid")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .or_else(|| game.navy_pending_target.clone());
    let Some(target) = target else {
        return Ok(());
    };
    let Some(nick) = apply_blockade(game, &target, now) else {
        return Ok(());
    };
    crate::save_state(&state)?;
    reply(
        server,
        channel,
        &themed(
            "pirate.navy_blockade",
            &["The Royal Navy has blockaded {target} for 24 hours."],
            &[("target", &nick)],
        )?,
    )?;
    crate::schedule(
        &crate::navy_job_id(server, channel),
        server,
        channel,
        None,
        next_announce(now, &settings, &mut crate::rng()?),
        "",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Player;

    #[test]
    fn most_notorious_captain_is_targeted() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ann".into(),
                notoriety: 5,
                ..Default::default()
            },
        );
        game.players.insert(
            "b".into(),
            Player {
                nick_cache: "Bob".into(),
                notoriety: 9,
                ..Default::default()
            },
        );
        assert_eq!(pick_target(&game), Some(("b".into(), "Bob".into())));
    }

    #[test]
    fn blockade_lasts_24h() {
        let mut game = Game::default();
        game.players.insert("a".into(), Player::default());
        assert_eq!(apply_blockade(&mut game, "a", 1000), Some(String::new()));
        assert_eq!(game.players["a"].navy_blockade_until, 1000 + 24 * 3600);
        assert!(apply_blockade(&mut game, "ghost", 1000).is_none());
    }
}
