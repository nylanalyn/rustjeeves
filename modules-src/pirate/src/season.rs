//! Seasons: sea rotation, end-of-season awards, Legends, and resource reset.

use crate::model::{Game, Player, VoyageResult};
use crate::{reply, setting_enabled, themed, PirateSettings, Rng};

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

pub(crate) fn compute_awards(game: &Game) -> SeasonAwards {
    let by = |f: &dyn Fn(&Player) -> i64| -> Option<(&Player, i64)> {
        game.players
            .values()
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
pub(crate) fn settle_voyages(game: &mut Game, _settings: &PirateSettings, rng: &mut Rng) {
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
            let _ = crate::voyage::resolve_npc(game, &voyage, voyage.kind, rng);
        }
    }
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
    }
}

pub(crate) fn end_season(
    game: &mut Game,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) -> (SeasonAwards, String, String) {
    settle_voyages(game, settings, rng);
    let awards = compute_awards(game);
    let legend = legend_for(&game.sea);
    let new_sea = next_sea(&game.sea).to_string();
    let mut uuids: Vec<String> = game.players.keys().cloned().collect();
    uuids.sort();
    for uuid in uuids {
        let Some(player) = game.players.get_mut(&uuid) else {
            continue;
        };
        if !player.legends.contains(&legend) {
            if player.legends.len() >= crate::model::MAX_LEGENDS {
                player.legends.remove(0);
            }
            player.legends.push(legend.clone());
        }
        player.seasons_played = player.seasons_played.saturating_add(1);
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
        // Intel describes an isle that no longer exists in this form, and nobody carries a
        // grudge — or a mercy window — across the horizon.
        player.raid_intel = None;
        player.raid_mercy_until = 0;
        player.false_flag = None;
        player.false_flag_ready_at = 0;
        player.season_raids_won = 0;
        player.season_defenses_won = 0;
        player.season_breaches = 0;
    }
    game.voyages.clear();
    game.prisoners.clear();
    game.ransoms.clear();
    game.recent_departures.clear();
    game.navy_pending_target = None;
    game.navy_pending_hit_at = 0;
    game.sea = new_sea.clone();
    game.season_index = game.season_index.saturating_add(1);
    game.season_started = now;
    (awards, legend, new_sea)
}

pub(crate) fn season_ends_at(game: &Game, settings: &PirateSettings) -> i64 {
    game.season_started + settings.season_length_days.max(1) * 86_400
}

pub(crate) fn days_remaining(game: &Game, settings: &PirateSettings, now: i64) -> i64 {
    (season_ends_at(game, settings) - now).max(0) / 86_400
}

pub(crate) fn handle_season_end(
    server: &str,
    channel: &str,
    game_key: &str,
) -> Result<(), extism_pdk::Error> {
    let settings = crate::pirate_settings(server, channel);
    let now = crate::now_secs();
    let mut state = crate::load_state()?;
    let Some(game) = state.games.get_mut(game_key) else {
        return Ok(());
    };
    // A disabled game does not turn its season over. Doing so silently would reset everyone's
    // gold and buildings and hand out Legends nobody saw awarded. The season clock is pushed
    // forward instead, so it resumes with a full season when the game is switched back on.
    if !setting_enabled(server, channel) {
        game.season_started = now;
        crate::save_state(&state)?;
        crate::schedule(
            &crate::season_job_id(server, channel),
            server,
            channel,
            None,
            now + settings.season_length_days * 86_400,
            "",
        )?;
        return Ok(());
    }
    let (awards, legend, new_sea) = end_season(game, &settings, now, &mut crate::rng()?);
    let survivors: Vec<(String, String)> = game
        .players
        .iter()
        .map(|(uuid, player)| (uuid.clone(), player.nick_cache.clone()))
        .collect();
    crate::save_state(&state)?;
    // One season under the belt for everyone who sailed it, awarded after the commit.
    for (uuid, nick) in survivors {
        crate::award_to(server, &uuid, &nick, channel, vec![("seasons_played", 1)])?;
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
    reply(
        server,
        channel,
        &themed(
            "pirate.season_end",
            &["The season is over. {legend} is awarded. The fleet sails for {sea}. {awards}"],
            &[
                ("legend", &legend),
                ("sea", sea_display(&new_sea)),
                ("awards", &award_text),
            ],
        )?,
    )?;
    crate::schedule(
        &crate::season_job_id(server, channel),
        server,
        channel,
        None,
        now + settings.season_length_days * 86_400,
        "",
    )?;
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
                ..Default::default()
            },
        );
        let (_, legend, new_sea) = end_season(&mut game, &settings, 10_000, &mut Rng::new(1));
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
    }
}
