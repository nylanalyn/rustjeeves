//! Command handlers, and the `dispatch` table that routes a chat command to one.
//!
//! Every `!command` the module answers is listed in [`dispatch`] — start there when tracing a
//! command, or when adding one.
//!
//! Handlers live here by default. The exceptions are feature areas large enough to own a file:
//! `!cast` in [`crate::cast`], `!reel` in [`crate::reel`], and the danger-mode commands in
//! [`crate::danger`]. Shared game mechanics (levelling, seasons, rod wear, RNG, persistence) stay
//! in `lib.rs`; this module should read as presentation and command plumbing over those.

use super::*;

/// Route a chat command to its handler. Unknown commands are ignored.
pub(super) fn dispatch(ctx: &Ctx, cmd: &str, arg: &str) -> Result<(), Error> {
    match cmd {
        "!cast" => cast::cmd_cast(ctx, arg)?,
        "!reel" => reel::cmd_reel(ctx)?,
        "!fishinfo" => cmd_fishinfo(ctx, arg)?,
        "!aquarium" => cmd_aquarium(ctx)?,
        "!mastery" => cmd_mastery(ctx, arg)?,
        "!records" => cmd_records(ctx, arg)?,
        "!rod" => cmd_rod(ctx)?,
        "!fix" => cmd_fix(ctx, arg)?,
        "!lure" => cmd_lure(ctx)?,
        "!chum" => cmd_chum(ctx)?,
        "!discard" => cmd_discard(ctx)?,
        "!dynamite" => cmd_dynamite(ctx)?,
        "!hands" => cmd_hands(ctx)?,
        "!danger" => danger::cmd_danger(ctx)?,
        "!yes" => danger::cmd_answer(ctx, true)?,
        "!no" => danger::cmd_answer(ctx, false)?,
        "!safety" => danger::cmd_safety(ctx)?,
        "!limbs" => danger::cmd_limbs(ctx)?,
        "!fish" | "!fishing" | "!fishstats" => {
            let sub = arg.split_whitespace().next().unwrap_or("");
            let rest = arg
                .split_once(char::is_whitespace)
                .map(|x| x.1)
                .unwrap_or("")
                .trim();
            match sub {
                "top" => cmd_top(ctx)?,
                "location" => cmd_location(ctx)?,
                "help" => cmd_help(ctx)?,
                "champions" | "champion" => cmd_champions(ctx)?,
                "expedition" | "expeditions" | "portal" => cmd_expedition(ctx)?,
                "universe" | "universes" | "worlds" | "world" => cmd_universe(ctx)?,
                "jump" | "return" | "travel" => cmd_jump(ctx, rest)?,
                "bless" => cmd_bless(ctx, rest)?,
                "dlc" => cmd_dlc(ctx, rest)?,
                _ => cmd_stats(ctx, arg)?,
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn resolve_player_key(state: &State, ctx: &Ctx, arg: &str) -> (String, String) {
    if arg.is_empty() {
        return (ctx.key(), ctx.addr.to_string());
    }
    let prefix = format!("{}/", ctx.server);
    let folded_arg = fold_nick(ctx.server, arg);
    let key = state
        .players
        .iter()
        .find(|(key, player)| {
            key.starts_with(&prefix) && fold_nick(ctx.server, &player.nick) == folded_arg
        })
        .map(|(key, _)| key.clone())
        .unwrap_or_else(|| format!("{}/{}", ctx.server, folded_arg));
    (key, arg.to_string())
}

pub(super) fn cmd_universe(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let key = ctx.key();
    let Some(active) = state.players.get(&key) else {
        return ctx.say(
            "universe_none",
            &["{user}, you haven't cast a line yet — no worlds to show."],
            &[("user", ctx.addr)],
        );
    };
    let stars = star_count(&state, &key);
    let cap = max_level(now_secs());
    // Active world first, then the frozen ones.
    let mut worlds = vec![(true, universe_label(active), active.level, active.starred)];
    if let Some(stash) = state.stash.get(&key) {
        for p in stash {
            worlds.push((false, universe_label(p), p.level, p.starred));
        }
    }
    let list = worlds
        .iter()
        .map(|(is_active, label, level, starred)| {
            format!(
                "{label}{} (L{level}){}",
                if *starred { " ★" } else { "" },
                if *is_active { " «here»" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    ctx.say_text(
        "universe_list",
        &format!(
            "{}'s worlds — Deep Stars ★{}: {}. Jump with !fish jump <name|number>; at level {} open a new one with !fish expedition.",
            ctx.addr, stars, list, cap
        ),
    )
}

pub(super) fn cmd_expedition(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let now = now_secs();
    if !state.players.contains_key(&key) {
        return ctx.say(
            "expedition_none",
            &["{user}, you haven't fished yet — reach the top of Prime first."],
            &[("user", ctx.addr)],
        );
    }
    if state.active_casts.contains_key(&key) {
        return ctx.say(
            "expedition_line_out",
            &["{user}, reel in your line before opening a portal (!reel)."],
            &[("user", ctx.addr)],
        );
    }
    // A world already at the cap earns its Deep Star now, even if the player never reeled again
    // after maxing (e.g. anyone who hit the cap before expeditions existed).
    let newly = claim_star_if_maxed(&mut state, &key, now);
    let cap = max_level(now);
    let active = state.players.get(&key).expect("checked above");
    if active.level < cap {
        if newly {
            save_state(&state)?;
        }
        return ctx.say(
            "expedition_not_maxed",
            &["{user}, you must reach the level cap ({cap}) in this world before a portal will open — you're level {level}."],
            &[("user", ctx.addr), ("cap", &cap.to_string()), ("level", &active.level.to_string())],
        );
    }
    let universe_count = 1 + state.stash.get(&key).map(Vec::len).unwrap_or(0);
    if universe_count >= MAX_UNIVERSES {
        if newly {
            save_state(&state)?;
        }
        return ctx.say(
            "expedition_full",
            &["{user}, you've opened as many worlds as the fabric of reality allows ({max})."],
            &[("user", ctx.addr), ("max", &MAX_UNIVERSES.to_string())],
        );
    }
    // Next index = one past the highest this identity has ever held (active or stashed).
    let highest = std::iter::once(active.universe_index)
        .chain(
            state
                .stash
                .get(&key)
                .into_iter()
                .flatten()
                .map(|p| p.universe_index),
        )
        .max()
        .unwrap_or(0);
    let new_index = highest + 1;
    let (world_name, theme) = expedition_flavour(new_index);
    let prev_label = universe_label(active);
    let stars = star_count(&state, &key);
    // Freeze the maxed world and drop into the fresh one.
    let old_active = state.players.remove(&key).expect("checked above");
    state.stash.entry(key.clone()).or_default().push(old_active);
    let fresh = Player {
        nick: ctx.nick.to_string(),
        universe_index: new_index,
        universe_name: world_name.clone(),
        universe_theme: theme,
        ..Default::default()
    };
    state.players.insert(key.clone(), fresh);
    save_state(&state)?;
    ctx.say_text(
        "expedition_launch",
        &format!(
            "{} steps through a shimmering portal into {}! A fresh start begins at level 1. {} is frozen safe — return anytime with !fish jump {}. Deep Stars: ★{}.",
            ctx.addr, world_name, prev_label, prev_label, stars
        ),
    )
}

pub(super) fn cmd_jump(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    if arg.trim().is_empty() {
        return ctx.say(
            "jump_usage",
            &["{user}, jump to which world? !fish universe lists them, then !fish jump <name|number>."],
            &[("user", ctx.addr)],
        );
    }
    if !state.players.contains_key(&key) {
        return ctx.say(
            "jump_none",
            &["{user}, you have no worlds yet."],
            &[("user", ctx.addr)],
        );
    }
    if state.active_casts.contains_key(&key) {
        return ctx.say(
            "jump_line_out",
            &["{user}, reel in your line before jumping worlds (!reel)."],
            &[("user", ctx.addr)],
        );
    }
    if state
        .players
        .get(&key)
        .is_some_and(|p| universe_matches(ctx.server, p, arg))
    {
        let label = universe_label(state.players.get(&key).expect("checked"));
        return ctx.say(
            "jump_already",
            &["{user}, you're already fishing in {world}."],
            &[("user", ctx.addr), ("world", &label)],
        );
    }
    let pos = state
        .stash
        .get(&key)
        .and_then(|v| v.iter().position(|p| universe_matches(ctx.server, p, arg)));
    let Some(pos) = pos else {
        return ctx.say(
            "jump_unknown",
            &["{user}, no world by that name. !fish universe lists yours."],
            &[("user", ctx.addr)],
        );
    };
    let chosen = state.stash.get_mut(&key).expect("has stash").remove(pos);
    let label = universe_label(&chosen);
    let level = chosen.level;
    let old_active = state
        .players
        .insert(key.clone(), chosen)
        .expect("was active");
    state.stash.entry(key.clone()).or_default().push(old_active);
    if let Some(p) = state.players.get_mut(&key) {
        p.nick = ctx.nick.to_string();
    }
    save_state(&state)?;
    ctx.say_text(
        "jump_done",
        &format!(
            "{} slips through to {} (level {}). Everything's just as you left it.",
            ctx.addr, label, level
        ),
    )
}

pub(super) fn cmd_stats(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let state = load_state()?;
    let level_cap = max_level(now_secs());
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(p) = state.players.get(&key) else {
        return ctx.say_text(
            "stats_unknown",
            &format!("{} hasn't gone fishing yet.", who),
        );
    };
    let loc = location_for_level(p.level);
    let biggest = p
        .biggest_fish_name
        .as_ref()
        .map(|n| format!("{:.2} lbs ({})", p.biggest_fish, n))
        .unwrap_or_else(|| format!("{:.2} lbs", p.biggest_fish));
    let xp = if p.level >= level_cap {
        format!("{} spendable (MAX)", p.xp)
    } else {
        format!("{}/{}", p.xp, xp_for_level(p.level))
    };
    let stars = star_count(&state, &key);
    let prestige = if stars > 0 {
        format!(" | ★{stars}")
    } else {
        String::new()
    };
    // Only mention the world when it isn't Prime, so ordinary play reads exactly as before.
    let world = if p.universe_index != 0 {
        format!(" | World: {}", universe_label(p))
    } else {
        String::new()
    };
    ctx.say_text(
        "stats",
        &format!(
        "Fishing stats for {}: Level {} ({}) | XP {} | Fish {} | Biggest {} | Casts {} | Junk {}{}{}",
        who, p.level, loc.name, xp, p.total_fish, biggest, p.total_casts, p.junk_collected, prestige, world
    ),
    )
}

pub(super) fn cmd_top(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let prefix = format!("{}/", ctx.server);
    let mut players: Vec<&Player> = state
        .players
        .iter()
        .filter(|(k, _)| k.starts_with(&prefix))
        .map(|(_, p)| p)
        .collect();
    if players.is_empty() {
        return ctx.say("top_empty", &["No one has gone fishing yet!"], &[]);
    }
    let mut by_fish = players.clone();
    by_fish.retain(|p| p.total_fish > 0);
    by_fish.sort_by_key(|p| std::cmp::Reverse(p.total_fish));
    let most: Vec<String> = by_fish
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, p)| format!("#{} {} ({})", i + 1, name_of(p), p.total_fish))
        .collect();

    players.retain(|p| p.biggest_fish > 0.0);
    players.sort_by(|a, b| {
        b.biggest_fish
            .partial_cmp(&a.biggest_fish)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let big: Vec<String> = players
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, p)| {
            format!(
                "#{} {} ({:.1} lbs {})",
                i + 1,
                name_of(p),
                p.biggest_fish,
                p.biggest_fish_name.clone().unwrap_or_default()
            )
        })
        .collect();

    // Deep Stars: how many worlds each angler has taken to the cap — the prestige ladder.
    let mut stars: Vec<(i64, String)> = state
        .prestige
        .iter()
        .filter(|(k, v)| k.starts_with(&prefix) && **v > 0)
        .map(|(k, v)| {
            (
                *v,
                state
                    .players
                    .get(k)
                    .map(name_of)
                    .unwrap_or_else(|| "Unknown".into()),
            )
        })
        .collect();
    stars.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let star_line: Vec<String> = stars
        .iter()
        .take(5)
        .enumerate()
        .map(|(i, (n, name))| format!("#{} {} (★{})", i + 1, name, n))
        .collect();

    let mut out = String::from("Fishing Leaderboards:");
    if !star_line.is_empty() {
        out.push_str(&format!(" Deep Stars: {}", star_line.join(", ")));
    }
    if !most.is_empty() {
        out.push_str(&format!(
            "{}Most Fish: {}",
            if star_line.is_empty() { " " } else { " | " },
            most.join(", ")
        ));
    }
    if !big.is_empty() {
        out.push_str(&format!(" | Biggest: {}", big.join(", ")));
    }
    ctx.say_text("top", &out)
}

pub(super) fn name_of(p: &Player) -> String {
    if p.nick.is_empty() {
        "Unknown".into()
    } else {
        p.nick.clone()
    }
}

pub(super) fn cmd_location(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let level_cap = max_level(now_secs());
    let level = state.players.get(&ctx.key()).map(|p| p.level).unwrap_or(0);
    let loc = location_for_level(level);
    let next = data()
        .locations
        .iter()
        .find(|l| l.level == level + 1 && l.level <= level_cap);
    let next_txt = match next {
        Some(n) => format!(" Next: {} at level {}.", n.name, n.level),
        None => " You've reached the final frontier.".into(),
    };
    ctx.say_text(
        "location",
        &format!(
            "{}, you're level {} fishing at {}.{}",
            ctx.addr, level, loc.name, next_txt
        ),
    )
}

pub(super) fn cmd_fishinfo(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let level_cap = max_level(now_secs());
    if arg.is_empty() {
        let names: Vec<&str> = data()
            .locations
            .iter()
            .filter(|location| location.level <= level_cap)
            .map(|location| location.name.as_str())
            .collect();
        return ctx.say_text(
            "fishinfo_help",
            &format!("Locations: {}. Try !fishinfo <location>.", names.join(", ")),
        );
    }
    let Some(loc) = find_location(arg) else {
        return ctx.say_text(
            "fishinfo_unknown",
            &format!("{}, no such location.", ctx.addr),
        );
    };
    if loc.level > level_cap {
        return ctx.say(
            "fishinfo_dormant",
            &["That part of the Void has not opened yet."],
            &[],
        );
    }
    let fish = data()
        .fish_by_location
        .get(&loc.name)
        .cloned()
        .unwrap_or_default();
    let names: Vec<String> = fish
        .iter()
        .take(12)
        .map(|f| format!("{} ({})", f.name, f.rarity))
        .collect();
    ctx.say_text(
        "fishinfo",
        &format!("{} (level {}): {}", loc.name, loc.level, names.join(", ")),
    )
}

pub(super) fn cmd_aquarium(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let Some(p) = state.players.get(&ctx.key()) else {
        return ctx.say_text(
            "aquarium_empty",
            &format!("{}, your aquarium is empty — go fish!", ctx.addr),
        );
    };
    if p.rare_catches.is_empty() {
        return ctx.say_text(
            "aquarium_no_rare",
            &format!("{}, no rare or legendary catches yet.", ctx.addr),
        );
    }
    let mut recent = p.rare_catches.clone();
    recent.reverse();
    let items: Vec<String> = recent
        .iter()
        .take(6)
        .map(|c| format!("{} {} ({:.1} lbs)", c.rarity, c.name, c.weight))
        .collect();
    ctx.say_text(
        "aquarium",
        &format!(
            "{}'s aquarium ({} total): {}",
            ctx.addr,
            p.rare_catches.len(),
            items.join(", ")
        ),
    )
}

pub(super) fn cmd_mastery(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say_text(
            "mastery_unknown",
            &format!("{who} hasn't gone fishing yet."),
        );
    };
    let changed = migrate_species_careers(player);
    let mut mastered: Vec<&SpeciesCareer> = player
        .species_careers
        .values()
        .filter(|career| mastery_for(career.catches).is_some())
        .collect();
    mastered.sort_by(|a, b| b.catches.cmp(&a.catches).then_with(|| a.name.cmp(&b.name)));
    let tiers = ["Bronze", "Silver", "Gold", "Iridescent"]
        .iter()
        .map(|tier| {
            let count = mastered
                .iter()
                .filter(|career| mastery_for(career.catches) == Some(*tier))
                .count();
            format!("{tier} {count}")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let highlights = mastered
        .iter()
        .take(6)
        .map(|career| {
            format!(
                "{} {} ({})",
                career.name,
                mastery_for(career.catches).unwrap_or(""),
                career.catches
            )
        })
        .collect::<Vec<_>>();
    if changed {
        save_state(&state)?;
    }
    let detail = if highlights.is_empty() {
        "No mastered species yet; Bronze begins at 5 catches.".to_string()
    } else {
        highlights.join(", ")
    };
    ctx.say_text(
        "mastery",
        &format!("{who}'s species mastery: {tiers} | {detail}"),
    )
}

pub(super) fn cmd_records(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let (key, who) = resolve_player_key(&state, ctx, arg);
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say_text(
            "records_unknown",
            &format!("{who} hasn't gone fishing yet."),
        );
    };
    let changed = migrate_species_careers(player);
    let mut records: Vec<&SpeciesCareer> = player
        .species_careers
        .values()
        .filter(|career| career.best_weight > 0.0)
        .collect();
    records.sort_by(|a, b| {
        b.best_quality
            .partial_cmp(&a.best_quality)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    let items = records
        .iter()
        .take(6)
        .map(|career| {
            let trophy = if career.best_quality >= 0.95 {
                " ★"
            } else {
                ""
            };
            format!(
                "{} {:.2} lbs (record quality {:.0}%; best natural {:.0}%{})",
                career.name,
                career.best_weight,
                career.best_record_quality * 100.0,
                career.best_quality * 100.0,
                trophy
            )
        })
        .collect::<Vec<_>>();
    if changed {
        save_state(&state)?;
    }
    if items.is_empty() {
        return ctx.say_text(
            "records_empty",
            &format!("{who} has no measured personal records yet; legacy catches still count toward mastery."),
        );
    }
    ctx.say_text(
        "records",
        &format!(
            "{who}'s best specimens by natural quality (★ = 95%+): {}",
            items.join(", ")
        ),
    )
}

pub(super) fn cmd_help(ctx: &Ctx) -> Result<(), Error> {
    if expansion_active(now_secs()) {
        ctx.say("help_void_expansion", &["Fishing: !cast [location] [bait <100-1700 XP>] then wait (1h+, best ~24h, risky after 24h) and !reel. Bait spends 100 XP per virtual rarity hour. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !mastery [nick], !records [nick], !rod/!fix [1-24h] (level 15+ reinforced rod, lowers break chance), !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite. Endgame: at max level !fish expedition opens a fresh parallel world (earns a Deep Star ★); !fish universe lists your worlds; !fish jump <name> switches between them."], &[])
    } else {
        ctx.say("help", &["Fishing: !cast [location] then wait (1h+, best ~24h, risky after 24h) and !reel. Also !fishing [nick]/top/location/champions, !fishinfo [loc], !aquarium, !mastery [nick], !records [nick], !rod/!fix [1-24h] (level 15+ reinforced rod, lowers break chance), !lure (30xp), !chum (250xp), !discard, and the ill-advised !dynamite. Endgame: at max level !fish expedition opens a fresh parallel world (earns a Deep Star ★); !fish universe lists your worlds; !fish jump <name> switches between them."], &[])
    }
}
// ── commands: displays ──────────────────────────────────────────────────────

pub(super) fn cmd_lure(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let mut rng = ctx.rng(&mut state)?;
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.active_lure.is_some() {
        return ctx.say_text(
            "lure_active",
            &format!("{}, you already have a lure rigged up!", ctx.addr),
        );
    }
    if player.xp < 30 {
        return ctx.say_text(
            "lure_no_xp",
            &format!("{}, not enough XP (need 30, have {}).", ctx.addr, player.xp),
        );
    }
    player.xp -= 30;
    player.active_lure = Some(if rng.below(2) == 0 {
        "rarity".into()
    } else {
        "size".into()
    });
    save_state(&state)?;
    ctx.say_text(
        "lure_success",
        &format!(
            "{} spends 30 XP and rigs up a mystery lure. Let's see what it attracts!",
            ctx.addr
        ),
    )
}

pub(super) fn cmd_chum(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let chum_notice = if let Some(c) = state.chum.get_mut(ctx.server) {
        let (until, theme_key, text) = if now < c.expires {
            let mins = (c.expires - now) / 60 + 1;
            (
                c.expires,
                "chum_active",
                format!(
                    "{}, the water is already chummed! {} minute(s) left.",
                    ctx.addr, mins
                ),
            )
        } else if now < c.cooldown_until {
            let mins = (c.cooldown_until - now) / 60 + 1;
            (
                c.cooldown_until,
                "chum_cooldown",
                format!(
                    "{}, the chum is on cooldown. {} minute(s) until it can be used again.",
                    ctx.addr, mins
                ),
            )
        } else {
            (0, "", String::new())
        };
        if until == 0 {
            None
        } else if c
            .cooldown_notices
            .get(&ctx.key())
            .is_some_and(|seen_until| *seen_until >= until)
        {
            return Ok(());
        } else {
            c.cooldown_notices.insert(ctx.key(), until);
            Some((theme_key, text))
        }
    } else {
        None
    };
    if let Some((theme_key, text)) = chum_notice {
        save_state(&state)?;
        return ctx.say_text(theme_key, &text);
    }
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    if player.xp < 250 {
        return ctx.say_text(
            "chum_no_xp",
            &format!(
                "{}, not enough XP (need 250, have {}).",
                ctx.addr, player.xp
            ),
        );
    }
    player.xp -= 250;
    state.chum.insert(
        ctx.server.to_string(),
        Chum {
            expires: now + 20 * 60,
            cooldown_until: now + 50 * 60,
            cooldown_notices: HashMap::new(),
            by_id: ctx.key(),
            by_name: ctx.nick.to_string(),
        },
    );
    save_state(&state)?;
    ctx.say_text("chum_success", &format!("{} tosses a handful of chum into the water! Fish should run large for the next 20 minutes!", ctx.addr))
}

pub(super) fn cmd_discard(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let player = state.players.entry(ctx.key()).or_default();
    player.nick = ctx.nick.to_string();
    match player.artifact.take() {
        Some(a) => {
            save_state(&state)?;
            ctx.say_text(
                "discard_success",
                &format!(
                    "{} tosses the {} into the water. All bonuses lost — casts return to normal.",
                    ctx.addr, a.name
                ),
            )
        }
        None => ctx.say_text(
            "discard_empty",
            &format!("{}, you don't have an artifact to discard.", ctx.addr),
        ),
    }
}

// ── commands: reinforced rod ────────────────────────────────────────────────

/// `!rod` — inspect the reinforced rod's current strength and any in-progress fix. Unlocks at
/// level [`ROD_UNLOCK_LEVEL`]; below that, the player is told to come back later.
pub(super) fn cmd_rod(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let (settled, level, strength, fixing_until) = {
        let player = state.players.entry(ctx.key()).or_default();
        player.nick = ctx.nick.to_string();
        let settled = settle_rod(player, now);
        (
            settled,
            player.level,
            player.rod_strength,
            player.fixing_until,
        )
    };
    if settled {
        save_state(&state)?;
    }
    if level < ROD_UNLOCK_LEVEL {
        return ctx.say(
            "rod_locked",
            &["{user}, reinforced rods are an old fisher's secret. Come back at level {level}."],
            &[("user", ctx.addr), ("level", &ROD_UNLOCK_LEVEL.to_string())],
        );
    }
    if fixing_until.is_some_and(|until| now < until) {
        let remaining = format_elapsed(fixing_until.unwrap() - now);
        return ctx.say(
            "rod_fixing",
            &["{user}, your rod is in the workshop being strengthened (strength {strength}/{max}) — {remaining} until it's ready."],
            &[
                ("user", ctx.addr),
                ("strength", &strength.to_string()),
                ("max", &ROD_MAX_STRENGTH.to_string()),
                ("remaining", &remaining),
            ],
        );
    }
    ctx.say(
        "rod_status",
        &["{user}, your rod: strength {strength}/{max}. Each point lowers break chance, to a floor of half the natural risk. Use !fix [1-24h] to add strength."],
        &[
            ("user", ctx.addr),
            ("strength", &strength.to_string()),
            ("max", &ROD_MAX_STRENGTH.to_string()),
        ],
    )
}

/// `!fix [hours]` — commit time to strengthen the rod (+1 strength per hour, capped at 24h per
/// `!fix`). While fixing, `!cast` is refused. Strength is granted when the time window elapses,
/// so offline time counts and there's no "commit then cancel" exploit.
pub(super) fn cmd_fix(ctx: &Ctx, arg: &str) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let settled = {
        let player = state.players.entry(ctx.key()).or_default();
        player.nick = ctx.nick.to_string();
        settle_rod(player, now)
    };
    if settled {
        save_state(&state)?;
    }
    let player = state.players.entry(ctx.key()).or_default();
    if player.level < ROD_UNLOCK_LEVEL {
        return ctx.say(
            "rod_locked",
            &["{user}, reinforced rods are an old fisher's secret. Come back at level {level}."],
            &[("user", ctx.addr), ("level", &ROD_UNLOCK_LEVEL.to_string())],
        );
    }
    if rod_in_workshop(player, now) {
        let remaining = format_elapsed(player.fixing_until.unwrap() - now);
        return ctx.say(
            "fix_already",
            &["{user}, you're already working on the rod — {remaining} until it's done."],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }
    if player.rod_strength >= ROD_MAX_STRENGTH {
        return ctx.say(
            "fix_maxed",
            &["{user}, your rod is already at maximum strength ({max}). Fish proud."],
            &[("user", ctx.addr), ("max", &ROD_MAX_STRENGTH.to_string())],
        );
    }
    // Parse hours: bare !fix = 1h; otherwise a whole number in 1..=ROD_FIX_MAX_HOURS.
    let hours = match parse_fix_hours(arg) {
        Ok(h) => h,
        Err(_) => {
            return ctx.say(
                "fix_usage",
                &["{user}, usage: !fix [hours 1-{max}]. Default is 1 hour per point of strength."],
                &[("user", ctx.addr), ("max", &ROD_FIX_MAX_HOURS.to_string())],
            );
        }
    };
    let until = now + (hours as i64) * 3600;
    player.fixing_until = Some(until);
    player.fixing_hours = Some(hours);
    save_state(&state)?;
    ctx.say(
        "fix_started",
        &["{user}, you set to work reinforcing the rod. Check back in {hours}h — casting is paused while it's in the workshop."],
        &[
            ("user", ctx.addr),
            ("hours", &hours.to_string()),
        ],
    )
}

/// Parse the `!fix` hours argument: empty = 1, otherwise a whole number in 1..=ROD_FIX_MAX_HOURS.
fn parse_fix_hours(arg: &str) -> Result<u8, &'static str> {
    let trimmed = arg.trim();
    if trimmed.is_empty() {
        return Ok(1);
    }
    let n: i64 = trimmed.parse().map_err(|_| "not a whole number")?;
    if !(1..=ROD_FIX_MAX_HOURS).contains(&n) {
        return Err("out of range");
    }
    Ok(n as u8)
}

// ── commands: champions, risk toys, admin ───────────────────────────────────

pub(super) fn cmd_champions(ctx: &Ctx) -> Result<(), Error> {
    let state = load_state()?;
    let crowned = state.champions.get(ctx.server);
    let has_any = crowned
        .is_some_and(|c| c.traveler.is_some() || c.caster.is_some() || c.collector.is_some());
    let Some(c) = crowned.filter(|_| has_any) else {
        return ctx.say(
            "champions_empty",
            &["No champions yet — the first champions will be crowned at the next season reset!"],
            &[],
        );
    };
    let mut parts = vec![format!("Fishing Champions ({}):", c.season)];
    if c.traveler.is_some() {
        parts.push(format!(
            "the Traveler: {} (level {}, {})",
            c.traveler_name, c.traveler_level, c.traveler_location
        ));
    }
    if c.caster.is_some() {
        parts.push(format!(
            "the Caster: {} ({:.1}m)",
            c.caster_name, c.caster_distance
        ));
    }
    if c.collector.is_some() {
        parts.push(format!(
            "the Collector: {} ({} rare/legendary catches)",
            c.collector_name, c.collector_count
        ));
    }
    ctx.say_text("champions", &parts.join(" | "))
}

pub(super) fn cmd_hands(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say(
            "fishing.hands_full",
            &["{user}, you have both hands. Keep it that way."],
            &[("user", ctx.addr)],
        );
    };

    let changed = settle_dynamite_hands(player, now);
    let hands = 2 - player.dynamite_hands_lost.clamp(0, 2);
    let regrow_at = player.dynamite_hands_regrow_at;
    if changed {
        save_state(&state)?;
    }

    match (hands, regrow_at) {
        (2, _) => ctx.say(
            "fishing.hands_full",
            &["{user}, you have both hands. Keep it that way."],
            &[("user", ctx.addr)],
        ),
        (1, Some(regrow_at)) => {
            let remaining = format_elapsed(regrow_at - now);
            ctx.say(
                "fishing.hands_one_regrowing",
                &["{user}, you have 1 hand left. The other grows back in {remaining}."],
                &[("user", ctx.addr), ("remaining", &remaining)],
            )
        }
        (1, None) => ctx.say(
            "fishing.hands_one",
            &["{user}, you have 1 hand left. Use !dynamite at your own risk."],
            &[("user", ctx.addr)],
        ),
        (0, Some(regrow_at)) => {
            let remaining = format_elapsed(regrow_at - now);
            ctx.say(
                "fishing.hands_none_regrowing",
                &["{user}, you have no hands left. Both grow back in {remaining}."],
                &[("user", ctx.addr), ("remaining", &remaining)],
            )
        }
        (0, None) => ctx.say(
            "fishing.hands_none",
            &["{user}, you have no hands left."],
            &[("user", ctx.addr)],
        ),
        _ => unreachable!(),
    }
}

pub(super) fn cmd_dynamite(ctx: &Ctx) -> Result<(), Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let mut rng = ctx.rng(&mut state)?;
    {
        let player = state.players.entry(key.clone()).or_default();
        player.nick = ctx.nick.to_string();
        season_stats_mut(player);
    }

    // Already banned? No hands, no dynamite.
    if let Some(exp) = active_dynamite_ban(state.players.get_mut(&key).unwrap(), now) {
        let days = (exp - now) / 86_400 + 1;
        save_state(&state)?;
        return ctx.say_text(
            "dynamite_banned",
            &format!(
                "{} reaches into the tackle box with no hands left. There's no dynamite there, \
             and no plausible way to light it either. ({days} day(s) remaining)",
                ctx.addr
            ),
        );
    }
    if let Some(exp) = state
        .players
        .get_mut(&key)
        .and_then(|player| player.danger.active_ban(now))
    {
        let remaining = format_elapsed(exp - now);
        save_state(&state)?;
        return ctx.say(
            "fishing.danger.dynamite_banned",
            &[
                "{user}, without any operational limbs you cannot light the dynamite. Rehabilitation concludes in {remaining}.",
            ],
            &[("user", ctx.addr), ("remaining", &remaining)],
        );
    }

    let roll = rng.f64();

    // 10% — thinks better of it.
    if roll < 0.10 {
        let chicken = [
            format!("{} pulls out the dynamite, stares at it for a long moment... and puts it back. Some decisions don't need to be made today. Goes to get a cup of tea.", ctx.addr),
            format!("{} hefts the dynamite thoughtfully, then sets it gently on a rock. The tea is calling. The fish can wait.", ctx.addr),
            format!("{} gets halfway through lighting the fuse before reconsidering. Honestly, a nice biscuit sounds better right now.", ctx.addr),
            format!("{} holds the dynamite aloft dramatically... then pockets it and wanders off in search of a kettle.", ctx.addr),
            format!("{} considers the dynamite. Considers the fish. Considers their own mortality. Decides tea is the wiser investment.", ctx.addr),
        ];
        save_state(&state)?;
        return ctx.say_text("dynamite_chicken", &chicken[rng.below(chicken.len())]);
    }

    // 20% — glorious success: a rare/legendary haul + a big XP grant (two levels' worth).
    if roll < 0.30 {
        let player = state.players.get_mut(&key).unwrap();
        let level_before = player.level;
        let (mut tl, mut tx, mut grant, mut levels) = (player.level, player.xp, 0i64, 0i64);
        let level_cap = max_level(now);
        while levels < 2 && tl < level_cap {
            grant += (xp_for_level(tl) - tx).max(0);
            tx = 0;
            tl += 1;
            levels += 1;
        }
        grant += 80 + rng.below(121) as i64; // 80-200

        let top = data().locations.iter().rfind(|l| l.level <= player.level);
        let loc_name = top
            .map(|l| l.name.clone())
            .unwrap_or_else(|| "Puddle".into());
        let eligible: Vec<String> = data()
            .locations
            .iter()
            .filter(|l| l.level <= player.level)
            .map(|l| l.name.clone())
            .collect();
        let haul_count = 3 + rng.below(4); // 3-6
        let mut haul: Vec<(String, String, f64)> = Vec::new();
        for _ in 0..haul_count {
            let rarity = ["rare", "rare", "legendary"][rng.below(3)];
            if let Some(fish) = select_fish(&mut rng, &loc_name, rarity, &eligible, true) {
                let fish = fish.clone();
                let weight = round2(rng.range(fish.max_weight * 0.7, fish.max_weight));
                let milestones = record_species_catch(player, &loc_name, &fish, weight, weight);
                player.total_fish += 1;
                if weight > player.biggest_fish {
                    player.biggest_fish = weight;
                    player.biggest_fish_name = Some(fish.name.clone());
                }
                player.rare_catches.push(RareCatch {
                    name: fish.name.clone(),
                    weight,
                    rarity: rarity.to_string(),
                    location: loc_name.clone(),
                    caught_at: now,
                });
                let seasonal = season_stats_mut(player);
                seasonal.fish_caught += 1;
                seasonal.unique_species.insert(fish.name.clone());
                seasonal.rare_catches += 1;
                seasonal.heaviest_catch = seasonal.heaviest_catch.max(weight);
                let marker = if milestones.new_record {
                    themed("record_marker", &[" RECORD"], &[])?
                } else {
                    String::new()
                };
                haul.push((format!("{}{marker}", fish.name), rarity.to_string(), weight));
            }
        }
        player.xp += grant;
        season_stats_mut(player).xp_earned += grant;
        let new_level = check_level_up(player, level_cap);

        let haul_str = if haul.is_empty() {
            "an eerie silence".to_string()
        } else {
            haul.iter()
                .map(|(n, r, w)| format!("{n} ({w:.1} lbs, {r})"))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut resp = format!(
            "KABOOM! {} hurls the dynamite into the fishing hole! The water ERUPTS. \
             Belly-up on the surface: {}. +{} XP from the sheer audacity of it.",
            ctx.addr, haul_str, grant
        );
        if let Some(lvl) = new_level {
            resp.push_str(&format!(
                " LEVEL UP x{}! Now level {} — {} awaits!",
                levels,
                lvl,
                location_for_level(lvl).name
            ));
        }
        let caught = haul.len() as u64;
        let level_gain = (player.level - level_before).max(0) as u64;
        save_state(&state)?;
        ctx.say_text("dynamite_success", &resp)?;
        ctx.award(vec![
            ("catches", caught),
            ("rare_catches", caught),
            ("level", level_gain),
        ])?;
        return Ok(());
    }

    // 70% — catastrophe. First costs a hand; a second costs fishing access for a week.
    let hands_lost = state
        .players
        .get(&key)
        .map(|p| p.dynamite_hands_lost)
        .unwrap_or(0);
    if hands_lost < 1 {
        let player = state.players.get_mut(&key).unwrap();
        player.dynamite_hands_lost = 1;
        player.dynamite_hands_regrow_at = Some(now + HAND_REGROW_SECS);
        let lines = [
            format!("{} lights the dynamite. The dynamite does not wait. There is a flash, a bang, and suddenly one hand is a matter for historians. The other remains available for poor decisions.", ctx.addr),
            format!("{} fumbles the dynamite. It goes off immediately. In their hand. The fish are fine. The hand is not. One hand left.", ctx.addr),
            format!("{} finds the fuse much shorter than expected. The resulting lesson costs exactly one hand. Fishing privileges remain, technically.", ctx.addr),
        ];
        let msg = lines[rng.below(lines.len())].clone();
        save_state(&state)?;
        return ctx.say_text("dynamite_one_hand", &msg);
    }

    let ban_until = now + HAND_REGROW_SECS;
    {
        let player = state.players.get_mut(&key).unwrap();
        player.dynamite_hands_lost = 2;
        player.dynamite_banned_until = Some(ban_until);
        player.dynamite_hands_regrow_at = Some(ban_until);
    }
    state.active_casts.remove(&key);
    let lines = [
        format!("{} lights the dynamite with their remaining hand. A flash. A bang. A full accounting of previous warnings. No hands remain — a 7-day fishing ban has been issued.", ctx.addr),
        format!("{} fumbles the dynamite again, into the only hand they had left. The fish are fine. The hands are gone. Banned from fishing for 7 days.", ctx.addr),
        format!("{} has made the same terrible mistake twice. The lake files the paperwork. No hands left, no fishing for 7 days, no exceptions.", ctx.addr),
    ];
    let msg = lines[rng.below(lines.len())].clone();
    save_state(&state)?;
    ctx.say_text("dynamite_banned_result", &msg)
}

pub(super) fn cmd_bless(ctx: &Ctx, target: &str) -> Result<(), Error> {
    if ctx.role != Some(Role::SuperAdmin) {
        return ctx.say_text(
            "bless_denied",
            &format!(
                "{}, only a super-admin may bestow such blessings.",
                ctx.addr
            ),
        );
    }
    if target.is_empty() {
        return ctx.say("bless_usage", &["Usage: !fish bless <nick>"], &[]);
    }
    let mut state = load_state()?;
    let tkey = format!("{}/{}", ctx.server, fold_nick(ctx.server, target));
    let player = state.players.entry(tkey).or_default();
    if player.nick.is_empty() {
        player.nick = target.to_string();
    }
    player.force_rare_legendary = true;
    save_state(&state)?;
    ctx.say_text(
        "bless_success",
        &format!("{}, your next catch will be rare or legendary.", target),
    )
}

pub(super) fn cmd_dlc(ctx: &Ctx, args: &str) -> Result<(), Error> {
    if ctx.role != Some(Role::SuperAdmin) {
        return ctx.say(
            "dlc_denied",
            &["{user}, premium fish couture may only be administered by a super-admin."],
            &[("user", ctx.addr)],
        );
    }
    let mut parts = args.split_whitespace();
    let action = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("");
    if !matches!(action, "grant" | "revoke" | "status")
        || target.is_empty()
        || parts.next().is_some()
    {
        return ctx.say(
            "dlc_usage",
            &["Usage: !fish dlc grant|revoke|status <nick>"],
            &[],
        );
    }
    let Some(profile) = profile_for_nick(ctx.server, target)? else {
        return ctx.say(
            "dlc_unknown",
            &["I cannot locate a profile for {nick}; they must speak before acquiring premium fishwear."],
            &[("nick", target)],
        );
    };
    let key = format!("{}/{}", ctx.server, profile.id);
    let mut state = load_state()?;
    let enabled = state.players.get(&key).is_some_and(|p| p.dlc_enabled);
    match action {
        "status" => ctx.say(
            "dlc_status",
            &["Premium Fish Couture for {nick}: {status}."],
            &[
                ("nick", &profile.nick),
                ("status", if enabled { "active" } else { "inactive" }),
            ],
        ),
        "grant" => {
            let player = state.players.entry(key).or_default();
            player.nick = profile.nick.clone();
            player.dlc_enabled = true;
            save_state(&state)?;
            ctx.say(
                "dlc_granted",
                &["Premium Fish Couture has been activated for {nick}. The invoice remains tastefully undisclosed."],
                &[("nick", &profile.nick)],
            )
        }
        "revoke" => {
            if let Some(player) = state.players.get_mut(&key) {
                player.dlc_enabled = false;
                save_state(&state)?;
            }
            ctx.say(
                "dlc_revoked",
                &["Premium Fish Couture has been withdrawn from {nick}. The fish return to ordinary nudity."],
                &[("nick", &profile.nick)],
            )
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fix_hours_parser_accepts_bare_and_bounded_input() {
        assert_eq!(parse_fix_hours("").unwrap(), 1);
        assert_eq!(parse_fix_hours("8").unwrap(), 8);
        assert_eq!(parse_fix_hours("  24 ").unwrap(), 24);
        assert!(parse_fix_hours("0").is_err(), "zero is out of range");
        assert!(parse_fix_hours("25").is_err(), "above the 24h cap");
        assert!(parse_fix_hours("lots").is_err(), "non-numeric rejected");
        assert!(parse_fix_hours("-3").is_err(), "negative rejected");
    }
}
