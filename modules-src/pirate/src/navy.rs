//! The Royal Navy: periodic blockades of the most notorious captain (PLAN §11). Job scheduling
//! lives here; effects are lazy timestamps on the target player.

use crate::model::Game;
use crate::model::NavyHarassment;
use crate::{announce, game_open, PirateSettings, Rng};

/// Pick the blockade target: the most notorious captain at sea. Ties — including the common case
/// where nobody has any notoriety yet — are broken at random, so no captain is singled out by the
/// spelling of their nick. Returns (uuid, nick); no eligible captain → None.
pub(crate) fn pick_target(game: &Game, now: i64, rng: &mut Rng) -> Option<(String, String)> {
    let eligible = || {
        game.players
            .iter()
            .filter(|(_, player)| !player.parked && !crate::blockade::active(player, now))
    };
    let top = eligible().map(|(_, player)| player.notoriety).max()?;
    // Sorted so the draw depends only on the rng, never on HashMap iteration order.
    let mut tied: Vec<(String, String)> = eligible()
        .filter(|(_, player)| player.notoriety == top)
        .map(|(uuid, player)| (uuid.clone(), player.nick_cache.clone()))
        .collect();
    tied.sort();
    rng.choice(&tied).cloned()
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

/// Apply a hidden-strength blockade: 24h of no launches and half gold income. Returns `None` —
/// changing nothing — when the target is gone, parked, already under a player blockade, or already
/// blockaded by the Navy (a replayed delivery or an overlapping Crimson alert never stacks).
pub(crate) fn apply_blockade(
    game: &mut Game,
    target: &str,
    now: i64,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> Option<String> {
    let escalation = game.navy_escalation.max(0);
    let player = game.players.get_mut(target)?;
    if player.parked || player.blockaded(now) || crate::blockade::active(player, now) {
        return None;
    }
    let min = settings.navy_strength_min.max(1);
    let max = settings.navy_strength_max.max(min);
    player.navy_blockade_strength = rng.between(min, max) + escalation;
    player.navy_blockade_until = now + 24 * 3600;
    Some(player.nick_cache.clone())
}

/// Give pre-counterplay state blobs a hidden strength the first time the new Navy rules see an
/// already-active blockade. Expired legacy timestamps are cleaned up without resurrecting them.
pub(crate) fn backfill_strength(
    game: &mut Game,
    now: i64,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> bool {
    let escalation = game.navy_escalation.max(0);
    let min = settings.navy_strength_min.max(1);
    let max = settings.navy_strength_max.max(min);
    let mut changed = false;
    for player in game.players.values_mut() {
        if player.parked {
            continue;
        }
        if player.blockaded(now) && player.navy_blockade_strength <= 0 {
            player.navy_blockade_strength = rng.between(min, max) + escalation;
            changed = true;
        } else if !player.blockaded(now) && player.navy_blockade_strength != 0 {
            player.navy_blockade_strength = 0;
            changed = true;
        }
    }
    changed
}

/// What the captain could make out of the fleet after a failed sortie — a hint, never the number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FleetSighting {
    /// The fleet had half again as many ships as the sortie, or more.
    Overwhelming,
    /// The fleet had more ships than the sortie.
    Outnumbered,
    /// The sortie matched or beat the fleet's numbers and still lost the roll.
    Unlucky,
}

impl FleetSighting {
    fn of(crew: i64, strength: i64) -> Self {
        if strength.saturating_mul(2) >= crew.saturating_mul(3) {
            FleetSighting::Overwhelming
        } else if strength > crew {
            FleetSighting::Outnumbered
        } else {
            FleetSighting::Unlucky
        }
    }

    pub(crate) fn describe(self) -> &'static str {
        match self {
            FleetSighting::Overwhelming => "the fleet vastly outnumbered your sortie",
            FleetSighting::Outnumbered => "the fleet had more ships than you sent",
            FleetSighting::Unlucky => "you matched their numbers, but the sea favoured the Navy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AssaultReport {
    pub target_nick: String,
    pub crew_sent: i64,
    pub blockade_strength: i64,
    pub won: bool,
    pub gold_lost: i64,
    pub rum_lost: i64,
    pub crew_lost: i64,
    /// Present on a failure: what the captain could make out of the fleet.
    pub sighting: Option<FleetSighting>,
}

/// Try to break an active blockade. The sortie and the fleet each roll power like a raid
/// (`ships × 10 × 0.8–1.2`), so more crew improves the odds without guaranteeing them — and a
/// failure costs a share of the regular crew sent, so throwing everyone in is a real gamble.
/// Half again the fleet's strength always wins; two-thirds of it or less always loses. A failure
/// also leaves the crew regrouping for `navy_assault_cooldown_hours`. Loyal crew never die.
pub(crate) fn assault(
    game: &mut Game,
    target: &str,
    crew: i64,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) -> Option<AssaultReport> {
    let player = game.players.get(target)?;
    if player.parked || !player.blockaded(now) || player.navy_blockade_strength <= 0 {
        return None;
    }
    let target_nick = player.nick_cache.clone();
    let strength = player.navy_blockade_strength;
    let regular_sent = crew.min(player.home_regular());
    let sortie_power = crew as f64 * 10.0 * rng.range(0.8, 1.2);
    let fleet_power = strength as f64 * 10.0 * rng.range(0.8, 1.2);
    let won = sortie_power > fleet_power;
    let (gold_lost, rum_lost, crew_lost) = if won {
        (0, 0, 0)
    } else {
        let pct = settings.navy_failure_loss_pct.clamp(1, 100);
        let crew_lost = if regular_sent > 0 {
            (regular_sent * pct / 100).max(1).min(regular_sent)
        } else {
            0
        };
        (player.gold * pct / 100, player.rum * pct / 100, crew_lost)
    };
    let player = game.players.get_mut(target)?;
    if won {
        player.navy_blockade_until = now;
        player.navy_blockade_strength = 0;
        player.navy_assault_ready_at = 0;
        game.navy_escalation = game
            .navy_escalation
            .saturating_add(settings.navy_escalation_strength.max(1));
    } else {
        player.gold = player.gold.saturating_sub(gold_lost);
        player.rum = player.rum.saturating_sub(rum_lost);
        player.crew_regular = player.crew_regular.saturating_sub(crew_lost);
        player.career_crew_lost = player.career_crew_lost.saturating_add(crew_lost);
        player.navy_assault_ready_at = now + settings.navy_assault_cooldown_hours.max(0) * 3_600;
    }
    Some(AssaultReport {
        target_nick,
        crew_sent: crew,
        blockade_strength: strength,
        won,
        gold_lost,
        rum_lost,
        crew_lost,
        sighting: (!won).then(|| FleetSighting::of(crew, strength)),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HarassmentReport {
    pub owner_nick: String,
    pub target_nick: String,
    pub crew_sent: i64,
    pub strength_reduced: i64,
}

/// Return a timed ally sortie and weaken the blockade, but never reduce it below one ship: the
/// besieged captain still has to make the final attempt to drive the Navy away.
pub(crate) fn resolve_harassment(
    game: &mut Game,
    sortie: &NavyHarassment,
    now: i64,
) -> Option<HarassmentReport> {
    let owner_nick = game.players.get(&sortie.owner_uuid)?.nick_cache.clone();
    // The besieged isle may be gone (data deletion); the sortie's crew still sail home.
    let target_nick = game
        .players
        .get(&sortie.target_uuid)
        .map(|target| target.nick_cache.clone())
        .unwrap_or_default();
    if let Some(owner) = game.players.get_mut(&sortie.owner_uuid) {
        owner.crew_regular += sortie.crew_regular;
        owner.crew_loyal += sortie.crew_loyal;
    }
    let crew_sent = sortie.crew_regular + sortie.crew_loyal;
    let strength_reduced = if let Some(target) = game.players.get_mut(&sortie.target_uuid) {
        if !target.parked && target.blockaded(now) && target.navy_blockade_strength > 0 {
            let before = target.navy_blockade_strength;
            target.navy_blockade_strength = (before - crew_sent).max(1);
            before - target.navy_blockade_strength
        } else {
            0
        }
    } else {
        0
    };
    Some(HarassmentReport {
        owner_nick,
        target_nick,
        crew_sent,
        strength_reduced,
    })
}

pub(crate) fn handle_navy_announce(server: &str, game_key: &str) -> Result<(), extism_pdk::Error> {
    let now = crate::now_secs();
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get(game_key) else {
        // No game, no navy: a fresh game re-arms the patrol through `ensure_jobs`.
        return Ok(());
    };
    let room = game
        .rooms
        .first()
        .map(|known| known.name.clone())
        .unwrap_or_default();
    // No sightings while the game is off — the 24h warning would never be read, and the blockade
    // that follows would land on a captain who was given no chance to answer it.
    if !game_open(server, game) {
        let settings = crate::pirate_settings(server);
        crate::schedule(
            &crate::navy_job_id(server),
            server,
            &room,
            None,
            next_announce(now, &settings, &mut crate::rng()?),
            "",
        )?;
        return Ok(());
    }
    let Some((target_uuid, target_nick)) = pick_target(game, now, &mut crate::rng()?) else {
        crate::save_state(&state)?;
        let settings = crate::pirate_settings(server);
        crate::schedule(
            &crate::navy_job_id(server),
            server,
            &room,
            None,
            next_navy_due(&settings, now, &mut crate::rng()?),
            "",
        )?;
        return Ok(());
    };
    let hit_at = now + 24 * 3600;
    if let Some(game) = state.games.get_mut(game_key) {
        game.navy_pending_target = Some(target_uuid.clone());
        game.navy_pending_hit_at = hit_at;
    }
    crate::save_state(&state)?;
    let payload = serde_json::to_string(&serde_json::json!({"target_uuid": target_uuid}))?;
    crate::schedule(
        &crate::navy_hit_job_id(server),
        server,
        &room,
        None,
        hit_at,
        &payload,
    )?;
    let game = state.games.get(game_key).expect("checked above");
    crate::log_failure(
        "navy sighting announcement",
        announce(
            server,
            game,
            "pirate.navy_sighting",
            &["The Royal Navy has sighted {target}! In 24 hours, the blockade falls."],
            &[("target", &target_nick)],
        ),
    );
    Ok(())
}

/// Land a Navy blockade. `primary` is the regular patrol's hit that follows a public sighting;
/// otherwise this is a Crimson Archipelago alert — an extra visit that leaves the patrol alone.
pub(crate) fn handle_navy_hit(
    server: &str,
    game_key: &str,
    payload: &str,
    primary: bool,
) -> Result<(), extism_pdk::Error> {
    let now = crate::now_secs();
    let settings = crate::pirate_settings(server);
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get(game_key) else {
        return Ok(());
    };
    let room = game
        .rooms
        .first()
        .map(|known| known.name.clone())
        .unwrap_or_default();
    // The patrol re-arms before anything else can fail or return early: a hit whose target has
    // vanished (parked, deleted, already under siege) must never end the Navy for good.
    if primary {
        crate::schedule(
            &crate::navy_job_id(server),
            server,
            &room,
            None,
            next_announce(now, &settings, &mut crate::rng()?),
            "",
        )?;
    }
    let open = game_open(server, game);
    let game = state.games.get_mut(game_key).expect("checked above");
    let target = serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("target_uuid")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
        })
        .or_else(|| primary.then(|| game.navy_pending_target.clone()).flatten());
    let mut changed = false;
    if primary && (game.navy_pending_target.is_some() || game.navy_pending_hit_at != 0) {
        // The sighting is spent whether or not the blockade lands.
        game.navy_pending_target = None;
        game.navy_pending_hit_at = 0;
        changed = true;
    }
    // The blockade is a 24h punishment. Landing it on a disabled game would sit out the whole
    // downtime unannounced and unanswerable, so it is dropped.
    let landed = match target {
        Some(target) if open => apply_blockade(game, &target, now, &settings, &mut crate::rng()?),
        _ => None,
    };
    if landed.is_none() && !changed {
        return Ok(());
    }
    crate::save_state(&state)?;
    if let Some(nick) = landed {
        let game = state.games.get(game_key).expect("checked above");
        crate::log_failure(
            "navy blockade announcement",
            announce(
                server,
                game,
                "pirate.navy_blockade",
                &["The Royal Navy has blockaded {target} for 24 hours."],
                &[("target", &nick)],
            ),
        );
    }
    Ok(())
}

pub(crate) fn handle_harassment(
    server: &str,
    game_key: &str,
    sortie_id: u64,
) -> Result<(), extism_pdk::Error> {
    let now = crate::now_secs();
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get(game_key) else {
        return Ok(());
    };
    let room = game
        .rooms
        .first()
        .map(|known| known.name.clone())
        .unwrap_or_default();
    let Some(index) = game
        .navy_harassments
        .iter()
        .position(|sortie| sortie.id == sortie_id && !sortie.resolved)
    else {
        return Ok(());
    };
    let sortie = game.navy_harassments[index].clone();
    if game
        .players
        .get(&sortie.owner_uuid)
        .is_some_and(|player| player.parked)
    {
        crate::schedule(
            &crate::navy_harass_job_id(server, sortie_id),
            server,
            &room,
            Some(sortie.owner_uuid),
            now + 3600,
            "",
        )?;
        return Ok(());
    }
    let game = state.games.get_mut(game_key).expect("checked above");
    let report = resolve_harassment(game, &sortie, now);
    game.navy_harassments[index].resolved = true;
    game.navy_harassments.retain(|sortie| !sortie.resolved);
    crate::save_state(&state)?;
    if let Some(report) = report {
        let reduced = report.strength_reduced.to_string();
        let game = state.games.get(game_key).expect("checked above");
        crate::log_failure("harassment return announcement", announce(
            server,
            game,
            "pirate.navy_harass_return",
            &[
                "⚓ {user}'s {crew}-crew harassment sortie returned; the blockade was weakened by {reduced}.",
            ],
            &[
                ("user", &report.owner_nick),
                ("crew", &report.crew_sent.to_string()),
                ("reduced", &reduced),
            ],
        ));
    }
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
        assert_eq!(
            pick_target(&game, 0, &mut Rng::new(1)),
            Some(("b".into(), "Bob".into()))
        );
    }

    #[test]
    fn parked_captains_are_not_navy_targets() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Away".into(),
                notoriety: 99,
                parked: true,
                ..Default::default()
            },
        );
        game.players.insert(
            "b".into(),
            Player {
                nick_cache: "Present".into(),
                notoriety: 1,
                ..Default::default()
            },
        );
        assert_eq!(
            pick_target(&game, 0, &mut Rng::new(1)),
            Some(("b".into(), "Present".into()))
        );
    }

    #[test]
    fn blockade_lasts_24h() {
        let mut game = Game::default();
        game.players.insert("a".into(), Player::default());
        let settings = PirateSettings::defaults();
        assert_eq!(
            apply_blockade(&mut game, "a", 1000, &settings, &mut Rng::new(1)),
            Some(String::new())
        );
        assert_eq!(game.players["a"].navy_blockade_until, 1000 + 24 * 3600);
        assert!((settings.navy_strength_min..=settings.navy_strength_max)
            .contains(&game.players["a"].navy_blockade_strength));
        assert!(apply_blockade(&mut game, "ghost", 1000, &settings, &mut Rng::new(1)).is_none());

        // A replayed hit (or an overlapping Crimson alert) never re-rolls or extends a blockade.
        let strength = game.players["a"].navy_blockade_strength;
        assert!(apply_blockade(&mut game, "a", 5_000, &settings, &mut Rng::new(9)).is_none());
        assert_eq!(game.players["a"].navy_blockade_until, 1000 + 24 * 3600);
        assert_eq!(game.players["a"].navy_blockade_strength, strength);
    }

    #[test]
    fn legacy_active_blockades_receive_hidden_strength_but_parked_isles_do_not() {
        let mut game = Game::default();
        game.players.insert(
            "active".into(),
            Player {
                navy_blockade_until: 10_000,
                ..Default::default()
            },
        );
        game.players.insert(
            "away".into(),
            Player {
                parked: true,
                navy_blockade_until: 10_000,
                ..Default::default()
            },
        );
        let settings = PirateSettings::defaults();
        assert!(backfill_strength(
            &mut game,
            1_000,
            &settings,
            &mut Rng::new(4)
        ));
        assert!(game.players["active"].navy_blockade_strength > 0);
        assert_eq!(game.players["away"].navy_blockade_strength, 0);
    }

    #[test]
    fn successful_assault_ends_blockade_and_escalates_next_visit() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ann".into(),
                crew_regular: 10,
                crew_loyal: 2,
                navy_blockade_until: 10_000,
                navy_blockade_strength: 6,
                ..Default::default()
            },
        );
        let settings = PirateSettings::defaults();
        // 10 ships against 6 wins on any roll (8.0 ≥ 7.2 at the extremes).
        let report = assault(&mut game, "a", 10, &settings, 1_000, &mut Rng::new(1)).unwrap();
        assert!(report.won);
        assert!(report.sighting.is_none());
        assert_eq!(game.players["a"].navy_blockade_strength, 0);
        assert_eq!(game.players["a"].navy_blockade_until, 1_000);
        assert_eq!(game.navy_escalation, settings.navy_escalation_strength);
        assert_eq!(game.players["a"].crew_regular, 10);
    }

    #[test]
    fn failed_assault_takes_stores_and_regular_crew_but_not_loyal_crew() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ann".into(),
                gold: 100,
                rum: 20,
                crew_regular: 8,
                crew_loyal: 2,
                navy_blockade_until: 10_000,
                navy_blockade_strength: 8,
                ..Default::default()
            },
        );
        let settings = PirateSettings::defaults();
        // 4 ships against 8 loses on any roll (4.8 < 6.4 at the extremes).
        let report = assault(&mut game, "a", 4, &settings, 1_000, &mut Rng::new(1)).unwrap();
        assert!(!report.won);
        assert_eq!(report.sighting, Some(FleetSighting::Overwhelming));
        assert_eq!(
            game.players["a"].navy_assault_ready_at,
            1_000 + settings.navy_assault_cooldown_hours * 3_600,
            "a beaten sortie has to regroup"
        );
        assert_eq!(game.players["a"].gold, 90);
        assert_eq!(game.players["a"].rum, 18);
        assert_eq!(game.players["a"].crew_regular, 7);
        assert_eq!(game.players["a"].crew_loyal, 2);
        assert!(game.players["a"].blockaded(1_000));
    }

    #[test]
    fn harassment_returns_crew_and_reduces_blockade_without_defeating_it() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ally".into(),
                crew_regular: 4,
                ..Default::default()
            },
        );
        game.players.insert(
            "b".into(),
            Player {
                nick_cache: "Besieged".into(),
                navy_blockade_until: 10_000,
                navy_blockade_strength: 5,
                ..Default::default()
            },
        );
        let sortie = NavyHarassment {
            id: 1,
            owner_uuid: "a".into(),
            target_uuid: "b".into(),
            crew_regular: 4,
            returns_at: 2_000,
            ..Default::default()
        };
        game.players.get_mut("a").unwrap().crew_regular -= 4;
        let report = resolve_harassment(&mut game, &sortie, 2_000).unwrap();
        assert_eq!(report.strength_reduced, 4);
        assert_eq!(game.players["b"].navy_blockade_strength, 1);
        assert_eq!(game.players["a"].crew_regular, 4);
    }

    #[test]
    fn a_sortie_against_a_vanished_isle_still_brings_its_crew_home() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ally".into(),
                ..Default::default()
            },
        );
        let sortie = NavyHarassment {
            id: 1,
            owner_uuid: "a".into(),
            target_uuid: "deleted".into(),
            crew_regular: 3,
            crew_loyal: 1,
            ..Default::default()
        };
        let report = resolve_harassment(&mut game, &sortie, 2_000).unwrap();
        assert_eq!(report.strength_reduced, 0);
        assert_eq!(
            (game.players["a"].crew_regular, game.players["a"].crew_loyal),
            (3, 1)
        );
    }

    #[test]
    fn a_tie_at_the_top_is_a_fair_draw_not_alphabetical() {
        let mut game = Game::default();
        for (uuid, nick) in [("a", "Anne"), ("b", "Bob"), ("c", "Cat")] {
            game.players.insert(
                uuid.into(),
                Player {
                    nick_cache: nick.into(),
                    ..Default::default()
                },
            );
        }
        let mut rng = Rng::new(5);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(pick_target(&game, 0, &mut rng).unwrap().0);
        }
        assert_eq!(seen.len(), 3, "every zero-notoriety captain can be sighted");
    }

    #[test]
    fn a_close_sortie_is_a_gamble_either_way() {
        let settings = PirateSettings::defaults();
        let (mut wins, mut losses) = (0, 0);
        let mut rng = Rng::new(21);
        for _ in 0..400 {
            let mut game = Game::default();
            game.players.insert(
                "a".into(),
                Player {
                    crew_regular: 8,
                    navy_blockade_until: 10_000,
                    navy_blockade_strength: 8,
                    ..Default::default()
                },
            );
            if assault(&mut game, "a", 8, &settings, 1_000, &mut rng)
                .unwrap()
                .won
            {
                wins += 1;
            } else {
                losses += 1;
            }
        }
        assert!(
            wins > 100 && losses > 100,
            "evenly matched: {wins}/{losses}"
        );
    }

    #[test]
    fn fleet_sightings_hint_without_revealing_the_number() {
        assert_eq!(FleetSighting::of(4, 6), FleetSighting::Overwhelming);
        assert_eq!(FleetSighting::of(5, 6), FleetSighting::Outnumbered);
        assert_eq!(FleetSighting::of(6, 6), FleetSighting::Unlucky);
    }
}
