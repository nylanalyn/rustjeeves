//! Guided private-message menu for voyage selection.

use crate::commands::{do_launch, game_key};
use crate::model::{FalseFlag, PmState, State, MAX_PM_STATES};
use crate::prisoners::{self, OfferError, Payment};
use crate::resolve_uuid;
use crate::voyage::{self, VoyageOption};
use crate::{award_to, load_state, now_secs, pirate_settings, reply, rng, save_state, themed};
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
    // The operator's kill switch has to reach here too. A session opened while the game was on
    // would otherwise stay fully playable after it was turned off — and its launches would post
    // back into a channel the operator had just silenced. The session is kept, not destroyed, so
    // re-enabling picks up where the captain left off.
    if !crate::setting_enabled(server, &channel) {
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.disabled_pm",
                &["The Pirate Isles are closed on {channel} for now. Your isle keeps until they open again."],
                &[("channel", &channel)],
            )?,
        );
    }
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
        let name = text.split_whitespace().nth(1).map(str::to_owned);
        let game = state
            .games
            .get_mut(&session.game)
            .ok_or_else(|| Error::msg("that game channel no longer exists"))?;
        let Some(name) = name else {
            // Same shop counter as the channel command, so prices are never a guess.
            let player = game
                .players
                .get(&msg.user_id)
                .ok_or_else(|| Error::msg("your island is missing"))?;
            let shop = crate::buildings::shop(&player.buildings, player.gold);
            let gold = player.gold.to_string();
            state.pm_sessions.insert(key, session);
            save_state(&state)?;
            return reply(
                server,
                &msg.nick,
                &themed(
                    "pirate.build_usage",
                    &["You have {gold}g: {shop}. Reply !build <name> to raise one."],
                    &[("gold", &gold), ("shop", &shop)],
                )?,
            );
        };
        let name = name.as_str();
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
        let offer = prisoners::offer_ransom(game, &msg.user_id, amount, id, now_secs());
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        let offer = match offer {
            Ok(offer) => offer,
            Err(error) => {
                let (key, default): (&str, &str) = match error {
                    OfferError::NoPrisoners => ("pirate.ransom_none", "You hold no prisoners."),
                    OfferError::Duplicate => (
                        "pirate.ransom_duplicate",
                        "You have already named a price for those prisoners.",
                    ),
                    OfferError::BadAmount | OfferError::NoSpace => (
                        "pirate.ransom_invalid",
                        "Give a positive ransom amount while ransom space remains.",
                    ),
                };
                return reply(server, &msg.nick, &themed(key, &[default], &[])?);
            }
        };
        let amount = amount.to_string();
        let count = offer.count.to_string();
        if !offer.target_nick.is_empty() {
            reply(server, &offer.target_nick, &themed("pirate.ransom_received", &["{holder} offers {count} prisoner(s) back for {amount}g. Reply !payransom or !abandon after opening !menu."], &[("holder", &msg.display), ("count", &count), ("amount", &amount)])?)?;
        }
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.ransom_offer",
                &["You offered {amount}g for {count} prisoner(s)."],
                &[("amount", &amount), ("count", &count)],
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
        let release = prisoners::release_prisoners(
            game,
            &msg.user_id,
            maroon,
            settings.notoriety_maroon,
            &mut rng()?,
        );
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        let Some(release) = release else {
            return reply(
                server,
                &msg.nick,
                &themed("pirate.prisoners_none", &["You hold no prisoners."], &[])?,
            );
        };
        if maroon {
            award_to(
                server,
                &msg.user_id,
                &msg.display,
                &channel,
                vec![("prisoners_marooned", release.total.max(0) as u64)],
            )?;
        }
        let count_text = if maroon {
            release.total.to_string()
        } else {
            release.pressed.to_string()
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
        let outcome = if abandon {
            prisoners::abandon_ransom(game, &msg.user_id)
        } else {
            prisoners::pay_ransom(game, &msg.user_id)
        };
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        match outcome {
            Payment::NoOffer => {
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.ransom_missing",
                        &["You have no ransom awaiting you."],
                        &[],
                    )?,
                )
            }
            Payment::Stale => {
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.ransom_stale",
                        &["Those prisoners are beyond ransom now. The offer is withdrawn."],
                        &[],
                    )?,
                )
            }
            Payment::Short { amount } => {
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.ransom_unpaid",
                        &["You need {amount}g to pay this ransom."],
                        &[("amount", &amount.to_string())],
                    )?,
                )
            }
            Payment::Paid { .. } => {}
        }
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
        // Fly the captain's canonical nick, not whatever spelling was typed, so the forged
        // departure is indistinguishable from a real one.
        let flag_nick = game
            .players
            .get(&target_uuid)
            .map(|player| player.nick_cache.clone())
            .filter(|nick| !nick.is_empty())
            .unwrap_or_else(|| crate::model::clean_nick(target_nick));
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
            nick: flag_nick.clone(),
        });
        player.false_flag_ready_at = now + settings.false_flag_cooldown_hours * 3600;
        state.pm_sessions.insert(key, session);
        save_state(&state)?;
        return reply(
            server,
            &msg.nick,
            &themed(
                "pirate.flag_bought",
                &["Your next quiet voyage will fly {target}'s colors — a public !raid declaration would name you anyway, so the flag keeps until then."],
                &[("target", &flag_nick)],
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
            Ok(departed) => {
                session.level = "menu".into();
                session.data = serde_json::Value::Null;
                state.pm_sessions.insert(key, session);
                save_state(&state)?;
                let own_nick = if msg.display.trim().is_empty() {
                    msg.nick.as_str()
                } else {
                    msg.display.as_str()
                };
                // The channel sees whoever's colours are flying, not necessarily who sailed.
                let user = departed.flown_as.as_deref().unwrap_or(own_nick);
                reply(
                    server,
                    &channel,
                    &themed(
                        "pirate.voyage_departure",
                        &["⚓ {user} sent {crew} crew on a {mission} mission."],
                        &[("user", user), ("crew", &crew_count), ("mission", mission)],
                    )?,
                )?;
                let flag = match &departed.flown_as {
                    Some(nick) => format!(" The harbour saw {nick}'s colors leave, not yours."),
                    None => String::new(),
                };
                return reply(
                    server,
                    &msg.nick,
                    &themed(
                        "pirate.menu_departure",
                        &["{departure}.{flag}"],
                        &[("departure", &departed.summary), ("flag", &flag)],
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
