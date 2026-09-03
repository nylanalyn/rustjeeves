//! Channel-facing Pirate Isles commands.

use crate::buildings;
use crate::combat;
use crate::model::{
    clean_nick, Departure, Game, NavyHarassment, Player, State, VoyageKind, MAX_DEPARTURES,
    MAX_NAVY_HARASSMENTS, MAX_PLAYERS,
};
use crate::navy;
use crate::voyage::{self, LaunchError};
use crate::{
    ensure_jobs, is_blacklisted, learn_room, load_state, now_secs, pirate_settings, reply,
    resolve_uuid, rng, save_state, schedule, setting_enabled, themed, PirateSettings,
};
use extism_pdk::Error;
use jeeves_abi::MessagePayload;

/// The serverwide game key: one game per network.
pub(crate) fn game_key(server: &str) -> String {
    server.to_string()
}

fn command(text: &str) -> Option<(String, Vec<&str>)> {
    let mut words = text.split_whitespace();
    let name = words.next()?.strip_prefix('!')?.to_ascii_lowercase();
    Some((name, words.collect()))
}

fn actor(msg: &MessagePayload) -> Result<&str, Error> {
    if msg.user_id.trim().is_empty() {
        Err(Error::msg(
            "pirate command received without a stable profile id",
        ))
    } else {
        Ok(msg.user_id.trim())
    }
}

/// Ensure the game record for this channel exists, and refresh the caller's nick cache if they
/// already hold an isle. Joining is deliberate — see [`enroll`] — so this never creates a player.
fn ensure_game(state: &mut State, key: &str, msg: &MessagePayload, now: i64) {
    let game = state.games.entry(key.to_owned()).or_insert_with(|| Game {
        season_started: now,
        ..Default::default()
    });
    if game.season_started == 0 {
        game.season_started = now;
    }
    if let Some(player) = game.players.get_mut(&msg.user_id) {
        player.nick_cache = clean_nick(&msg.nick);
    }
}

/// Why a captain could not sign on.
#[derive(Debug)]
enum SignonError {
    /// They already hold an isle here.
    Already,
    /// The serverwide game is at its player cap.
    Full,
}

/// Give this captain an isle. Explicit, so nobody is enrolled into a PvP game — and starts
/// accruing missed paydays — merely for asking what `!here` shows.
fn enroll(
    state: &mut State,
    key: &str,
    msg: &MessagePayload,
    settings: &PirateSettings,
    now: i64,
) -> Result<(), SignonError> {
    let game = state.games.entry(key.to_owned()).or_default();
    if game.players.contains_key(&msg.user_id) {
        return Err(SignonError::Already);
    }
    if game.players.len() >= settings.player_cap.min(MAX_PLAYERS as i64) as usize {
        return Err(SignonError::Full);
    }
    game.players.insert(
        msg.user_id.clone(),
        Player {
            nick_cache: clean_nick(&msg.nick),
            gold: settings.starting_gold,
            rum: settings.starting_rum,
            crew_regular: settings.starting_regular_crew,
            crew_loyal: settings.loyal_crew_count,
            shield_until: now + settings.new_player_shield_hours * 3600,
            created_at: now,
            ..Default::default()
        },
    );
    Ok(())
}

/// The four lines a new captain needs: who they are, the daily obligation, the loop, and where
/// the depth lives. Everything else is discoverable from `!help pirate`.
fn welcome(server: &str, nick: &str, settings: &PirateSettings) -> Result<(), Error> {
    reply(
        server,
        nick,
        &themed(
            "pirate.signon_welcome",
            &["⚓ Welcome to the Pirate Isles! You command an isle with {gold} gold, {crew} crew, and a Cove to hide them in. Hold it, grow it, and make your name before the season turns."],
            &[
                ("gold", &settings.starting_gold.to_string()),
                (
                    "crew",
                    &(settings.starting_regular_crew + settings.loyal_crew_count).to_string(),
                ),
            ],
        )?,
    )?;
    reply(
        server,
        nick,
        &themed(
            "pirate.signon_wages",
            &["Your crew want paying every day: !pay for gold, !rum for rum. Miss a payday and loyalty rots — miss enough and they start deserting."],
            &[],
        )?,
    )?;
    reply(
        server,
        nick,
        &themed(
            "pirate.signon_voyages",
            &["!menu opens your captain's menu here in PM to send crew out on a voyage. When they sail home, !collect in the channel banks the spoils."],
            &[],
        )?,
    )?;
    reply(
        server,
        nick,
        &themed(
            "pirate.signon_help",
            &["Scout an isle on a voyage and you may raid it afterwards; !raid <captain> <crew> declares war openly instead. !crew shows your isle, and !help pirate has the rest. Fair winds!"],
            &[],
        )?,
    )
}

fn launch_error(error: LaunchError) -> String {
    match error {
        LaunchError::MinCrew(min) => format!("that voyage needs at least {min} crew"),
        LaunchError::CrewShort(home) => format!("you only have {home} crew home"),
        LaunchError::TooManyVoyages(max) => format!("you already have {max} active voyages"),
        LaunchError::Blockaded => "the Royal Navy blockade prevents launches".into(),
        LaunchError::NoTarget => "that target is not available".into(),
        LaunchError::TargetParked => {
            "that captain is away from the seas and cannot be targeted".into()
        }
        LaunchError::SelfTarget => "you cannot target yourself".into(),
        LaunchError::TargetShielded => "that captain is still under a new-captain shield".into(),
        LaunchError::TargetBusy => "that captain already has two raids inbound".into(),
        LaunchError::TargetRecentlyRaided => {
            "that isle was raided recently and is still licking its wounds".into()
        }
        LaunchError::NoIntel => {
            "you have no fresh scout report to sail on — scout an isle first".into()
        }
    }
}

fn reply_error(server: &str, target: &str, message: &str) -> Result<(), Error> {
    reply(
        server,
        target,
        &themed(
            "pirate.error",
            &["Arrr: {message}."],
            &[("message", message)],
        )?,
    )
}

fn employed_crew(game: &Game, uuid: &str) -> Option<(i64, i64)> {
    let player = game.players.get(uuid)?;
    let (voyage_regular, voyage_loyal) = game
        .voyages
        .iter()
        .filter(|voyage| voyage.owner_uuid == uuid && !voyage.resolved)
        .fold((0i64, 0i64), |(regular, loyal), voyage| {
            (
                regular.saturating_add(voyage.crew_regular.max(0)),
                loyal.saturating_add(voyage.crew_loyal.max(0)),
            )
        });
    Some((
        player.crew_regular.max(0).saturating_add(voyage_regular),
        player.crew_loyal.max(0).saturating_add(voyage_loyal),
    ))
}

fn active_harassments(game: &Game, uuid: &str) -> usize {
    game.navy_harassments
        .iter()
        .filter(|sortie| sortie.owner_uuid == uuid && !sortie.resolved)
        .count()
}

fn pause_player(
    state: &mut State,
    server: &str,
    channel: &str,
    uuid: &str,
    settings: &PirateSettings,
    now: i64,
) -> Result<Vec<voyage::Resolution>, Error> {
    let key = game_key(server);
    let (jobs, pending_navy_for_player) = {
        let game = state
            .games
            .get(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let mut jobs = game
            .voyages
            .iter()
            .filter(|voyage| voyage.owner_uuid == uuid && !voyage.resolved)
            .map(|voyage| crate::voyage_job_id(server, voyage.id))
            .collect::<Vec<_>>();
        jobs.extend(
            game.navy_harassments
                .iter()
                .filter(|sortie| sortie.owner_uuid == uuid && !sortie.resolved)
                .map(|sortie| crate::navy_harass_job_id(server, sortie.id)),
        );
        jobs.extend(
            game.voyages
                .iter()
                .filter(|voyage| {
                    voyage.kind == VoyageKind::Raid
                        && voyage.target_uuid.as_deref() == Some(uuid)
                        && !voyage.resolved
                })
                .map(|voyage| crate::voyage_job_id(server, voyage.id)),
        );
        if game
            .players
            .get(uuid)
            .is_some_and(|player| player.loyal_cove_until > now)
        {
            jobs.push(crate::loyal_return_job_id(server, uuid));
        }
        let pending_navy_for_player = game.navy_pending_target.as_deref() == Some(uuid);
        if pending_navy_for_player {
            jobs.push(crate::navy_hit_job_id(server));
        }
        (jobs, pending_navy_for_player)
    };
    for id in jobs {
        crate::cancel_schedule(&id)?;
    }
    let mut cancelled = Vec::new();
    let mut rearm_navy = false;
    {
        let game = state
            .games
            .get_mut(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        if pending_navy_for_player {
            game.navy_pending_target = None;
            game.navy_pending_hit_at = 0;
            rearm_navy = true;
        }
        let player = game
            .players
            .get_mut(uuid)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        player.parked = true;
        player.parked_at = now;
        let incoming_ids = game
            .voyages
            .iter()
            .filter(|voyage| {
                voyage.kind == VoyageKind::Raid
                    && voyage.target_uuid.as_deref() == Some(uuid)
                    && !voyage.resolved
            })
            .map(|voyage| voyage.id)
            .collect::<Vec<_>>();
        for id in incoming_ids {
            if let Some(resolution) = voyage::resolve_voyage(game, id, &mut rng()?, settings, now) {
                cancelled.push(resolution);
            }
        }
    }
    if rearm_navy {
        crate::schedule(
            &crate::navy_job_id(server),
            server,
            channel,
            None,
            navy::next_navy_due(settings, now, &mut rng()?),
            "",
        )?;
    }
    Ok(cancelled)
}

fn shift_timer(value: &mut i64, parked_at: i64, paused_for: i64) {
    if *value > parked_at {
        *value = value.saturating_add(paused_for);
    }
}

fn resume_player(
    state: &mut State,
    server: &str,
    channel: &str,
    uuid: &str,
    now: i64,
) -> Result<(), Error> {
    let key = game_key(server);
    let (voyages, harassments, loyal_return_due) = {
        let game = state
            .games
            .get_mut(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let parked_at = game
            .players
            .get(uuid)
            .ok_or_else(|| Error::msg("your island is missing"))?
            .parked_at;
        let paused_for = now.saturating_sub(parked_at).max(0);
        for voyage in &mut game.voyages {
            if voyage.owner_uuid == uuid && !voyage.resolved {
                shift_timer(&mut voyage.started_at, parked_at, paused_for);
                shift_timer(&mut voyage.returns_at, parked_at, paused_for);
            }
        }
        for sortie in &mut game.navy_harassments {
            if sortie.owner_uuid == uuid && !sortie.resolved {
                shift_timer(&mut sortie.started_at, parked_at, paused_for);
                shift_timer(&mut sortie.returns_at, parked_at, paused_for);
            }
        }
        let player = game
            .players
            .get_mut(uuid)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        shift_timer(&mut player.shield_until, parked_at, paused_for);
        shift_timer(&mut player.loyal_cove_until, parked_at, paused_for);
        shift_timer(&mut player.humiliated_until, parked_at, paused_for);
        shift_timer(&mut player.navy_blockade_until, parked_at, paused_for);
        shift_timer(&mut player.raid_mercy_until, parked_at, paused_for);
        shift_timer(&mut player.false_flag_ready_at, parked_at, paused_for);
        if let Some(intel) = player.raid_intel.as_mut() {
            shift_timer(&mut intel.expires_at, parked_at, paused_for);
        }
        let loyal_return_due = (player.loyal_cove_until > now).then_some(player.loyal_cove_until);
        player.parked = false;
        player.parked_at = 0;
        (
            game.voyages
                .iter()
                .filter(|voyage| voyage.owner_uuid == uuid && !voyage.resolved)
                .map(|voyage| (voyage.id, voyage.returns_at))
                .collect::<Vec<_>>(),
            game.navy_harassments
                .iter()
                .filter(|sortie| sortie.owner_uuid == uuid && !sortie.resolved)
                .map(|sortie| (sortie.id, sortie.returns_at))
                .collect::<Vec<_>>(),
            loyal_return_due,
        )
    };
    for (id, due) in voyages {
        crate::schedule(
            &crate::voyage_job_id(server, id),
            server,
            channel,
            Some(uuid.into()),
            due.max(now + 1),
            "",
        )?;
    }
    for (id, due) in harassments {
        crate::schedule(
            &crate::navy_harass_job_id(server, id),
            server,
            channel,
            Some(uuid.into()),
            due.max(now + 1),
            "",
        )?;
    }
    if let Some(due) = loyal_return_due {
        crate::schedule(
            &crate::loyal_return_job_id(server, uuid),
            server,
            channel,
            Some(uuid.into()),
            due.max(now + 1),
            &serde_json::to_string(&serde_json::json!({
                "profile_id": uuid,
            }))?,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn handle_sail(
    state: &mut State,
    server: &str,
    channel: &str,
    msg: &MessagePayload,
    uuid: &str,
    args: &[&str],
    settings: &PirateSettings,
    now: i64,
) -> Result<(), Error> {
    let key = game_key(server);
    let Some(first) = args.first() else {
        return reply_error(
            server,
            channel,
            "usage is !sail <crew> against your blockade, or !sail <captain> <crew> to harass",
        );
    };
    if args.len() == 1 {
        let Some(crew) = first.parse::<i64>().ok().filter(|crew| *crew > 0) else {
            return reply_error(
                server,
                channel,
                "your blockade sortie needs a positive crew count",
            );
        };
        let game = state
            .games
            .get_mut(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let Some(player) = game.players.get(uuid) else {
            return reply_error(server, channel, "your island is missing");
        };
        if player.parked {
            return reply_error(
                server,
                channel,
                "your ship is parked; reply !unpark before sailing",
            );
        }
        if !player.blockaded(now) || player.navy_blockade_strength <= 0 {
            return reply_error(
                server,
                channel,
                "no active Navy blockade is waiting at your isle",
            );
        }
        if crew > player.home_crew(now) {
            return reply_error(
                server,
                channel,
                &format!("you only have {} crew home", player.home_crew(now)),
            );
        }
        let report = navy::assault(game, uuid, crew, settings, now)
            .ok_or_else(|| Error::msg("the Navy blockade is no longer active"))?;
        save_state(state)?;
        if report.won {
            return reply(
                server,
                channel,
                &themed(
                    "pirate.navy_assault_won",
                    &[
                        "⚔️ {user}'s {crew}-crew sortie broke the Royal Navy blockade! The fleet retreats; it will return stronger next time this season.",
                    ],
                    &[("user", &msg.display), ("crew", &report.crew_sent.to_string())],
                )?,
            );
        }
        return reply(
            server,
            channel,
            &themed(
                "pirate.navy_assault_lost",
                &[
                    "🚢 The Navy repelled {user}'s {crew}-crew sortie. The blockade holds; the fleet took {gold}g, {rum} rum, and {lost} crew.",
                ],
                &[
                    ("user", &msg.display),
                    ("crew", &report.crew_sent.to_string()),
                    ("gold", &report.gold_lost.to_string()),
                    ("rum", &report.rum_lost.to_string()),
                    ("lost", &report.crew_lost.to_string()),
                ],
            )?,
        );
    }
    if args.len() != 2 {
        return reply_error(
            server,
            channel,
            "usage is !sail <crew> against your blockade, or !sail <captain> <crew> to harass",
        );
    }
    let target_nick = args[0];
    let Some(crew) = args[1].parse::<i64>().ok().filter(|crew| *crew > 0) else {
        return reply_error(
            server,
            channel,
            "a harassment sortie needs a positive crew count",
        );
    };
    let target_uuid = {
        let game = state
            .games
            .get(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let Some(target_uuid) = resolve_uuid(game, server, target_nick)? else {
            return reply_error(server, channel, "that captain is not on these seas");
        };
        target_uuid
    };
    if target_uuid == uuid {
        return reply_error(
            server,
            channel,
            "use !sail <crew> to attack your own blockade",
        );
    }
    let id = state.alloc_id();
    let (regular_used, loyal_used, target_display) = {
        let game = state
            .games
            .get_mut(&key)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let target = game
            .players
            .get(&target_uuid)
            .ok_or_else(|| Error::msg("that captain is not on these seas"))?;
        if target.parked {
            return reply_error(server, channel, "that captain is away from the seas");
        }
        if !target.blockaded(now) || target.navy_blockade_strength <= 0 {
            return reply_error(server, channel, "that captain has no active Navy blockade");
        }
        if game.navy_harassments.len() >= MAX_NAVY_HARASSMENTS {
            return reply_error(
                server,
                channel,
                "too many harassment sorties are already at sea",
            );
        }
        if voyage::active_voyages(game, uuid) as i64 + active_harassments(game, uuid) as i64
            >= settings.max_active_voyages
        {
            return reply_error(server, channel, "you already have too many sorties at sea");
        }
        let player = game
            .players
            .get_mut(uuid)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        if player.home_crew(now) < crew {
            return reply_error(
                server,
                channel,
                &format!("you only have {} crew home", player.home_crew(now)),
            );
        }
        let regular_used = crew.min(player.home_regular());
        let loyal_used = crew - regular_used;
        player.crew_regular -= regular_used;
        player.crew_loyal -= loyal_used;
        let target_display = target_nick.to_string();
        game.navy_harassments.push(NavyHarassment {
            id,
            owner_uuid: uuid.into(),
            target_uuid,
            crew_regular: regular_used,
            crew_loyal: loyal_used,
            started_at: now,
            returns_at: now + settings.navy_harass_hours.max(1) * 3600,
            resolved: false,
        });
        (regular_used, loyal_used, target_display)
    };
    crate::schedule(
        &crate::navy_harass_job_id(server, id),
        server,
        channel,
        Some(uuid.into()),
        now + settings.navy_harass_hours.max(1) * 3600,
        "",
    )?;
    save_state(state)?;
    let sent = (regular_used + loyal_used).to_string();
    reply(
        server,
        channel,
        &themed(
            "pirate.navy_harass_departure",
            &[
                "⚓ {user} sent {crew} crew to harass {target}'s blockade; they return in {hours} hour(s).",
            ],
            &[
                ("user", &msg.display),
                ("crew", &sent),
                ("target", &target_display),
                ("hours", &settings.navy_harass_hours.to_string()),
            ],
        )?,
    )
}

fn wage_cost(regular: i64, loyal: i64, unit: i64, soft_cap: i64) -> i64 {
    let regular_at_base = regular.min(soft_cap.max(0));
    let regular_over_cap = regular.saturating_sub(regular_at_base);
    regular_at_base
        .saturating_add(regular_over_cap.saturating_mul(2))
        .saturating_add(loyal)
        .saturating_mul(unit)
}

fn summary(game: &Game, uuid: &str, settings: &PirateSettings, now: i64) -> Option<String> {
    let player = game.players.get(uuid)?;
    let active = voyage::active_voyages(game, uuid);
    let pending_details: Vec<_> = game
        .voyages
        .iter()
        .filter(|voyage| voyage.owner_uuid == uuid && voyage.resolved && !voyage.collected)
        .map(|voyage| voyage::VoyageReport::from_voyage(voyage).pending_summary())
        .collect();
    let pending = pending_details.len();
    let pending_text = if pending_details.is_empty() {
        "none".to_string()
    } else {
        pending_details.join(", ")
    };
    let parked = if player.parked { " (PARKED)" } else { "" };
    let cove = if now < player.loyal_cove_until {
        " (loyal crew in the cove)"
    } else {
        ""
    };
    let intel = match player.fresh_intel(now) {
        Some(intel) => format!(
            " Intel on {}'s isle for {}h — !raid <crew> to strike.",
            intel.target_nick,
            (intel.expires_at - now + 3_599) / 3_600
        ),
        None => String::new(),
    };
    let blockade = if player.blockaded(now) && player.navy_blockade_strength > 0 {
        " Royal Navy blockade active — use !sail <crew>; allies can !sail <you> <crew>.".into()
    } else {
        String::new()
    };
    let (brothel_gold, _) = buildings::brothel_take(&player.buildings, settings);
    let brothel_text = if brothel_gold > 0 {
        format!(", Brothel +{brothel_gold}g/day")
    } else {
        String::new()
    };
    Some(format!(
        "{}: {}g, {} rum, {} regular + {} loyal crew{}, loyalty {}, notoriety {}, {} ({}g daily upkeep{brothel_text}, {}% vault protection{}). Active voyages: {active}; collectable: {pending} ({pending_text}){parked}.{blockade}{intel}",
        player.nick_cache,
        player.gold,
        player.rum,
        player.crew_regular,
        player.home_loyal(now),
        cove,
        player.loyalty_tier,
        player.notoriety,
        buildings::describe(&player.buildings),
        buildings::total_upkeep(&player.buildings),
        (buildings::vault_protection(&player.buildings) * 100.0) as i64,
        if player.humiliated(now) { ", Humiliated" } else { "" },
    ))
}

/// A launched voyage: the captain's private confirmation, plus the nick the channel should see if
/// a false flag was flown.
pub(crate) struct Departed {
    pub(crate) summary: String,
    pub(crate) flown_as: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn do_launch(
    state: &mut State,
    server: &str,
    channel: &str,
    uuid: &str,
    nick: &str,
    kind: VoyageKind,
    target_uuid: Option<String>,
    crew: i64,
    public: bool,
    settings: &PirateSettings,
    now: i64,
) -> Result<Departed, Error> {
    let key = game_key(server);
    let id = state.alloc_id();
    let game = state
        .games
        .get_mut(&key)
        .ok_or_else(|| Error::msg("game does not exist"))?;
    voyage::validate_launch(
        game,
        uuid,
        kind,
        target_uuid.as_deref(),
        crew,
        settings,
        now,
    )
    .map_err(|error| Error::msg(launch_error(error)))?;
    let launched = voyage::launch(
        game,
        id,
        uuid,
        kind,
        target_uuid.clone(),
        crew,
        public,
        now,
        &mut rng()?,
    );
    let seconds = launched.secs.max(60);
    // What the harbour sees. A false flag puts someone else's colours on this departure — in the
    // public log and in `!here` alike — which is the whole point of having paid for one.
    let shown_nick = launched.flown_as.clone().unwrap_or_else(|| {
        game.players
            .get(uuid)
            .map(|p| p.nick_cache.clone())
            .unwrap_or_else(|| nick.into())
    });
    game.recent_departures.push(Departure {
        nick: shown_nick,
        crew,
        at: now,
    });
    if game.recent_departures.len() > MAX_DEPARTURES {
        let excess = game.recent_departures.len() - MAX_DEPARTURES;
        game.recent_departures.drain(0..excess);
    }
    schedule(
        &crate::voyage_job_id(server, id),
        server,
        channel,
        Some(uuid.into()),
        now + seconds,
        "",
    )?;
    let mission = voyage::voyage_def(kind).name;
    Ok(Departed {
        summary: format!(
            "{mission} #{id} is underway and returns in {} hour(s)",
            (seconds + 3599) / 3600
        ),
        flown_as: launched.flown_as,
    })
}

pub(crate) fn handle_channel(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    let Some((name, args)) = command(&msg.text) else {
        return Ok(());
    };
    if !matches!(
        name.as_str(),
        "crew"
            | "pay"
            | "rum"
            | "here"
            | "raid"
            | "sail"
            | "captain"
            | "collect"
            | "build"
            | "menu"
            | "park"
            | "unpark"
            | "signon"
    ) {
        return Ok(());
    }
    // A blacklisted room refuses the game outright — with a note, so players know why the isles
    // shun this channel. A merely disabled room stays silent, as before.
    if is_blacklisted(server, &msg.target) {
        return reply(
            server,
            &msg.target,
            &themed(
                "pirate.blacklisted",
                &["⚓ The Pirate Isles are blacklisted in this channel — they sail elsewhere."],
                &[],
            )?,
        );
    }
    if !setting_enabled(server, &msg.target) {
        return Ok(());
    }
    let uuid = actor(msg)?;
    let channel = msg.target.as_str();
    let key = game_key(server);
    let settings = pirate_settings(server);
    let now = now_secs();
    let mut state = load_state()?;
    ensure_game(&mut state, &key, msg, now);
    // The serverwide game learns where it is played from, so timed announcements know where to
    // broadcast and blacklisted rooms stay out of the loop.
    if let Some(game) = state.games.get_mut(&key) {
        learn_room(game, channel, now);
    }
    let needs_migration = state.games.get(&key).is_some_and(|game| {
        game.players.iter().any(|(_, player)| {
            (player.blockaded(now) && player.navy_blockade_strength <= 0)
                || (!player.blockaded(now) && player.navy_blockade_strength != 0)
        })
    });
    let migrated = if needs_migration {
        let game = state
            .games
            .get_mut(&key)
            .ok_or_else(|| Error::msg("game does not exist"))?;
        navy::backfill_strength(game, now, &settings, &mut rng()?)
    } else {
        false
    };
    if migrated {
        save_state(&state)?;
    }

    if name == "signon" {
        let result = enroll(&mut state, &key, msg, &settings, now);
        save_state(&state)?;
        return match result {
            Err(SignonError::Already) => reply(
                server,
                channel,
                &themed(
                    "pirate.signon_already",
                    &["You already hold an isle here, {user}. !crew shows how it fares."],
                    &[("user", &msg.display)],
                )?,
            ),
            Err(SignonError::Full) => reply(
                server,
                channel,
                &themed(
                    "pirate.island_full",
                    &["These seas are full — {cap} captains already hold isles here. Wait for the season to turn, {user}."],
                    &[("cap", &settings.player_cap.to_string()), ("user", &msg.display)],
                )?,
            ),
            Ok(()) => {
                reply(
                    server,
                    channel,
                    &themed(
                        "pirate.signon",
                        &["🏴‍☠️ {user} has raised their colors over a new isle! Sailing orders sent by PM."],
                        &[("user", &msg.display)],
                    )?,
                )?;
                welcome(server, &msg.nick, &settings)
            }
        };
    }

    // Everything else needs an isle. `!here` and `!captain` are lookouts' business and stay open
    // to anyone, so a channel can be watched without being enrolled into a PvP game.
    let enrolled = state
        .games
        .get(&key)
        .is_some_and(|game| game.players.contains_key(uuid));
    if !enrolled && !matches!(name.as_str(), "here" | "captain") {
        save_state(&state)?;
        return reply(
            server,
            channel,
            &themed(
                "pirate.not_signed_on",
                &["You hold no isle on these seas yet, {user}. Say !signon to claim one."],
                &[("user", &msg.display)],
            )?,
        );
    }
    // Timers are per-game, so they only need to exist once somebody actually plays here.
    if enrolled {
        ensure_jobs(&mut state, server, channel, &settings, now)?;
    }
    voyage::resolve_overdue(&mut state, server, &key, &settings, now)?;

    let parked = state
        .games
        .get(&key)
        .and_then(|game| game.players.get(uuid))
        .is_some_and(|player| player.parked);
    if parked && name != "unpark" {
        return reply(
            server,
            channel,
            &themed(
                "pirate.parked_blocked",
                &["Your ship is parked. Reply !unpark here before resuming gameplay."],
                &[],
            )?,
        );
    }

    match name.as_str() {
        "park" => {
            if parked {
                return reply(
                    server,
                    channel,
                    &themed(
                        "pirate.park_already",
                        &["Your ship is already parked."],
                        &[],
                    )?,
                );
            }
            let cancelled = pause_player(&mut state, server, channel, uuid, &settings, now)?;
            save_state(&state)?;
            // Cancelled raids announce like any other resolution: to every played room.
            let game = state.games.get(&key).cloned().unwrap_or_default();
            for resolution in &cancelled {
                voyage::deliver_resolution(server, &game, resolution)?;
            }
            reply(
                server,
                channel,
                &themed(
                    "pirate.parked",
                    &["⚓ {user} has parked their ship. Loyalty penalties are paused; reply !unpark to resume."],
                    &[("user", &msg.display)],
                )?,
            )?;
        }
        "unpark" => {
            let is_parked = state
                .games
                .get(&key)
                .and_then(|game| game.players.get(uuid))
                .ok_or_else(|| Error::msg("your island is missing"))?
                .parked;
            if !is_parked {
                return reply(
                    server,
                    channel,
                    &themed(
                        "pirate.unpark_already",
                        &["Your ship is already active."],
                        &[],
                    )?,
                );
            }
            resume_player(&mut state, server, channel, uuid, now)?;
            save_state(&state)?;
            reply(
                server,
                channel,
                &themed(
                    "pirate.unparked",
                    &["⚓ {user} has unparked their ship. Welcome back."],
                    &[("user", &msg.display)],
                )?,
            )?;
        }
        "crew" => {
            if !args.is_empty() {
                save_state(&state)?;
                return reply(
                    server,
                    channel,
                    &themed(
                        "pirate.crew_nosy",
                        &["Aye {user}, that ledger belongs to another captain. Mind your own deck; plain !crew shows your crew."],
                        &[("user", &msg.display)],
                    )?,
                );
            }
            let text = state
                .games
                .get(&key)
                .and_then(|game| summary(game, uuid, &settings, now))
                .unwrap_or_else(|| "your island is missing".into());
            save_state(&state)?;
            reply(
                server,
                channel,
                &themed("pirate.me", &["{summary}"], &[("summary", &text)])?,
            )?;
        }
        "pay" | "rum" => {
            let use_gold = name == "pay";
            // Wages are a once-a-day obligation; `paid_today` is the flag the rollover consumes,
            // so a second payment would buy nothing but still empty the hold.
            let already_paid = state
                .games
                .get(&key)
                .and_then(|game| game.players.get(uuid))
                .is_some_and(|player| player.paid_today);
            if already_paid {
                save_state(&state)?;
                return reply(
                    server,
                    channel,
                    &themed(
                        "pirate.pay_already",
                        &["Your crew are still drinking the pay you already offered today, {user}."],
                        &[("user", &msg.display)],
                    )?,
                );
            }
            let (regular, loyal) = state
                .games
                .get(&key)
                .and_then(|game| employed_crew(game, uuid))
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let crew = regular.saturating_add(loyal);
            let player = state
                .games
                .get_mut(&key)
                .and_then(|game| game.players.get_mut(uuid))
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let cost = if use_gold {
                wage_cost(
                    regular,
                    loyal,
                    settings.crew_wage_gold,
                    settings.crew_soft_cap,
                )
            } else {
                wage_cost(
                    regular,
                    loyal,
                    settings.crew_wage_rum,
                    settings.crew_soft_cap,
                )
            };
            let balance = if use_gold {
                &mut player.gold
            } else {
                &mut player.rum
            };
            if *balance < cost {
                let resource = if use_gold { "gold" } else { "rum" };
                let needed = cost.to_string();
                reply_error(
                    server,
                    channel,
                    &format!("you need {needed} {resource} to pay {crew} crew"),
                )?;
            } else {
                *balance -= cost;
                player.paid_today = true;
                player.loyalty_tier = 3;
                player.unpaid_days = 0;
                let cost = cost.to_string();
                save_state(&state)?;
                reply(
                    server,
                    channel,
                    &themed(
                        "pirate.pay",
                        &["{user} paid the crew's {resource} wages: {cost}."],
                        &[
                            ("user", &msg.display),
                            ("resource", if use_gold { "gold" } else { "rum" }),
                            ("cost", &cost),
                        ],
                    )?,
                )?;
            }
        }
        "build" => {
            let Some(name) = args.first() else {
                // Bare `!build` is the shop counter: options *and* prices against your purse.
                let player = state
                    .games
                    .get(&key)
                    .and_then(|game| game.players.get(uuid))
                    .ok_or_else(|| Error::msg("your island is missing"))?;
                let shop = buildings::shop(&player.buildings, player.gold);
                let gold = player.gold.to_string();
                save_state(&state)?;
                return reply(
                    server,
                    channel,
                    &themed(
                        "pirate.build_shop",
                        &["What shall we build, {user}? You have {gold}g: {shop}. Say !build <name> to raise one."],
                        &[("user", &msg.display), ("gold", &gold), ("shop", &shop)],
                    )?,
                );
            };
            let Some(def) = buildings::building_def(name) else {
                return reply_error(server, channel, "that building does not exist");
            };
            let player = state
                .games
                .get_mut(&key)
                .and_then(|game| game.players.get_mut(uuid))
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let Some(cost) = buildings::next_cost(&player.buildings, def) else {
                return reply_error(
                    server,
                    channel,
                    "that building is already at its maximum level",
                );
            };
            if player.gold < cost {
                return reply_error(
                    server,
                    channel,
                    &format!("you need {cost} gold to build the next level"),
                );
            }
            let level = buildings::level(&player.buildings, def.key) + 1;
            player.gold -= cost;
            buildings::set_level(&mut player.buildings, def.key, level);
            let level = level.to_string();
            let cost = cost.to_string();
            save_state(&state)?;
            reply(
                server,
                channel,
                &themed(
                    "pirate.build",
                    &["{user} built {building} L{level} for {cost}g: {effect}."],
                    &[
                        ("user", &msg.display),
                        ("building", def.name),
                        ("level", &level),
                        ("cost", &cost),
                        ("effect", def.effect),
                    ],
                )?,
            )?;
        }
        "collect" => {
            let game = state
                .games
                .get_mut(&key)
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let collected = voyage::collect_pending(game, uuid, settings.scout_intel_hours, now);
            save_state(&state)?;
            // Awarded after the commit, on the stable uuid the host stamped.
            crate::award_to(
                server,
                uuid,
                &msg.display,
                channel,
                vec![
                    ("voyages", collected.count as u64),
                    ("rum_collected", collected.rum.max(0) as u64),
                ],
            )?;
            let count = collected.count.to_string();
            let gold = collected.gold.to_string();
            let rum = collected.rum.to_string();
            let crew = collected.new_crew.to_string();
            let details = collected
                .reports
                .iter()
                .map(voyage::VoyageReport::public_summary)
                .collect::<Vec<_>>()
                .join("; ");
            let details = if details.is_empty() {
                "nothing was waiting".to_string()
            } else {
                details
            };
            for report in &collected.reports {
                if let Some(scout) = &report.scout {
                    combat::deliver_scout_snapshot(server, &msg.nick, scout)?;
                }
            }
            // A fresh report is a licence to raid that isle, privately, until it goes stale.
            if let Some(intel) = state
                .games
                .get(&key)
                .and_then(|game| game.players.get(uuid))
                .and_then(|player| player.fresh_intel(now))
            {
                reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.intel_ready",
                        &["Your scouts know {target}'s isle for the next {hours} hour(s). Reply !raid <crew> in the channel to strike before the trail goes cold."],
                        &[
                            ("target", &intel.target_nick),
                            ("hours", &settings.scout_intel_hours.to_string()),
                        ],
                    )?,
                )?;
            }
            reply(
                server,
                channel,
                &themed(
                    "pirate.collect_spoils",
                    &["{user} collected {count} voyage(s): {details}. Banked total: {gold}g, {rum} rum, and {crew} regular crew."],
                    &[
                        ("user", &msg.display),
                        ("count", &count),
                        ("details", &details),
                        ("gold", &gold),
                        ("rum", &rum),
                        ("crew", &crew),
                    ],
                )?,
            )?;
        }
        "here" => {
            let game = state
                .games
                .get(&key)
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let sea = crate::season::sea_display(&game.sea);
            let days = crate::season::days_remaining(game, &settings, now).to_string();
            let departures = if game.recent_departures.is_empty() {
                "none".into()
            } else {
                game.recent_departures
                    .iter()
                    .map(|d| format!("{} ({} crew)", d.nick, d.crew))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let text = format!(
                "{} captain(s); sea: {}; {days} day(s) remain; recent departures: {}",
                game.players.len(),
                sea,
                departures
            );
            save_state(&state)?;
            reply(
                server,
                channel,
                &themed("pirate.here", &["{text}"], &[("text", &text)])?,
            )?;
        }
        "captain" => {
            let game = state
                .games
                .get(&key)
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let target = if let Some(arg) = args.first() {
                resolve_uuid(game, server, arg)?.unwrap_or_default()
            } else {
                uuid.into()
            };
            let Some(player) = game.players.get(&target) else {
                return reply_error(server, channel, "that captain has no island here");
            };
            let text = format!("{}: {} voyages, {} raids won, {} defenses won, {}g plundered, {} prisoners taken, {} Legends.", player.nick_cache, player.career_voyages, player.career_raids_won, player.career_defenses_won, player.career_gold_plundered, player.career_prisoners_taken, player.legends.len());
            save_state(&state)?;
            reply(
                server,
                channel,
                &themed("pirate.profile", &["{text}"], &[("text", &text)])?,
            )?;
        }
        "sail" => {
            return handle_sail(
                &mut state, server, channel, msg, uuid, &args, &settings, now,
            );
        }
        "raid" => {
            // Two routes to a raid, told apart by whether the first argument is a number.
            //   !raid <crew>         — the ambush: sails on a collected scout report, silent, free
            //   !raid <nick> <crew>  — the declaration: pick anyone, but say so and wear the fame
            let stealth = args
                .first()
                .is_some_and(|value| value.parse::<i64>().is_ok());
            let intel = state
                .games
                .get(&key)
                .and_then(|game| game.players.get(uuid))
                .and_then(|player| player.fresh_intel(now).cloned());
            let (target_uuid, target_nick, crew) = if stealth {
                let crew = args
                    .first()
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(0);
                let Some(intel) = intel.clone() else {
                    return reply_error(server, channel, &launch_error(LaunchError::NoIntel));
                };
                (intel.target_uuid, intel.target_nick, crew)
            } else {
                let Some(target_nick) = args.first() else {
                    return reply_error(
                        server,
                        channel,
                        "usage is !raid <crew> after a scout, or !raid <captain> <crew> to declare",
                    );
                };
                let crew = args
                    .get(1)
                    .and_then(|value| value.parse::<i64>().ok())
                    .unwrap_or(0);
                let target_uuid = state
                    .games
                    .get(&key)
                    .and_then(|game| resolve_uuid(game, server, target_nick).ok().flatten());
                let Some(target_uuid) = target_uuid else {
                    return reply_error(server, channel, "that captain is not on these seas");
                };
                (target_uuid, (*target_nick).to_string(), crew)
            };
            let result = do_launch(
                &mut state,
                server,
                channel,
                uuid,
                &msg.nick,
                VoyageKind::Raid,
                Some(target_uuid),
                crew,
                !stealth,
                &settings,
                now,
            );
            match result {
                Ok(departed) => {
                    if let Some(player) = state
                        .games
                        .get_mut(&key)
                        .and_then(|g| g.players.get_mut(uuid))
                    {
                        if stealth {
                            // The report is spent whether or not the raid goes well.
                            player.raid_intel = None;
                        } else {
                            player.notoriety += settings.notoriety_public_raid;
                        }
                    }
                    save_state(&state)?;
                    if stealth {
                        // No channel line: the isle finds out when the sails appear.
                        let flag = match &departed.flown_as {
                            Some(nick) => format!(" You sail under {nick}'s colors."),
                            None => String::new(),
                        };
                        reply(
                            server,
                            &msg.nick,
                            &themed(
                                "pirate.raid_ambush",
                                &["Your crew slip out of the harbour under cover of dark, bound for {target}'s isle. {departure}.{flag}"],
                                &[
                                    ("target", &target_nick),
                                    ("departure", &departed.summary),
                                    ("flag", &flag),
                                ],
                            )?,
                        )?;
                    } else {
                        reply(
                            server,
                            channel,
                            &themed(
                                "pirate.raid_departure",
                                &["🏴‍☠️ {user} DECLARES WAR ON {target}! {departure}."],
                                &[
                                    ("user", &msg.display),
                                    ("target", &target_nick),
                                    ("departure", &departed.summary),
                                ],
                            )?,
                        )?;
                    }
                }
                Err(error) => reply_error(server, channel, &error.to_string())?,
            }
        }
        "menu" => {
            save_state(&state)?;
            crate::pm::open_menu(server, msg)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fold_nick, model::Voyage};

    #[test]
    fn command_parser_requires_bang() {
        assert!(command("hello").is_none());
        assert_eq!(command("!crew now").unwrap().0, "crew");
    }

    #[test]
    fn crew_arguments_are_captured_so_nosy_lookups_can_be_deflected() {
        let (_, args) = command("!crew SomeNick").unwrap();
        assert_eq!(args, vec!["SomeNick"]);
        assert!(command("!crew").unwrap().1.is_empty());
    }

    #[test]
    fn nick_lookup_is_irc_casefolded() {
        assert_eq!(fold_nick("net", "[Captain]"), "{captain}");
    }

    fn sender(uuid: &str, nick: &str) -> MessagePayload {
        serde_json::from_value(serde_json::json!({
            "user_id": uuid,
            "nick": nick,
            "display": nick,
            "target": "#isles",
            "text": "!signon",
            "is_private": false,
        }))
        .expect("message payload")
    }

    #[test]
    fn joining_is_deliberate_and_bounded_by_the_player_cap() {
        let settings = PirateSettings {
            player_cap: 2,
            ..PirateSettings::defaults()
        };
        let mut state = State::default();
        let key = game_key("net");
        let alice = sender("alice", "Alice");

        // Merely being seen in the channel does not enrol anyone.
        ensure_game(&mut state, &key, &alice, 1_000);
        assert!(state.games[&key].players.is_empty());

        assert!(enroll(&mut state, &key, &alice, &settings, 1_000).is_ok());
        let player = &state.games[&key].players["alice"];
        assert_eq!(player.gold, settings.starting_gold);
        assert_eq!(player.crew_loyal, settings.loyal_crew_count);
        assert_eq!(
            player.shield_until,
            1_000 + settings.new_player_shield_hours * 3600,
            "new captains sail under a shield"
        );

        // Signing on twice is refused rather than resetting the isle.
        assert!(matches!(
            enroll(&mut state, &key, &alice, &settings, 2_000),
            Err(SignonError::Already)
        ));
        assert_eq!(
            state.games[&key].players["alice"].gold,
            settings.starting_gold
        );

        assert!(enroll(&mut state, &key, &sender("bob", "Bob"), &settings, 1_000).is_ok());
        assert!(matches!(
            enroll(
                &mut state,
                &key,
                &sender("carol", "Carol"),
                &settings,
                1_000
            ),
            Err(SignonError::Full)
        ));
        assert_eq!(state.games[&key].players.len(), 2);
    }

    #[test]
    fn a_returning_captains_nick_cache_follows_their_rename() {
        let mut state = State::default();
        let key = game_key("net");
        enroll(
            &mut state,
            &key,
            &sender("alice", "Alice"),
            &PirateSettings::defaults(),
            1_000,
        )
        .unwrap();
        ensure_game(&mut state, &key, &sender("alice", "Alicia"), 2_000);
        assert_eq!(state.games[&key].players["alice"].nick_cache, "Alicia");
    }

    #[test]
    fn wages_include_crew_at_sea_and_charge_over_soft_cap_double() {
        let mut game = Game::default();
        game.players.insert(
            "a".into(),
            Player {
                crew_regular: 12,
                crew_loyal: 1,
                ..Default::default()
            },
        );
        game.voyages.push(Voyage {
            owner_uuid: "a".into(),
            crew_regular: 2,
            crew_loyal: 1,
            ..Default::default()
        });

        assert_eq!(employed_crew(&game, "a"), Some((14, 2)));
        assert_eq!(wage_cost(14, 2, 5, 12), 90);
    }
}
