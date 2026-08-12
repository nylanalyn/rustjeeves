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
    /// The target is in absence mode and cannot be interacted with.
    TargetParked,
    /// Cannot target yourself.
    SelfTarget,
    /// Target still enjoys new-player immunity.
    TargetShielded,
    /// Target already has two raids inbound.
    TargetBusy,
    /// Target was raided recently and is out of the pool while they lick their wounds.
    TargetRecentlyRaided,
    /// A stealth raid was attempted with no fresh scout report to act on.
    NoIntel,
}

pub(crate) fn active_voyages(game: &Game, uuid: &str) -> usize {
    game.voyages
        .iter()
        .filter(|v| v.owner_uuid == uuid && !v.resolved)
        .count()
}

/// Captains a scout may be *offered* against. A collected report unlocks a raid, so the pool is
/// narrowed to isles that will still be raidable — otherwise the menu hands out dead-end intel.
/// [`validate_launch`] stays deliberately permissive for scouts; this only shapes the roll.
pub(crate) fn valid_scout_targets(game: &Game, uuid: &str, now: i64) -> Vec<(String, String)> {
    game.players
        .iter()
        .filter(|(other, p)| {
            other.as_str() != uuid && !p.parked && !p.shielded(now) && !p.licking_wounds(now)
        })
        .map(|(other, p)| (other.clone(), p.nick_cache.clone()))
        .collect()
}

/// Roll the PM menu's voyage options: `count` distinct NPC missions, plus a scout option when a
/// valid target exists.
///
/// The menu deliberately offers no raid. Raids are reached one of two ways — a collected scout
/// report (silent, rolled target) or a public `!raid <nick>` declaration (loud, costly). A free
/// raid in the menu would make scouting pointless.
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
    if let Some((target_uuid, target_nick)) = rng.choice(&valid_scout_targets(game, uuid, now)) {
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
            if defender.parked {
                return Err(LaunchError::TargetParked);
            }
            if defender.shielded(now) {
                return Err(LaunchError::TargetShielded);
            }
            // Applies to both raid routes. A public declaration that ignored the mercy window
            // would just move the pile-on from the random roll to the channel.
            if defender.licking_wounds(now) {
                return Err(LaunchError::TargetRecentlyRaided);
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
        } else if defender.parked {
            return Err(LaunchError::TargetParked);
        }
    }
    Ok(())
}

/// A voyage that has just cast off.
pub(crate) struct Launched {
    /// Duration in seconds, for the scheduler.
    pub(crate) secs: i64,
    /// The nick this departure should appear under, when a false flag was flown. The voyage still
    /// records the true owner — this only changes what onlookers see leave the harbour.
    pub(crate) flown_as: Option<String>,
}

/// Deduct crew and record the voyage. Call only after [`validate_launch`] passed; the caller
/// schedules the `voyage:` job and announces the departure. Crew split: regular first, loyal only
/// for the remainder.
///
/// A held false flag is spent here, but only on a voyage that slips out quietly. A public `!raid`
/// declaration names you in the channel by definition, so flying colours you have paid for would
/// be wasted — the flag keeps until there is a departure worth disguising.
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
) -> Launched {
    let (shipyard, false_flag_nick, regular_used, loyal_used) = {
        let player = game.players.get_mut(uuid).expect("validated");
        let regular_used = crew.min(player.home_regular());
        let loyal_used = crew - regular_used;
        player.crew_regular -= regular_used;
        player.crew_loyal -= loyal_used;
        let flag = if is_public {
            None
        } else {
            player.false_flag.take().map(|flag| flag.nick)
        };
        (
            crate::buildings::shipyard_speed(&player.buildings),
            flag,
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
        false_flag_nick: false_flag_nick.clone(),
        started_at: now,
        returns_at: now + secs,
        resolved: false,
        collected: false,
        result: None,
    });
    Launched {
        secs,
        flown_as: false_flag_nick,
    }
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
    /// A raid was already at sea when its target entered absence mode.
    RaidCancelled {
        owner_nick: String,
        target_nick: String,
    },
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
    if voyage.kind == VoyageKind::Raid
        && voyage
            .target_uuid
            .as_deref()
            .and_then(|target| game.players.get(target))
            .is_some_and(|player| player.parked)
    {
        let owner_nick = game
            .players
            .get(&voyage.owner_uuid)
            .map(|player| player.nick_cache.clone())
            .unwrap_or_default();
        let target_nick = voyage
            .target_uuid
            .as_deref()
            .and_then(|target| game.players.get(target))
            .map(|player| player.nick_cache.clone())
            .unwrap_or_default();
        combat::return_home(game, &voyage, false);
        if let Some(stored) = game.voyages.iter_mut().find(|v| v.id == voyage.id) {
            stored.collected = true;
            stored.result = Some(VoyageResult::default());
        }
        return Some(Resolution::RaidCancelled {
            owner_nick,
            target_nick,
        });
    }
    match voyage.kind {
        VoyageKind::Raid => Some(combat::resolve_raid(game, &voyage, rng, settings, now)),
        VoyageKind::Scout => Some(combat::resolve_scout(
            game,
            &voyage,
            rng,
            settings.scout_intel_hours,
            now,
        )),
        kind => Some(resolve_npc(game, &voyage, kind, rng)),
    }
}

pub(crate) fn resolve_npc(
    game: &mut Game,
    voyage: &Voyage,
    kind: VoyageKind,
    rng: &mut Rng,
) -> Resolution {
    // The owner can vanish mid-voyage (data deletion). Fizzle the way the raid path does rather
    // than trapping the whole guest call.
    if !game.players.contains_key(&voyage.owner_uuid) {
        combat::return_home(game, voyage, true);
        return Resolution::Fizzled {
            owner_uuid: voyage.owner_uuid.clone(),
            owner_nick: String::new(),
        };
    }
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
            ..Default::default()
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
    pub reports: Vec<VoyageReport>,
    /// Gold actually banked after an active navy blockade halved it.
    pub halved: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VoyageReport {
    pub id: u64,
    pub kind: VoyageKind,
    pub gold: i64,
    pub rum: i64,
    pub new_crew: i64,
    pub crew_lost: i64,
    pub raid: Option<crate::model::RaidResult>,
    pub scout: Option<crate::model::ScoutResult>,
    pub fizzled: bool,
}

impl VoyageReport {
    pub(crate) fn from_voyage(voyage: &Voyage) -> Self {
        let result = voyage.result.clone().unwrap_or_default();
        Self {
            id: voyage.id,
            kind: voyage.kind,
            gold: result.gold,
            rum: result.rum,
            new_crew: result.new_crew,
            crew_lost: result.crew_lost,
            raid: result.raid,
            scout: result.scout,
            fizzled: result.fizzled,
        }
    }

    pub(crate) fn mission(&self) -> &'static str {
        voyage_def(self.kind).name
    }

    /// Safe for channel output: scout details are deliberately omitted.
    pub(crate) fn public_summary(&self) -> String {
        if self.fizzled {
            return format!("{} #{} returned empty-handed", self.mission(), self.id);
        }
        if let Some(raid) = &self.raid {
            return format!(
                "Raid on {}: {}; +{}g, {} crew lost, {} captured",
                raid.target_nick, raid.outcome, self.gold, self.crew_lost, raid.prisoners_lost
            );
        }
        if let Some(scout) = &self.scout {
            return format!(
                "Scout of {} returned; the report was sent privately",
                scout.target_nick
            );
        }
        format!(
            "{} #{}: +{}g, +{} rum, +{} regular crew, {} crew lost",
            self.mission(),
            self.id,
            self.gold,
            self.rum,
            self.new_crew,
            self.crew_lost
        )
    }

    pub(crate) fn pending_summary(&self) -> String {
        if self.fizzled {
            format!("{} #{} (empty-handed)", self.mission(), self.id)
        } else if let Some(raid) = &self.raid {
            format!("Raid on {} #{}", raid.target_nick, self.id)
        } else if let Some(scout) = &self.scout {
            format!("Scout of {} #{}", scout.target_nick, self.id)
        } else {
            format!("{} #{}", self.mission(), self.id)
        }
    }
}

/// Claim all resolved, uncollected voyages: bank the loot (halved under a navy blockade), press
/// new crew, and prune the collected voyages from the game. Pure.
///
/// Collecting a scout report also arms the raid it unlocks (`intel_hours` of freshness). The
/// scout target was rolled, never chosen, so this is the only route to a stealth raid — being
/// raided is luck of the draw rather than someone deciding they dislike you.
pub(crate) fn collect_pending(
    game: &mut Game,
    uuid: &str,
    intel_hours: i64,
    now: i64,
) -> CollectSummary {
    let blockaded = game.players.get(uuid).is_some_and(|p| p.blockaded(now));
    let mut summary = CollectSummary::default();
    let mut done = Vec::new();
    for voyage in game.voyages.iter_mut() {
        if voyage.owner_uuid != uuid || !voyage.resolved || voyage.collected {
            continue;
        }
        summary.count += 1;
        if let Some(result) = &voyage.result {
            summary.gold += result.gold;
            summary.rum += result.rum;
            summary.new_crew += result.new_crew;
        }
        summary.reports.push(VoyageReport::from_voyage(voyage));
        voyage.collected = true;
        done.push(voyage.id);
    }
    if summary.count == 0 {
        return summary;
    }
    if blockaded && summary.gold > 0 {
        summary.gold /= 2;
        for report in &mut summary.reports {
            report.gold /= 2;
        }
        summary.halved = true;
    }
    // The freshest scout report in this batch arms the raid; an older one in the same collect
    // would only overwrite it with staler intel.
    let intel = summary
        .reports
        .iter()
        .filter_map(|report| report.scout.as_ref())
        .rfind(|scout| !scout.target_uuid.is_empty())
        .map(|scout| crate::model::RaidIntel {
            target_uuid: scout.target_uuid.clone(),
            target_nick: scout.target_nick.clone(),
            expires_at: if scout.intel_expires_at > 0 {
                scout.intel_expires_at
            } else {
                now + intel_hours.max(1) * 3_600
            },
        });
    if let Some(player) = game.players.get_mut(uuid) {
        player.gold += summary.gold;
        player.rum += summary.rum;
        player.crew_regular += summary.new_crew;
        // Career totals are booked here, at the moment the voyage is claimed, so `!captain` and
        // the achievement stats track play as it happens rather than jumping at season end.
        player.career_voyages += summary.count as i64;
        player.career_rum_collected += summary.rum.max(0);
        if intel.is_some() {
            player.raid_intel = intel;
        }
    }
    game.voyages.retain(|v| !done.contains(&v.id));
    summary
}

/// Render and deliver one resolution: the public channel summary plus the
/// owner's private details. Scout intel remains private and is delivered when the owner collects.
pub(crate) fn deliver_resolution(
    server: &str,
    channel: &str,
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
                channel,
                &themed(
                    "pirate.voyage_return_channel",
                    &["⚓ {user}'s {mission} returned: {loot}; {lost} crew lost. Use !collect to claim the spoils."],
                    &[
                        ("user", owner_nick),
                        ("mission", voyage_def(*kind).name),
                        ("loot", &loot),
                        ("lost", &lost),
                    ],
                )?,
            )?;
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
        Resolution::Raid(report) => combat::deliver_raid_report(server, channel, report)?,
        Resolution::Scout(report) => {
            reply(
                server,
                channel,
                &themed(
                    "pirate.scout_return_channel",
                    &["⚓ {user}'s scout returned; the report was sent privately."],
                    &[("user", &report.owner_nick)],
                )?,
            )?;
            combat::deliver_scout_snapshot(server, &report.owner_nick, &report.result)?;
        }
        Resolution::RaidCancelled {
            owner_nick,
            target_nick,
        } => {
            reply(
                server,
                channel,
                &themed(
                    "pirate.raid_cancelled_channel",
                    &[
                        "⚓ {target}'s isle is out of the world; {user}'s raid was called off and the crew returned.",
                    ],
                    &[("target", target_nick), ("user", owner_nick)],
                )?,
            )?;
            reply(
                server,
                owner_nick,
                &themed(
                    "pirate.raid_cancelled",
                    &["Your raid on {target} was called off because that captain entered absence mode. Your crew are home."],
                    &[("target", target_nick)],
                )?,
            )?;
        }
        Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        } => {
            let _ = owner_uuid;
            if owner_nick.is_empty() {
                // The owner is gone; there is nobody to tell and no nick to address.
                return Ok(());
            }
            reply(
                server,
                channel,
                &themed(
                    "pirate.voyage_fizzled_channel",
                    &["⚓ {user}'s voyage returned empty-handed: the isle they sailed for is abandoned."],
                    &[("user", owner_nick)],
                )?,
            )?;
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
    // A disabled game does not tick. A raid resolving here would plunder gold and take prisoners
    // with every announcement suppressed — the victim would never learn it happened. Leaving the
    // voyage unresolved is safe: [`resolve_overdue`] force-resolves it, announcements and all, on
    // the first command after the game is switched back on.
    if !crate::setting_enabled(server, channel) {
        return Ok(());
    }
    let mut state = crate::load_state()?;
    let now = crate::now_secs();
    let settings = crate::pirate_settings(server, channel);
    if state
        .games
        .get(game_key)
        .and_then(|game| game.voyages.iter().find(|voyage| voyage.id == voyage_id))
        .and_then(|voyage| {
            state
                .games
                .get(game_key)
                .and_then(|game| game.players.get(&voyage.owner_uuid))
        })
        .is_some_and(|player| player.parked)
    {
        crate::schedule(
            &crate::voyage_job_id(server, channel, voyage_id),
            server,
            channel,
            None,
            now + 3600,
            "",
        )?;
        return Ok(());
    }
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
        deliver_resolution(server, channel, &resolution)?;
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
/// just clears it early so `!crew` stops showing the cove note. Idempotent.
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
    loop {
        let due = state.games.get(game_key).and_then(|game| {
            game.voyages
                .iter()
                .find(|v| {
                    !v.resolved
                        && v.returns_at <= now
                        && game
                            .players
                            .get(&v.owner_uuid)
                            .is_none_or(|player| !player.parked)
                })
                .map(|v| v.id)
        });
        let Some(voyage_id) = due else { break };
        let resolution = {
            let game = state.games.get_mut(game_key).expect("checked above");
            resolve_voyage(game, voyage_id, &mut crate::rng()?, settings, now)
        };
        if let Some(resolution) = resolution {
            crate::save_state(state)?;
            deliver_resolution(server, channel, &resolution)?;
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
        game.players.get_mut("b").unwrap().parked = true;
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Raid, Some("b"), 1, &settings, now),
            Err(LaunchError::TargetParked)
        );
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Scout, Some("b"), 1, &settings, now),
            Err(LaunchError::TargetParked)
        );
    }

    #[test]
    fn a_false_flag_is_flown_by_quiet_voyages_and_kept_by_public_ones() {
        use crate::model::FalseFlag;
        let flagged = || {
            let mut game = game_with_two();
            game.players.get_mut("a").unwrap().false_flag = Some(FalseFlag { nick: "Bob".into() });
            game
        };

        // A quiet departure flies the colours and spends the flag.
        let mut game = flagged();
        let launched = launch(
            &mut game,
            1,
            "a",
            VoyageKind::Merchant,
            None,
            2,
            false,
            1_000,
            &mut Rng::new(1),
        );
        assert_eq!(launched.flown_as.as_deref(), Some("Bob"));
        assert_eq!(game.voyages[0].false_flag_nick.as_deref(), Some("Bob"));
        assert!(game.players["a"].false_flag.is_none(), "spent");
        assert_eq!(
            game.voyages[0].owner_uuid, "a",
            "the disguise never touches the true owner"
        );

        // A public declaration names the attacker anyway, so the flag is not wasted on it.
        let mut game = flagged();
        let launched = launch(
            &mut game,
            2,
            "a",
            VoyageKind::Raid,
            Some("b".into()),
            2,
            true,
            1_000,
            &mut Rng::new(1),
        );
        assert!(launched.flown_as.is_none());
        assert!(game.voyages[0].false_flag_nick.is_none());
        assert!(
            game.players["a"].false_flag.is_some(),
            "the flag keeps for a departure worth disguising"
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
                ..Default::default()
            }),
            ..Default::default()
        });
        let summary = collect_pending(&mut game, "a", 12, 1_000);
        assert_eq!(summary.count, 1);
        assert_eq!(summary.gold, 80);
        assert_eq!(summary.rum, 2);
        assert_eq!(summary.new_crew, 1);
        assert_eq!(summary.reports[0].new_crew, 1);
        assert!(summary.reports[0]
            .public_summary()
            .contains("+1 regular crew"));
        let player = &game.players["a"];
        assert_eq!(player.gold, 580);
        assert_eq!(player.rum, 2);
        assert_eq!(player.crew_regular, 6);
        assert!(game.voyages.is_empty(), "collected voyage pruned");
        assert_eq!(collect_pending(&mut game, "a", 12, 1_000).count, 0);
    }

    #[test]
    fn collecting_books_career_totals_as_play_happens() {
        let mut game = game_with_two();
        for id in 1..=2 {
            game.voyages.push(Voyage {
                id,
                owner_uuid: "a".into(),
                kind: VoyageKind::Rum,
                resolved: true,
                result: Some(VoyageResult {
                    rum: 5,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        collect_pending(&mut game, "a", 12, 1_000);
        let player = &game.players["a"];
        assert_eq!(
            player.career_voyages, 2,
            "booked at collect, not season end"
        );
        assert_eq!(player.career_rum_collected, 10);
        // A second collect with nothing waiting must not inflate the totals.
        collect_pending(&mut game, "a", 12, 1_000);
        assert_eq!(game.players["a"].career_voyages, 2);
    }

    #[test]
    fn collecting_a_scout_report_arms_a_raid_on_that_isle() {
        let mut game = game_with_two();
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "a".into(),
            kind: VoyageKind::Scout,
            resolved: true,
            result: Some(VoyageResult {
                scout: Some(crate::model::ScoutResult {
                    target_uuid: "b".into(),
                    target_nick: "Bob".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
        collect_pending(&mut game, "a", 12, 1_000);
        let intel = game.players["a"].fresh_intel(1_000).cloned().unwrap();
        assert_eq!(intel.target_uuid, "b");
        assert_eq!(intel.expires_at, 1_000 + 12 * 3_600);
        // And it goes cold on schedule.
        assert!(game.players["a"].fresh_intel(1_000 + 12 * 3_600).is_none());
    }

    #[test]
    fn a_recently_raided_isle_leaves_the_target_pools_and_refuses_raids() {
        let settings = PirateSettings::default();
        let now = 10_000_i64;
        let mut game = game_with_two();
        game.players.get_mut("b").unwrap().raid_mercy_until = now + 3_600;

        assert!(
            valid_scout_targets(&game, "a", now).is_empty(),
            "Bob is out of the scout roll, so no raid can be unlocked against him"
        );
        // The public declaration route is bound by the same window, or the pile-on just moves
        // from the random roll to the channel.
        assert_eq!(
            validate_launch(&game, "a", VoyageKind::Raid, Some("b"), 1, &settings, now),
            Err(LaunchError::TargetRecentlyRaided)
        );
        // Once it lapses, Bob is fair game again.
        let later = now + 3_601;
        assert_eq!(valid_scout_targets(&game, "a", later).len(), 1);
        assert!(
            validate_launch(&game, "a", VoyageKind::Raid, Some("b"), 1, &settings, later).is_ok()
        );
    }

    #[test]
    fn a_voyage_whose_owner_vanished_fizzles_instead_of_trapping() {
        let mut game = game_with_two();
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "ghost".into(),
            kind: VoyageKind::Merchant,
            crew_regular: 2,
            ..Default::default()
        });
        let voyage = game.voyages[0].clone();
        let resolution = resolve_npc(&mut game, &voyage, VoyageKind::Merchant, &mut Rng::new(1));
        assert!(matches!(resolution, Resolution::Fizzled { .. }));
        assert!(
            game.voyages[0].resolved,
            "and the voyage is not left at sea"
        );
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
        let summary = collect_pending(&mut game, "a", 12, 1_000);
        assert!(summary.halved);
        assert_eq!(summary.gold, 40);
        assert_eq!(summary.rum, 3, "only gold income is halved");
    }
}
