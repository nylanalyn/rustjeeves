//! `!cast` — putting a line in the water.
//!
//! Parses the `[location] [bait <XP>]` argument, checks everything that can stop a cast (an
//! existing line, a rod in the workshop, a dynamite or danger-mode ban, an unmet level
//! requirement), and records the pending [`Cast`]. [`crate::reel`] is the other half.
//!
//! `!cast <nick>` delegates: an angler serving a dynamite ban can have someone else cast for them,
//! and the *target* owns the resulting cast and all its rewards.

use super::*;

#[derive(Debug, PartialEq)]
pub(super) struct CastRequest {
    pub(super) location: String,
    pub(super) bait_xp: i64,
}

pub(super) fn parse_cast_request(arg: &str) -> Result<CastRequest, &'static str> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let Some(bait_index) = words
        .iter()
        .position(|word| word.eq_ignore_ascii_case("bait"))
    else {
        return Ok(CastRequest {
            location: arg.trim().to_string(),
            bait_xp: 0,
        });
    };
    if bait_index + 2 != words.len() {
        return Err("Use !cast [location] bait <XP>, for example !cast Purple Void bait 500.");
    }
    let bait_xp = words[bait_index + 1]
        .parse::<i64>()
        .map_err(|_| "Bait must be an XP amount from 100 to 1700, in steps of 100.")?;
    if !(BAIT_XP_PER_HOUR..=MAX_BAIT_XP).contains(&bait_xp) || bait_xp % BAIT_XP_PER_HOUR != 0 {
        return Err("Bait must be an XP amount from 100 to 1700, in steps of 100.");
    }
    Ok(CastRequest {
        location: words[..bait_index].join(" "),
        bait_xp,
    })
}

pub(super) fn delegated_cast_target(arg: &str) -> Option<&str> {
    let target = arg.trim();
    (!target.is_empty()
        && target.split_whitespace().count() == 1
        && find_location(target).is_none())
    .then_some(target)
}

pub(super) fn cmd_cast(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    // A single non-location nick can cast on behalf of someone whose second dynamite mishap
    // removed both hands. The target owns the active cast and will receive every later reward
    // from `!reel`; the helper never receives a cast of their own.
    if let Some(target) = delegated_cast_target(arg) {
        if let Some(profile) = profile_for_nick(ctx.server, target)? {
            if profile.id == ctx.user_id {
                return ctx.say(
                    "fishing.cast_delegate_self",
                    &["{user}, !cast <nick> is for helping another angler with a dynamite ban."],
                    &[("user", ctx.addr)],
                );
            }

            let mut state = load_state()?;
            let target_key = format!("{}/{}", ctx.server, profile.id);
            let migrated = migrate_identity(&mut state, ctx.server, &profile.nick, &profile.id);
            let ban = state
                .players
                .get_mut(&target_key)
                .and_then(|player| active_dynamite_ban(player, now_secs()));
            // `active_dynamite_ban` may settle an expired recovery, so persist even when this
            // attempt cannot be delegated. This also preserves any identity migration above.
            if migrated || state.players.contains_key(&target_key) {
                save_state(&state)?;
            }
            if ban.is_none() {
                return ctx.say(
                    "fishing.cast_delegate_not_banned",
                    &["{nick} is not currently serving a dynamite fishing ban, so they must cast for themself."],
                    &[("nick", &profile.nick)],
                );
            }

            let delegated = Ctx {
                server: ctx.server,
                dest: ctx.dest,
                nick: &profile.nick,
                addr: &profile.nick,
                user_id: &profile.id,
                role: None,
            };
            return cmd_cast_inner(&delegated, "", true);
        }
    }

    cmd_cast_inner(ctx, arg, false)
}

fn cmd_cast_inner(ctx: &Ctx, arg: &str, allow_dynamite_ban: bool) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let now = now_secs();

    if let Some(cast) = state.active_casts.get(&key) {
        let elapsed = format_elapsed(now - cast.timestamp);
        ctx.say(
            "cast_already_active",
            &["{user}, you already have a line in the water at {location} ({elapsed}). Use !reel to bring it in."],
            &[
                ("user", ctx.addr),
                ("location", &cast.location),
                ("elapsed", &elapsed),
            ],
        )?;
        return Ok(());
    }

    let request = match parse_cast_request(arg) {
        Ok(request) => request,
        Err(message) => return ctx.say_text("cast_usage", message),
    };
    if request.bait_xp > 0 && !expansion_active(now) {
        return ctx.say(
            "bait_not_available",
            &["Bait becomes available when the new fishing season begins."],
            &[],
        );
    }

    let player = state.players.entry(key.clone()).or_default();
    player.nick = ctx.nick.to_string();
    // Snapshot a legacy save before this cast changes any lifetime counters.
    season_stats_mut(player);

    // A rod in the workshop blocks new casts. An elapsed fix window is settled (committed hours
    // folded into rod_strength) so casting resumes the moment the fix completes.
    settle_rod(player, now);
    if rod_in_workshop(player, now) {
        let remaining = format_elapsed(player.fixing_until.unwrap() - now);
        return ctx.say(
            "cast_while_fixing",
            &["{user}, your rod is in the workshop — {remaining} until it's ready to fish again."],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }

    // No hands, no fishing — the price of a previous !dynamite.
    if !allow_dynamite_ban {
        if let Some(exp) = active_dynamite_ban(player, now) {
            let days = (exp - now) / 86_400 + 1;
            return ctx.say_text(
                "cast_no_hands",
                &format!(
                    "{} approaches the water's edge, holds up both stumps in quiet contemplation, \
             and shuffles back home. ({days} day(s) remaining on the ban)",
                    ctx.addr
                ),
            );
        }
    }
    if let Some(exp) = player.danger.active_ban(now) {
        let remaining = format_elapsed(exp - now);
        save_state(&state)?;
        return ctx.say(
            "fishing.danger.cast_banned",
            &[
                "{user}, you are currently insufficiently limbed to operate fishing equipment. Rehabilitation concludes in {remaining}.",
            ],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }
    let level = player.level;

    // Pick the location: a named (unlocked) one, or the best for the player's level.
    let (location, named) = if request.location.is_empty() {
        (location_for_level(level).clone(), false)
    } else {
        match find_location(&request.location) {
            Some(loc) if loc.level > max_level(now) => {
                return ctx.say(
                    "cast_location_dormant",
                    &["That part of the Void has not opened yet."],
                    &[],
                );
            }
            Some(loc) if loc.level <= level => (loc.clone(), true),
            Some(loc) => {
                ctx.say_text(
                    "cast_location_locked",
                    &format!(
                        "{}, you haven't unlocked {} yet — need level {} (you're {}).",
                        ctx.addr, loc.name, loc.level, level
                    ),
                )?;
                return Ok(());
            }
            None => {
                let avail: Vec<&str> = data()
                    .locations
                    .iter()
                    .filter(|l| l.level <= level && l.level <= max_level(now))
                    .map(|l| l.name.as_str())
                    .collect();
                ctx.say_text(
                    "cast_location_unknown",
                    &format!(
                        "{}, no such spot. You can fish: {}.",
                        ctx.addr,
                        avail.join(", ")
                    ),
                )?;
                return Ok(());
            }
        }
    };

    if request.bait_xp > player.xp {
        return ctx.say(
            "bait_no_xp",
            &["{user}, that bait costs {cost} XP, but you only have {xp}."],
            &[
                ("user", ctx.addr),
                ("cost", &request.bait_xp.to_string()),
                ("xp", &player.xp.to_string()),
            ],
        );
    }
    player.xp -= request.bait_xp;
    let bait_hours = request.bait_xp / BAIT_XP_PER_HOUR;

    let champ_dist = champion_bonus(&state, ctx.server, &key, "distance");
    let mut rng = ctx.rng(&mut state)?;
    let player = state.players.get_mut(&key).unwrap();
    let mut distance = cast_distance(&mut rng, level, &location);
    let art_dist = artifact_bonus(player, "distance");
    if art_dist > 0.0 {
        distance = round1(distance * (1.0 + art_dist));
    }
    if champ_dist > 0.0 {
        distance = round1(distance * (1.0 + champ_dist));
    }
    player.total_casts += 1;
    if distance > player.furthest_cast {
        player.furthest_cast = distance;
    }
    season_stats_mut(player).furthest_cast = season_stats_mut(player).furthest_cast.max(distance);
    let artifact = player.artifact.clone();
    state.active_casts.insert(
        key.clone(),
        Cast {
            timestamp: now,
            distance,
            location: location.name.clone(),
            allow_lower_fish: !named,
            bait_hours,
        },
    );

    let danger_loadout = state
        .players
        .get(&key)
        .filter(|player| player.danger.enabled)
        .map(|player| player.danger.weapon().to_string());
    let cast_msg = if let Some(weapon) = danger_loadout.as_deref() {
        themed(
            "fishing.danger.cast",
            &[
                "You fire the {weapon} {location} to establish dominance. The lake returns fire. Engagement distance: {distance}m.",
            ],
            &[
                ("weapon", weapon),
                ("location", &location_prep(&location)),
                ("distance", &distance.to_string()),
            ],
        )?
    } else {
        match &artifact {
            Some(a) => format!(
                "{}, it sails {}m {}, {}...",
                a.cast_text,
                distance,
                location_prep(&location),
                a.float_text
            ),
            None => {
                let template = rng
                    .choice(&data().cast_messages)
                    .cloned()
                    .unwrap_or_else(|| "You cast {distance}m {loc}...".into());
                template
                    .replace("{distance}", &format!("{distance}"))
                    .replace("{loc}", &location_prep(&location))
            }
        }
    };
    let announce = maybe_trigger_event(&mut rng, &mut state, ctx.server, &location.name, now);
    save_state(&state)?;
    if request.bait_xp > 0 {
        ctx.say(
            "cast_success_baited",
            &["{user}, {cast} The bait cost {cost} XP and brings peak rarity {hours}h closer for this cast."],
            &[
                ("user", ctx.addr),
                ("cast", &cast_msg),
                ("cost", &request.bait_xp.to_string()),
                ("hours", &bait_hours.to_string()),
            ],
        )?;
    } else {
        // Keep the existing theme key and placeholder contract stable for operators who already
        // customised ordinary cast messages.
        if danger_loadout.is_some() {
            ctx.say(
                "fishing.danger.cast_success",
                &["{user}, {cast}"],
                &[("user", ctx.addr), ("cast", &cast_msg)],
            )?;
        } else {
            ctx.say_text("cast_success", &format!("{}, {}", ctx.addr, cast_msg))?;
        }
    }
    if let Some(a) = announce {
        ctx.say_text("event_started", &a)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_bait_parser_is_bounded_and_keeps_multiword_locations() {
        assert_eq!(
            parse_cast_request("Purple Void bait 500"),
            Ok(CastRequest {
                location: "Purple Void".into(),
                bait_xp: 500,
            })
        );
        assert_eq!(
            parse_cast_request("bait 1700"),
            Ok(CastRequest {
                location: String::new(),
                bait_xp: 1700,
            })
        );
        assert!(parse_cast_request("bait 50").is_err());
        assert!(parse_cast_request("bait 1800").is_err());
        assert!(parse_cast_request("bait 500 extra").is_err());
    }

    #[test]
    fn delegated_cast_target_only_accepts_a_single_non_location_nick() {
        assert_eq!(
            delegated_cast_target("HelpfulAngler"),
            Some("HelpfulAngler")
        );
        assert_eq!(delegated_cast_target("Purple Void"), None);
        assert_eq!(delegated_cast_target("bait 500"), None);
        assert_eq!(delegated_cast_target("two words"), None);
        assert_eq!(delegated_cast_target(""), None);
    }
}
