//! Seasons: sea rotation, end-of-season awards, Legends, and resource reset.

use crate::model::{Game, Player, VoyageResult};
use crate::{announce, game_open, PirateSettings, Rng};

pub(crate) const BLACK_SEA: &str = "black_sea";
pub(crate) const FROZEN_NORTH: &str = "frozen_north";

/// (key, display name). Season rotation walks this table in order.
pub(crate) const SEAS: &[(&str, &str)] = &[
    ("tortuga", "Tortuga Isles"),
    (BLACK_SEA, "the Black Sea"),
    ("crimson", "the Crimson Archipelago"),
    ("sargasso", "the Sargasso Depths"),
    (FROZEN_NORTH, "the Frozen North"),
    ("shattered_reef", "the Shattered Reef"),
];

pub(crate) fn sea_display(key: &str) -> &'static str {
    SEAS.iter()
        .find(|(k, _)| *k == key)
        .map(|(_, name)| *name)
        .unwrap_or("Tortuga Isles")
}

pub(crate) fn next_sea(key: &str) -> &'static str {
    let index = SEAS
        .iter()
        .position(|(k, _)| *k == key)
        .map(|i| i + 1)
        .unwrap_or(1);
    SEAS[index % SEAS.len()].0
}

pub(crate) fn legend_for(sea: &str) -> String {
    format!("{} Holds", sea_display(sea).trim_start_matches("the "))
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct SeasonAwards {
    pub(crate) gold_king: Option<(String, i64)>,
    pub(crate) raid_lord: Option<(String, i64)>,
    pub(crate) fortress: Option<(String, i64, i64)>,
    pub(crate) notorious: Option<(String, i64)>,
}

/// Whether a captain actually sailed this season: they did something after it began and were not
/// retired for inactivity. Only they earn the season's Legend, awards, and a season played.
pub(crate) fn sailed_this_season(player: &Player, season_started: i64) -> bool {
    !player.auto_retired && player.last_activity_at >= season_started
}

pub(crate) fn compute_awards(game: &Game) -> SeasonAwards {
    let by = |f: &dyn Fn(&Player) -> i64| -> Option<(&Player, i64)> {
        game.players
            .values()
            .filter(|player| sailed_this_season(player, game.season_started))
            .filter_map(|player| {
                let score = f(player);
                (score > 0).then_some((player, score))
            })
            .max_by(|(a, av), (b, bv)| av.cmp(bv).then_with(|| b.nick_cache.cmp(&a.nick_cache)))
    };
    SeasonAwards {
        gold_king: by(&|p| p.gold).map(|(p, score)| (p.nick_cache.clone(), score)),
        raid_lord: by(&|p| p.season_raids_won).map(|(p, score)| (p.nick_cache.clone(), score)),
        fortress: by(&|p| p.season_defenses_won)
            .map(|(p, score)| (p.nick_cache.clone(), score, p.season_breaches)),
        notorious: by(&|p| p.notoriety).map(|(p, score)| (p.nick_cache.clone(), score)),
    }
}

/// Resolve NPC voyages at the boundary, call PvP voyages home, and automatically collect every
/// resolved reward. The state is discarded immediately after season reset, so this is the only
/// point where the boundary needs to preserve their rewards.
/// Voyages the season boundary claimed on a captain's behalf: `(uuid, voyages, rum)`, so the
/// caller can award the same stats a manual `!collect` would have.
pub(crate) type AutoCollected = Vec<(String, u64, u64)>;

pub(crate) fn settle_voyages(
    game: &mut Game,
    _settings: &PirateSettings,
    rng: &mut Rng,
    now: i64,
) -> AutoCollected {
    // Blockades do not cross a season boundary: held rewards return to their owners and crews
    // go home before captains receive the new season's starting resources.
    let blockaded: Vec<String> = game
        .players
        .iter()
        .filter_map(|(id, p)| p.player_blockade.as_ref().map(|_| id.clone()))
        .collect();
    for id in blockaded {
        crate::blockade::settle(game, &id, true);
    }
    let ids: Vec<u64> = game
        .voyages
        .iter()
        .filter(|v| !v.resolved)
        .map(|v| v.id)
        .collect();
    for id in ids {
        let Some(voyage) = game.voyages.iter().find(|v| v.id == id).cloned() else {
            continue;
        };
        if voyage.kind.is_pvp() {
            if let Some(owner) = game.players.get_mut(&voyage.owner_uuid) {
                owner.crew_regular += voyage.crew_regular;
                owner.crew_loyal += voyage.crew_loyal;
            }
            if let Some(stored) = game.voyages.iter_mut().find(|v| v.id == id) {
                stored.resolved = true;
                stored.result = Some(VoyageResult::default());
            }
        } else {
            let _ = crate::voyage::resolve_npc(game, &voyage, voyage.kind, rng, now);
        }
    }
    let mut collected: std::collections::BTreeMap<String, (u64, u64)> = Default::default();
    for voyage in &game.voyages {
        let Some(result) = &voyage.result else {
            continue;
        };
        let Some(owner) = game.players.get_mut(&voyage.owner_uuid) else {
            continue;
        };
        owner.gold += result.gold;
        owner.rum += result.rum;
        owner.crew_regular += result.new_crew;
        owner.career_voyages += 1;
        owner.career_rum_collected += result.rum.max(0);
        let entry = collected.entry(voyage.owner_uuid.clone()).or_default();
        entry.0 += 1;
        entry.1 += result.rum.max(0) as u64;
    }
    for sortie in game
        .navy_harassments
        .iter()
        .filter(|sortie| !sortie.resolved)
    {
        if let Some(owner) = game.players.get_mut(&sortie.owner_uuid) {
            owner.crew_regular += sortie.crew_regular;
            owner.crew_loyal += sortie.crew_loyal;
        }
    }
    game.navy_harassments.clear();
    collected
        .into_iter()
        .map(|(uuid, (voyages, rum))| (uuid, voyages, rum))
        .collect()
}

pub(crate) fn end_season(
    game: &mut Game,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) -> SeasonEnd {
    let collected = settle_voyages(game, settings, rng, now);
    let awards = compute_awards(game);
    let legend = legend_for(&game.sea);
    let new_sea = next_sea(&game.sea).to_string();
    let season_started = game.season_started;
    let mut uuids: Vec<String> = game.players.keys().cloned().collect();
    uuids.sort();
    let mut participants = Vec::new();
    for uuid in uuids {
        let Some(player) = game.players.get_mut(&uuid) else {
            continue;
        };
        // The Legend and the season on the record are earned by sailing, not by being on the
        // roster: parked-all-season and retired captains keep their isles but not the honours.
        if sailed_this_season(player, season_started) {
            if !player.legends.contains(&legend) {
                if player.legends.len() >= crate::model::MAX_LEGENDS {
                    player.legends.remove(0);
                }
                player.legends.push(legend.clone());
            }
            player.seasons_played = player.seasons_played.saturating_add(1);
            participants.push((uuid.clone(), player.nick_cache.clone()));
        }
        let bonus = i64::from(player.seasons_played.min(3));
        player.gold = settings.starting_gold;
        player.rum = settings.starting_rum;
        player.crew_regular = settings.starting_regular_crew + bonus;
        player.crew_loyal = settings.loyal_crew_count;
        player.notoriety = 0;
        player.loyalty_tier = 3;
        player.paid_today = false;
        player.unpaid_days = 0;
        player.buildings = Default::default();
        player.shield_until = now + settings.new_player_shield_hours * 3600;
        player.loyal_cove_until = 0;
        player.humiliated_until = 0;
        player.navy_blockade_until = 0;
        player.navy_blockade_strength = 0;
        player.navy_assault_ready_at = 0;
        // Stragglers from last season's routs are part of the crew reset below.
        player.stragglers.clear();
        if player.parked {
            player.parked_at = now;
        } else {
            player.parked_at = 0;
        }
        // Intel describes an isle that no longer exists in this form, and nobody carries a
        // grudge — or a mercy window — across the horizon.
        player.raid_intel = None;
        player.raid_mercy_until = 0;
        player.false_flag = None;
        player.false_flag_ready_at = 0;
        player.season_raids_won = 0;
        player.season_defenses_won = 0;
        player.season_breaches = 0;
        // The active role and its first-recruitment history survive; the one seasonal switch resets.
        player.specialist_switched_this_season = false;
    }
    game.voyages.clear();
    game.prisoners.clear();
    game.ransoms.clear();
    game.navy_pending_target = None;
    game.navy_pending_hit_at = 0;
    game.navy_escalation = 0;
    game.sea = new_sea.clone();
    game.season_index = game.season_index.saturating_add(1);
    game.season_started = now;
    SeasonEnd {
        awards,
        legend,
        new_sea,
        collected,
        participants,
    }
}

/// Everything a season turnover produced, for the caller to award and announce.
pub(crate) struct SeasonEnd {
    pub(crate) awards: SeasonAwards,
    pub(crate) legend: String,
    pub(crate) new_sea: String,
    pub(crate) collected: AutoCollected,
    /// `(uuid, nick)` of every captain who sailed the season that just ended.
    pub(crate) participants: Vec<(String, String)>,
}

pub(crate) fn season_ends_at(game: &Game, settings: &PirateSettings) -> i64 {
    game.season_started + settings.season_length_days.max(1) * 86_400
}

/// Whether the season has actually run its course. A minute of slack absorbs scheduler jitter; a
/// replayed delivery lands in the new season and is refused.
pub(crate) fn season_due(game: &Game, settings: &PirateSettings, now: i64) -> bool {
    now >= season_ends_at(game, settings) - 60
}

pub(crate) fn days_remaining(game: &Game, settings: &PirateSettings, now: i64) -> i64 {
    (season_ends_at(game, settings) - now).max(0) / 86_400
}

pub(crate) fn handle_season_end(server: &str, game_key: &str) -> Result<(), extism_pdk::Error> {
    let settings = crate::pirate_settings(server);
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
    // A disabled game does not turn its season over. Doing so silently would reset everyone's
    // gold and buildings and hand out Legends nobody saw awarded. The season clock is pushed
    // forward instead, so it resumes with a full season when the game is switched back on.
    if !game_open(server, game) {
        let game = state.games.get_mut(game_key).expect("checked above");
        game.season_started = now;
        crate::save_state(&state)?;
        crate::schedule(
            &crate::season_job_id(server),
            server,
            &room,
            None,
            now + settings.season_length_days * 86_400,
            "",
        )?;
        return Ok(());
    }
    // At-least-once delivery: a retried (or stale) job lands in a season that is not due yet —
    // for a retry, the one this very handler just started. Re-arm for the real end instead of
    // turning the season over a second time.
    if !season_due(game, &settings, now) {
        let ends_at = season_ends_at(game, &settings);
        crate::schedule(
            &crate::season_job_id(server),
            server,
            &room,
            None,
            ends_at,
            "",
        )?;
        return Ok(());
    }
    let game = state.games.get_mut(game_key).expect("checked above");
    let SeasonEnd {
        awards,
        legend,
        new_sea,
        collected,
        participants,
    } = end_season(game, &settings, now, &mut crate::rng()?);
    crate::pm::reset_server_menus(&mut state, server, now);
    // Re-arm before committing, so nothing that fails below can make the host replay the reset.
    crate::schedule(
        &crate::season_job_id(server),
        server,
        &room,
        None,
        now + settings.season_length_days * 86_400,
        "",
    )?;
    crate::save_state(&state)?;
    // One season under the belt for everyone who sailed it, plus whatever the boundary collected
    // for them, awarded after the commit.
    let nick_of = |uuid: &str| {
        state
            .games
            .get(game_key)
            .and_then(|game| game.players.get(uuid))
            .map(|player| player.nick_cache.clone())
            .unwrap_or_default()
    };
    let mut recipients: Vec<&str> = participants
        .iter()
        .map(|(uuid, _)| uuid.as_str())
        .chain(collected.iter().map(|(uuid, ..)| uuid.as_str()))
        .collect();
    recipients.sort_unstable();
    recipients.dedup();
    for uuid in recipients {
        let (voyages, rum) = collected
            .iter()
            .find(|(owner, ..)| owner == uuid)
            .map(|(_, voyages, rum)| (*voyages, *rum))
            .unwrap_or_default();
        let sailed = u64::from(participants.iter().any(|(id, _)| id == uuid));
        crate::log_failure(
            "season award",
            crate::award_to(
                server,
                uuid,
                &nick_of(uuid),
                &room,
                vec![
                    ("seasons_played", sailed),
                    ("voyages", voyages),
                    ("rum_collected", rum),
                ],
            ),
        );
    }
    let award_text = [
        awards
            .gold_king
            .as_ref()
            .map(|(n, v)| format!("Gold King: {n} ({v})")),
        awards
            .raid_lord
            .as_ref()
            .map(|(n, v)| format!("Raid Lord: {n} ({v})")),
        awards
            .fortress
            .as_ref()
            .map(|(n, v, _)| format!("Fortress: {n} ({v})")),
        awards
            .notorious
            .as_ref()
            .map(|(n, v)| format!("Most Notorious: {n} ({v})")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join("; ");
    let game = state.games.get(game_key).expect("checked above");
    crate::log_failure(
        "season-end announcement",
        announce(
            server,
            game,
            "pirate.season_end",
            &["The season is over. {legend} is awarded. The fleet sails for {sea}. {awards}"],
            &[
                ("legend", &legend),
                ("sea", sea_display(&new_sea)),
                ("awards", &award_text),
            ],
        ),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Player;

    #[test]
    fn seas_rotate_in_order() {
        assert_eq!(next_sea("tortuga"), BLACK_SEA);
        assert_eq!(next_sea("shattered_reef"), "tortuga");
        assert_eq!(legend_for("black_sea"), "Black Sea Holds");
    }

    #[test]
    fn awards_pick_the_right_captains() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ann".into(),
                gold: 500,
                season_raids_won: 4,
                notoriety: 3,
                ..Default::default()
            },
        );
        game.players.insert(
            "b".into(),
            Player {
                nick_cache: "Bob".into(),
                gold: 100,
                season_raids_won: 9,
                season_defenses_won: 2,
                notoriety: 12,
                ..Default::default()
            },
        );
        let awards = compute_awards(&game);
        assert_eq!(awards.gold_king, Some(("Ann".into(), 500)));
        assert_eq!(awards.raid_lord, Some(("Bob".into(), 9)));
        assert_eq!(awards.fortress, Some(("Bob".into(), 2, 0)));
        assert_eq!(awards.notorious, Some(("Bob".into(), 12)));
    }

    #[test]
    fn end_season_resets_resources_and_keeps_legends() {
        let settings = PirateSettings::default();
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Ann".into(),
                gold: 9999,
                crew_regular: 20,
                notoriety: 50,
                specialist: Some(crate::model::Specialist::RaidLeader),
                specialist_recruited: true,
                specialist_switched_this_season: true,
                ..Default::default()
            },
        );
        let SeasonEnd {
            legend, new_sea, ..
        } = end_season(&mut game, &settings, 10_000, &mut Rng::new(1));
        assert_eq!(legend, "Tortuga Isles Holds");
        assert_eq!(new_sea, BLACK_SEA);
        let player = &game.players["a"];
        assert_eq!(player.seasons_played, 1);
        assert_eq!(player.legends, vec![legend]);
        assert_eq!(player.gold, settings.starting_gold);
        assert_eq!(player.crew_regular, settings.starting_regular_crew + 1);
        assert_eq!(player.crew_loyal, settings.loyal_crew_count);
        assert!(game.voyages.is_empty());
        assert!(
            player.raid_intel.is_none(),
            "intel does not cross the horizon"
        );
        assert_eq!(player.raid_mercy_until, 0);
        assert_eq!(
            player.specialist,
            Some(crate::model::Specialist::RaidLeader),
            "specialist survives the season reset"
        );
        assert!(player.specialist_recruited);
        assert!(!player.specialist_switched_this_season);
    }

    #[test]
    fn a_replayed_season_end_finds_the_new_season_not_due() {
        let settings = PirateSettings::defaults();
        let mut game = Game {
            season_started: 1_000,
            ..Default::default()
        };
        let end = 1_000 + settings.season_length_days * 86_400;
        assert!(!season_due(&game, &settings, end - 3_600));
        assert!(season_due(&game, &settings, end));
        end_season(&mut game, &settings, end, &mut Rng::new(1));
        assert!(
            !season_due(&game, &settings, end + 30),
            "the host's 30s retry must not turn the season over again"
        );
    }

    #[test]
    fn season_end_reports_the_voyages_it_collected_for_achievements() {
        let settings = PirateSettings::defaults();
        let mut game = Game::default();
        game.players.insert("a".into(), Player::default());
        game.voyages.push(crate::model::Voyage {
            id: 1,
            owner_uuid: "a".into(),
            resolved: true,
            result: Some(VoyageResult {
                rum: 5,
                ..Default::default()
            }),
            ..Default::default()
        });
        let collected = end_season(&mut game, &settings, 10, &mut Rng::new(1)).collected;
        assert_eq!(collected, vec![("a".to_string(), 1, 5)]);
    }

    #[test]
    fn only_captains_who_sailed_the_season_earn_its_honours() {
        let settings = PirateSettings::defaults();
        let mut game = Game {
            season_started: 1_000,
            ..Default::default()
        };
        let captain = |nick: &str, gold, last_activity_at, auto_retired| Player {
            nick_cache: nick.into(),
            gold,
            last_activity_at,
            auto_retired,
            ..Default::default()
        };
        game.players
            .insert("active".into(), captain("Ann", 100, 5_000, false));
        game.players
            .insert("idle".into(), captain("Ida", 9_999, 500, false));
        game.players
            .insert("retired".into(), captain("Ret", 9_999, 5_000, true));

        let awards = compute_awards(&game);
        assert_eq!(
            awards.gold_king,
            Some(("Ann".into(), 100)),
            "a hoard nobody sailed with wins nothing"
        );

        let end = end_season(&mut game, &settings, 10_000, &mut Rng::new(1));
        assert_eq!(end.participants, vec![("active".into(), "Ann".into())]);
        assert_eq!(game.players["active"].legends.len(), 1);
        assert_eq!(game.players["active"].seasons_played, 1);
        for idle in ["idle", "retired"] {
            assert!(
                game.players[idle].legends.is_empty(),
                "{idle} earns no Legend"
            );
            assert_eq!(game.players[idle].seasons_played, 0);
        }
    }
}
