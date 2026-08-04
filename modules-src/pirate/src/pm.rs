//! Guided private-message menu for voyage selection.

use crate::commands::{do_launch, game_key};
use crate::model::{FalseFlag, PmState, Ransom, State, MAX_PM_STATES, MAX_RANSOMS};
use crate::resolve_uuid;
use crate::voyage::{self, VoyageOption};
use crate::{load_state, now_secs, pirate_settings, reply, rng, save_state, themed};
use extism_pdk::Error;
use jeeves_abi::MessagePayload;

fn session_key(server: &str, uuid: &str) -> String {
    format!("{server}/{uuid}")
}

fn channel_from_game<'a>(server: &str, game: &'a str) -> Option<&'a str> {
    game.strip_prefix(server)?.strip_prefix('/')
}

fn menu_text(options: &[VoyageOption]) -> String {
    options
        .iter()
        .enumerate()
        .map(|(i, option)| format!("{}: {}", i + 1, option.label()))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn send_menu(server: &str, target: &str, options: &[VoyageOption]) -> Result<(), Error> {
    let choices = menu_text(options);
    reply(
        server,
        target,
        &themed(
            "pirate.menu",
            &["Choose a voyage with !pirate <number>: {choices}"],
            &[("choices", &choices)],
        )?,
    )
}

fn menu_input(text: &str) -> &str {
    let trimmed = text.trim();
    let Some(rest) = trimmed
        .strip_prefix("!pirate")
        .or_else(|| trimmed.strip_prefix("pirate"))
    else {
        return trimmed;
    };
    rest.trim()
}

pub(crate) fn open_menu(server: &str, channel: &str, msg: &MessagePayload) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Err(Error::msg("pirate menu opened without a stable profile id"));
    }
    let mut state = load_state()?;
    if !state
        .pm_sessions
        .contains_key(&session_key(server, &msg.user_id))
        && state.pm_sessions.len() >= MAX_PM_STATES
    {
        return Err(Error::msg("the pirate menu is busy; try again shortly"));
    }
    let key = session_key(server, &msg.user_id);
    let mut session = PmState {
        game: game_key(server, channel),
        level: "menu".into(),
        data: serde_json::Value::Null,
        last_active: crate::now_secs(),
    };
    roll_menu(&mut state, server, &msg.user_id, &mut session, channel)?;
    let options: Vec<VoyageOption> = serde_json::from_value(session.data.clone())?;
    state.pm_sessions.insert(key, session);
    save_state(&state)?;
    send_menu(server, &msg.nick, &options)
}

fn roll_menu(
    state: &mut State,
    server: &str,
    uuid: &str,
    session: &mut PmState,
    channel: &str,
) -> Result<(), Error> {
    let settings = pirate_settings(server, channel);
    let options = {
        let game = state
            .games
            .get(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        voyage::roll_options(
            game,
            uuid,
            settings.voyage_options_count as usize,
            now_secs(),
            &mut rng()?,
        )
    };
    session.data = serde_json::to_value(&options)?;
    session.last_active = now_secs();
    Ok(())
}

pub(crate) fn handle_pm(server: &str, msg: &MessagePayload) -> Result<(), Error> {
    if msg.user_id.is_empty() {
        return Ok(());
    }
    let mut state = load_state()?;
    let key = session_key(server, &msg.user_id);
    let Some(mut session) = state.pm_sessions.remove(&key) else {
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.menu_missing",
                &["Use !menu in a Pirate Isles channel first."],
                &[],
            )?,
        );
    };
    let Some(channel) = channel_from_game(server, &session.game).map(str::to_owned) else {
        return Err(Error::msg("pirate PM session has an invalid game key"));
    };
    if state
        .games
        .get(&session.game)
        .and_then(|game| game.players.get(&msg.user_id))
        .is_some_and(|player| player.parked)
    {
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.parked_pm",
                &["Your ship is parked. Reply !unpark in the Pirate Isles channel before using the PM menu."],
                &[],
            )?,
        );
    }
    let settings = pirate_settings(server, &channel);
    let text = msg.text.trim();
    let normalized = text.to_ascii_lowercase();
    let menu_text = menu_input(text);
    let menu_normalized = menu_text.to_ascii_lowercase();
    if normalized.starts_with("!build") || normalized == "build" {
        let Some(name) = text.split_whitespace().nth(1) else {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.build_usage",
                    &["Reply !build <vault|cove|walls|shipyard|tavern>."],
                    &[],
                )?,
            );
        };
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let Some(def) = crate::buildings::building_def(name) else {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.build_missing",
                    &["That building does not exist."],
                    &[],
                )?,
            );
        };
        let player = game
            .players
            .get_mut(&msg.user_id)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        let Some(cost) = crate::buildings::next_cost(&player.buildings, def) else {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.build_max",
                    &["That building is already maxed."],
                    &[],
                )?,
            );
        };
        if player.gold < cost {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.build_gold",
                    &["You need {cost}g for that upgrade."],
                    &[("cost", &cost.to_string())],
                )?,
            );
        }
        let level = crate::buildings::level(&player.buildings, def.key) + 1;
        player.gold -= cost;
        crate::buildings::set_level(&mut player.buildings, def.key, level);
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.build_done",
                &["You built {building} L{level}."],
                &[("building", def.name), ("level", &level.to_string())],
            )?,
        );
    }
    if normalized.starts_with("!ransom") || normalized == "ransom" {
        let amount = text
            .split_whitespace()
            .nth(1)
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let id = state.alloc_id();
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let Some(prisoner) = game
            .prisoners
            .iter()
            .find(|prisoner| prisoner.holder_uuid == msg.user_id)
            .cloned()
        else {
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed("pirate.ransom_none", &["You hold no prisoners."], &[])?,
            );
        };
        let target_nick = game
            .players
            .get(&prisoner.origin_uuid)
            .map(|player| player.nick_cache.clone())
            .unwrap_or_default();
        if amount <= 0 || amount > 100_000 || game.ransoms.len() >= MAX_RANSOMS {
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.ransom_invalid",
                    &["Give a positive ransom amount while ransom space remains."],
                    &[],
                )?,
            );
        }
        game.ransoms.push(Ransom {
            id,
            holder_uuid: msg.user_id.clone(),
            target_uuid: prisoner.origin_uuid.clone(),
            amount,
            count: prisoner.count,
            offered_at: now_secs(),
        });
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        let amount = amount.to_string();
        if !target_nick.is_empty() {
            reply(server, &target_nick, &themed("pirate.ransom_received", &["{holder} offers {count} prisoner(s) back for {amount}g. Reply !payransom or !abandon after opening !menu."], &[("holder", &msg.display), ("count", &prisoner.count.to_string()), ("amount", &amount)])?)?;
        }
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.ransom_offer",
                &["You offered {amount}g for {count} prisoner(s)."],
                &[("amount", &amount), ("count", &prisoner.count.to_string())],
            )?,
        );
    }
    if normalized == "!pressgang"
        || normalized == "pressgang"
        || normalized == "!maroon"
        || normalized == "maroon"
    {
        let maroon = normalized.contains("maroon");
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let held = game
            .prisoners
            .iter()
            .filter(|prisoner| prisoner.holder_uuid == msg.user_id)
            .cloned()
            .collect::<Vec<_>>();
        if held.is_empty() {
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed("pirate.prisoners_none", &["You hold no prisoners."], &[])?,
            );
        }
        game.prisoners
            .retain(|prisoner| prisoner.holder_uuid != msg.user_id);
        let total: i64 = held.iter().map(|prisoner| prisoner.count.max(0)).sum();
        let mut pressed = 0i64;
        let mut escaped = 0i64;
        if maroon {
            if let Some(player) = game.players.get_mut(&msg.user_id) {
                player.notoriety += settings.notoriety_maroon * total;
                player.career_prisoners_marooned += total;
            }
        } else {
            let mut random = rng()?;
            for prisoner in held {
                let count = prisoner.count.clamp(0, 1_000);
                for _ in 0..count {
                    if random.chance(0.5) {
                        pressed += 1;
                    } else {
                        escaped += 1;
                    }
                }
                if let Some(origin) = game.players.get_mut(&prisoner.origin_uuid) {
                    origin.crew_regular += escaped;
                    escaped = 0;
                }
            }
            if let Some(player) = game.players.get_mut(&msg.user_id) {
                player.crew_regular += pressed;
            }
        }
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        let count_text = if maroon {
            total.to_string()
        } else {
            pressed.to_string()
        };
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.prisoners_resolved",
                &["You {action} {count} prisoner(s)."],
                &[
                    ("action", if maroon { "marooned" } else { "press-ganged" }),
                    ("count", &count_text),
                ],
            )?,
        );
    }
    if normalized == "!payransom"
        || normalized == "payransom"
        || normalized == "!abandon"
        || normalized == "abandon"
    {
        let abandon = normalized.contains("abandon");
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let Some(index) = game
            .ransoms
            .iter()
            .position(|ransom| ransom.target_uuid == msg.user_id)
        else {
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.ransom_missing",
                    &["You have no ransom awaiting you."],
                    &[],
                )?,
            );
        };
        let ransom = game.ransoms[index].clone();
        if abandon {
            game.ransoms.remove(index);
            if let Some(player) = game.players.get_mut(&msg.user_id) {
                player.notoriety -= 1;
            }
        } else {
            let Some(player) = game.players.get_mut(&msg.user_id) else {
                return Err(Error::msg("your island is missing"));
            };
            if player.gold < ransom.amount {
                state.pm_sessions.insert(key, session);
                save_state(&state)?;
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.ransom_unpaid",
                        &["You need {amount}g to pay this ransom."],
                        &[("amount", &ransom.amount.to_string())],
                    )?,
                );
            }
            player.gold -= ransom.amount;
            player.crew_regular += ransom.count;
            if let Some(holder) = game.players.get_mut(&ransom.holder_uuid) {
                holder.gold += ransom.amount;
            }
            game.prisoners.retain(|prisoner| {
                !(prisoner.holder_uuid == ransom.holder_uuid && prisoner.origin_uuid == msg.user_id)
            });
            game.ransoms.remove(index);
        }
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.ransom_done",
                &["Your crew has been {action}."],
                &[("action", if abandon { "abandoned" } else { "freed" })],
            )?,
        );
    }
    if normalized.starts_with("!flag") || normalized == "flag" {
        let Some(target_nick) = text.split_whitespace().nth(1) else {
            return reply(
                server,
                &msg.nick,
                &themed("pirate.flag_usage", &["Reply !flag <captain>."], &[])?,
            );
        };
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let Some(target_uuid) = resolve_uuid(game, server, target_nick)? else {
            return reply(
                server,
                &msg.nick,
                &themed("pirate.flag_missing", &["That captain is not here."], &[])?,
            );
        };
        if target_uuid == msg.user_id {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.flag_self",
                    &["A false flag must belong to another captain."],
                    &[],
                )?,
            );
        }
        let now = now_secs();
        let player = game
            .players
            .get_mut(&msg.user_id)
            .ok_or_else(|| Error::msg("your island is missing"))?;
        if player.gold < settings.false_flag_cost {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.flag_gold",
                    &["You need {cost}g for a false flag."],
                    &[("cost", &settings.false_flag_cost.to_string())],
                )?,
            );
        }
        if now < player.false_flag_ready_at {
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.flag_cooldown",
                    &["Your flag-maker is not ready yet."],
                    &[],
                )?,
            );
        }
        player.gold -= settings.false_flag_cost;
        player.false_flag = Some(FalseFlag {
            nick: target_nick
                .chars()
                .filter(|c| !c.is_control())
                .take(32)
                .collect(),
        });
        player.false_flag_ready_at = now + settings.false_flag_cooldown_hours * 3600;
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.flag_bought",
                &["Your next voyage will fly {target}'s colors."],
                &[("target", target_nick)],
            )?,
        );
    }
    if normalized == "!menu"
        || normalized == "menu"
        || normalized == "!voyage"
        || normalized == "voyage"
    {
        roll_menu(&mut state, server, &msg.user_id, &mut session, &channel)?;
        let options: Vec<VoyageOption> = serde_json::from_value(session.data.clone())?;
        send_menu(server, &msg.nick, &options)?;
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return Ok(());
    }
    if session.level == "crew" {
        let crew = menu_text
            .strip_prefix("crew")
            .unwrap_or(menu_text)
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|crew| *crew > 0);
        let Some(crew) = crew else {
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.menu_crew",
                    &["Reply with !pirate crew <count> using a positive crew count."],
                    &[],
                )?,
            );
        };
        let option: VoyageOption = serde_json::from_value(session.data.clone())?;
        let mission = voyage::voyage_def(option.kind).name;
        let crew_count = crew.to_string();
        let result = do_launch(
            &mut state,
            server,
            &channel,
            &msg.user_id,
            &msg.nick,
            option.kind,
            option.target_uuid,
            crew,
            false,
            &settings,
            now_secs(),
        );
        match result {
            Ok(departure) => {
                session.level = "menu".into();
                session.data = serde_json::Value::Null;
                state.pm_sessions.insert(key, session);
                save_state(&state)?;
                let user = if msg.display.trim().is_empty() {
                    msg.nick.as_str()
                } else {
                    msg.display.as_str()
                };
                reply(
                    server,
                    &channel,
                    &themed(
                        "pirate.voyage_departure",
                        &["⚓ {user} sent {crew} crew on a {mission} mission."],
                        &[("user", user), ("crew", &crew_count), ("mission", mission)],
                    )?,
                )?;
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.menu_departure",
                        &["{departure}."],
                        &[("departure", &departure)],
                    )?,
                );
            }
            Err(error) => {
                state.pm_sessions.insert(key, session);
                save_state(&state)?;
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.menu_error",
                        &["Arrr: {error}."],
                        &[("error", &error.to_string())],
                    )?,
                );
            }
        }
    }
    let Some(choice) = menu_normalized.parse::<usize>().ok().filter(|n| *n > 0) else {
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.menu_help",
                &["Reply !voyage for options, then use !pirate <number>."],
                &[],
            )?,
        );
    };
    let options: Vec<VoyageOption> =
        serde_json::from_value(session.data.clone()).unwrap_or_default();
    let Some(option) = options.get(choice - 1).cloned() else {
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.menu_choice",
                &["That is not one of the offered voyages."],
                &[],
            )?,
        );
    };
    session.level = "crew".into();
    session.data = serde_json::to_value(option)?;
    state.pm_sessions.insert(key, session);
    save_state(&state)?;
    reply(
        server,
        &msg.nick,
        &themed(
            "pirate.menu_crew_prompt",
            &["How many crew will sail? Reply !pirate crew <count>; you have {available} available."],
            &[(
                "available",
                &state
                    .games
                    .get(&format!("{server}/{channel}"))
                    .and_then(|game| game.players.get(&msg.user_id))
                    .map(|player| player.home_crew(now_secs()))
                    .unwrap_or(0)
                    .to_string(),
            )],
        )?,
    )?;
    Ok(())
}
