//! Channel-facing Pirate Isles commands.

use crate::buildings;
use crate::model::{
    clean_nick, Departure, Game, Player, State, VoyageKind, MAX_DEPARTURES, MAX_PLAYERS,
};
use crate::voyage::{self, LaunchError};
use crate::{
    ensure_jobs, load_state, now_secs, pirate_settings, reply, resolve_uuid, rng, save_state,
    schedule, setting_enabled, themed, PirateSettings,
};
use extism_pdk::Error;
use jeeves_abi::MessagePayload;

pub(crate) fn game_key(server: &str, channel: &str) -> String {
    format!("{server}/{channel}")
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

fn ensure_game<'a>(
    state: &'a mut State,
    key: &str,
    msg: &MessagePayload,
    settings: &PirateSettings,
    now: i64,
) -> Result<&'a mut Game, Error> {
    let game = state.games.entry(key.to_owned()).or_insert_with(|| Game {
        season_started: now,
        ..Default::default()
    });
    if game.season_started == 0 {
        game.season_started = now;
    }
    if !game.players.contains_key(&msg.user_id) {
        if game.players.len() >= settings.player_cap.min(MAX_PLAYERS as i64) as usize {
            return Err(Error::msg("this island is already full"));
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
    } else if let Some(player) = game.players.get_mut(&msg.user_id) {
        player.nick_cache = clean_nick(&msg.nick);
    }
    Ok(game)
}

fn launch_error(error: LaunchError) -> String {
    match error {
        LaunchError::MinCrew(min) => format!("that voyage needs at least {min} crew"),
        LaunchError::CrewShort(home) => format!("you only have {home} crew home"),
        LaunchError::TooManyVoyages(max) => format!("you already have {max} active voyages"),
        LaunchError::Blockaded => "the Royal Navy blockade prevents launches".into(),
        LaunchError::NoTarget => "that target is not available".into(),
        LaunchError::SelfTarget => "you cannot target yourself".into(),
        LaunchError::TargetShielded => "that captain is still under a new-captain shield".into(),
        LaunchError::TargetBusy => "that captain already has two raids inbound".into(),
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

fn summary(game: &Game, uuid: &str, now: i64) -> Option<String> {
    let player = game.players.get(uuid)?;
    let active = voyage::active_voyages(game, uuid);
    let pending = game
        .voyages
        .iter()
        .filter(|v| v.owner_uuid == uuid && v.resolved && !v.collected)
        .count();
    let cove = if now < player.loyal_cove_until {
        " (loyal crew in the cove)"
    } else {
        ""
    };
    Some(format!(
        "{}: {}g, {} rum, {} regular + {} loyal crew{}, loyalty {}, notoriety {}, {} ({}g daily upkeep, {}% vault protection{}). Active voyages: {active}; collectable: {pending}.",
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
) -> Result<String, Error> {
    let key = game_key(server, channel);
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
    let seconds = voyage::launch(
        game,
        id,
        uuid,
        kind,
        target_uuid.clone(),
        crew,
        public,
        now,
        &mut rng()?,
    )
    .max(60);
    let shown_nick = game
        .players
        .get(uuid)
        .map(|p| p.nick_cache.clone())
        .unwrap_or_else(|| nick.into());
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
        &crate::voyage_job_id(server, channel, id),
        server,
        channel,
        Some(uuid.into()),
        now + seconds,
        "",
    )?;
    let mission = voyage::voyage_def(kind).name;
    Ok(format!(
        "{mission} #{id} is underway and returns in {} hour(s)",
        (seconds + 3599) / 3600
    ))
}

pub(crate) fn handle_channel(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if !setting_enabled(server, &msg.target) {
        return Ok(());
    }
    let Some((name, args)) = command(&msg.text) else {
        return Ok(());
    };
    if !matches!(
        name.as_str(),
        "me" | "pay" | "rum" | "here" | "raid" | "profile" | "collect" | "build" | "menu"
    ) {
        return Ok(());
    }
    let uuid = actor(msg)?;
    let channel = msg.target.as_str();
    let key = game_key(server, channel);
    let settings = pirate_settings(server, channel);
    let now = now_secs();
    let mut state = load_state()?;
    ensure_game(&mut state, &key, msg, &settings, now)?;
    ensure_jobs(&mut state, server, channel, &settings, now)?;
    voyage::resolve_overdue(&mut state, server, channel, &key, &settings, now)?;

    match name.as_str() {
        "me" => {
            let text = state
                .games
                .get(&key)
                .and_then(|game| summary(game, uuid, now))
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
            let player = state
                .games
                .get_mut(&key)
                .and_then(|game| game.players.get_mut(uuid))
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let crew = player.home_crew(now);
            let cost = if use_gold {
                crew * settings.crew_wage_gold
            } else {
                crew * settings.crew_wage_rum
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
                return reply_error(
                    server,
                    channel,
                    "usage is !build <vault|cove|walls|shipyard|tavern>",
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
            let collected = voyage::collect_pending(game, uuid, now);
            save_state(&state)?;
            let count = collected.count.to_string();
            let gold = collected.gold.to_string();
            let rum = collected.rum.to_string();
            reply(
                server,
                channel,
                &themed(
                    "pirate.collect",
                    &["{user} collected {count} voyage(s): {gold}g and {rum} rum."],
                    &[
                        ("user", &msg.display),
                        ("count", &count),
                        ("gold", &gold),
                        ("rum", &rum),
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
        "profile" => {
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
        "raid" => {
            let Some(target_nick) = args.first() else {
                return reply_error(server, channel, "usage is !raid <captain> <crew>");
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
            let result = do_launch(
                &mut state,
                server,
                channel,
                uuid,
                &msg.nick,
                VoyageKind::Raid,
                Some(target_uuid),
                crew,
                true,
                &settings,
                now,
            );
            match result {
                Ok(departure) => {
                    if let Some(player) = state
                        .games
                        .get_mut(&key)
                        .and_then(|g| g.players.get_mut(uuid))
                    {
                        player.notoriety += settings.notoriety_public_raid;
                    }
                    save_state(&state)?;
                    reply(
                        server,
                        channel,
                        &themed(
                            "pirate.raid_departure",
                            &["{user} has declared a raid: {departure}."],
                            &[("user", &msg.display), ("departure", &departure)],
                        )?,
                    )?;
                }
                Err(error) => reply_error(server, channel, &error.to_string())?,
            }
        }
        "menu" => {
            save_state(&state)?;
            crate::pm::open_menu(server, channel, msg)?;
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fold_nick;

    #[test]
    fn command_parser_requires_bang() {
        assert!(command("hello").is_none());
        assert_eq!(command("!me now").unwrap().0, "me");
    }

    #[test]
    fn nick_lookup_is_irc_casefolded() {
        assert_eq!(fold_nick("net", "[Captain]"), "{captain}");
    }
}
