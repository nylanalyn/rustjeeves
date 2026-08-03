//! Raid combat math (PLAN §7). `resolve_combat` is pure and unit-tested; `apply_raid` mutates
//! game state and returns a report the caller turns into themed messages. `resolve_raid` /
//! `resolve_scout` plug raids and scouts into the voyage-resolution path.

use crate::buildings;
use crate::model::{Buildings, Game, Player, Prisoner, RaidResult, Voyage, VoyageResult};
use crate::{reply, themed, PirateSettings, Rng};
use extism_pdk::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    CrushingVictory,
    Victory,
    Defeat,
    CrushingDefeat,
}

impl Outcome {
    pub(crate) fn attacker_won(self) -> bool {
        matches!(self, Outcome::CrushingVictory | Outcome::Victory)
    }
    pub(crate) fn note(self) -> &'static str {
        match self {
            Outcome::CrushingVictory => "crushing_victory",
            Outcome::Victory => "victory",
            Outcome::Defeat => "defeat",
            Outcome::CrushingDefeat => "crushing_defeat",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CombatSpec {
    pub(crate) attack_crew: i64,
    /// Visible defenders (regular + available loyal minus cove-hidden).
    pub(crate) defense_visible: i64,
    /// Cove-hidden defenders, worth +2 power each as a surprise.
    pub(crate) defense_hidden: i64,
    /// The defender's buildings (walls/tavern/vault feed power and protection).
    pub(crate) buildings: Buildings,
    pub(crate) defender_gold: i64,
    pub(crate) attacker_humiliated: bool,
    pub(crate) defender_unpaid_days: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CombatResult {
    pub(crate) outcome: Outcome,
    pub(crate) attack_power: i64,
    pub(crate) defense_power: i64,
    pub(crate) loot_gold: i64,
    /// Attacking regular crew that do not come home (dead or captured).
    pub(crate) attacker_crew_lost: i64,
    /// Of those, how many the defender captures (Defeat / Crushing Defeat).
    pub(crate) attacker_crew_captured: i64,
    /// Defender salvage on a successful defense.
    pub(crate) salvage_gold: i64,
    /// Notoriety the defender gains (Crushing Defeat only).
    pub(crate) defender_notoriety: i64,
    /// Who receives the Humiliated debuff, if anyone.
    pub(crate) humiliated: Humiliated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Humiliated {
    Nobody,
    Attacker,
    Defender,
}

pub(crate) fn attack_power(spec: &CombatSpec, roll: f64) -> i64 {
    let base = spec.attack_crew as f64 * 10.0 * roll;
    let penalized = if spec.attacker_humiliated {
        base * 0.9
    } else {
        base
    };
    penalized.round() as i64
}

pub(crate) fn defense_power(spec: &CombatSpec, roll: f64, disloyal_penalty_pct: i64) -> i64 {
    let base = spec.defense_visible as f64 * 10.0 * roll;
    let bonus = buildings::walls_bonus(&spec.buildings)
        + buildings::tavern_bonus(&spec.buildings)
        + spec.defense_hidden as f64 * 2.0;
    let morale = (spec.defender_unpaid_days as i64 * disloyal_penalty_pct).min(25);
    ((base + bonus) * (100 - morale) as f64 / 100.0).round() as i64
}

/// Fraction of gold the Vault protects (L1 50%, L2 75%).
pub(crate) fn vault_protected_pct(vault: u8) -> i64 {
    match vault {
        0 => 0,
        1 => 50,
        _ => 75,
    }
}

/// Gold a raid can actually reach: total minus Vault protection.
pub(crate) fn vulnerable_gold(total: i64, vault: u8) -> i64 {
    total * (100 - vault_protected_pct(vault)) / 100
}

pub(crate) fn classify(attack: i64, defense: i64) -> Outcome {
    if attack as f64 > defense as f64 * 1.5 {
        Outcome::CrushingVictory
    } else if attack > defense && attack.saturating_mul(2) < defense.saturating_mul(3) {
        Outcome::Victory
    } else if (attack as f64) < defense as f64 * 0.5 {
        Outcome::CrushingDefeat
    } else {
        Outcome::Defeat
    }
}

/// Full combat resolution. Crew-loss rolls come from `rng`; power rolls too.
pub(crate) fn resolve_combat(
    spec: &CombatSpec,
    settings: &PirateSettings,
    rng: &mut Rng,
) -> CombatResult {
    let attack = attack_power(spec, rng.range(0.8, 1.2));
    let defense = defense_power(
        spec,
        rng.range(0.8, 1.2),
        settings.disloyal_scout_penalty_pct,
    );
    // Zero defenders: automatic (non-crushing) victory.
    let outcome = if spec.defense_visible + spec.defense_hidden == 0 {
        Outcome::Victory
    } else {
        classify(attack, defense)
    };
    let vulnerable = vulnerable_gold(spec.defender_gold, spec.buildings.vault);
    let mut result = CombatResult {
        outcome,
        attack_power: attack,
        defense_power: defense,
        loot_gold: 0,
        attacker_crew_lost: 0,
        attacker_crew_captured: 0,
        salvage_gold: 0,
        defender_notoriety: 0,
        humiliated: Humiliated::Nobody,
    };
    match outcome {
        Outcome::CrushingVictory => {
            result.loot_gold = vulnerable * settings.raid_gold_pct_crushing / 100;
            result.attacker_crew_lost = rng.between(0, 1);
            result.humiliated = Humiliated::Defender;
        }
        Outcome::Victory => {
            result.loot_gold = vulnerable * settings.raid_gold_pct_victory / 100;
            result.attacker_crew_lost = rng.between(1, 2);
        }
        Outcome::Defeat => {
            let lost = (spec.attack_crew * settings.crew_loss_pct_defeat + 99) / 100;
            result.attacker_crew_lost = lost.max(1).min(spec.attack_crew);
            result.attacker_crew_captured = result.attacker_crew_lost;
            result.salvage_gold = 50 * result.attacker_crew_captured;
        }
        Outcome::CrushingDefeat => {
            result.attacker_crew_lost = spec.attack_crew;
            result.attacker_crew_captured = spec.attack_crew;
            result.salvage_gold = 200;
            result.defender_notoriety = 10;
            result.humiliated = Humiliated::Attacker;
        }
    }
    result.attacker_crew_lost = result.attacker_crew_lost.min(spec.attack_crew);
    result.attacker_crew_captured = result.attacker_crew_captured.min(result.attacker_crew_lost);
    result
}

#[derive(Debug, Clone)]
pub(crate) struct RaidReport {
    pub(crate) outcome: Outcome,
    pub(crate) attack_power: i64,
    pub(crate) defense_power: i64,
    /// Gold stolen from the defender, waiting for the attacker's `!collect`.
    pub(crate) loot_gold: i64,
    /// Attacking regular crew lost (dead or captured).
    pub(crate) crew_lost: i64,
    /// Of those, crew the defender took prisoner.
    pub(crate) crew_captured: i64,
    pub(crate) salvage_gold: i64,
    pub(crate) attacker_uuid: String,
    pub(crate) defender_uuid: String,
    pub(crate) attacker_nick: String,
    pub(crate) defender_nick: String,
    /// Some(real attacker nick) when a false flag was flown and revealed on arrival.
    pub(crate) false_flag_reveal: Option<String>,
    /// Crimson Archipelago: both islands get a navy visit within 24h.
    pub(crate) navy_alert: bool,
    /// The defender's loyal crew retreated to the cove (attacker won, loyal crew present).
    pub(crate) loyal_retreated: bool,
}

impl RaidReport {
    pub(crate) fn attacker_won(&self) -> bool {
        self.outcome.attacker_won()
    }
    pub(crate) fn defender_won(&self) -> bool {
        !self.outcome.attacker_won()
    }
}

/// Home defense split: visible crew vs cove-hidden. Cove crew still fight with their +2
/// surprise in combat even when scouts cannot see them.
pub(crate) fn defense_split(player: &Player, now: i64) -> (i64, i64) {
    let total = player.crew_regular.max(0) + player.home_loyal(now);
    let hidden = buildings::cove_hides(&player.buildings).min(total);
    (total - hidden, hidden)
}

/// Apply a raid voyage's arrival to the game: combat, gold deduction from the defender,
/// prisoners, careers, debuffs. The stolen gold itself is returned in the report and banked by
/// the attacker at `!collect`; crew return is handled by [`resolve_raid`].
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_raid(
    game: &mut Game,
    attacker_uuid: &str,
    defender_uuid: &str,
    crew_sent: i64,
    crew_regular_sent: i64,
    false_flag_nick: Option<&str>,
    sea: &str,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
    prisoner_id: u64,
) -> Option<RaidReport> {
    let (visible, hidden) = defense_split(game.players.get(defender_uuid)?, now);
    let spec = CombatSpec {
        attack_crew: crew_sent,
        defense_visible: visible,
        defense_hidden: hidden,
        buildings: game.players.get(defender_uuid)?.buildings.clone(),
        defender_gold: game.players.get(defender_uuid)?.gold,
        attacker_humiliated: game
            .players
            .get(attacker_uuid)
            .is_some_and(|p| now < p.humiliated_until),
        defender_unpaid_days: game.players.get(defender_uuid)?.unpaid_days,
    };
    let result = resolve_combat(&spec, settings, rng);
    // Losses and capture only ever hit regular crew; loyal crew always come home.
    let crew_lost = result.attacker_crew_lost.min(crew_regular_sent);
    let crew_captured = result.attacker_crew_captured.min(crew_lost);
    let attacker_blockaded = game
        .players
        .get(attacker_uuid)
        .is_some_and(|p| now < p.navy_blockade_until);
    let loot = if attacker_blockaded {
        result.loot_gold / 2
    } else {
        result.loot_gold
    };

    let attacker_nick = game.players.get(attacker_uuid)?.nick_cache.clone();
    let defender_nick = game.players.get(defender_uuid)?.nick_cache.clone();
    let false_flag_reveal = false_flag_nick.map(|_| attacker_nick.clone());

    // The defender's gold is deducted at resolution; the attacker banks it on `!collect`.
    if loot > 0 {
        if let Some(defender) = game.players.get_mut(defender_uuid) {
            defender.gold = (defender.gold - loot).max(0);
        }
        if let Some(attacker) = game.players.get_mut(attacker_uuid) {
            attacker.career_gold_plundered += loot.max(0);
        }
    }
    // Careers, notoriety, debuffs.
    let mut loyal_retreated = false;
    if result.outcome.attacker_won() {
        if let Some(attacker) = game.players.get_mut(attacker_uuid) {
            attacker.career_raids_won += 1;
            attacker.season_raids_won += 1;
            attacker.career_crew_lost += crew_lost;
        }
        if let Some(defender) = game.players.get_mut(defender_uuid) {
            defender.season_breaches += 1;
            if defender.crew_loyal > 0 {
                defender.loyal_cove_until = now + settings.loyal_cove_cooldown_hours * 3600;
                loyal_retreated = true;
            }
        }
    } else {
        if let Some(defender) = game.players.get_mut(defender_uuid) {
            defender.career_defenses_won += 1;
            defender.season_defenses_won += 1;
            defender.notoriety += result.defender_notoriety;
            defender.gold += result.salvage_gold;
            defender.career_prisoners_taken += crew_captured;
        }
        if let Some(attacker) = game.players.get_mut(attacker_uuid) {
            attacker.career_crew_lost += crew_lost;
        }
        if crew_captured > 0 && game.prisoners.len() < crate::model::MAX_PRISONERS {
            game.prisoners.push(Prisoner {
                id: prisoner_id,
                holder_uuid: defender_uuid.to_string(),
                origin_uuid: attacker_uuid.to_string(),
                count: crew_captured,
                captured_at: now,
            });
        }
    }
    match result.humiliated {
        Humiliated::Attacker => {
            if let Some(attacker) = game.players.get_mut(attacker_uuid) {
                attacker.humiliated_until = now + settings.humiliated_debuff_hours * 3600;
                attacker.notoriety -= 2;
            }
        }
        Humiliated::Defender => {
            if let Some(defender) = game.players.get_mut(defender_uuid) {
                defender.humiliated_until = now + settings.humiliated_debuff_hours * 3600;
                defender.notoriety -= 2;
            }
        }
        Humiliated::Nobody => {}
    }

    Some(RaidReport {
        outcome: result.outcome,
        attack_power: result.attack_power,
        defense_power: result.defense_power,
        loot_gold: loot,
        crew_lost,
        crew_captured,
        salvage_gold: result.salvage_gold,
        attacker_uuid: attacker_uuid.to_string(),
        defender_uuid: defender_uuid.to_string(),
        attacker_nick,
        defender_nick,
        false_flag_reveal,
        navy_alert: sea == "crimson",
        loyal_retreated,
    })
}

/// Resolve a raid voyage's arrival: combat, crew return, stored result. Returns `None` only when
/// the voyage itself is unknown (handled by the caller).
pub(crate) fn resolve_raid(
    game: &mut Game,
    voyage: &Voyage,
    rng: &mut Rng,
    settings: &PirateSettings,
    now: i64,
) -> crate::voyage::Resolution {
    let owner_uuid = voyage.owner_uuid.clone();
    let owner_nick = game
        .players
        .get(&owner_uuid)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_default();
    let Some(defender_uuid) = voyage.target_uuid.clone() else {
        return_home(game, voyage);
        return crate::voyage::Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        };
    };
    if !game.players.contains_key(&defender_uuid) || !game.players.contains_key(&owner_uuid) {
        // The target (or the attacker) vanished mid-voyage; crew drift home.
        return_home(game, voyage);
        return crate::voyage::Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        };
    }
    let sea = game.sea.clone();
    let report = apply_raid(
        game,
        &owner_uuid,
        &defender_uuid,
        voyage.crew_sent(),
        voyage.crew_regular,
        voyage.false_flag_nick.as_deref(),
        &sea,
        settings,
        now,
        rng,
        voyage.id,
    )
    .expect("both players checked above");
    // Surviving regular crew and every loyal crew sail home.
    let regular_back = (voyage.crew_regular - report.crew_lost).max(0);
    if let Some(attacker) = game.players.get_mut(&owner_uuid) {
        attacker.crew_regular += regular_back;
        attacker.crew_loyal += voyage.crew_loyal;
    }
    let target_nick = report.defender_nick.clone();
    if let Some(v) = game.voyages.iter_mut().find(|v| v.id == voyage.id) {
        v.resolved = true;
        v.result = Some(VoyageResult {
            gold: report.loot_gold,
            rum: 0,
            new_crew: 0,
            crew_lost: report.crew_lost,
            raid: Some(RaidResult {
                outcome: report.outcome.note().into(),
                target_uuid: defender_uuid,
                target_nick,
                prisoners_lost: report.crew_captured,
            }),
        });
    }
    crate::voyage::Resolution::Raid(Box::new(report))
}

/// Return every crew of a voyage to its owner and mark it resolved with no loot.
fn return_home(game: &mut Game, voyage: &Voyage) {
    if let Some(owner) = game.players.get_mut(&voyage.owner_uuid) {
        owner.crew_regular += voyage.crew_regular;
        owner.crew_loyal += voyage.crew_loyal;
    }
    if let Some(v) = game.voyages.iter_mut().find(|v| v.id == voyage.id) {
        v.resolved = true;
        v.result = Some(VoyageResult::default());
    }
}

/// Intel snapshot PM'd to a scout on return.
#[derive(Debug, Clone)]
pub(crate) struct ScoutReport {
    pub(crate) owner_nick: String,
    pub(crate) target_nick: String,
    pub(crate) visible_crew: i64,
    pub(crate) approx_gold: i64,
    pub(crate) buildings: String,
    pub(crate) low_morale: bool,
    /// Shattered Reef: the cove failed to hide crew this time.
    pub(crate) leaked: bool,
}

/// Intel snapshot contents. Returns (visible crew, approx gold, buildings summary, morale note,
/// hidden-leak note). Pure and unit-tested.
pub(crate) fn scout_intel(
    target: &Player,
    sea: &str,
    rng: &mut Rng,
) -> (i64, i64, String, bool, bool) {
    let total = target.crew_regular.max(0) + target.crew_loyal.max(0);
    let mut hidden = buildings::cove_hides(&target.buildings).min(total);
    let mut leaked = false;
    if sea == "shattered_reef" && hidden > 0 && rng.f64() < 0.5 {
        hidden = 0;
        leaked = true;
    }
    let visible = total - hidden;
    // Approximate gold: ±10% jitter, rounded to tens.
    let jitter = 0.9 + rng.f64() * 0.2;
    let approx = ((target.gold as f64 * jitter) / 10.0).round() as i64 * 10;
    let low_morale = target.unpaid_days > 0;
    (
        visible,
        approx.max(0),
        buildings::describe(&target.buildings),
        low_morale,
        leaked,
    )
}

/// Resolve a scout voyage's arrival: intel snapshot, crew home, stored (empty) result.
pub(crate) fn resolve_scout(
    game: &mut Game,
    voyage: &Voyage,
    rng: &mut Rng,
    now: i64,
) -> crate::voyage::Resolution {
    let owner_uuid = voyage.owner_uuid.clone();
    let owner_nick = game
        .players
        .get(&owner_uuid)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_default();
    let Some(target_uuid) = voyage.target_uuid.clone() else {
        return_home(game, voyage);
        return crate::voyage::Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        };
    };
    let sea = game.sea.clone();
    let Some(target) = game.players.get(&target_uuid) else {
        return_home(game, voyage);
        return crate::voyage::Resolution::Fizzled {
            owner_uuid,
            owner_nick,
        };
    };
    let (visible_crew, approx_gold, buildings_summary, low_morale, leaked) =
        scout_intel(target, &sea, rng);
    let target_nick = target.nick_cache.clone();
    let _ = now;
    return_home(game, voyage);
    crate::voyage::Resolution::Scout(Box::new(ScoutReport {
        owner_nick,
        target_nick,
        visible_crew,
        approx_gold,
        buildings: buildings_summary,
        low_morale,
        leaked,
    }))
}

/// Public raid resolution: one themed channel line (when the game is still enabled), the false
/// flag reveal when there was one, and a PM to the attacker with their personal outcome.
pub(crate) fn deliver_raid_report(
    server: &str,
    channel: &str,
    enabled: bool,
    report: &RaidReport,
) -> Result<(), Error> {
    if enabled {
        if let Some(real) = &report.false_flag_reveal {
            reply(
                server,
                channel,
                &themed(
                    "pirate.false_flag_reveal",
                    &["Wait... those are {attacker}'s colors! FALSE FLAG!"],
                    &[("attacker", real)],
                )?,
            )?;
        }
        let (key, default): (&str, &str) = match report.outcome {
            Outcome::CrushingDefeat => (
                "pirate.raid_crushing_defense",
                "💥 {attacker}'s fleet descends on {defender}'s isle! ⚔️ CRUSHING DEFENSE! {defender}'s fortress obliterated the raid — {attacker} lost ALL {lost} crew (captured!). {defender} salvaged {salvage}g and gains 10 Notoriety. {attacker} is Humiliated (-2 Notoriety, -10% attack for 24h).",
            ),
            Outcome::Defeat => (
                "pirate.raid_defender_wins",
                "💥 {attacker}'s fleet descends on {defender}'s isle! ({attack} vs {defense}) 🛡️ {defender} WINS! {attacker} loses {lost} crew — {defender} captures {captured} prisoners and salvages {salvage}g.",
            ),
            _ => (
                "pirate.raid_attacker_wins",
                "💥 {attacker}'s fleet descends on {defender}'s isle! ({attack} vs {defense}) ⚔️ {attacker} WINS! {attacker} plunders {loot}g; {defender} is left counting the damage.",
            ),
        };
        reply(
            server,
            channel,
            &themed(
                key,
                &[default],
                &[
                    ("attacker", &report.attacker_nick),
                    ("defender", &report.defender_nick),
                    ("attack", &report.attack_power.to_string()),
                    ("defense", &report.defense_power.to_string()),
                    ("lost", &report.crew_lost.to_string()),
                    ("captured", &report.crew_captured.to_string()),
                    ("salvage", &report.salvage_gold.to_string()),
                    ("loot", &report.loot_gold.to_string()),
                ],
            )?,
        )?;
    }
    // The attacker always gets a private outcome line with their collect hint.
    let (key, default): (&str, &str) = if report.attacker_won() {
        (
            "pirate.raid_return_won",
            "Your raid on {defender}'s isle succeeded! Plunder: {loot}g. Crew lost: {lost}. Use !collect in the channel to claim your spoils.",
        )
    } else {
        (
            "pirate.raid_return_lost",
            "Your raid on {defender}'s isle was repelled! Crew lost: {lost} ({captured} captured). No spoils this time.",
        )
    };
    reply(
        server,
        &report.attacker_nick,
        &themed(
            key,
            &[default],
            &[
                ("defender", &report.defender_nick),
                ("loot", &report.loot_gold.to_string()),
                ("lost", &report.crew_lost.to_string()),
                ("captured", &report.crew_captured.to_string()),
            ],
        )?,
    )?;
    Ok(())
}

/// PM the scout their intel snapshot.
pub(crate) fn deliver_scout_report(server: &str, report: &ScoutReport) -> Result<(), Error> {
    let mut notes = Vec::new();
    if report.low_morale {
        notes.push("Tavern talk suggests morale is low. Some crew might not fight to the death.");
    }
    if report.leaked {
        notes.push("The reef betrayed them — even the cove could not hide their crew.");
    }
    let note = if notes.is_empty() {
        "Cove may hide additional crew.".to_string()
    } else {
        notes.join(" ")
    };
    reply(
        server,
        &report.owner_nick,
        &themed(
            "pirate.scout_report",
            &["{target}'s isle (as of ~2 hours ago): Visible crew: {crew}. Gold: ~{gold}g. Buildings: {buildings}. Note: {note}"],
            &[
                ("target", &report.target_nick),
                ("crew", &report.visible_crew.to_string()),
                ("gold", &report.approx_gold.to_string()),
                ("buildings", &report.buildings),
                ("note", &note),
            ],
        )?,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> CombatSpec {
        CombatSpec {
            attack_crew: 5,
            defense_visible: 2,
            defense_hidden: 0,
            buildings: Buildings {
                cove: 0,
                ..Default::default()
            },
            defender_gold: 1000,
            attacker_humiliated: false,
            defender_unpaid_days: 0,
        }
    }

    fn settings() -> PirateSettings {
        PirateSettings::default()
    }

    #[test]
    fn outcome_bands_classify_correctly() {
        assert_eq!(classify(151, 100), Outcome::CrushingVictory);
        assert_eq!(
            classify(150, 100),
            Outcome::Defeat,
            "exactly 1.5x is not crushing"
        );
        assert_eq!(classify(101, 100), Outcome::Victory);
        assert_eq!(
            classify(100, 100),
            Outcome::Defeat,
            "tie goes to the defender"
        );
        assert_eq!(classify(60, 100), Outcome::Defeat);
        assert_eq!(classify(49, 100), Outcome::CrushingDefeat);
        assert_eq!(
            classify(50, 100),
            Outcome::Defeat,
            "exactly 0.5x is a plain defeat"
        );
    }

    #[test]
    fn vault_protects_its_percentage() {
        assert_eq!(vulnerable_gold(1000, 0), 1000);
        assert_eq!(vulnerable_gold(1000, 1), 500);
        assert_eq!(vulnerable_gold(1000, 2), 250);
        let mut s = spec();
        s.buildings.vault = 2;
        let result = resolve_combat(&s, &settings(), &mut Rng::new(3));
        if result.outcome == Outcome::Victory {
            assert_eq!(result.loot_gold, 250 * 15 / 100);
        }
    }

    #[test]
    fn zero_defenders_is_auto_victory_never_crushing() {
        let mut s = spec();
        s.defense_visible = 0;
        s.defense_hidden = 0;
        for seed in 1..50 {
            let result = resolve_combat(&s, &settings(), &mut Rng::new(seed));
            assert_eq!(result.outcome, Outcome::Victory);
            assert_eq!(result.loot_gold, 150, "15% of 1000 vulnerable");
        }
    }

    #[test]
    fn buildings_and_morale_shift_defense_power() {
        let mut s = spec();
        let plain = defense_power(&s, 1.0, 5);
        s.buildings.walls = 2;
        s.buildings.tavern = 1;
        s.defense_hidden = 2;
        let fortified = defense_power(&s, 1.0, 5);
        assert_eq!(fortified - plain, 30 + 5 + 4);
        s.defender_unpaid_days = 10;
        let demoralized = defense_power(&s, 1.0, 5);
        assert_eq!(demoralized, (fortified * 3) / 4, "penalty caps at 25%");
    }

    #[test]
    fn humiliated_attackers_fight_at_ninety_percent() {
        let mut s = spec();
        assert_eq!(attack_power(&s, 1.0), 50);
        s.attacker_humiliated = true;
        assert_eq!(attack_power(&s, 1.0), 45);
    }

    #[test]
    fn defeat_loses_half_rounded_up_and_crushing_defeat_loses_all() {
        let mut s = spec();
        s.attack_crew = 5;
        // Force a defeat: tiny attack roll vs huge defense.
        s.defense_visible = 100;
        let mut saw_defeat = false;
        let mut saw_crushing = false;
        for seed in 1..200 {
            let result = resolve_combat(&s, &settings(), &mut Rng::new(seed));
            match result.outcome {
                Outcome::Defeat => {
                    saw_defeat = true;
                    assert_eq!(result.attacker_crew_lost, 3, "50% of 5 rounded up");
                    assert_eq!(result.attacker_crew_captured, 3);
                    assert_eq!(result.salvage_gold, 150);
                }
                Outcome::CrushingDefeat => {
                    saw_crushing = true;
                    assert_eq!(result.attacker_crew_lost, 5);
                    assert_eq!(result.salvage_gold, 200);
                    assert_eq!(result.defender_notoriety, 10);
                    assert_eq!(result.humiliated, Humiliated::Attacker);
                }
                _ => panic!("100 defenders must always win"),
            }
        }
        assert!(
            !saw_defeat && saw_crushing,
            "a five-crew fleet against 100 defenders is always crushing defeat"
        );
    }

    #[test]
    fn scout_intel_hides_cove_crew_and_jitters_gold() {
        let target = Player {
            gold: 640,
            crew_regular: 5,
            crew_loyal: 0,
            buildings: Buildings {
                cove: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        for seed in 1..50 {
            let (visible, approx, _, _, leaked) =
                scout_intel(&target, "tortuga", &mut Rng::new(seed));
            assert_eq!(visible, 3, "cove hides 2");
            assert!(!leaked);
            assert!(approx % 10 == 0);
            assert!((560..=720).contains(&approx));
        }
    }

    #[test]
    fn shattered_reef_sometimes_leaks_hidden_crew() {
        let target = Player {
            crew_regular: 5,
            buildings: Buildings {
                cove: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut leaks = 0;
        for seed in 1..100 {
            let (_, _, _, _, leaked) = scout_intel(&target, "shattered_reef", &mut Rng::new(seed));
            leaks += leaked as u32;
        }
        assert!(
            leaks > 15 && leaks < 85,
            "roughly half the scouts leak: {leaks}"
        );
    }

    #[test]
    fn raid_losses_hit_regular_crew_only_and_loot_waits_for_collect() {
        let mut game = Game::default();
        game.players.insert(
            "atk".into(),
            Player {
                nick_cache: "Al".into(),
                crew_regular: 0,
                crew_loyal: 0,
                ..Default::default()
            },
        );
        game.players.insert(
            "def".into(),
            Player {
                nick_cache: "Dave".into(),
                gold: 1000,
                crew_regular: 0,
                crew_loyal: 0,
                buildings: Buildings {
                    cove: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        game.voyages.push(Voyage {
            id: 7,
            owner_uuid: "atk".into(),
            kind: crate::model::VoyageKind::Raid,
            target_uuid: Some("def".into()),
            crew_regular: 0,
            crew_loyal: 2,
            ..Default::default()
        });
        let voyage = game.voyages[0].clone();
        let resolution = resolve_raid(&mut game, &voyage, &mut Rng::new(9), &settings(), 1_000);
        let crate::voyage::Resolution::Raid(report) = resolution else {
            panic!("expected raid resolution")
        };
        assert!(report.attacker_won(), "undefended isle falls");
        assert_eq!(report.crew_lost, 0, "no regular crew sent, none lost");
        assert_eq!(report.loot_gold, 150);
        let attacker = &game.players["atk"];
        assert_eq!(attacker.crew_loyal, 2, "loyal crew returned");
        assert_eq!(attacker.gold, 0, "loot waits for !collect");
        let stored = game.voyages[0].result.as_ref().unwrap();
        assert_eq!(stored.gold, 150);
        assert_eq!(game.players["def"].gold, 850);
    }
}
