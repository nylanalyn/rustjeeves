//! The voyage system (PLAN-PIRATE.md §6): the mission catalog, option rolls for the PM menu,
//! launch validation, and resolution of returned voyages. Game math is pure over the model tree;
//! the timer/catch-up handlers at the bottom are the only functions that talk to the host.

use crate::model::{Game, Voyage, VoyageKind, VoyageResult};
use crate::{award_to, combat, reply, themed, PirateSettings, Rng};
use extism_pdk::Error;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Risk {
    Low,
    Med,
    High,
}

#[derive(Clone, Copy)]
pub(crate) struct VoyageDef {
    pub kind: VoyageKind,
    pub name: &'static str,
    pub hours: i64,
    pub min_crew: i64,
    pub risk: Risk,
}

/// The NPC mission catalog (PLAN §6.1). Raid and scout are not here: they need a live target and
/// are rolled separately.
pub(crate) const CATALOG: &[VoyageDef] = &[
    VoyageDef {
        kind: VoyageKind::Merchant,
        name: "Merchant Convoy",
        hours: 4,
        min_crew: 2,
        risk: Risk::Low,
    },
    VoyageDef {
        kind: VoyageKind::Rum,
        name: "Rum Runners",
        hours: 2,
        min_crew: 2,
        risk: Risk::Low,
    },
    VoyageDef {
        kind: VoyageKind::Pressgang,
        name: "Pressgang",
        hours: 3,
        min_crew: 2,
        risk: Risk::Low,
    },
    VoyageDef {
        kind: VoyageKind::Smuggler,
        name: "Smuggler's Cache",
        hours: 4,
        min_crew: 3,
        risk: Risk::Med,
    },
    VoyageDef {
        kind: VoyageKind::NavyPayroll,
        name: "Navy Payroll",
        hours: 6,
        min_crew: 5,
        risk: Risk::High,
    },
    VoyageDef {
        kind: VoyageKind::Explore,
        name: "Explore Unknown",
        hours: 6,
        min_crew: 4,
        risk: Risk::Med,
    },
];

pub(crate) fn voyage_def(kind: VoyageKind) -> VoyageDef {
    CATALOG
        .iter()
        .copied()
        .find(|def| def.kind == kind)
        .unwrap_or(match kind {
            VoyageKind::Raid => VoyageDef {
                kind,
                name: "Raid",
                hours: 4,
                min_crew: 1,
                risk: Risk::Low,
            },
            VoyageKind::Scout => VoyageDef {
                kind,
                name: "Scout",
                hours: 2,
                min_crew: 1,
                risk: Risk::Low,
            },
            _ => CATALOG[0],
        })
}

/// One voyage option offered by the PM menu.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct VoyageOption {
    pub kind: VoyageKind,
    #[serde(default)]
    pub target_uuid: Option<String>,
    #[serde(default)]
    pub target_nick: Option<String>,
}

impl VoyageOption {
    pub(crate) fn label(&self) -> String {
        let def = voyage_def(self.kind);
        match (&self.kind, &self.target_nick) {
            (VoyageKind::Raid, Some(nick)) => format!("Raid {nick}'s isle ({}h)", def.hours),
            (VoyageKind::Scout, Some(nick)) => format!("Scout {nick}'s isle ({}h)", def.hours),
            _ => format!("{} ({}h, min {} crew)", def.name, def.hours, def.min_crew),
        }
    }
}

/// Base reward roll for an NPC mission: (gold, rum, new regular crew, extra crew lost).
/// Explore uses the 50/30/15/5 table, whose 5% outcome carries its own crew loss.
pub(crate) fn roll_reward(kind: VoyageKind, rng: &mut Rng) -> (i64, i64, i64, i64) {
    match kind {
        VoyageKind::Merchant => (rng.between(60, 100), 0, 0, 0),
        VoyageKind::Rum => (0, rng.between(4, 6), 0, 0),
        VoyageKind::Pressgang => (0, 0, rng.between(1, 2), 0),
        VoyageKind::Smuggler => (40, rng.between(2, 3), 0, 0),
        VoyageKind::NavyPayroll => (rng.between(150, 250), 0, 0, 0),
        VoyageKind::Explore => {
            let roll = rng.f64();
            if roll < 0.50 {
                (60, 0, 0, 0)
            } else if roll < 0.80 {
                (100, 0, 0, 0)
            } else if roll < 0.95 {
                (40, 2, 0, 0)
            } else {
                (50, 0, 0, 1)
            }
        }
        _ => (0, 0, 0, 0),
    }
}

/// Risk-roll crew losses (PLAN §6.1): Med = 10% lose 1, High = 25% lose 1–2. Only regular crew
/// can be lost; loyal crew always come home.
pub(crate) fn risk_losses(risk: Risk, rng: &mut Rng) -> i64 {
    match risk {
        Risk::Low => 0,
        Risk::Med => i64::from(rng.chance(0.10)),
        Risk::High => {
            if rng.chance(0.25) {
                rng.between(1, 2)
            } else {
                0
            }
        }
    }
}

/// Voyage duration in seconds after the Shipyard speed bonus and sea modifiers. The Black Sea's
/// storms add 1–2 hours to every ETA.
pub(crate) fn duration_secs(def: &VoyageDef, shipyard_speed: f64, sea: &str, rng: &mut Rng) -> i64 {
    let mut secs = (def.hours as f64 * 3_600.0 * shipyard_speed) as i64;
    if sea == crate::season::BLACK_SEA {
        secs += rng.between(1, 2) * 3_600;
    }
    secs.max(60)
}

/// Why a voyage cannot launch.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LaunchError {
    /// Below the mission's minimum crew (or zero).
    MinCrew(i64),
    /// Not enough crew home right now.
    CrewShort(i64),
    /// Already at the active-voyage cap.
    TooManyVoyages(i64),
    /// The Royal Navy blockade forbids launches.
    Blockaded,
    /// Raid/scout without a valid target.
    NoTarget,
    /// Cannot target yourself.
    SelfTarget,
    /// Target still enjoys new-player immunity.
    TargetShielded,
    /// Target already has two raids inbound.
    TargetBusy,
}

pub(crate) fn active_voyages(game: &Game, uuid: &str) -> usize {
    game.voyages
        .iter()
        .filter(|v| v.owner_uuid == uuid && !v.resolved)
        .count()
}

/// Captains a raid may target right now: not self, not shielded, fewer than two raids inbound.
pub(crate) fn valid_raid_targets(game: &Game, uuid: &str, now: i64) -> Vec<(String, String)> {
    game.players
        .iter()
        .filter(|(other, p)| {
            other.as_str() != uuid
                && !p.shielded(now)
                && game
                    .voyages
                    .iter()
                    .filter(|v| {
                        v.kind == VoyageKind::Raid
                            && !v.resolved
                            && v.target_uuid.as_deref() == Some(other.as_str())
                    })
                    .count()
                    < 2
        })
        .map(|(other, p)| (other.clone(), p.nick_cache.clone()))
        .collect()
}

/// Captains a scout may target: anyone else.
pub(crate) fn valid_scout_targets(game: &Game, uuid: &str) -> Vec<(String, String)> {
    game.players
        .iter()
        .filter(|(other, _)| other.as_str() != uuid)
        .map(|(other, p)| (other.clone(), p.nick_cache.clone()))
        .collect()
}

/// Roll the PM menu's voyage options: `count` distinct NPC missions, plus a raid and/or scout
/// option when valid targets exist.
pub(crate) fn roll_options(
    game: &Game,
    uuid: &str,
    count: usize,
    now: i64,
    rng: &mut Rng,
) -> Vec<VoyageOption> {
    let mut pool: Vec<VoyageOption> = CATALOG
        .iter()
        .map(|def| VoyageOption {
            kind: def.kind,
            target_uuid: None,
            target_nick: None,
        })
        .collect();
    if let Some((target_uuid, target_nick)) = rng.choice(&valid_raid_targets(game, uuid, now)) {
        pool.push(VoyageOption {
            kind: VoyageKind::Raid,
            target_uuid: Some(target_uuid.clone()),
            target_nick: Some(target_nick.clone()),
        });
    }
    if let Some((target_uuid, target_nick)) = rng.choice(&valid_scout_targets(game, uuid)) {
        pool.push(VoyageOption {
            kind: VoyageKind::Scout,
            target_uuid: Some(target_uuid.clone()),
            target_nick: Some(target_nick.clone()),
        });
    }
    // Partial Fisher–Yates: pick `count` distinct options.
    let mut picked = Vec::new();
    for _ in 0..count.min(pool.len()) {
        let index = rng.below(pool.len());
        picked.push(pool.swap_remove(index));
    }
    picked
}

/// Validate a launch. `crew` is the total crew the captain wants to send (regular first, loyal
/// filling in). Pure: reads the game, returns the problem.
pub(crate) fn validate_launch(
    game: &Game,
    uuid: &str,
    kind: VoyageKind,
    target_uuid: Option<&str>,
    crew: i64,
    settings: &PirateSettings,
    now: i64,
) -> Result<(), LaunchError> {
    let player = game.players.get(uuid).ok_or(LaunchError::NoTarget)?;
    let def = voyage_def(kind);
    if crew < def.min_crew.max(1) {
        return Err(LaunchError::MinCrew(def.min_crew.max(1)));
    }
    if player.blockaded(now) {
        return Err(LaunchError::Blockaded);
    }
    if active_voyages(game, uuid) as i64 >= settings.max_active_voyages {
        return Err(LaunchError::TooManyVoyages(settings.max_active_voyages));
    }
    if player.home_crew(now) < crew {
        return Err(LaunchError::CrewShort(player.home_crew(now)));
    }
    if kind.is_pvp() {
        let target = target_uuid.ok_or(LaunchError::NoTarget)?;
        if target == uuid {
            return Err(LaunchError::SelfTarget);
        }
        let defender = game.players.get(target).ok_or(LaunchError::NoTarget)?;
        if kind == VoyageKind::Raid {
            if defender.shielded(now) {
                return Err(LaunchError::TargetShielded);
            }
            let inbound = game
                .voyages
                .iter()
                .filter(|v| {
                    v.kind == VoyageKind::Raid
                        && !v.resolved
                        && v.target_uuid.as_deref() == Some(target)
                })
                .count();
            if inbound >= 2 {
                return Err(LaunchError::TargetBusy);
            }
        }
    }
    Ok(())
}

/// Deduct crew and record the voyage. Call only after [`validate_launch`] passed; the caller
/// schedules the `voyage:` job and announces the departure. Returns the voyage duration in
/// seconds (for the scheduler). Crew split: regular first, loyal only for the remainder.
#[allow(clippy::too_many_arguments)]
pub(crate) fn launch(
    game: &mut Game,
    voyage_id: u64,
    uuid: &str,
    kind: VoyageKind,
    target_uuid: Option<String>,
    crew: i64,
    is_public: bool,
    now: i64,
    rng: &mut Rng,
) -> i64 {
    let (shipyard, false_flag_nick, regular_used, loyal_used) = {
        let player = game.players.get_mut(uuid).expect("validated");
        let regular_used = crew.min(player.home_regular());
        let loyal_used = crew - regular_used;
        player.crew_regular -= regular_used;
        player.crew_loyal -= loyal_used;
        (
            crate::buildings::shipyard_speed(&player.buildings),
            player.false_flag.take().map(|flag| flag.nick),
            regular_used,
            loyal_used,
        )
    };
    let def = voyage_def(kind);
    let sea = game.sea.clone();
    let secs = duration_secs(&def, shipyard, &sea, rng);
    game.voyages.push(Voyage {
        id: voyage_id,
        owner_uuid: uuid.into(),
        kind,
        target_uuid,
        crew_regular: regular_used,
        crew_loyal: loyal_used,
        is_public,
        false_flag_nick,
        started_at: now,
        returns_at: now + secs,
        resolved: false,
        collected: false,
        result: None,
    });
    secs
}

/// What a resolved voyage produced, for the caller to render and send.
pub(crate) enum Resolution {
    Npc {
        owner_uuid: String,
        owner_nick: String,
        kind: VoyageKind,
        gold: i64,
        rum: i64,
        new_crew: i64,
        crew_lost: i64,
    },
    Raid(Box<combat::RaidReport>),
    Scout(Box<combat::ScoutReport>),
    /// The target vanished mid-voyage; crew came home with nothing.
    Fizzled {
        owner_uuid: String,
        owner_nick: String,
    },
}

/// Resolve one due voyage: compute the outcome, return surviving crew home, store the result for
/// `!collect`. Pure over the game tree. Returns `None` when the voyage is unknown or resolved.
pub(crate) fn resolve_voyage(
    game: &mut Game,
    voyage_id: u64,
    rng: &mut Rng,
    settings: &PirateSettings,
    now: i64,
) -> Option<Resolution> {
    let index = game
        .voyages
        .iter()
        .position(|v| v.id == voyage_id && !v.resolved)?;
    let voyage = game.voyages[index].clone();
    match voyage.kind {
        VoyageKind::Raid => Some(combat::resolve_raid(game, &voyage, rng, settings, now)),
        VoyageKind::Scout => Some(combat::resolve_scout(game, &voyage, rng, now)),
        kind => Some(resolve_npc(game, &voyage, kind, rng)),
    }
}

pub(crate) fn resolve_npc(
    game: &mut Game,
    voyage: &Voyage,
    kind: VoyageKind,
    rng: &mut Rng,
) -> Resolution {
    let (mut gold, rum, new_crew, bonus_loss) = roll_reward(kind, rng);
    let mut loss = bonus_loss;
    if kind != VoyageKind::Explore {
        loss += risk_losses(voyage_def(kind).risk, rng);
    }
    let crew_lost = loss.min(voyage.crew_regular).max(0);
    // The Frozen North pays half again as much gold on every voyage.
    if game.sea == crate::season::FROZEN_NORTH {
        gold = gold * 3 / 2;
    }
    let (owner_uuid, owner_nick) = {
        let player = game
            .players
            .get_mut(&voyage.owner_uuid)
            .expect("voyage owner is a player");
        player.crew_regular += voyage.crew_regular - crew_lost;
        player.crew_loyal += voyage.crew_loyal;
        player.career_crew_lost += crew_lost;
        (voyage.owner_uuid.clone(), player.nick_cache.clone())
    };
    if let Some(v) = game.voyages.iter_mut().find(|v| v.id == voyage.id) {
        v.resolved = true;
        v.result = Some(VoyageResult {
            gold,
            rum,
            new_crew,
            crew_lost,
            raid: None,
        });
    }
    Resolution::Npc {
        owner_uuid,
        owner_nick,
        kind,
        gold,
        rum,
        new_crew,
        crew_lost,
    }
}

/// Everything one captain may collect right now.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CollectSummary {
    pub count: usize,
    pub gold: i64,
    pub rum: i64,
    pub new_crew: i64,
    /// Gold actually banked after an active navy blockade halved it.
    pub halved: bool,
}

/// Claim all resolved, uncollected voyages: bank the loot (halved under a navy blockade), press
/// new crew, and prune the collected voyages from the game. Pure.
pub(crate) fn collect_pending(game: &mut Game, uuid: &str, now: i64) -> CollectSummary {
    let blockaded = game.players.get(uuid).is_some_and(|p| p.blockaded(now));
    let mut summary = CollectSummary::default();
    let mut done = Vec::new();
    for voyage in game.voyages.iter_mut() {
        if voyage.owner_uuid != uuid || !voyage.resolved || voyage.collected {
            continue;
        }
        if let Some(result) = &voyage.result {
            summary.count += 1;
            summary.gold += result.gold;
            summary.rum += result.rum;
            summary.new_crew += result.new_crew;
        }
        voyage.collected = true;
        done.push(voyage.id);
    }
    if summary.count == 0 {
        return summary;
    }
    if blockaded && summary.gold > 0 {
        summary.gold /= 2;
        summary.halved = true;
    }
    if let Some(player) = game.players.get_mut(uuid) {
        player.gold += summary.gold;
        player.rum += summary.rum;
        player.crew_regular += summary.new_crew;
    }
    game.voyages.retain(|v| !done.contains(&v.id));
    summary
}

/// Render and deliver one resolution (PM to the owner always; channel lines only when the game is
/// still enabled and the voyage is public). Also emits the achievement stats the resolution earned.
pub(crate) fn deliver_resolution(
    server: &str,
    channel: &str,
    enabled: bool,
    resolution: &Resolution,
) -> Result<(), Error> {
    match resolution {
        Resolution::Npc {
            owner_uuid,
            owner_nick,
            kind,
            gold,
            rum,
            new_crew,
            crew_lost,
            ..
        } => {
            let _ = owner_uuid;
            let mut loot = Vec::new();
            if *gold > 0 {
                loot.push(format!("{gold} gold"));
            }
            if *rum > 0 {
                loot.push(format!("{rum} rum"));
            }
            if *new_crew > 0 {
                loot.push(format!("{new_crew} new crew"));
            }
            let loot = if loot.is_empty() {
                "nothing but salt spray".to_string()
            } else {
                loot.join(", ")
            };
            let lost = crew_lost.to_string();
            reply(
                server,
                owner_nick,
                &themed(
                    "pirate.voyage_return",
                    &["Your crew have returned from the {mission}! Loot: {loot}. Crew lost: {lost}. Use !collect in the channel to claim your spoils."],
                    &[
                        ("mission", voyage_def(*kind).name),
                        ("loot", &loot),
                        ("lost", &lost),
                    ],
                )?,
            )?;
        }
        Resolution::Raid(report) => combat::deliver_raid_report(server, channel, enabled, report)?,
        Resolution::Scout(report) => combat::deliver_scout_report(server, report)?,
        Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        } => {
            let _ = owner_uuid;
            reply(
                server,
                owner_nick,
                &themed(
                    "pirate.voyage_fizzled",
                    &["Your crew drifted home — the isle they sailed for is abandoned. Nothing gained, nothing lost."],
                    &[],
                )?,
            )?;
        }
    }
    Ok(())
}

/// Fire one voyage's resolution from its scheduler job. Retry-safe: an unknown or already
/// resolved voyage is a successful no-op (see [`resolve_voyage`]).
pub(crate) fn handle_voyage_timer(
    server: &str,
    channel: &str,
    game_key: &str,
    voyage_id: u64,
) -> Result<(), Error> {
    let mut state = crate::load_state()?;
    let now = crate::now_secs();
    let enabled = crate::setting_enabled(server, channel);
    let settings = crate::pirate_settings(server, channel);
    let mut resolution = None;
    if let Some(game) = state.games.get_mut(game_key) {
        resolution = resolve_voyage(game, voyage_id, &mut crate::rng()?, &settings, now);
    }
    if let Some(resolution) = resolution {
        crate::save_state(&state)?;
        // Awards after the state commit, keyed on stable UUIDs.
        if let Resolution::Raid(report) = &resolution {
            let mut attacker_stats: Vec<(&str, u64)> = Vec::new();
            if report.attacker_won() {
                attacker_stats.push(("raids_won", 1));
                if report.loot_gold > 0 {
                    attacker_stats.push(("gold_plundered", report.loot_gold as u64));
                }
            }
            award_to(
                server,
                &report.attacker_uuid,
                &report.attacker_nick,
                channel,
                attacker_stats,
            )?;
            if report.defender_won() {
                award_to(
                    server,
                    &report.defender_uuid,
                    &report.defender_nick,
                    channel,
                    vec![
                        ("defenses_won", 1),
                        ("prisoners_taken", report.crew_captured.max(0) as u64),
                    ],
                )?;
            }
        }
        deliver_resolution(server, channel, enabled, &resolution)?;
        // Follow-up jobs (Crimson navy alert, loyal-cove return) are one-shot and idempotent.
        if let Resolution::Raid(report) = &resolution {
            let mut rng = crate::rng()?;
            if report.navy_alert {
                for (which, target) in [("a", &report.attacker_uuid), ("b", &report.defender_uuid)]
                {
                    let payload = serde_json::to_string(&serde_json::json!({
                        "target_uuid": target,
                    }))?;
                    crate::schedule(
                        &format!(
                            "{}:{voyage_id}:{which}",
                            crate::navy_hit_job_id(server, channel)
                        ),
                        server,
                        channel,
                        None,
                        now + rng.between(1, 24) * 3_600,
                        &payload,
                    )?;
                }
            }
            if report.loyal_retreated {
                crate::schedule(
                    &crate::loyal_return_job_id(server, channel, &report.defender_uuid),
                    server,
                    channel,
                    Some(report.defender_uuid.clone()),
                    now + settings.loyal_cove_cooldown_hours * 3_600,
                    &serde_json::to_string(&serde_json::json!({
                        "profile_id": report.defender_uuid,
                    }))?,
                )?;
            }
        }
    }
    Ok(())
}

/// Loyal crew return from the cove. The timestamp is the real mechanism (lazy expiry); this job
/// just clears it early so `!me` stops showing the cove note. Idempotent.
pub(crate) fn handle_loyal_return(_server: &str, game_key: &str, uuid: &str) -> Result<(), Error> {
    let mut state = crate::load_state()?;
    if let Some(game) = state.games.get_mut(game_key) {
        if let Some(player) = game.players.get_mut(uuid) {
            if player.loyal_cove_until != 0 {
                player.loyal_cove_until = 0;
                crate::save_state(&state)?;
            }
        }
    }
    Ok(())
}

/// Safety net (PLAN §20): force-resolve any overdue voyages whose scheduler job was lost. Called
/// at the top of command handling in a game channel.
pub(crate) fn resolve_overdue(
    state: &mut crate::model::State,
    server: &str,
    channel: &str,
    game_key: &str,
    settings: &PirateSettings,
    now: i64,
) -> Result<(), Error> {
    let enabled = crate::setting_enabled(server, channel);
    loop {
        let due = state.games.get(game_key).and_then(|game| {
            game.voyages
                .iter()
                .find(|v| !v.resolved && v.returns_at <= now)
                .map(|v| v.id)
        });
        let Some(voyage_id) = due else { break };
        let resolution = {
            let game = state.games.get_mut(game_key).expect("checked above");
            resolve_voyage(game, voyage_id, &mut crate::rng()?, settings, now)
        };
        if let Some(resolution) = resolution {
            crate::save_state(state)?;
            deliver_resolution(server, channel, enabled, &resolution)?;
        } else {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Player;

    fn game_with_two() -> Game {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                nick_cache: "Al".into(),
                gold: 500,
                crew_regular: 5,
                crew_loyal: 2,
                ..Default::default()
            },
        );
        game.players.insert(
            "b".into(),
            Player {
                nick_cache: "Bob".into(),
                gold: 500,
                crew_regular: 5,
                crew_loyal: 2,
                ..Default::default()
            },
        );
        game
    }

    #[test]
    fn npc_rewards_stay_inside_the_catalog_ranges() {
        let mut rng = Rng::new(42);
        for _ in 0..500 {
            let (gold, rum, crew, loss) = roll_reward(VoyageKind::Merchant, &mut rng);
            assert!((60..=100).contains(&gold) && rum == 0 && crew == 0 && loss == 0);
            let (gold, rum, ..) = roll_reward(VoyageKind::Rum, &mut rng);
            assert!(gold == 0 && (4..=6).contains(&rum));
            let (_, _, crew, _) = roll_reward(VoyageKind::Pressgang, &mut rng);
            assert!((1..=2).contains(&crew));
            let (gold, rum, ..) = roll_reward(VoyageKind::Smuggler, &mut rng);
            assert!(gold == 40 && (2..=3).contains(&rum));
            let (gold, ..) = roll_reward(VoyageKind::NavyPayroll, &mut rng);
            assert!((150..=250).contains(&gold));
            let (gold, rum, _, loss) = roll_reward(VoyageKind::Explore, &mut rng);
            assert!(
                matches!(
                    (gold, rum, loss),
                    (60, 0, 0) | (100, 0, 0) | (40, 2, 0) | (50, 0, 1)
                ),
                "unexpected explore outcome: {gold}g {rum}r loss {loss}"
            );
        }
    }

    #[test]
    fn explore_distribution_covers_every_outcome() {
        let mut rng = Rng::new(7);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..2_000 {
            seen.insert(roll_reward(VoyageKind::Explore, &mut rng));
        }
        assert_eq!(seen.len(), 4, "all four explore outcomes occur");
    }

    #[test]
    fn risk_rolls_never_exceed_their_bands() {
        let mut rng = Rng::new(99);
        for _ in 0..1_000 {
            assert_eq!(risk_losses(Risk::Low, &mut rng), 0);
            assert!((0..=1).contains(&risk_losses(Risk::Med, &mut rng)));
            assert!((0..=2).contains(&risk_losses(Risk::High, &mut rng)));
        }
    }

    #[test]
    fn black_sea_adds_one_to_two_hours_and_shipyard_speeds_up() {
        let def = voyage_def(VoyageKind::Merchant);
        let mut rng = Rng::new(5);
        for _ in 0..50 {
            let plain = duration_secs(&def, 1.0, "tortuga", &mut rng);
            assert_eq!(plain, 4 * 3_600);
            let stormy = duration_secs(&def, 1.0, crate::season::BLACK_SEA, &mut rng);
            assert!((5 * 3_600..=6 * 3_600).contains(&stormy));
            let fast = duration_secs(&def, 0.65, "tortuga", &mut rng);
            assert_eq!(fast, (4.0 * 3_600.0 * 0.65) as i64);
        }
    }

    #[test]
    fn frozen_north_pays_fifty_percent_more_gold() {
        let mut game = game_with_two();
        game.sea = crate::season::FROZEN_NORTH.into();
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "a".into(),
            kind: VoyageKind::Merchant,
            crew_regular: 2,
            ..Default::default()
        });
        let settings = PirateSettings::default();
        let resolution = resolve_voyage(&mut game, 1, &mut Rng::new(1), &settings, 1_000).unwrap();
        let Resolution::Npc { gold, .. } = resolution else {
            panic!("expected npc resolution")
        };
        assert!((90..=150).contains(&gold), "60–100 × 1.5");
    }

    #[test]
    fn validation_enforces_caps_blockades_and_shields() {
        let settings = PirateSettings::default();
        let now = 1_000_i64;
        let mut game = game_with_two();
        // Min crew.
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Merchant, None, 1, &settings, now),
            Err(LaunchError::MinCrew(2))
        );
        // Crew short.
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Merchant, None, 99, &settings, now),
            Err(LaunchError::CrewShort(7))
        );
        // Fine.
        assert!(
            validate_launch(&game, "a", Voyage::default().kind, None, 2, &settings, now).is_ok()
        );
        // Blockaded.
        game.players.get_mut("a").unwrap().navy_blockade_until = now + 3_600;
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Merchant, None, 2, &settings, now),
            Err(LaunchError::Blockaded)
        );
        game.players.get_mut("a").unwrap().navy_blockade_until = 0;
        // Voyage cap.
        for id in 1..=2 {
            game.voyages.push(Voyage {
                id,
                owner_uuid: "a".into(),
                kind: VoyageKind::Merchant,
                crew_regular: 2,
                ..Default::default()
            });
        }
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Merchant, None, 2, &settings, now),
            Err(LaunchError::TooManyVoyages(2))
        );
        game.voyages.clear();
        // Self-target and shield.
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Raid, Some("a"), 1, &settings, now),
            Err(LaunchError::SelfTarget)
        );
        game.players.get_mut("b").unwrap().shield_until = now + 3_600;
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Raid, Some("b"), 1, &settings, now),
            Err(LaunchError::TargetShielded)
        );
        // Scouts may still target a shielded isle.
        assert!(
            validate_launch(&game, "a", VoyageKind::Scout, Some("b"), 1, &settings, now).is_ok()
        );
    }

    #[test]
    fn npc_resolution_returns_crew_and_stores_the_result() {
        let mut game = game_with_two();
        game.voyages.push(Voyage {
            id: 9,
            owner_uuid: "a".into(),
            kind: VoyageKind::Merchant,
            crew_regular: 3,
            crew_loyal: 1,
            ..Default::default()
        });
        game.players.get_mut("a").unwrap().crew_regular = 2;
        game.players.get_mut("a").unwrap().crew_loyal = 1;
        let settings = PirateSettings::default();
        let resolution = resolve_voyage(&mut game, 9, &mut Rng::new(3), &settings, 1_000).unwrap();
        let Resolution::Npc { crew_lost, .. } = resolution else {
            panic!("expected npc resolution")
        };
        assert_eq!(crew_lost, 0, "merchant is low risk");
        let player = &game.players["a"];
        assert_eq!(player.crew_regular, 5, "crew returned home");
        assert_eq!(player.crew_loyal, 2);
        let voyage = &game.voyages[0];
        assert!(voyage.resolved && !voyage.collected);
        assert!(voyage.result.as_ref().is_some_and(|r| r.gold >= 60));
        // Resolving again is a no-op.
        assert!(resolve_voyage(&mut game, 9, &mut Rng::new(3), &settings, 1_000).is_none());
    }

    #[test]
    fn collect_banks_loot_and_prunes_voyages() {
        let mut game = game_with_two();
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "a".into(),
            kind: VoyageKind::Merchant,
            resolved: true,
            result: Some(VoyageResult {
                gold: 80,
                rum: 2,
                new_crew: 1,
                crew_lost: 0,
                raid: None,
            }),
            ..Default::default()
        });
        let summary = collect_pending(&mut game, "a", 1_000);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.gold, 80);
        assert_eq!(summary.rum, 2);
        assert_eq!(summary.new_crew, 1);
        let player = &game.players["a"];
        assert_eq!(player.gold, 580);
        assert_eq!(player.rum, 2);
        assert_eq!(player.crew_regular, 6);
        assert!(game.voyages.is_empty(), "collected voyage pruned");
        assert_eq!(collect_pending(&mut game, "a", 1_000).count, 0);
    }

    #[test]
    fn blockade_halves_collected_gold() {
        let mut game = game_with_two();
        game.players.get_mut("a").unwrap().navy_blockade_until = 2_000;
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "a".into(),
            resolved: true,
            result: Some(VoyageResult {
                gold: 81,
                rum: 3,
                ..Default::default()
            }),
            ..Default::default()
        });
        let summary = collect_pending(&mut game, "a", 1_000);
        assert!(summary.halved);
        assert_eq!(summary.gold, 40);
        assert_eq!(summary.rum, 3, "only gold income is halved");
    }
}
