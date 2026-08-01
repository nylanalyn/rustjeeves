//! `!reel` — resolving a pending cast into a catch.
//!
//! The scoring core of the game, and the one place most systems meet: wait time drives rarity and
//! weight, then events, chum, lures, artifacts, champion bonuses and the secret hour-666 catch
//! layer on top. Line breaks and rod wear are settled here, then XP, levelling, records, species
//! mastery and achievements are committed together.
//!
//! Every `ctx.award` call is placed after the `save_state` that persists what it is awarding for,
//! per the module contract in AGENTS.md — keep that ordering when editing.

use super::*;

pub(super) fn cmd_reel(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();

    let Some(cast) = state.active_casts.remove(&key) else {
        ctx.say(
            "reel_no_cast",
            &["{user}, you don't have a line out. Use !cast first."],
            &[("user", ctx.addr)],
        )?;
        return Ok(());
    };
    // Snapshot a legacy save before this reel changes any lifetime counters.
    {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        season_stats_mut(player);
    }
    let now = now_secs();
    let elapsed_seconds = now - cast.timestamp;
    let wait_hours = elapsed_seconds as f64 / 3600.0;
    let secret_fish = vampire_shark(elapsed_seconds);
    let vampire_hour = secret_fish.is_some();
    let location_name = cast.location.clone();
    let location = data()
        .locations
        .iter()
        .find(|l| l.name == location_name)
        .cloned()
        .unwrap_or_else(|| data().locations[0].clone());
    let mut rng = ctx.rng(&mut state)?;

    // Active event (and its effect) for this network/location.
    let event = active_event_for(&mut state, ctx.server, &location_name, now);
    let effect = event.as_ref().and_then(|e| e.effect.clone());
    let ev_mult = event.as_ref().map(|e| e.multiplier).unwrap_or(1.0);

    // A feeding-frenzy (time_boost) makes the line "wait" effectively longer.
    let effective_wait = if effect.as_deref() == Some("time_boost") {
        wait_hours / ev_mult
    } else {
        wait_hours
    };
    // Bait advances only the rarity gates. It cannot make an early reel valid, grow the fish,
    // or reduce the danger of leaving a line out past 24 hours.
    let rarity_wait = effective_wait + cast.bait_hours as f64;
    let danger_enabled = state
        .players
        .get(&key)
        .is_some_and(|player| player.danger.enabled);

    // Too early — the cast is consumed but the hook is empty.
    if effective_wait < MIN_WAIT_HOURS {
        let m = rng
            .choice(&data().too_early_messages)
            .cloned()
            .unwrap_or_else(|| "Nothing but an empty hook.".into());
        save_state(&state)?;
        if danger_enabled {
            return ctx.say(
                "fishing.danger.reel_too_early",
                &["{user}, no hostile contact. The lake may be reloading. ({ordinary_result})"],
                &[("user", ctx.addr), ("ordinary_result", &m)],
            );
        }
        return ctx.say_text("reel_too_early", &format!("{}, {}", ctx.addr, m));
    }

    // Danger zone — the longer past 24h, the likelier a bad outcome.
    if !vampire_hour && wait_hours > DANGER_THRESHOLD_HOURS {
        let natural_bad = (0.1 + (wait_hours - DANGER_THRESHOLD_HOURS) * 0.05).min(0.9);
        // A reinforced rod resists the wear of a neglected line (floored at 50% of natural risk).
        let rod = state
            .players
            .get(&key)
            .map(|p| current_rod_strength(p, now))
            .unwrap_or(0);
        let bad_chance = effective_break_chance(natural_bad, rod);
        if rng.f64() < bad_chance {
            let kind = ["line_break", "fish_escaped", "junk"][rng.below(3)];
            let player = state.players.entry(key.clone()).or_default();
            player.nick = ctx.nick.to_string();
            let text = if kind == "junk" {
                player.junk_collected += 1;
                let junk = junk_item(&mut rng, &location.kind);
                format!(
                    "After {:.1}h you reel in... {}. Maybe don't leave your line so long.",
                    wait_hours, junk
                )
            } else {
                if kind == "line_break" {
                    player.lines_broken += 1;
                }
                data()
                    .danger_zone_messages
                    .get(kind)
                    .and_then(|v| rng.choice(v))
                    .cloned()
                    .unwrap_or_else(|| "It got away.".into())
            };
            save_state(&state)?;
            ctx.say_text("reel_danger", &format!("{}, {}", ctx.addr, text))?;
            if kind == "line_break" {
                ctx.award(vec![("line_breaks", 1)])?;
            }
            return Ok(());
        }
    }

    // `!fish bless` forces a rare/legendary catch (and skips junk + line-break below).
    let forced_rare = state
        .players
        .get(&key)
        .map(|p| p.force_rare_legendary)
        .unwrap_or(false);

    // Plain junk — base 10%, boosted by murky-waters events, reduced by a junk-shield artifact.
    let mut junk_chance = 0.10;
    if effect.as_deref() == Some("junk_boost") {
        junk_chance *= ev_mult;
    }
    let shield = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "junk_shield"))
        .unwrap_or(0.0);
    junk_chance *= 1.0 - shield;
    if !vampire_hour && !forced_rare && rng.f64() < junk_chance {
        // 15% of the time, an artifact turns up instead of junk.
        if rng.f64() < 0.15 {
            if let Some(art) = rng.choice(&data().artifacts).cloned() {
                let player = state.players.entry(key.clone()).or_default();
                player.nick = ctx.nick.to_string();
                let old = player.artifact.replace(art.clone());
                save_state(&state)?;
                let mut resp = format!(
                    "{} reels in... something else is tangled in the line! You found the {}! Your casts will never be the same.",
                    ctx.addr, art.name
                );
                if let Some(o) = old {
                    resp.push_str(&format!(" (Replaced: {})", o.name));
                }
                ctx.say_text("reel_artifact", &resp)?;
                ctx.award(vec![("artifacts", 1)])?;
                return Ok(());
            }
        }
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.junk_collected += 1;
        player.xp += 5;
        season_stats_mut(player).xp_earned += 5;
        let junk = junk_item(&mut rng, &location.kind);
        save_state(&state)?;
        return ctx.say_text(
            "reel_junk",
            &format!(
                "{} reels in... {}. At least you're cleaning up! (+5 XP)",
                ctx.addr, junk
            ),
        );
    }

    // A catch. Gather player-derived boosts before mutating.
    let player_level = state.players.get(&key).map(|p| p.level).unwrap_or(0);
    let art_rarity = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "rarity"))
        .unwrap_or(0.0);
    let art_xp = state
        .players
        .get(&key)
        .map(|p| artifact_bonus(p, "xp"))
        .unwrap_or(0.0);
    let lure = state.players.get(&key).and_then(|p| p.active_lure.clone());
    let eligible: Vec<String> = if cast.allow_lower_fish {
        data()
            .locations
            .iter()
            .filter(|l| l.level <= player_level)
            .map(|l| l.name.clone())
            .collect()
    } else {
        Vec::new()
    };
    let lure_rarity = if lure.as_deref() == Some("rarity") {
        0.40
    } else {
        0.0
    };
    let event_rare_mult = if effect.as_deref() == Some("rare_boost") {
        ev_mult
    } else {
        1.0
    };
    let champ_rarity = champion_bonus(&state, ctx.server, &key, "rarity");
    let champ_xp = champion_bonus(&state, ctx.server, &key, "xp");
    let champ_titles = champion_titles(&state, ctx.server, &key);
    let mut rarity = if vampire_hour {
        "legendary".to_string()
    } else {
        select_rarity(
            &mut rng,
            rarity_wait,
            event_rare_mult,
            art_rarity + lure_rarity + champ_rarity,
        )
    };
    // Forced rare/legendary (from !fish bless): try rare then legendary at this spot, no fallback.
    let mut forced_applied = false;
    let mut fish = secret_fish;
    if forced_rare && !vampire_hour {
        let mut order = ["rare", "legendary"];
        if rng.below(2) == 1 {
            order.swap(0, 1);
        }
        for f in order {
            if let Some(found) = select_fish(&mut rng, &location_name, f, &eligible, false) {
                fish = Some(found.clone());
                rarity = f.to_string();
                forced_applied = true;
                break;
            }
        }
    }
    let mut fish = match fish
        .or_else(|| select_fish(&mut rng, &location_name, &rarity, &eligible, true).cloned())
    {
        Some(f) => f,
        None => {
            save_state(&state)?;
            return ctx.say(
                "reel_escaped",
                &["The fish got away at the last moment!"],
                &[],
            );
        }
    };
    let natural_weight = calc_weight(&mut rng, &fish, effective_wait);
    let mut weight = natural_weight;
    if !vampire_hour && lure.as_deref() == Some("size") {
        weight = round2(weight * 1.30);
    }
    // Chum: server-wide +40% size while active; clear once past its cooldown.
    let chum_active = match state.chum.get(ctx.server) {
        Some(c) if now < c.expires => true,
        Some(c) if now >= c.cooldown_until => {
            state.chum.remove(ctx.server);
            false
        }
        _ => false,
    };
    if chum_active && !vampire_hour {
        weight = round2(weight * 1.40);
    }

    // Line-break: bigger fish, bigger risk (a blessed catch never snaps). A reinforced rod
    // reduces the snap chance, floored at 50% of the natural risk so megafauna stay survivable
    // but never safe.
    let natural_break = 0.02 + (weight / 1000.0) * 0.15;
    let rod = state
        .players
        .get(&key)
        .map(|p| current_rod_strength(p, now))
        .unwrap_or(0);
    let break_chance = effective_break_chance(natural_break, rod);
    if !vampire_hour && !forced_applied && rng.f64() < break_chance {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        player.lines_broken += 1;
        save_state(&state)?;
        ctx.say_text(
            "reel_line_break",
            &format!(
            "{}, a massive tug — a {}! But it's too much... SNAP! The line breaks and it's gone.",
            ctx.addr, fish.name
        ),
        )?;
        ctx.award(vec![("line_breaks", 1)])?;
        return Ok(());
    }

    // Land it.
    let mut bonus_msgs: Vec<String> = Vec::new();
    let player = state.players.entry(key.clone()).or_default();
    player.nick = ctx.nick.to_string();
    // Cosmetic reskin: in a themed expedition world the same fish wears a themed name, so this
    // world's aquarium, records, and catch line all read differently from Prime's.
    if !player.universe_theme.is_empty() {
        fish.name = themed_fish_name(&player.universe_theme, &fish.name);
    }
    player.total_fish += 1;
    // Fold any completed !fix into rod_strength before touching rod state, so committed time is
    // never lost. Big fish (>2000 lb) wear the line: every ROD_DECAY_EVERYth such catch costs 1
    // strength. Small fish never wear a deep-sea rod.
    settle_rod(player, now);
    if apply_rod_wear(player, weight) {
        bonus_msgs.push(themed(
            "rod_worn",
            &["Your rod's line shows its strain from that beast (-1 strength)."],
            &[],
        )?);
    }
    if weight > player.biggest_fish {
        player.biggest_fish = weight;
        player.biggest_fish_name = Some(fish.name.clone());
    }
    let milestones = record_species_catch(player, &location_name, &fish, weight, natural_weight);
    {
        let seasonal = season_stats_mut(player);
        seasonal.fish_caught += 1;
        seasonal.unique_species.insert(fish.name.clone());
        seasonal.heaviest_catch = seasonal.heaviest_catch.max(weight);
        if rarity == "rare" || rarity == "legendary" {
            seasonal.rare_catches += 1;
        }
    }
    if forced_applied {
        player.force_rare_legendary = false;
    }
    if !player.locations_fished.contains(&location_name) {
        player.locations_fished.push(location_name.clone());
    }
    if rarity == "rare" || rarity == "legendary" {
        player.rare_catches.push(RareCatch {
            name: fish.name.clone(),
            weight,
            rarity: rarity.clone(),
            location: location_name.clone(),
            caught_at: now,
        });
    }

    // XP: base * rarity multiplier * weight bonus, then event/artifact/boost-rod/random.
    let rarity_mult = data()
        .rarity_xp_multiplier
        .get(&rarity)
        .copied()
        .unwrap_or(1);
    let weight_bonus = 1.0 + (weight / 50.0);
    let mut xp = (10.0 * rarity_mult as f64 * weight_bonus) as i64;
    if effect.as_deref() == Some("xp_boost") {
        xp = (xp as f64 * ev_mult) as i64;
    }
    if art_xp > 0.0 {
        xp = (xp as f64 * (1.0 + art_xp)) as i64;
    }
    if champ_xp > 0.0 {
        xp = (xp as f64 * (1.0 + champ_xp)) as i64;
        bonus_msgs.push("Traveler's blessing: +20% XP.".into());
    }
    if player.xp_boost_catches > 0 {
        xp *= 2;
        player.xp_boost_catches -= 1;
        bonus_msgs.push("Rod boost! x2 XP.".into());
        if player.xp_boost_catches == 0 {
            bonus_msgs.push("The rod's glow fades.".into());
        }
    }
    let roll = rng.f64();
    let mut extra = 0i64;
    if roll < 0.01 {
        extra = 40 + rng.below(51) as i64; // 40-90
        bonus_msgs.push(format!("Treasure haul! +{extra} XP."));
    } else if roll < 0.05 {
        extra = 8 + rng.below(13) as i64; // 8-20
        bonus_msgs.push(format!("Lucky find! +{extra} XP."));
    }
    if player.xp_boost_catches == 0 && rng.f64() < 0.007 {
        player.xp_boost_catches = 5;
        bonus_msgs.push("You found a better rod! Next 5 catches give double XP.".into());
    }
    let total_xp = xp + extra;
    player.xp += total_xp;
    season_stats_mut(player).xp_earned += total_xp;

    // Consume a rigged lure and note its payoff.
    let lure_reveal = match lure.as_deref() {
        Some("rarity") => {
            player.active_lure = None;
            " The rarity lure pays off!"
        }
        Some("size") => {
            player.active_lure = None;
            " The size lure pays off!"
        }
        _ => "",
    };

    let level_before = player.level;
    let new_level = check_level_up(player, max_level(now));
    // Reaching the cap for the first time in this world earns a permanent Deep Star.
    let newly_starred = player.level >= max_level(now) && !player.starred;
    if newly_starred {
        player.starred = true;
    }

    let article = match rarity.as_str() {
        "uncommon" => "an uncommon ".to_string(),
        "rare" => "a RARE ".to_string(),
        "legendary" => "a LEGENDARY ".to_string(),
        _ => "a ".to_string(),
    };
    let who = if champ_titles.is_empty() {
        ctx.addr.to_string()
    } else {
        format!("{} {}", ctx.addr, champ_titles)
    };
    let danger_weapon = player
        .danger
        .enabled
        .then(|| player.danger.weapon().to_string());
    let mut response = if vampire_hour {
        themed(
            "fishing.vampire_shark",
            &[
                "At hour 666, the water turns red. {user} reels in a LEGENDARY VAMPIRE SHARK weighing exactly {weight} lbs! It was never in the game. Until now. (+{xp} XP)",
            ],
            &[
                ("user", &who),
                ("weight", &format!("{weight:.2}")),
                ("xp", &total_xp.to_string()),
            ],
        )?
    } else if let Some(weapon) = &danger_weapon {
        themed(
            "fishing.danger.reel_catch",
            &[
                "{user} defeats {article}hostile {fish} weighing {weight} lbs after {hours}h using the {weapon}! (+{xp} XP)",
            ],
            &[
                ("user", &who),
                ("article", &article),
                ("fish", &fish.name),
                ("weight", &format!("{weight:.2}")),
                ("hours", &format!("{wait_hours:.1}")),
                ("weapon", weapon),
                ("xp", &total_xp.to_string()),
            ],
        )?
    } else {
        format!(
            "{} reels in {}{} weighing {:.2} lbs after {:.1}h! (+{} XP)",
            who, article, fish.name, weight, wait_hours, total_xp
        )
    };
    if player.dlc_enabled {
        let skin = themed(
            "dlc_skins",
            &[
                "wearing a very small fedora",
                "dressed as a nautical butler",
                "wearing a monocle of unreasonable confidence",
            ],
            &[],
        )?;
        response.push_str(&themed(
            "dlc_flourish",
            &[" It is {skin}."],
            &[("skin", &skin)],
        )?);
    }
    if !bonus_msgs.is_empty() {
        response.push(' ');
        response.push_str(&bonus_msgs.join(" "));
    }
    if chum_active {
        response.push_str(" (chummed waters!)");
    }
    if cast.bait_hours > 0 {
        response.push_str(&format!(
            " Bait added {}h to the rarity roll.",
            cast.bait_hours
        ));
    }
    if milestones.new_record {
        if milestones.previous_record > 0.0 {
            let previous = format!("{:.2}", milestones.previous_record);
            response.push_str(&themed(
                "record_broken",
                &[" NEW PERSONAL RECORD! Previous: {previous} lbs."],
                &[("previous", &previous)],
            )?);
        } else {
            response.push_str(&themed(
                "record_first",
                &[" First personal record for this species!"],
                &[],
            )?);
        }
    }
    if milestones.trophy {
        response.push_str(&themed(
            "record_trophy",
            &[" Trophy specimen (95%+ natural size)!"],
            &[],
        )?);
    }
    if milestones.mastery != milestones.previous_mastery {
        if let Some(tier) = milestones.mastery {
            response.push_str(&themed(
                "mastery_achieved",
                &[" {tier} mastery achieved!"],
                &[("tier", tier)],
            )?);
        }
    }
    response.push_str(lure_reveal);
    if let Some(lvl) = new_level {
        response.push_str(&format!(
            " LEVEL UP! You're now level {lvl} and can fish at {}!",
            location_for_level(lvl).name
        ));
        // Crossing into level 15 unlocks the reinforced rod. Announce it once so the player
        // discovers the feature naturally rather than having to guess !rod exists.
        if level_before < ROD_UNLOCK_LEVEL && lvl >= ROD_UNLOCK_LEVEL {
            response.push_str(&themed(
                "rod_unlocked",
                &[" You can now reinforce your fishing rod! Use !rod to inspect it and !fix [1-24h] to add strength — a stronger line lands bigger fish."],
                &[],
            )?);
        }
    }
    if newly_starred {
        response.push_str(&themed(
            "star_earned",
            &[" ✦ You've mastered this world and earned a Deep Star! Open a new one with !fish expedition, or !fish universe to see your worlds."],
            &[],
        )?);
    }
    let mut danger_full_injury = false;
    if danger_weapon.is_some() {
        let event_roll = rng.f64();
        let event_choice = rng.below(1024);
        match player.danger.resolve_catch(now, event_roll, event_choice) {
            danger::CatchOutcome::Quiet => {}
            danger::CatchOutcome::Weapon { weapon } => {
                response.push_str(&themed(
                    "fishing.danger.weapon_drop",
                    &[
                        " The defeated fish drops a {weapon}. It is now your weapon. Nobody asks why the fish had it.",
                    ],
                    &[("weapon", &weapon)],
                )?);
            }
            danger::CatchOutcome::Injury { limb, banned_until } => {
                if banned_until.is_some() {
                    danger_full_injury = true;
                    response.push_str(&themed(
                        "fishing.danger.final_limb",
                        &[
                            " The fish explodes. The lake repossesses your {limb}. With no operational limbs remaining, you receive a three-day fishing ban.",
                        ],
                        &[("limb", limb)],
                    )?);
                } else {
                    response.push_str(&themed(
                        "fishing.danger.limb_lost",
                        &[
                            " The fish explodes during the exchange and you misplace your {limb}. This has no practical effect, somehow.",
                        ],
                        &[("limb", limb)],
                    )?);
                }
            }
        }
    }
    let level_gain = (player.level - level_before).max(0) as u64;
    // `player` borrow has ended; record the star against the identity now.
    if newly_starred {
        *state.prestige.entry(key.clone()).or_insert(0) += 1;
    }
    save_state(&state)?;
    ctx.say_text("reel_catch", &response)?;
    let mut increments = vec![("catches", 1), ("level", level_gain)];
    if rarity == "rare" || rarity == "legendary" {
        increments.push(("rare_catches", 1));
    }
    if vampire_hour {
        increments.push(("vampire_sharks", 1));
    }
    if danger_full_injury {
        increments.push(("danger_full_injuries", 1));
    }
    ctx.award(increments)
}
