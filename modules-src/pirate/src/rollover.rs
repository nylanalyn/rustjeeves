//! Daily rollover: payday check, loyalty decay, desertion, building upkeep/degradation, and the
//! Sargasso Depths mutiny fleets. The core is pure; the caller renders themed announcements.

use crate::buildings;
use crate::model::{AutoPay, Game, Player};
use crate::{announce, game_open, PirateSettings, Rng};

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
    /// Purser outcomes worth a private word: skims and shortfalls. Honest paydays stay quiet.
    pub(crate) purser: Vec<PurserNote>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PurserNote {
    pub(crate) uuid: String,
    pub(crate) nick: String,
    pub(crate) outcome: PurserOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PurserOutcome {
    /// Wages (and the fee) paid honestly.
    Paid { cost: i64, fee: i64 },
    /// Wages paid, and the purser helped himself to `skim` gold on the way out.
    Skimmed { cost: i64, fee: i64, skim: i64 },
    /// The hold could not cover wages plus fee; the crew went unpaid.
    Short { needed: i64, resource: AutoPay },
}

/// The purser's payday for one captain with a standing `!pay auto` order: wages plus his fee
/// (`autopay_fee_pct`), from the chosen hold. When the coffers sit above `autopay_skim_threshold`
/// gold he may also skim 1..=`autopay_skim_max_pct`% of the excess (`autopay_skim_chance_pct`).
/// Pure; `employed` is the captain's `(regular, loyal)` payroll. Returns `None` when there is no
/// standing order or the crew were already paid by hand.
pub(crate) fn purser_pays(
    player: &mut Player,
    employed: (i64, i64),
    settings: &PirateSettings,
    rng: &mut Rng,
) -> Option<PurserOutcome> {
    let resource = player.auto_pay?;
    if player.paid_today {
        return None;
    }
    let unit = match resource {
        AutoPay::Gold => settings.crew_wage_gold,
        AutoPay::Rum => settings.crew_wage_rum,
    };
    let cost = crate::commands::wage_cost(employed.0, employed.1, unit, settings.crew_soft_cap);
    // The fee rounds up: a purser never works for free.
    let fee = (cost.saturating_mul(settings.autopay_fee_pct.max(0)) + 99) / 100;
    let needed = cost.saturating_add(fee);
    let balance = match resource {
        AutoPay::Gold => &mut player.gold,
        AutoPay::Rum => &mut player.rum,
    };
    if *balance < needed {
        return Some(PurserOutcome::Short { needed, resource });
    }
    *balance -= needed;
    player.paid_today = true;
    let excess = player.gold - settings.autopay_skim_threshold.max(0);
    let chance = settings.autopay_skim_chance_pct.clamp(0, 100) as f64 / 100.0;
    if excess > 0 && rng.chance(chance) {
        let pct = rng.between(1, settings.autopay_skim_max_pct.max(1));
        let skim = (excess * pct / 100).max(1);
        player.gold -= skim;
        return Some(PurserOutcome::Skimmed { cost, fee, skim });
    }
    Some(PurserOutcome::Paid { cost, fee })
}

pub(crate) fn retirement_candidates(game: &mut Game, now: i64, days: i64) -> Vec<String> {
    let threshold = days.saturating_mul(86_400);
    let blockaders = game
        .players
        .values()
        .filter_map(|player| {
            player
                .player_blockade
                .as_ref()
                .filter(|blockade| blockade.until > now)
                .map(|blockade| blockade.blockader_uuid.clone())
        })
        .collect::<std::collections::HashSet<_>>();
    game.players
        .iter_mut()
        .filter_map(|(uuid, player)| {
            // Old saves had no activity timestamp. Give each a full inactivity window from this
            // migration rollover instead of treating its creation date as last activity.
            if player.last_activity_at == 0 {
                player.last_activity_at = now;
                return None;
            }
            (threshold > 0
                && !player.is_npc()
                && !player.parked
                && !blockaders.contains(uuid.as_str())
                && now.saturating_sub(player.last_activity_at) >= threshold)
                .then(|| uuid.clone())
        })
        .collect()
}

/// Degrade one building level: the highest-level, most expensive building first.
/// One payday pass over the game. `paid_today` flags reset for the new day.
pub(crate) fn daily_rollover(
    game: &mut Game,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> RolloverReport {
    let mut report = RolloverReport::default();
    let sargasso = game.sea == "sargasso";
    let mut uuids: Vec<String> = game.players.keys().cloned().collect();
    uuids.sort();
    for uuid in uuids {
        let employed = crate::commands::employed_crew(game, &uuid).unwrap_or_default();
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
        // The brothel earns whether or not the crew are paid — and the scandal accrues either
        // way, feeding the Notoriety that decides who the Royal Navy sights next.
        let (income, scandal) = buildings::brothel_take(&player.buildings, settings);
        player.gold = player.gold.saturating_add(income);
        player.notoriety = player.notoriety.saturating_add(scandal);
        match purser_pays(player, employed, settings, rng) {
            Some(outcome @ (PurserOutcome::Skimmed { .. } | PurserOutcome::Short { .. })) => {
                report.purser.push(PurserNote {
                    uuid: uuid.clone(),
                    nick: player.nick_cache.clone(),
                    outcome,
                });
            }
            Some(PurserOutcome::Paid { .. }) | None => {}
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
        .filter(|(_, p)| !p.parked && p.home_crew(now) > 0)
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
        attack_bonus_pct: 0,
        defense_bonus_pct: 0,
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

pub(crate) fn handle_daily(server: &str, game_key: &str) -> Result<(), extism_pdk::Error> {
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
    // A disabled game does not tick. Running payday anyway would rot loyalty, desert crew, and
    // degrade buildings while `!pay` is unreachable — punishing captains for an operator's
    // decision. The job is rescheduled so the game resumes cleanly when it is switched back on.
    if !game_open(server, game) {
        crate::schedule(
            &crate::daily_job_id(server),
            server,
            &room,
            None,
            next_rollover(now, settings.rollover_hour_utc),
            "",
        )?;
        return Ok(());
    }
    // At-least-once delivery: a retried job must never pay out (or punish) the same day twice.
    if rollover_already_ran(game, now) {
        crate::schedule(
            &crate::daily_job_id(server),
            server,
            &room,
            None,
            next_rollover(now, settings.rollover_hour_utc),
            "",
        )?;
        return Ok(());
    }
    let to_retire = retirement_candidates(
        state.games.get_mut(game_key).expect("checked above"),
        now,
        settings.retire_after_days,
    );
    let mut retired = Vec::new();
    let mut cancelled = Vec::new();
    for uuid in to_retire {
        let nick = state.games[game_key].players[&uuid].nick_cache.clone();
        cancelled.extend(crate::commands::pause_player(
            &mut state, server, &room, &uuid, &settings, now, true,
        )?);
        retired.push(nick);
    }
    let (report, mutiny) = {
        let game = state.games.get_mut(game_key).expect("checked above");
        let report = daily_rollover(game, &settings, &mut crate::rng()?);
        let mutiny = if report.mutineers > 0 && game.sea == "sargasso" {
            resolve_mutiny(game, report.mutineers, &settings, now, &mut crate::rng()?)
        } else {
            None
        };
        game.last_rollover_at = now;
        (report, mutiny)
    };
    // Re-arm before committing: once tomorrow's job has replaced this one, a failure below can no
    // longer trigger a retry of today's payday.
    crate::schedule(
        &crate::daily_job_id(server),
        server,
        &room,
        None,
        next_rollover(now, settings.rollover_hour_utc),
        "",
    )?;
    crate::save_state(&state)?;
    let game = state.games.get(game_key).expect("checked above");
    for resolution in &cancelled {
        crate::log_failure(
            "retirement raid cancellation notice",
            crate::voyage::deliver_resolution(server, game, resolution),
        );
    }
    if !retired.is_empty() {
        crate::log_failure(
            "retirement announcement",
            announce(
                server,
                game,
                "pirate.retired",
                &["The following captains have been retired after a long absence: {captains}. Their isles are safely parked; reply !unpark to return."],
                &[("captains", &retired.join(", "))],
            ),
        );
    }
    if !report.paid.is_empty() {
        crate::log_failure(
            "payday announcement",
            announce(
                server,
                game,
                "pirate.daily_paid",
                &["Payday has passed. Paid captains: {captains}."],
                &[("captains", &report.paid.join(", "))],
            ),
        );
    }
    if !report.unpaid.is_empty() {
        let count = report.unpaid.len().to_string();
        crate::log_failure(
            "missed-payday announcement",
            announce(
                server,
                game,
                "pirate.daily_unpaid",
                &["{count} captain(s) missed payday; loyalty and buildings suffer."],
                &[("count", &count)],
            ),
        );
    }
    if let Some(mutiny) = mutiny {
        let result = if mutiny.repelled {
            "repelled"
        } else {
            "escaped with plunder"
        };
        let thieves = mutiny.mutineers.to_string();
        crate::log_failure(
            "mutiny announcement",
            announce(
                server,
                game,
                "pirate.mutiny",
                &["A mutiny fleet of {thieves} deserter(s) struck {target}: {result}."],
                &[
                    ("thieves", &thieves),
                    ("target", &mutiny.target_nick),
                    ("result", result),
                ],
            ),
        );
    }
    for note in &report.purser {
        crate::log_failure("purser notice", purser_notice(server, note));
    }
    Ok(())
}

/// A private word from the purser when something about payday deserves the captain's attention.
fn purser_notice(server: &str, note: &PurserNote) -> Result<(), extism_pdk::Error> {
    let text = match &note.outcome {
        PurserOutcome::Skimmed { cost, fee, skim } => crate::themed(
            "pirate.purser_skimmed",
            &["Your purser paid the crew ({cost} wages + {fee} fee)... and the coffers look {skim}g lighter than they should. Heavy purses tempt light fingers."],
            &[
                ("cost", &cost.to_string()),
                ("fee", &fee.to_string()),
                ("skim", &skim.to_string()),
            ],
        )?,
        PurserOutcome::Short { needed, resource } => crate::themed(
            "pirate.purser_short",
            &["Your purser could not make payday: wages and his fee come to {needed} {resource}, and the hold is short. The crew went unpaid."],
            &[
                ("needed", &needed.to_string()),
                (
                    "resource",
                    match resource {
                        AutoPay::Gold => "gold",
                        AutoPay::Rum => "rum",
                    },
                ),
            ],
        )?,
        PurserOutcome::Paid { .. } => return Ok(()),
    };
    crate::pm_captain(server, &note.uuid, &note.nick, &text)
}

/// Whether a rollover already ran within the last hour. Real rollovers are ~24h apart (an operator
/// moving `rollover_hour_utc` can shorten one gap, never below an hour); a retried delivery of the
/// same job lands seconds later.
pub(crate) fn rollover_already_ran(game: &Game, now: i64) -> bool {
    game.last_rollover_at > 0 && now.saturating_sub(game.last_rollover_at) < 3_600
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Buildings, Player, PlayerBlockade};

    #[test]
    fn npc_captains_never_retire() {
        let now = 91 * 86_400;
        let mut game = Game::default();
        game.players.insert(
            "npc:kidd".into(),
            Player {
                last_activity_at: 1,
                npc: Some(crate::model::NpcCaptain::default()),
                ..Default::default()
            },
        );
        assert!(retirement_candidates(&mut game, now, 90).is_empty());
    }

    #[test]
    fn inactivity_retirement_has_a_legacy_grace_and_can_be_disabled() {
        let now = 91 * 86_400;
        let mut game = Game::default();
        game.players.insert("legacy".into(), Player::default());
        game.players.insert(
            "inactive".into(),
            Player {
                last_activity_at: 1,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "inactive".into(),
                    until: now + 1,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        assert!(retirement_candidates(&mut game, now, 90).is_empty());
        game.players
            .get_mut("inactive")
            .unwrap()
            .player_blockade
            .as_mut()
            .unwrap()
            .until = now;
        assert_eq!(retirement_candidates(&mut game, now, 90), vec!["inactive"]);
        assert_eq!(game.players["legacy"].last_activity_at, now);
        assert!(retirement_candidates(&mut game, now + 100 * 86_400, 0).is_empty());
    }

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
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
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
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
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
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
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
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
        assert_eq!(game.players["a"].crew_regular, 2);
        assert_eq!(report.unpaid[0].deserted, 0);
    }

    #[test]
    fn the_brothel_earns_whether_or_not_the_crew_are_paid() {
        let mut game = game_with(Player {
            gold: 100,
            paid_today: true,
            buildings: Buildings {
                brothel: 1,
                ..Default::default()
            },
            ..Default::default()
        });
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
        assert!(report.paid.len() == 1, "paid path still applies");
        let player = &game.players["a"];
        // +25 brothel income; upkeep drains 15 (brothel) + 15 (the default Cove L1).
        assert_eq!(player.gold, 95);
        assert_eq!(player.notoriety, 1, "L1 scandal accrues");
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
        daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
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
        daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
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
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
        assert_eq!(report.mutineers, 1);
        game.sea = "tortuga".into();
        game.players.get_mut("a").unwrap().loyalty_tier = 0;
        let report = daily_rollover(&mut game, &PirateSettings::default(), &mut Rng::new(1));
        assert_eq!(report.mutineers, 0, "other seas lose deserters quietly");
    }

    #[test]
    fn next_rollover_is_the_next_occurrence_of_the_hour() {
        let midnight = 86_400 * 1000;
        assert_eq!(next_rollover(midnight + 1, 0), midnight + 86_400);
        assert_eq!(next_rollover(midnight - 1, 0), midnight);
        assert_eq!(next_rollover(midnight + 3600, 6), midnight + 6 * 3600);
    }

    #[test]
    fn a_retried_rollover_delivery_is_recognised_as_already_run() {
        let mut game = Game::default();
        assert!(
            !rollover_already_ran(&game, 100_000),
            "a fresh game has never rolled over"
        );
        game.last_rollover_at = 100_000;
        assert!(
            rollover_already_ran(&game, 100_030),
            "a 30s host retry is a duplicate"
        );
        assert!(
            !rollover_already_ran(&game, 100_000 + 86_400),
            "tomorrow is a new day"
        );
    }

    fn purser_player(gold: i64, rum: i64, order: AutoPay) -> Player {
        Player {
            nick_cache: "Pat".into(),
            gold,
            rum,
            crew_regular: 4,
            auto_pay: Some(order),
            ..Default::default()
        }
    }

    #[test]
    fn the_purser_pays_wages_plus_his_fee_at_rollover() {
        let settings = PirateSettings {
            autopay_skim_chance_pct: 0,
            ..PirateSettings::default()
        };
        let mut game = Game::default();
        game.players
            .insert("a".into(), purser_player(100, 0, AutoPay::Gold));
        let report = daily_rollover(&mut game, &settings, &mut Rng::new(1));
        // 4 crew × 5g = 20g wages, +20% fee = 24g; then the starting Cove's 15g upkeep.
        assert_eq!(game.players["a"].gold, 100 - 24 - 15);
        assert_eq!(game.players["a"].loyalty_tier, 3);
        assert_eq!(report.paid, vec!["Pat".to_string()]);
        assert!(report.purser.is_empty(), "an honest payday needs no word");
    }

    #[test]
    fn the_purser_can_pay_in_rum_and_reports_a_short_hold() {
        let settings = PirateSettings::default();
        let mut rum = purser_player(0, 10, AutoPay::Rum);
        assert_eq!(
            purser_pays(&mut rum, (4, 0), &settings, &mut Rng::new(1)),
            Some(PurserOutcome::Paid { cost: 4, fee: 1 })
        );
        assert_eq!(rum.rum, 5);

        let mut broke = purser_player(10, 0, AutoPay::Gold);
        assert_eq!(
            purser_pays(&mut broke, (4, 0), &settings, &mut Rng::new(1)),
            Some(PurserOutcome::Short {
                needed: 24,
                resource: AutoPay::Gold
            })
        );
        assert!(!broke.paid_today);
        assert_eq!(broke.gold, 10, "a short hold is left untouched");
    }

    #[test]
    fn the_purser_only_skims_heavy_coffers() {
        let settings = PirateSettings {
            autopay_skim_chance_pct: 100,
            ..PirateSettings::default()
        };
        let mut light = purser_player(400, 0, AutoPay::Gold);
        assert!(matches!(
            purser_pays(&mut light, (4, 0), &settings, &mut Rng::new(1)),
            Some(PurserOutcome::Paid { .. })
        ));

        let mut heavy = purser_player(2_024, 0, AutoPay::Gold);
        let Some(PurserOutcome::Skimmed { skim, .. }) =
            purser_pays(&mut heavy, (4, 0), &settings, &mut Rng::new(1))
        else {
            panic!("a heavy purse is skimmed at 100%");
        };
        // 2,000 left after wages; 1,500 over the threshold; 1–5% of that.
        assert!((15..=75).contains(&skim), "skim {skim}");
        assert_eq!(heavy.gold, 2_000 - skim);
    }

    #[test]
    fn paying_by_hand_leaves_the_purser_idle() {
        let mut player = purser_player(100, 0, AutoPay::Gold);
        player.paid_today = true;
        assert_eq!(
            purser_pays(
                &mut player,
                (4, 0),
                &PirateSettings::default(),
                &mut Rng::new(1)
            ),
            None
        );
        assert_eq!(player.gold, 100);
    }
}
