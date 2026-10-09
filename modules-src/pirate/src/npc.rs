//! NPC captains: simulated rivals that keep the seas busy and give raiders a target.
//!
//! An NPC is an ordinary [`Player`] with an id under [`NPC_ID_PREFIX`] and an [`NpcCaptain`]
//! marker. It is raided, blockaded, scouted, and sighted by the Navy like anyone. Its own play is
//! a check-in every few hours ([`handle_tick`]). Ordinary voyages are simulated quietly, wages are
//! usually paid, and buildings go up when affordable. Its raids are real voyages, so they go
//! through combat and are announced. Persona tier sets how rich and how bold it is; temperament
//! sets how often it raids and what it does with prisoners.
//!
//! NPCs never receive PMs ([`crate::pm_captain`]), achievements ([`crate::award_to`]), Legends, or
//! season awards, and never count against the player cap or retire.

use crate::model::{is_npc_id, Game, NpcCaptain, Player, VoyageKind, NPC_ID_PREFIX};
use crate::prisoners::{self, Payment};
use crate::{buildings, voyage, PirateSettings, Rng};
use extism_pdk::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tier {
    Easy,
    Normal,
    Hard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Temperament {
    /// Raids rarely; ransoms prisoners cheaply and pays to get its own crew back.
    Cautious,
    /// Raids at an ordinary pace; ransoms dear and only pays a fair price.
    Greedy,
    /// Raids often; maroons its prisoners and abandons its own.
    Ruthless,
}

pub(crate) struct Persona {
    /// Stable roster key; the captain id is `npc:{key}`.
    pub(crate) key: &'static str,
    pub(crate) name: &'static str,
    /// Used instead of `name` while a human captain holds that nick.
    pub(crate) alias: &'static str,
    pub(crate) tier: Tier,
    pub(crate) temperament: Temperament,
    /// Building keys in the order this captain raises them.
    pub(crate) builds: &'static [&'static str],
}

/// The persona roster, in spawn order: `npc_captains = n` sails the first `n`. The order mixes
/// tiers so a small fleet already has an easy mark and a dangerous rival.
pub(crate) const ROSTER: &[Persona] = &[
    Persona {
        key: "barnacle",
        name: "Barnacle",
        alias: "BarnacleBill",
        tier: Tier::Easy,
        temperament: Temperament::Cautious,
        builds: &["walls", "vault", "tavern"],
    },
    Persona {
        key: "bonny",
        name: "Bonny",
        alias: "AnneBonny",
        tier: Tier::Normal,
        temperament: Temperament::Greedy,
        builds: &["vault", "brothel", "shipyard"],
    },
    Persona {
        key: "blackbeard",
        name: "Blackbeard",
        alias: "EdwardTeach",
        tier: Tier::Hard,
        temperament: Temperament::Ruthless,
        builds: &["walls", "shipyard", "tavern", "cove", "vault", "brothel"],
    },
    Persona {
        key: "pegleg",
        name: "Pegleg",
        alias: "PeglegPete",
        tier: Tier::Easy,
        temperament: Temperament::Greedy,
        builds: &["brothel", "vault"],
    },
    Persona {
        key: "kidd",
        name: "Kidd",
        alias: "CaptainKidd",
        tier: Tier::Normal,
        temperament: Temperament::Cautious,
        builds: &["walls", "cove", "vault"],
    },
    Persona {
        key: "morgan",
        name: "Morgan",
        alias: "HenryMorgan",
        tier: Tier::Normal,
        temperament: Temperament::Ruthless,
        builds: &["shipyard", "walls", "tavern"],
    },
];

pub(crate) fn persona(key: &str) -> Option<&'static Persona> {
    ROSTER.iter().find(|persona| persona.key == key)
}

pub(crate) fn npc_id(key: &str) -> String {
    format!("{NPC_ID_PREFIX}{key}")
}

fn persona_of(player: &Player) -> Option<&'static Persona> {
    player.npc.as_ref().and_then(|npc| persona(&npc.persona))
}

/// How a tier plays. Chances are per three hours of check-ins; [`per_turn`] converts them for the
/// configured `npc_checkin_minutes`.
struct TierParams {
    /// Starting gold as a percent of `starting_gold`.
    gold_pct: i64,
    /// Regular crew on top of `starting_regular_crew`.
    crew_bonus: i64,
    /// Regular crew the NPC stops recruiting at.
    crew_ceiling: i64,
    /// Chance a simulated voyage comes home this check-in.
    voyage_chance: f64,
    /// Base chance to launch a raid this check-in, before temperament.
    raid_chance: f64,
    /// Percent of home crew sent on a raid.
    commit_pct: i64,
    /// Highest level the NPC builds any building to.
    build_cap: u8,
    /// Chance the NPC pays wages on a given payday.
    pay_chance: f64,
    /// Gold the NPC grows toward, as a percent of the strongest active person's. 0 = no scaling.
    scale_gold_pct: i64,
    /// Crew (home and away) the NPC grows toward, as a percent of the strongest active person's.
    scale_crew_pct: i64,
    /// How bold the NPC is: it raids only when its attack is expected to beat this percent of the
    /// defense its scouts can see.
    nerve_pct: i64,
}

fn params(tier: Tier) -> TierParams {
    match tier {
        Tier::Easy => TierParams {
            gold_pct: 60,
            crew_bonus: -1,
            crew_ceiling: 6,
            voyage_chance: 0.40,
            raid_chance: 0.04,
            commit_pct: 40,
            build_cap: 1,
            pay_chance: 0.70,
            scale_gold_pct: 0,
            scale_crew_pct: 0,
            nerve_pct: 120,
        },
        Tier::Normal => TierParams {
            gold_pct: 100,
            crew_bonus: 1,
            crew_ceiling: 10,
            voyage_chance: 0.60,
            raid_chance: 0.08,
            commit_pct: 55,
            build_cap: 1,
            pay_chance: 0.90,
            scale_gold_pct: 30,
            scale_crew_pct: 50,
            nerve_pct: 110,
        },
        Tier::Hard => TierParams {
            gold_pct: 150,
            crew_bonus: 4,
            crew_ceiling: 16,
            voyage_chance: 0.85,
            raid_chance: 0.12,
            commit_pct: 80,
            build_cap: 2,
            pay_chance: 1.0,
            // Home defenders get walls, tavern, and cove bonuses, so matching the strongest
            // person's crew is not enough to threaten them; the boss outnumbers them.
            scale_gold_pct: 70,
            scale_crew_pct: 125,
            nerve_pct: 90,
        },
    }
}

fn raid_multiplier(temperament: Temperament) -> f64 {
    match temperament {
        Temperament::Cautious => 0.5,
        Temperament::Greedy => 1.0,
        Temperament::Ruthless => 1.5,
    }
}

/// Extra nerve (percentage points) by temperament: the cautious want better odds.
fn nerve_shift(temperament: Temperament) -> i64 {
    match temperament {
        Temperament::Cautious => 20,
        Temperament::Greedy => 0,
        Temperament::Ruthless => -15,
    }
}

/// The span the tier chances are written for.
const CHANCE_SPAN_SECS: f64 = 3.0 * 3_600.0;
/// Share of the gap to its scaled strength an NPC closes per three hours.
const CATCH_UP_PER_SPAN: f64 = 0.10;

/// Convert a chance per three hours into a chance per check-in of `minutes`, so the configured
/// interval changes how steady NPCs are, not how much they do.
fn per_turn(chance: f64, minutes: i64) -> f64 {
    let turns = minutes.max(1) as f64 * 60.0 / CHANCE_SPAN_SECS;
    1.0 - (1.0 - chance.clamp(0.0, 1.0)).powf(turns)
}
/// An isle an NPC sailed against stays off every NPC's list for this long.
const NPC_TARGET_COOLDOWN_SECS: i64 = 24 * 3_600;
/// NPCs leave a captain alone who has not played for this long.
const ABSENT_SECS: i64 = 3 * 86_400;
/// An NPC holder waits this long for a ransom before press-ganging the prisoners.
const RANSOM_PATIENCE_SECS: i64 = 24 * 3_600;
/// Smallest crew an NPC keeps home before it considers raiding.
const MIN_RAID_HOME_CREW: i64 = 3;

pub(crate) fn next_tick(now: i64, settings: &PirateSettings, rng: &mut Rng) -> i64 {
    let secs = settings.npc_checkin_minutes.max(1) * 60;
    now + secs + rng.between(-secs / 6, secs / 6)
}

/// The strongest active person's gold and total crew: what scaled NPCs grow toward.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Benchmark {
    gold: i64,
    crew: i64,
}

fn benchmark(game: &Game, now: i64) -> Benchmark {
    game.players
        .iter()
        .filter(|(_, player)| {
            !player.is_npc()
                && !player.parked
                && !player.auto_retired
                && now - player.last_activity_at < ABSENT_SECS
        })
        .fold(Benchmark::default(), |best, (uuid, player)| {
            let crew = crate::commands::employed_crew(game, uuid)
                .map_or(0, |(regular, loyal)| regular + loyal);
            Benchmark {
                gold: best.gold.max(player.gold),
                crew: best.crew.max(crew),
            }
        })
}

/// Grow a scaled NPC a step toward its share of the benchmark: new regular crew and a stipend.
/// Never shrinks it; what Lando takes, it has to rebuild.
fn catch_up(game: &mut Game, id: &str, tier: &TierParams, bench: Benchmark, minutes: i64) {
    let step = per_turn(CATCH_UP_PER_SPAN, minutes);
    let employed =
        crate::commands::employed_crew(game, id).map_or(0, |(regular, loyal)| regular + loyal);
    let Some(player) = game.players.get_mut(id) else {
        return;
    };
    let crew_gap = bench.crew * tier.scale_crew_pct / 100 - employed;
    if crew_gap > 0 {
        player.crew_regular += ((crew_gap as f64 * step).ceil() as i64).max(1);
    }
    let gold_gap = bench.gold * tier.scale_gold_pct / 100 - player.gold;
    if gold_gap > 0 {
        player.gold += ((gold_gap as f64 * step).ceil() as i64).max(1);
    }
}

/// Give an NPC its tier's starting isle. Shared by spawning and the season reset. NPCs carry no
/// new-captain shield: they are there to be hit.
fn outfit(player: &mut Player, persona: &Persona, settings: &PirateSettings, now: i64) {
    let tier = params(persona.tier);
    player.gold = settings.starting_gold * tier.gold_pct / 100;
    player.rum = settings.starting_rum;
    player.crew_regular = (settings.starting_regular_crew + tier.crew_bonus).max(1);
    player.crew_loyal = settings.loyal_crew_count;
    player.shield_until = 0;
    player.last_activity_at = now;
}

/// Season reset for an NPC: back to its persona's starting isle. A no-op for humans.
pub(crate) fn reset_for_season(player: &mut Player, settings: &PirateSettings, now: i64) {
    if let Some(persona) = persona_of(player) {
        outfit(player, persona, settings, now);
    }
}

/// What a roster sync changed, for announcements.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RosterChange {
    pub(crate) arrived: Vec<String>,
    pub(crate) departed: Vec<String>,
    /// `(old, new)` display names.
    pub(crate) renamed: Vec<(String, String)>,
}

/// Nothing in flight involves this captain, so it can leave without stranding anyone's crew or
/// spoils.
fn idle(game: &Game, id: &str) -> bool {
    let involved = |uuid: &str| uuid == id;
    !game.voyages.iter().any(|voyage| {
        involved(&voyage.owner_uuid) || voyage.target_uuid.as_deref().is_some_and(involved)
    }) && !game
        .navy_harassments
        .iter()
        .any(|sortie| involved(&sortie.owner_uuid) || involved(&sortie.target_uuid))
        && !game.players.iter().any(|(uuid, player)| {
            player
                .player_blockade
                .as_ref()
                .is_some_and(|blockade| involved(uuid) || involved(&blockade.blockader_uuid))
        })
        && game.navy_pending_target.as_deref() != Some(id)
}

/// Bring the NPC fleet to `settings.npc_captains`: spawn the next personas, retire the surplus once
/// nothing they are part of is still at sea, and step aside from any nick a human captain now
/// holds. Pure apart from IRC casefolding.
pub(crate) fn sync_roster(
    game: &mut Game,
    server: &str,
    settings: &PirateSettings,
    now: i64,
) -> RosterChange {
    let mut change = RosterChange::default();
    let wanted = settings.npc_captains.clamp(0, ROSTER.len() as i64) as usize;
    let human_nicks: Vec<String> = game
        .players
        .values()
        .filter(|player| !player.is_npc())
        .map(|player| crate::fold_nick(server, &player.nick_cache))
        .collect();
    let free = |nick: &str| !human_nicks.contains(&crate::fold_nick(server, nick));
    let pick_name = |persona: &Persona| {
        [persona.name, persona.alias]
            .into_iter()
            .find(|name| free(name))
    };

    for (index, persona) in ROSTER.iter().enumerate() {
        let id = npc_id(persona.key);
        let present = game.players.contains_key(&id);
        if index < wanted && !present {
            // A persona whose name and alias are both taken by people waits for a later check-in.
            let Some(name) = pick_name(persona) else {
                continue;
            };
            let mut player = Player {
                nick_cache: name.to_string(),
                created_at: now,
                npc: Some(NpcCaptain {
                    persona: persona.key.to_string(),
                    wage_day: 0,
                }),
                ..Default::default()
            };
            outfit(&mut player, persona, settings, now);
            game.players.insert(id, player);
            change.arrived.push(name.to_string());
        } else if index >= wanted && present && idle(game, &id) {
            let nick = game.players[&id].nick_cache.clone();
            crate::lifecycle::remove_captains(game, std::slice::from_ref(&id));
            change.departed.push(nick);
        } else if present {
            let current = game.players[&id].nick_cache.clone();
            if !free(&current) {
                if let Some(name) = pick_name(persona) {
                    if let Some(player) = game.players.get_mut(&id) {
                        player.nick_cache = name.to_string();
                    }
                    change.renamed.push((current, name.to_string()));
                }
            }
        }
    }
    change
}

/// Something an NPC check-in did that people should hear about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NpcEvent {
    RaidLaunched {
        voyage_id: u64,
        owner_uuid: String,
        npc: String,
        target: String,
        secs: i64,
    },
    RansomOffered {
        npc: String,
        target_uuid: String,
        target_nick: String,
        count: i64,
        amount: i64,
    },
    RansomPaid {
        npc: String,
        holder_uuid: String,
        holder_nick: String,
        freed: i64,
        amount: i64,
    },
    RansomAbandoned {
        npc: String,
        holder_uuid: String,
        holder_nick: String,
        count: i64,
    },
    Marooned {
        npc: String,
        count: i64,
    },
    Taunt {
        npc: String,
        captain: String,
    },
}

fn alloc(next_id: &mut u64) -> u64 {
    *next_id = next_id.saturating_add(1);
    *next_id
}

/// The payday an NPC's wage decision belongs to: days counted from the rollover hour, so one
/// decision lands before each rollover.
fn wage_day(now: i64, settings: &PirateSettings) -> i64 {
    (now - settings.rollover_hour_utc.clamp(0, 23) * 3_600).div_euclid(86_400)
}

fn nick_of(game: &Game, uuid: &str) -> String {
    game.players
        .get(uuid)
        .map(|player| player.nick_cache.clone())
        .unwrap_or_default()
}

/// A quiet voyage: the loot a real one would bring home, without the announcements.
fn simulate_voyage(game: &mut Game, id: &str, crew_ceiling: i64, now: i64, rng: &mut Rng) {
    let Some(def) = rng.choice(voyage::CATALOG).copied() else {
        return;
    };
    let (mut gold, rum, new_crew, bonus_loss) = voyage::roll_reward(def.kind, rng);
    let mut loss = bonus_loss;
    if def.kind != VoyageKind::Explore {
        loss += voyage::risk_losses(def.risk, rng);
    }
    if game.sea == crate::season::FROZEN_NORTH {
        gold = gold * 3 / 2;
    }
    // A player blockade skims an NPC's returns into escrow exactly as it does a person's.
    let (gold, rum, _, _) = voyage::intercept_rewards(game, id, now, gold, rum, rng);
    let Some(player) = game.players.get_mut(id) else {
        return;
    };
    let lost = loss.min(player.crew_regular).max(0);
    player.gold += gold;
    player.rum += rum;
    player.crew_regular = (player.crew_regular - lost + new_crew).min(crew_ceiling.max(
        // Never sack crew it already has; just stop recruiting past the ceiling.
        player.crew_regular - lost,
    ));
    player.career_crew_lost += lost;
    player.career_voyages += 1;
    player.career_rum_collected += rum.max(0);
}

fn pay_if_due(
    game: &mut Game,
    id: &str,
    tier: &TierParams,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) {
    let day = wage_day(now, settings);
    let Some(npc) = game
        .players
        .get_mut(id)
        .and_then(|player| player.npc.as_mut())
    else {
        return;
    };
    if npc.wage_day == day {
        return;
    }
    npc.wage_day = day;
    if game.players.get(id).is_some_and(|player| player.paid_today) || !rng.chance(tier.pay_chance)
    {
        return;
    }
    if crate::commands::pay_wages(game, id, true, settings).is_err() {
        let _ = crate::commands::pay_wages(game, id, false, settings);
    }
}

/// Raise one building level when the coffers can spare it after a couple of paydays.
fn build_if_flush(
    game: &mut Game,
    id: &str,
    persona: &Persona,
    tier: &TierParams,
    settings: &PirateSettings,
) {
    let Some((regular, loyal)) = crate::commands::employed_crew(game, id) else {
        return;
    };
    let reserve = crate::commands::wage_cost(
        regular,
        loyal,
        settings.crew_wage_gold,
        settings.crew_soft_cap,
    ) * 2
        + 50;
    let Some(player) = game.players.get_mut(id) else {
        return;
    };
    for key in persona.builds {
        let Some(def) = buildings::building_def(key) else {
            continue;
        };
        let level = buildings::level(&player.buildings, def.key);
        if level >= tier.build_cap.min(def.max_level) {
            continue;
        }
        let Some(cost) = buildings::next_cost(&player.buildings, def) else {
            continue;
        };
        if player.gold - cost >= reserve {
            player.gold -= cost;
            buildings::set_level(&mut player.buildings, def.key, level + 1);
        }
        // One building per check-in; the first affordable-in-principle choice decides it.
        return;
    }
}

/// Prisoners this NPC holds: marooned, ransomed, or press-ganged once nobody pays.
#[allow(clippy::too_many_arguments)]
fn handle_held(
    game: &mut Game,
    id: &str,
    persona: &Persona,
    settings: &PirateSettings,
    next_id: &mut u64,
    now: i64,
    rng: &mut Rng,
    events: &mut Vec<NpcEvent>,
) {
    if !game
        .prisoners
        .iter()
        .any(|prisoner| prisoner.holder_uuid == id)
    {
        return;
    }
    let npc = nick_of(game, id);
    let per_head = match persona.temperament {
        Temperament::Ruthless => {
            if let Some(release) =
                prisoners::release_prisoners(game, id, true, settings.notoriety_maroon, rng)
            {
                events.push(NpcEvent::Marooned {
                    npc,
                    count: release.total,
                });
            }
            return;
        }
        Temperament::Cautious => 20,
        Temperament::Greedy => 40,
    };
    let stale = game
        .ransoms
        .iter()
        .any(|ransom| ransom.holder_uuid == id && now - ransom.offered_at >= RANSOM_PATIENCE_SECS);
    if stale {
        let _ = prisoners::release_prisoners(game, id, false, settings.notoriety_maroon, rng);
        return;
    }
    let count = game
        .prisoners
        .iter()
        .find(|prisoner| prisoner.holder_uuid == id)
        .map(|prisoner| prisoner.count.max(1))
        .unwrap_or(1);
    let offer_id = alloc(next_id);
    if let Ok(offer) = prisoners::offer_ransom(game, id, count * per_head, offer_id, now) {
        events.push(NpcEvent::RansomOffered {
            npc,
            target_uuid: offer.target_uuid,
            target_nick: offer.target_nick,
            count: offer.count,
            amount: count * per_head,
        });
    }
}

/// A ransom someone wrote against this NPC's captured crew: pay, wait, or abandon.
fn answer_ransom(
    game: &mut Game,
    id: &str,
    persona: &Persona,
    now: i64,
    events: &mut Vec<NpcEvent>,
) {
    let Some(ransom) = game
        .ransoms
        .iter()
        .find(|ransom| ransom.target_uuid == id)
        .cloned()
    else {
        return;
    };
    let gold = game.players.get(id).map_or(0, |player| player.gold);
    let willing = match persona.temperament {
        Temperament::Cautious => true,
        Temperament::Greedy => ransom.amount <= ransom.count.max(1) * 25,
        Temperament::Ruthless => false,
    };
    let npc = nick_of(game, id);
    let holder_nick = nick_of(game, &ransom.holder_uuid);
    if willing && gold >= ransom.amount {
        if let Payment::Paid { freed, amount } = prisoners::pay_ransom(game, id) {
            events.push(NpcEvent::RansomPaid {
                npc,
                holder_uuid: ransom.holder_uuid,
                holder_nick,
                freed,
                amount,
            });
        }
    } else if !willing || now - ransom.offered_at >= RANSOM_PATIENCE_SECS {
        if let Payment::Paid { .. } = prisoners::abandon_ransom(game, id) {
            events.push(NpcEvent::RansomAbandoned {
                npc,
                holder_uuid: ransom.holder_uuid,
                holder_nick,
                count: ransom.count,
            });
        }
    }
}

/// Isles an NPC may sail against now: everything a declared raid allows, minus anyone already
/// under attack, anyone an NPC hit recently, and people who have stopped playing.
fn raid_targets(
    game: &Game,
    id: &str,
    crew: i64,
    nerve_pct: i64,
    settings: &PirateSettings,
    now: i64,
) -> Vec<String> {
    let mut targets: Vec<String> = game
        .players
        .iter()
        .filter(|(uuid, player)| {
            uuid.as_str() != id
                && !player.auto_retired
                && now - player.npc_raided_at >= NPC_TARGET_COOLDOWN_SECS
                && (player.is_npc() || now - player.last_activity_at < ABSENT_SECS)
                && !game.voyages.iter().any(|voyage| {
                    voyage.kind == VoyageKind::Raid
                        && !voyage.resolved
                        && voyage.target_uuid.as_deref() == Some(uuid.as_str())
                })
                && voyage::validate_launch(
                    game,
                    id,
                    VoyageKind::Raid,
                    Some(uuid),
                    crew,
                    settings,
                    now,
                )
                .is_ok()
                && looks_winnable(game, id, uuid, crew, nerve_pct, settings, now)
        })
        .map(|(uuid, _)| uuid.clone())
        .collect();
    targets.sort();
    targets
}

/// Whether `crew` look like enough, judged the way a scout would: the real combat math at even
/// rolls, against the defenders in plain sight. Crew hidden in a cove can still turn it.
fn looks_winnable(
    game: &Game,
    id: &str,
    target: &str,
    crew: i64,
    nerve_pct: i64,
    settings: &PirateSettings,
    now: i64,
) -> bool {
    let Some(mut spec) = crate::combat::raid_spec(game, id, target, crew, now) else {
        return false;
    };
    spec.defense_hidden = 0;
    let attack = crate::combat::attack_power(&spec, 1.0);
    let defense = crate::combat::defense_power(&spec, 1.0, settings.disloyal_scout_penalty_pct);
    attack * 100 > defense * nerve_pct
}

#[allow(clippy::too_many_arguments)]
fn maybe_raid(
    game: &mut Game,
    id: &str,
    persona: &Persona,
    tier: &TierParams,
    settings: &PirateSettings,
    next_id: &mut u64,
    now: i64,
    rng: &mut Rng,
    events: &mut Vec<NpcEvent>,
) {
    let Some(player) = game.players.get(id) else {
        return;
    };
    let home = player.home_crew(now);
    let raiding = game.voyages.iter().any(|voyage| {
        voyage.owner_uuid == id && voyage.kind == VoyageKind::Raid && !voyage.resolved
    });
    let chance = per_turn(
        tier.raid_chance * raid_multiplier(persona.temperament),
        settings.npc_checkin_minutes,
    );
    if raiding || home < MIN_RAID_HOME_CREW || !rng.chance(chance) {
        return;
    }
    let crew = (home * tier.commit_pct / 100).max(1);
    let nerve = tier.nerve_pct + nerve_shift(persona.temperament);
    let targets = raid_targets(game, id, crew, nerve, settings, now);
    let Some(target) = rng.choice(&targets).cloned() else {
        return;
    };
    let voyage_id = alloc(next_id);
    let launched = voyage::launch(
        game,
        voyage_id,
        id,
        VoyageKind::Raid,
        Some(target.clone()),
        crew,
        true,
        now,
        rng,
    );
    if let Some(defender) = game.players.get_mut(&target) {
        defender.npc_raided_at = now;
    }
    if let Some(player) = game.players.get_mut(id) {
        player.notoriety += settings.notoriety_public_raid;
    }
    events.push(NpcEvent::RaidLaunched {
        voyage_id,
        owner_uuid: id.to_string(),
        npc: nick_of(game, id),
        target: nick_of(game, &target),
        secs: launched.secs.max(60),
    });
}

/// One check-in for every NPC captain. Pure over the game tree; `next_id` is the state's id
/// counter. The caller schedules launched raids and delivers the events.
pub(crate) fn check_in(
    game: &mut Game,
    next_id: &mut u64,
    settings: &PirateSettings,
    now: i64,
    rng: &mut Rng,
) -> Vec<NpcEvent> {
    let mut events = Vec::new();
    let mut ids: Vec<String> = game
        .players
        .iter()
        .filter(|(uuid, player)| is_npc_id(uuid) && player.is_npc() && !player.parked)
        .map(|(uuid, _)| uuid.clone())
        .collect();
    ids.sort();
    let bench = benchmark(game, now);
    let minutes = settings.npc_checkin_minutes;
    for id in &ids {
        let Some(persona) = game.players.get(id).and_then(persona_of) else {
            continue;
        };
        let tier = params(persona.tier);
        if let Some(player) = game.players.get_mut(id) {
            player.last_activity_at = now;
        }
        // Spoils from its raids are banked straight away; nobody needs to type !collect.
        voyage::collect_pending(game, id, settings.scout_intel_hours, now);
        let blockaded = game
            .players
            .get(id)
            .is_some_and(|player| player.blockaded(now));
        catch_up(game, id, &tier, bench, minutes);
        let ceiling = tier
            .crew_ceiling
            .max(bench.crew * tier.scale_crew_pct / 100);
        if !blockaded && rng.chance(per_turn(tier.voyage_chance, minutes)) {
            simulate_voyage(game, id, ceiling, now, rng);
        }
        pay_if_due(game, id, &tier, settings, now, rng);
        build_if_flush(game, id, persona, &tier, settings);
        handle_held(game, id, persona, settings, next_id, now, rng, &mut events);
        answer_ransom(game, id, persona, now, &mut events);
        maybe_raid(
            game,
            id,
            persona,
            &tier,
            settings,
            next_id,
            now,
            rng,
            &mut events,
        );
    }
    let taunt_chance = per_turn(
        settings.npc_chatter_pct.clamp(0, 100) as f64 / 400.0,
        minutes,
    );
    if !ids.is_empty() && rng.chance(taunt_chance) {
        if let Some(id) = rng.choice(&ids) {
            let mut captains: Vec<String> = game
                .players
                .iter()
                .filter(|(uuid, player)| {
                    uuid.as_str() != id.as_str() && !player.parked && !player.auto_retired
                })
                .map(|(_, player)| player.nick_cache.clone())
                .collect();
            captains.sort();
            if let Some(captain) = rng.choice(&captains).cloned() {
                events.push(NpcEvent::Taunt {
                    npc: nick_of(game, id),
                    captain,
                });
            }
        }
    }
    events
}

/// The banter line an NPC may answer a raid with: `(key, defaults, npc, other captain)`.
pub(crate) fn banter_for(
    report: &crate::combat::RaidReport,
) -> Option<(&'static str, &'static [&'static str], String, String)> {
    let attacker = is_npc_id(&report.attacker_uuid);
    let defender = is_npc_id(&report.defender_uuid);
    let (key, defaults, npc, other): (_, &'static [&'static str], _, _) =
        match (attacker, defender, report.attacker_won()) {
            (true, _, true) => (
                "pirate.npc_banter_raided",
                BANTER_RAIDED,
                &report.attacker_nick,
                &report.defender_nick,
            ),
            (true, _, false) => (
                "pirate.npc_banter_beaten",
                BANTER_BEATEN,
                &report.attacker_nick,
                &report.defender_nick,
            ),
            (false, true, true) => (
                "pirate.npc_banter_sacked",
                BANTER_SACKED,
                &report.defender_nick,
                &report.attacker_nick,
            ),
            (false, true, false) => (
                "pirate.npc_banter_held",
                BANTER_HELD,
                &report.defender_nick,
                &report.attacker_nick,
            ),
            (false, false, _) => return None,
        };
    Some((key, defaults, npc.clone(), other.clone()))
}

const BANTER_RAIDED: &[&str] = &[
    "☠ {npc}: \"Thank ye kindly for the gold, {target}. I'll spend it slowly.\"",
    "☠ {npc}: \"Ye call that a defense, {target}? My cabin boy hits harder.\"",
    "☠ {npc}: \"Lock yer vault next time, {target}. Or don't — saves me the bother.\"",
];
const BANTER_BEATEN: &[&str] = &[
    "☠ {npc}: \"Lucky swell, {target}. The tide turns.\"",
    "☠ {npc}: \"Keep my lads fed, {target}. I'll be back for 'em.\"",
    "☠ {npc}: \"That was a scouting party. Aye. Obviously.\"",
];
const BANTER_SACKED: &[&str] = &[
    "☠ {npc}: \"Enjoy it while it lasts, {target}. I know where ye sleep.\"",
    "☠ {npc}: \"Take it, {target}. There's more where that came from — and I'll be fetching yours.\"",
    "☠ {npc}: \"A scratch! A flesh wound! Mark me, {target}.\"",
];
const BANTER_HELD: &[&str] = &[
    "☠ {npc}: \"Is that all ye brought, {target}? My parrot's tougher.\"",
    "☠ {npc}: \"Come back when ye've learned which end of the cutlass is sharp, {target}.\"",
    "☠ {npc}: \"Thanks for the extra hands, {target}. They'll swab my decks nicely.\"",
];
const TAUNTS: &[&str] = &[
    "☠ {npc}: \"Quiet seas today. {captain}'s isle looks awfully undefended from here.\"",
    "☠ {npc}: \"I hear {captain} still pays their crew in IOUs.\"",
    "☠ {npc}: \"Raise yer glasses, lads — to {captain}'s gold, soon to be ours.\"",
    "☠ {npc}: \"Any captain brave enough to come for me? No? Didn't think so.\"",
];

/// Maybe answer a raid an NPC fought with a line of trash talk. Best-effort, after the commit.
pub(crate) fn raid_banter(
    server: &str,
    game: &Game,
    report: &crate::combat::RaidReport,
    settings: &PirateSettings,
) -> Result<(), Error> {
    let Some((key, defaults, npc, target)) = banter_for(report) else {
        return Ok(());
    };
    if !crate::rng()?.chance(settings.npc_chatter_pct.clamp(0, 100) as f64 / 100.0) {
        return Ok(());
    }
    crate::announce(
        server,
        game,
        key,
        defaults,
        &[("npc", &npc), ("target", &target)],
    )
}

/// Announce and PM what a check-in did. Each delivery is independent and best-effort.
fn deliver(server: &str, game: &Game, roster: &RosterChange, events: &[NpcEvent]) {
    for npc in &roster.arrived {
        crate::log_failure(
            "npc arrival",
            crate::announce(
                server,
                game,
                "pirate.npc_arrived",
                &["☠ A new rival flies the black flag: {npc} has claimed an isle in these waters."],
                &[("npc", npc)],
            ),
        );
    }
    for npc in &roster.departed {
        crate::log_failure(
            "npc departure",
            crate::announce(
                server,
                game,
                "pirate.npc_departed",
                &["☠ {npc} has slipped over the horizon and quit these waters."],
                &[("npc", npc)],
            ),
        );
    }
    for (old, new) in &roster.renamed {
        crate::log_failure(
            "npc rename",
            crate::announce(
                server,
                game,
                "pirate.npc_renamed",
                &["☠ The captain once called {old} sails as {new} now — the name is taken."],
                &[("old", old), ("new", new)],
            ),
        );
    }
    for event in events {
        let result = match event {
            NpcEvent::RaidLaunched { npc, target, secs, .. } => crate::announce(
                server,
                game,
                "pirate.npc_raid_departure",
                &["☠ {npc}'s sails are on the horizon, bound for {target}'s isle! They make landfall in about {hours}h."],
                &[("npc", npc), ("target", target), ("hours", &((secs + 3_599) / 3_600).to_string())],
            ),
            NpcEvent::RansomOffered { npc, target_uuid, target_nick, count, amount } => crate::themed(
                "pirate.ransom_received",
                &[crate::pm::RANSOM_RECEIVED],
                &[("holder", npc), ("count", &count.to_string()), ("amount", &amount.to_string())],
            )
            .and_then(|text| crate::pm_captain(server, target_uuid, target_nick, &text)),
            NpcEvent::RansomPaid { npc, holder_uuid, holder_nick, freed, amount } => crate::themed(
                "pirate.npc_ransom_paid",
                &["{npc} paid your ransom: {amount}g for {count} crew."],
                &[("npc", npc), ("amount", &amount.to_string()), ("count", &freed.to_string())],
            )
            .and_then(|text| crate::pm_captain(server, holder_uuid, holder_nick, &text)),
            NpcEvent::RansomAbandoned { npc, holder_uuid, holder_nick, count } => crate::themed(
                "pirate.npc_ransom_abandoned",
                &["{npc} won't pay for their {count} crew. They're yours to maroon or press-gang."],
                &[("npc", npc), ("count", &count.to_string())],
            )
            .and_then(|text| crate::pm_captain(server, holder_uuid, holder_nick, &text)),
            NpcEvent::Marooned { npc, count } => crate::announce(
                server,
                game,
                "pirate.npc_marooned",
                &["☠ {npc} marooned {count} prisoner(s) on a sandbar with one pistol between them."],
                &[("npc", npc), ("count", &count.to_string())],
            ),
            NpcEvent::Taunt { npc, captain } => crate::announce(
                server,
                game,
                "pirate.npc_taunt",
                TAUNTS,
                &[("npc", npc), ("captain", captain)],
            ),
        };
        crate::log_failure("npc event", result);
    }
}

/// The NPC check-in job. Re-arms itself first so no failure below can end the NPCs for good.
pub(crate) fn handle_tick(server: &str, game_key: &str) -> Result<(), Error> {
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
    let mut rng = crate::rng()?;
    crate::schedule(
        &crate::npc_tick_job_id(server),
        server,
        &room,
        None,
        next_tick(now, &settings, &mut rng),
        "",
    )?;
    // A disabled game does not tick, and a retried delivery must not check in twice: anything
    // much closer than the configured interval is a redelivery.
    let retry_secs = settings.npc_checkin_minutes.max(1) * 60 / 3;
    if !crate::game_open(server, game) || now - game.npc_last_tick_at < retry_secs {
        return Ok(());
    }
    let crate::model::State { games, next_id, .. } = &mut state;
    let game = games.get_mut(game_key).expect("checked above");
    game.npc_last_tick_at = now;
    let roster = sync_roster(game, server, &settings, now);
    let events = check_in(game, next_id, &settings, now, &mut rng);
    // Raids are real voyages: they come home on the same timer as everyone else's.
    for event in &events {
        if let NpcEvent::RaidLaunched {
            voyage_id, secs, ..
        } = event
        {
            crate::schedule(
                &crate::voyage_job_id(server, *voyage_id),
                server,
                &room,
                None,
                now + secs,
                "",
            )?;
        }
    }
    crate::save_state(&state)?;
    let game = state.games.get(game_key).expect("checked above");
    deliver(server, game, &roster, &events);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Prisoner, Ransom, Voyage};

    const NOW: i64 = 1_000_000_000;

    fn settings(npcs: i64) -> PirateSettings {
        PirateSettings {
            npc_captains: npcs,
            ..PirateSettings::defaults()
        }
    }

    fn human(nick: &str) -> Player {
        Player {
            nick_cache: nick.into(),
            gold: 500,
            crew_regular: 5,
            last_activity_at: NOW,
            ..Default::default()
        }
    }

    fn game_with_npcs(npcs: i64) -> Game {
        let mut game = Game::default();
        game.players.insert("lando".into(), human("Lando"));
        sync_roster(&mut game, "net", &settings(npcs), NOW);
        game
    }

    #[test]
    fn the_roster_spawns_in_order_unshielded_and_outside_the_human_seats() {
        let game = game_with_npcs(3);
        let npcs: Vec<_> = ROSTER[..3].iter().map(|p| npc_id(p.key)).collect();
        for id in &npcs {
            let npc = &game.players[id];
            assert!(npc.is_npc() && is_npc_id(id));
            assert!(!npc.shielded(NOW), "NPCs are there to be raided");
        }
        assert!(!game.players.contains_key(&npc_id(ROSTER[3].key)));
        // Tiers shape the starting isle.
        let barnacle = &game.players[&npc_id("barnacle")];
        let blackbeard = &game.players[&npc_id("blackbeard")];
        assert!(blackbeard.gold > barnacle.gold);
        assert!(blackbeard.crew_regular > barnacle.crew_regular);
    }

    #[test]
    fn an_npc_never_takes_a_human_captains_nick() {
        let mut game = Game::default();
        game.players.insert("u1".into(), human("blackbeard"));
        let change = sync_roster(&mut game, "net", &settings(3), NOW);
        assert_eq!(
            game.players[&npc_id("blackbeard")].nick_cache,
            "EdwardTeach"
        );
        assert!(change.arrived.contains(&"EdwardTeach".to_string()));

        // A person who later takes an NPC's name pushes the NPC onto its alias.
        game.players.insert("u2".into(), human("BONNY"));
        let change = sync_roster(&mut game, "net", &settings(3), NOW);
        assert_eq!(game.players[&npc_id("bonny")].nick_cache, "AnneBonny");
        assert_eq!(change.renamed, vec![("Bonny".into(), "AnneBonny".into())]);
    }

    #[test]
    fn lowering_the_count_retires_npcs_only_once_nothing_involves_them() {
        let mut game = game_with_npcs(2);
        let bonny = npc_id("bonny");
        game.voyages.push(Voyage {
            id: 7,
            owner_uuid: "lando".into(),
            kind: VoyageKind::Raid,
            target_uuid: Some(bonny.clone()),
            ..Default::default()
        });
        let change = sync_roster(&mut game, "net", &settings(1), NOW);
        assert!(
            change.departed.is_empty(),
            "a raid is still sailing for her"
        );
        assert!(game.players.contains_key(&bonny));

        game.voyages.clear();
        game.prisoners.push(Prisoner {
            id: 1,
            holder_uuid: "lando".into(),
            origin_uuid: bonny.clone(),
            count: 2,
            captured_at: NOW,
        });
        let change = sync_roster(&mut game, "net", &settings(1), NOW);
        assert_eq!(change.departed, vec!["Bonny".to_string()]);
        assert!(!game.players.contains_key(&bonny));
        assert!(game.prisoners.is_empty(), "her crew leave with her");
        assert!(game.players.contains_key(&npc_id("barnacle")));
    }

    #[test]
    fn npcs_pay_wages_once_per_payday() {
        let mut game = game_with_npcs(3);
        let id = npc_id("blackbeard"); // Hard tier always pays.
        let mut next = 0;
        check_in(&mut game, &mut next, &settings(3), NOW, &mut Rng::new(1));
        let npc = &game.players[&id];
        assert!(npc.paid_today);
        let gold = npc.gold;
        // Same payday, wages already settled: no second payment.
        game.players.get_mut(&id).unwrap().paid_today = false;
        let day = game.players[&id].npc.as_ref().unwrap().wage_day;
        check_in(
            &mut game,
            &mut next,
            &settings(3),
            NOW + 60,
            &mut Rng::new(2),
        );
        assert_eq!(game.players[&id].npc.as_ref().unwrap().wage_day, day);
        assert!(!game.players[&id].paid_today);
        assert!(game.players[&id].gold >= gold, "no wages taken twice");
    }

    #[test]
    fn npc_raids_respect_shields_mercy_absence_and_pile_ons() {
        let s = settings(1);
        let mut game = game_with_npcs(1);
        let id = npc_id("barnacle");
        game.players.get_mut(&id).unwrap().crew_regular = 10;
        let targets = |game: &Game| raid_targets(game, &id, 4, 100, &s, NOW);
        assert_eq!(targets(&game), vec!["lando".to_string()]);

        let lando = game.players.get_mut("lando").unwrap();
        lando.shield_until = NOW + 10;
        assert!(targets(&game).is_empty(), "shielded");
        let lando = game.players.get_mut("lando").unwrap();
        lando.shield_until = 0;
        lando.raid_mercy_until = NOW + 10;
        assert!(targets(&game).is_empty(), "licking wounds");
        let lando = game.players.get_mut("lando").unwrap();
        lando.raid_mercy_until = 0;
        lando.npc_raided_at = NOW - 3_600;
        assert!(targets(&game).is_empty(), "an NPC hit them recently");
        let lando = game.players.get_mut("lando").unwrap();
        lando.npc_raided_at = 0;
        lando.last_activity_at = NOW - ABSENT_SECS;
        assert!(targets(&game).is_empty(), "absent captains are left alone");
        game.players.get_mut("lando").unwrap().last_activity_at = NOW;
        game.voyages.push(Voyage {
            id: 1,
            owner_uuid: "someone".into(),
            kind: VoyageKind::Raid,
            target_uuid: Some("lando".into()),
            ..Default::default()
        });
        assert!(targets(&game).is_empty(), "one raid at a time per isle");
    }

    #[test]
    fn chances_scale_with_the_check_in_interval() {
        assert!((per_turn(0.3, 180) - 0.3).abs() < 1e-9);
        let hourly = per_turn(0.3, 60);
        assert!(hourly < 0.3);
        // Three hourly check-ins add up to the same odds as one three-hour check-in.
        assert!((1.0 - (1.0 - hourly).powi(3) - 0.3).abs() < 1e-9);
        let s = settings(1);
        let mut rng = Rng::new(1);
        for _ in 0..50 {
            let due = next_tick(NOW, &s, &mut rng) - NOW;
            assert!((50 * 60..=70 * 60).contains(&due), "{due}");
        }
    }

    fn whale() -> Player {
        Player {
            nick_cache: "Lando".into(),
            gold: 13_700,
            crew_regular: 72,
            crew_loyal: 2,
            last_activity_at: NOW,
            buildings: crate::model::Buildings {
                vault: 2,
                cove: 1,
                walls: 2,
                shipyard: 2,
                tavern: 1,
                brothel: 2,
            },
            ..Default::default()
        }
    }

    #[test]
    fn scaled_npcs_grow_toward_the_strongest_active_captain() {
        let s = settings(3);
        let mut game = Game::default();
        game.players.insert("lando".into(), whale());
        sync_roster(&mut game, "net", &s, NOW);
        let bench = benchmark(&game, NOW);
        assert_eq!(
            bench,
            Benchmark {
                gold: 13_700,
                crew: 74
            }
        );

        let easy = game.players[&npc_id("barnacle")].clone();
        for turn in 0..200 {
            for key in ["barnacle", "blackbeard"] {
                let id = npc_id(key);
                let tier = params(persona(key).unwrap().tier);
                catch_up(&mut game, &id, &tier, bench, 60 + turn % 2);
            }
        }
        let boss = &game.players[&npc_id("blackbeard")];
        assert!(boss.crew_regular + boss.crew_loyal >= 74 * 125 / 100);
        assert!(boss.gold >= 13_700 * 70 / 100 - 1);
        let barnacle = &game.players[&npc_id("barnacle")];
        assert_eq!(barnacle.gold, easy.gold, "easy marks stay easy");
        assert_eq!(barnacle.crew_regular, easy.crew_regular);

        // Someone who stopped playing sets no bar.
        game.players.get_mut("lando").unwrap().last_activity_at = NOW - ABSENT_SECS;
        assert_eq!(benchmark(&game, NOW), Benchmark::default());
    }

    #[test]
    fn npcs_only_pick_fights_they_expect_to_win() {
        let s = settings(3);
        let mut game = Game::default();
        game.players.insert("lando".into(), whale());
        sync_roster(&mut game, "net", &s, NOW);
        let boss = npc_id("blackbeard");
        let nerve = params(Tier::Hard).nerve_pct + nerve_shift(Temperament::Ruthless);
        let home = game.players[&boss].home_crew(NOW);
        let crew = home * params(Tier::Hard).commit_pct / 100;
        assert!(
            !looks_winnable(&game, &boss, "lando", crew, nerve, &s, NOW),
            "a fresh Blackbeard does not throw {crew} crew at a fortress"
        );
        let bench = benchmark(&game, NOW);
        for _ in 0..200 {
            catch_up(&mut game, &boss, &params(Tier::Hard), bench, 60);
        }
        let home = game.players[&boss].home_crew(NOW);
        let crew = home * params(Tier::Hard).commit_pct / 100;
        assert!(
            looks_winnable(&game, &boss, "lando", crew, nerve, &s, NOW),
            "a grown Blackbeard is a real threat"
        );
        // ...while a normal NPC, which only grows to half that, still keeps well clear.
        let bonny = npc_id("bonny");
        let nerve = params(Tier::Normal).nerve_pct + nerve_shift(Temperament::Greedy);
        assert!(!looks_winnable(&game, &bonny, "lando", 3, nerve, &s, NOW));
    }

    #[test]
    fn a_launched_npc_raid_is_a_real_public_voyage() {
        let s = settings(3);
        let mut game = game_with_npcs(3);
        let mut next = 0;
        let mut launched = None;
        for seed in 0..500 {
            let mut trial = game.clone();
            let events = check_in(&mut trial, &mut next, &s, NOW, &mut Rng::new(seed));
            if let Some(event) = events
                .iter()
                .find(|event| matches!(event, NpcEvent::RaidLaunched { .. }))
            {
                launched = Some((trial, event.clone()));
                break;
            }
        }
        let (trial, event) = launched.expect("some check-in launches a raid");
        game = trial;
        let NpcEvent::RaidLaunched {
            voyage_id,
            owner_uuid,
            ..
        } = event
        else {
            unreachable!()
        };
        let voyage = game.voyages.iter().find(|v| v.id == voyage_id).unwrap();
        assert_eq!(voyage.owner_uuid, owner_uuid);
        assert_eq!(voyage.kind, VoyageKind::Raid);
        assert!(voyage.is_public);
        assert!(!voyage.resolved);
        let target = voyage.target_uuid.clone().unwrap();
        assert_eq!(game.players[&target].npc_raided_at, NOW);
        // Never more than one raid in flight per NPC.
        let events = check_in(&mut game, &mut next, &s, NOW + 60, &mut Rng::new(9));
        assert!(!events.iter().any(|event| matches!(
            event,
            NpcEvent::RaidLaunched { owner_uuid: owner, .. } if *owner == owner_uuid
        )));
    }

    #[test]
    fn prisoners_are_handled_by_temperament() {
        let s = settings(3);
        let mut game = game_with_npcs(3);
        for holder in ["barnacle", "blackbeard"] {
            game.prisoners.push(Prisoner {
                id: if holder == "barnacle" { 1 } else { 2 },
                holder_uuid: npc_id(holder),
                origin_uuid: "lando".into(),
                count: 3,
                captured_at: NOW,
            });
        }
        let mut next = 100;
        let events = check_in(&mut game, &mut next, &s, NOW, &mut Rng::new(3));
        assert!(events.contains(&NpcEvent::Marooned {
            npc: "Blackbeard".into(),
            count: 3
        }));
        assert!(events.contains(&NpcEvent::RansomOffered {
            npc: "Barnacle".into(),
            target_uuid: "lando".into(),
            target_nick: "Lando".into(),
            count: 3,
            amount: 60,
        }));
        // Nobody pays for a day: the cautious holder press-gangs them instead.
        check_in(
            &mut game,
            &mut next,
            &s,
            NOW + RANSOM_PATIENCE_SECS,
            &mut Rng::new(4),
        );
        assert!(game.prisoners.is_empty());
        assert!(game.ransoms.is_empty());
    }

    #[test]
    fn npcs_answer_ransoms_on_their_crew_by_temperament() {
        let s = settings(3);
        let mut game = game_with_npcs(3);
        for (id, target) in [(1, "barnacle"), (2, "blackbeard")] {
            game.prisoners.push(Prisoner {
                id,
                holder_uuid: "lando".into(),
                origin_uuid: npc_id(target),
                count: 2,
                captured_at: NOW,
            });
            game.ransoms.push(Ransom {
                id: 10 + id,
                prisoner_id: id,
                holder_uuid: "lando".into(),
                target_uuid: npc_id(target),
                amount: 50,
                count: 2,
                offered_at: NOW,
            });
        }
        let lando_gold = game.players["lando"].gold;
        let mut next = 100;
        let events = check_in(&mut game, &mut next, &s, NOW, &mut Rng::new(5));
        assert!(events.iter().any(|event| matches!(
            event,
            NpcEvent::RansomPaid { npc, amount: 50, .. } if npc == "Barnacle"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            NpcEvent::RansomAbandoned { npc, .. } if npc == "Blackbeard"
        )));
        assert_eq!(game.players["lando"].gold, lando_gold + 50);
    }

    #[test]
    fn npcs_build_within_their_tier_and_keep_a_wage_reserve() {
        let s = settings(3);
        let mut game = game_with_npcs(3);
        let id = npc_id("barnacle");
        game.players.get_mut(&id).unwrap().gold = 10_000;
        let mut next = 0;
        for tick in 0..20 {
            check_in(
                &mut game,
                &mut next,
                &s,
                NOW + tick * 60,
                &mut Rng::new(tick as u64),
            );
        }
        let buildings = &game.players[&id].buildings;
        assert_eq!(buildings.walls, 1, "easy NPCs stop at level 1");
        assert!(buildings.vault <= 1);

        let npc = game.players.get_mut(&id).unwrap();
        npc.gold = 100;
        npc.buildings = Default::default();
        build_if_flush(
            &mut game,
            &id,
            persona("barnacle").unwrap(),
            &params(Tier::Easy),
            &s,
        );
        assert_eq!(
            game.players[&id].buildings.walls, 0,
            "the wage reserve comes first"
        );
        assert_eq!(game.players[&id].gold, 100);
    }

    #[test]
    fn banter_speaks_for_whichever_side_is_an_npc() {
        let mut report = crate::combat::RaidReport {
            outcome: crate::combat::Outcome::Victory,
            attack_power: 1,
            defense_power: 0,
            loot_gold: 0,
            gross_loot_gold: 0,
            intercepted_gold: 0,
            intercepted_rum: 0,
            crew_lost: 0,
            crew_captured: 0,
            salvage_gold: 0,
            attacker_uuid: npc_id("bonny"),
            defender_uuid: "lando".into(),
            attacker_nick: "Bonny".into(),
            defender_nick: "Lando".into(),
            false_flag_reveal: None,
            navy_alert: false,
            loyal_retreated: false,
            navy_halved: false,
            humiliated_hours: 0,
        };
        let line = |report: &crate::combat::RaidReport| {
            banter_for(report).map(|(key, _, npc, other)| (key, npc, other))
        };
        assert_eq!(
            line(&report),
            Some(("pirate.npc_banter_raided", "Bonny".into(), "Lando".into()))
        );
        report.outcome = crate::combat::Outcome::Defeat;
        assert_eq!(line(&report).unwrap().0, "pirate.npc_banter_beaten");
        std::mem::swap(&mut report.attacker_uuid, &mut report.defender_uuid);
        std::mem::swap(&mut report.attacker_nick, &mut report.defender_nick);
        assert_eq!(
            line(&report),
            Some(("pirate.npc_banter_held", "Bonny".into(), "Lando".into()))
        );
        report.outcome = crate::combat::Outcome::CrushingVictory;
        assert_eq!(line(&report).unwrap().0, "pirate.npc_banter_sacked");
        report.defender_uuid = "someone".into();
        assert_eq!(
            line(&report),
            None,
            "people fighting people get no NPC commentary"
        );
    }

    #[test]
    fn season_reset_restores_the_persona_and_skips_honours() {
        let s = settings(3);
        let mut game = game_with_npcs(3);
        game.season_started = NOW - 10;
        let id = npc_id("blackbeard");
        let npc = game.players.get_mut(&id).unwrap();
        npc.gold = 99_999;
        npc.season_raids_won = 50;
        npc.notoriety = 50;
        let start_gold = {
            let mut fresh = Player::default();
            outfit(&mut fresh, persona("blackbeard").unwrap(), &s, NOW);
            fresh.gold
        };
        let awards = crate::season::compute_awards(&game);
        assert_ne!(
            awards.gold_king.as_ref().map(|(n, _)| n.as_str()),
            Some("Blackbeard")
        );
        assert_ne!(
            awards.raid_lord.as_ref().map(|(n, _)| n.as_str()),
            Some("Blackbeard")
        );
        let end = crate::season::end_season(&mut game, &s, NOW, &mut Rng::new(1));
        assert!(!end.participants.iter().any(|(uuid, _)| is_npc_id(uuid)));
        let npc = &game.players[&id];
        assert!(npc.legends.is_empty(), "no Legends for NPCs");
        assert_eq!(npc.gold, start_gold);
        assert!(!npc.shielded(NOW + 1));
        assert!(
            game.players["lando"].shielded(NOW + 1),
            "people still get their shield"
        );
    }
}
