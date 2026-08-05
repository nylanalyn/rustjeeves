//! Daily rollover: payday check, loyalty decay, desertion, building upkeep/degradation, and the
//! Sargasso Depths mutiny fleets. The core is pure; the caller renders themed announcements.

use crate::buildings;
use crate::model::Game;
use crate::{reply, setting_enabled, themed, PirateSettings, Rng};

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct UnpaidEntry {
    pub(crate) nick: String,
    pub(crate) unpaid_days: u32,
    pub(crate) deserted: u32,
    pub(crate) degraded: Vec<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RolloverReport {
    pub(crate) paid: Vec<String>,
    pub(crate) unpaid: Vec<UnpaidEntry>,
    /// Crew who deserted today and formed a mutiny fleet (Sargasso Depths).
    pub(crate) mutineers: u32,
}

/// Degrade one building level: the highest-level, most expensive building first.
/// One payday pass over the game. `paid_today` flags reset for the new day.
pub(crate) fn daily_rollover(game: &mut Game, _settings: &PirateSettings) -> RolloverReport {
    let mut report = RolloverReport::default();
    let sargasso = game.sea == "sargasso";
    let mut uuids: Vec<String> = game.players.keys().cloned().collect();
    uuids.sort();
    for uuid in uuids {
        let Some(player) = game.players.get_mut(&uuid) else {
            continue;
        };
        if player.parked {
            // Parked captains are explicitly absent: no payday penalty, desertion, or building
            // upkeep/degradation is applied while they are away. They receive no gameplay
            // actions until they unpark in the channel.
            player.paid_today = false;
            continue;
        }
        if player.paid_today {
            player.paid_today = false;
            player.loyalty_tier = 3;
            player.unpaid_days = 0;
            report.paid.push(player.nick_cache.clone());
            // Paid crew: building upkeep drains gold; a building whose upkeep cannot be
            // covered degrades one level.
            for def in buildings::BUILDINGS {
                loop {
                    let lvl = buildings::level(&player.buildings, def.key);
                    if lvl == 0 {
                        break;
                    }
                    let cost = buildings::upkeep_for(&player.buildings, def);
                    if player.gold >= cost {
                        player.gold -= cost;
                        break;
                    }
                    buildings::set_level(&mut player.buildings, def.key, lvl - 1);
                }
            }
        } else {
            player.unpaid_days += 1;
            player.loyalty_tier = (player.loyalty_tier - 1).max(0);
            let mut deserted = 0;
            // Loyalty 0: one regular crew deserts per day; a Tavern keeps the crew drinking
            // instead of deserting.
            if player.loyalty_tier == 0 && player.buildings.tavern == 0 && player.crew_regular > 0 {
                player.crew_regular -= 1;
                player.career_crew_lost += 1;
                deserted = 1;
            }
            let mut degraded = Vec::new();
            if let Some(key) = buildings::degrade_one(&mut player.buildings) {
                let level = buildings::level(&player.buildings, key);
                let name = buildings::building_def(key)
                    .map(|def| def.name)
                    .unwrap_or(key);
                degraded.push(format!("{name} L{level}"));
            }
            if deserted > 0 && sargasso {
                report.mutineers = report.mutineers.saturating_add(deserted);
            }
            report.unpaid.push(UnpaidEntry {
                nick: player.nick_cache.clone(),
                unpaid_days: player.unpaid_days,
                deserted,
                degraded,
            });
        }
    }
    report
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MutinyReport {
    pub(crate) target_nick: String,
    pub(crate) mutineers: u32,
    pub(crate) defenders_lost: u32,
    pub(crate) gold_stolen: i64,
    pub(crate) repelled: bool,
}

/// Sargasso Depths: deserting crew form a mutiny fleet and hit a random island. A simplified
/// raid: mutineers vs visible home defense; on a win they grab 5% of vulnerable gold and a
/// defender; the loot sails off the edge of the map (nobody gains it).
pub(crate) fn resolve_mutiny(
    game: &mut Game,
    mutineers: u32,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) -> Option<MutinyReport> {
    if mutineers == 0 {
        return None;
    }
    let targets: Vec<String> = game
        .players
        .iter()
        .filter(|(_, p)| p.home_crew(now) > 0)
        .map(|(uuid, _)| uuid.clone())
        .collect();
    let target_uuid = rng.choice(&targets)?.clone();
    let (visible, hidden) = crate::combat::defense_split(game.players.get(&target_uuid)?, now);
    let spec = crate::combat::CombatSpec {
        attack_crew: i64::from(mutineers),
        defense_visible: visible,
        defense_hidden: hidden,
        buildings: game.players.get(&target_uuid)?.buildings.clone(),
        defender_gold: game.players.get(&target_uuid)?.gold,
        attacker_humiliated: false,
        defender_unpaid_days: game.players.get(&target_uuid)?.unpaid_days,
    };
    let result = crate::combat::resolve_combat(&spec, settings, rng);
    let target = game.players.get_mut(&target_uuid)?;
    let target_nick = target.nick_cache.clone();
    if result.outcome.attacker_won() {
        let stolen = crate::combat::vulnerable_gold(target.gold, target.buildings.vault) * 5 / 100;
        target.gold -= stolen;
        let lost = target.crew_regular.min(1);
        target.crew_regular -= lost;
        target.career_crew_lost += lost;
        Some(MutinyReport {
            target_nick,
            mutineers,
            defenders_lost: lost as u32,
            gold_stolen: stolen,
            repelled: false,
        })
    } else {
        Some(MutinyReport {
            target_nick,
            mutineers,
            defenders_lost: 0,
            gold_stolen: 0,
            repelled: true,
        })
    }
}

/// Next daily-rollover due time: the next occurrence of `hour` UTC.
pub(crate) fn next_rollover(now: i64, hour_utc: i64) -> i64 {
    let day_start = now - now.rem_euclid(86_400);
    let mut due = day_start + hour_utc.clamp(0, 23) * 3600;
    if due <= now {
        due += 86_400;
    }
    due
}

pub(crate) fn handle_daily(
    server: &str,
    channel: &str,
    game_key: &str,
) -> Result<(), extism_pdk::Error> {
    let settings = crate::pirate_settings(server, channel);
    let now = crate::now_secs();
    // A disabled game does not tick. Running payday anyway would rot loyalty, desert crew, and
    // degrade buildings while `!pay` is unreachable — punishing captains for an operator's
    // decision. The job is rescheduled so the game resumes cleanly when it is switched back on.
    if !setting_enabled(server, channel) {
        crate::schedule(
            &crate::daily_job_id(server, channel),
            server,
            channel,
            None,
            next_rollover(now, settings.rollover_hour_utc),
            "",
        )?;
        return Ok(());
    }
    let mut state = crate::load_state()?;
    let (report, mutiny) = if let Some(game) = state.games.get_mut(game_key) {
        let report = daily_rollover(game, &settings);
        let mutiny = if report.mutineers > 0 && game.sea == "sargasso" {
            resolve_mutiny(game, report.mutineers, &settings, now, &mut crate::rng()?)
        } else {
            None
        };
        (report, mutiny)
    } else {
        return Ok(());
    };
    crate::save_state(&state)?;
    if !report.paid.is_empty() {
        reply(
            server,
            channel,
            &themed(
                "pirate.daily_paid",
                &["Payday has passed. Paid captains: {captains}."],
                &[("captains", &report.paid.join(", "))],
            )?,
        )?;
    }
    if !report.unpaid.is_empty() {
        let count = report.unpaid.len().to_string();
        reply(
            server,
            channel,
            &themed(
                "pirate.daily_unpaid",
                &["{count} captain(s) missed payday; loyalty and buildings suffer."],
                &[("count", &count)],
            )?,
        )?;
    }
    if let Some(mutiny) = mutiny {
        let result = if mutiny.repelled {
            "repelled"
        } else {
            "escaped with plunder"
        };
        let thieves = mutiny.mutineers.to_string();
        reply(
            server,
            channel,
            &themed(
                "pirate.mutiny",
                &["A mutiny fleet of {thieves} deserter(s) struck {target}: {result}."],
                &[
                    ("thieves", &thieves),
                    ("target", &mutiny.target_nick),
                    ("result", result),
                ],
            )?,
        )?;
    }
    crate::schedule(
        &crate::daily_job_id(server, channel),
        server,
        channel,
        None,
        next_rollover(now, settings.rollover_hour_utc),
        "",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Buildings, Player};

    fn game_with(player: Player) -> Game {
        let mut game = Game::default();
        game.players.insert("a".into(), player);
        game
    }

    #[test]
    fn paid_players_reset_and_stay_loyal() {
        let mut game = game_with(Player {
            nick_cache: "Ann".into(),
            gold: 100,
            crew_regular: 3,
            crew_loyal: 2,
            paid_today: true,
            unpaid_days: 2,
            loyalty_tier: 1,
            buildings: Buildings {
                cove: 0,
                walls: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let report = daily_rollover(&mut game, &PirateSettings::default());
        assert_eq!(report.paid, vec!["Ann".to_string()]);
        assert!(report.unpaid.is_empty());
        let player = &game.players["a"];
        assert!(!player.paid_today, "flag resets for the new day");
        assert_eq!(player.loyalty_tier, 3);
        assert_eq!(player.unpaid_days, 0);
        assert_eq!(player.gold, 90, "walls L1 upkeep drains 10g");
    }

    #[test]
    fn unpaid_players_decay_then_desert() {
        let mut game = game_with(Player {
            nick_cache: "Bob".into(),
            crew_regular: 2,
            loyalty_tier: 1,
            ..Default::default()
        });
        let report = daily_rollover(&mut game, &PirateSettings::default());
        let player = &game.players["a"];
        assert_eq!(player.loyalty_tier, 0);
        assert_eq!(player.crew_regular, 1, "loyalty 0 deserts one crew");
        assert_eq!(report.unpaid[0].deserted, 1);
        assert_eq!(report.unpaid[0].unpaid_days, 1);
    }

    #[test]
    fn parked_players_skip_payday_penalties_and_upkeep() {
        let mut game = game_with(Player {
            gold: 100,
            loyalty_tier: 1,
            paid_today: true,
            parked: true,
            buildings: Buildings {
                vault: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let report = daily_rollover(&mut game, &PirateSettings::default());
        let player = &game.players["a"];
        assert!(report.paid.is_empty());
        assert!(report.unpaid.is_empty());
        assert_eq!(player.loyalty_tier, 1);
        assert_eq!(player.gold, 100);
        assert_eq!(player.buildings.vault, 1);
        assert!(!player.paid_today);
    }

    #[test]
    fn tavern_suppresses_desertion() {
        let mut game = game_with(Player {
            crew_regular: 2,
            loyalty_tier: 0,
            buildings: Buildings {
                tavern: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let report = daily_rollover(&mut game, &PirateSettings::default());
        assert_eq!(game.players["a"].crew_regular, 2);
        assert_eq!(report.unpaid[0].deserted, 0);
    }

    #[test]
    fn unpaid_upkeep_degrades_one_building_level() {
        let mut game = game_with(Player {
            buildings: Buildings {
                vault: 2,
                walls: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        daily_rollover(&mut game, &PirateSettings::default());
        let b = &game.players["a"].buildings;
        assert_eq!(b.vault, 1, "highest-upkeep building degrades first");
        assert_eq!(b.walls, 1, "only one building degrades per rollover");
    }

    #[test]
    fn paid_but_broke_buildings_degrade_instead_of_charging() {
        let mut game = game_with(Player {
            gold: 5,
            paid_today: true,
            buildings: Buildings {
                vault: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        daily_rollover(&mut game, &PirateSettings::default());
        let player = &game.players["a"];
        assert_eq!(player.buildings.vault, 0, "could not afford 10g upkeep");
        assert_eq!(player.gold, 5, "no partial charges");
    }

    #[test]
    fn sargasso_deserters_form_mutiny_fleets() {
        let mut game = game_with(Player {
            crew_regular: 3,
            loyalty_tier: 0,
            ..Default::default()
        });
        game.sea = "sargasso".into();
        let report = daily_rollover(&mut game, &PirateSettings::default());
        assert_eq!(report.mutineers, 1);
        game.sea = "tortuga".into();
        game.players.get_mut("a").unwrap().loyalty_tier = 0;
        let report = daily_rollover(&mut game, &PirateSettings::default());
        assert_eq!(report.mutineers, 0, "other seas lose deserters quietly");
    }

    #[test]
    fn next_rollover_is_the_next_occurrence_of_the_hour() {
        let midnight = 86_400 * 1000;
        assert_eq!(next_rollover(midnight + 1, 0), midnight + 86_400);
        assert_eq!(next_rollover(midnight - 1, 0), midnight);
        assert_eq!(next_rollover(midnight + 3600, 6), midnight + 6 * 3600);
    }
}
