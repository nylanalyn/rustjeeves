//! Wormhole quests — the rare freak `!reel` that fishes the angler instead of the fish.
//!
//! A successful ordinary landing has a small chance of snagging a wormhole: the catch is lost
//! and the angler is assigned one task (catch one oddly named wormhole fish, or find one odd
//! piece of wormhole junk). Wormhole casts ignore location, bait, and wait-time rules; the
//! target turns up at a fixed fair rate (2 in 6 casts) so nobody is trapped forever. Completing
//! the task pays a level-relative XP windfall and returns the angler to ordinary fishing.

use super::*;

/// Chance that a successful ordinary landing is instead a wormhole. Checked after the
/// line-break roll and never during the hour-666 catch, so it only ever eats a normal fish.
pub(super) const WORMHOLE_TRIGGER_CHANCE: f64 = 0.02;

/// Target rate: the quest item turns up this often out of every [`WORMHOLE_TARGET_ROLL`] casts.
const WORMHOLE_TARGET_HITS: usize = 2;
const WORMHOLE_TARGET_ROLL: usize = 6;

/// The quest reward: exactly this many levels' worth of XP at the angler's current position on
/// the curve, so it stays big forever as leveling gets slower.
const WORMHOLE_REWARD_LEVELS: i64 = 3;

/// Oddly named fish that only exist inside a wormhole. Bare names; narration adds "the".
const WORMHOLE_FISH: &[&str] = &[
    "Quantum Carp",
    "Melancholic Flounder",
    "Reverse Eel",
    "Thursday Cod",
    "Screaming Halibut",
    "Non-Euclidean Mackerel",
    "Bureaucratic Pike",
    "Gentrified Sardine",
    "Suspiciously Round Trout",
    "Boolean Bass",
    "Vowelless Tuna",
    "Cartographer's Anglerfish",
];

/// Odd junk that only washes through a wormhole. Each entry is self-contained, article and all.
const WORMHOLE_JUNK: &[&str] = &[
    "a clock that runs backward",
    "an unopened letter addressed to you",
    "a snow globe of this exact fishing spot",
    "a rotary phone that occasionally rings",
    "a map of a lake that does not exist",
    "a jar of expired starlight",
    "half a chess set (only the white pieces)",
    "a cassette tape labeled DO NOT LISTEN",
    "an umbrella, bone-dry, open, upside down",
    "a single left-footed flipper",
    "a spoon bent into a perfect treble clef",
    "someone else's homework, waterlogged",
];

/// Assign a fresh quest: coin-flip between a fish and a piece of junk.
pub(super) fn new_quest(rng: &mut Rng) -> Wormhole {
    if rng.below(2) == 0 {
        Wormhole {
            kind: WormholeKind::Fish,
            target: rng
                .choice(WORMHOLE_FISH)
                .expect("non-empty pool")
                .to_string(),
            casts: 0,
        }
    } else {
        Wormhole {
            kind: WormholeKind::Junk,
            target: rng
                .choice(WORMHOLE_JUNK)
                .expect("non-empty pool")
                .to_string(),
            casts: 0,
        }
    }
}

/// Human phrase for the quest, used in every message that names it.
pub(super) fn quest_task_text(quest: &Wormhole) -> String {
    match quest.kind {
        WormholeKind::Fish => format!("catch the {}", quest.target),
        WormholeKind::Junk => format!("find {}", quest.target),
    }
}

/// Does this cast produce the quest item? A flat 2-in-6, so the expected stay is a few casts.
fn target_hits(rng: &mut Rng) -> bool {
    rng.below(WORMHOLE_TARGET_ROLL) < WORMHOLE_TARGET_HITS
}

/// Something that is not the target, drawn from both pools, for the near-miss narration.
fn detritus(rng: &mut Rng, target: &str) -> &'static str {
    let mut pool: Vec<&'static str> = WORMHOLE_FISH.to_vec();
    pool.extend_from_slice(WORMHOLE_JUNK);
    pool.retain(|item| *item != target);
    rng.choice(&pool).expect("pool holds more than one item")
}

/// `!cast` while inside a wormhole: the quest decides everything. Location and bait arguments
/// are ignored, no rod or wait-time rules apply, and `!reel` resolves the line immediately.
pub(super) fn cmd_wormhole_cast(
    ctx: &Ctx,
    state: &mut State,
    key: &str,
    now: i64,
) -> Result<(), Error> {
    let quest = state
        .players
        .get(key)
        .and_then(|p| p.wormhole.clone())
        .expect("caller checked the quest exists");
    let task = quest_task_text(&quest);
    let player = state.players.get_mut(key).expect("quest holder exists");
    player.nick = ctx.nick.to_string();
    player.total_casts += 1;
    state.active_casts.insert(
        key.to_string(),
        Cast {
            timestamp: now,
            distance: 0.0,
            location: "the Wormhole".into(),
            allow_lower_fish: false,
            bait_hours: 0,
            wormhole: true,
        },
    );
    save_state(state)?;
    ctx.say(
        "fishing.wormhole.cast",
        &["{user} casts into the churning nothing — distance means nothing here. Task: {task}. Reel when ready."],
        &[("user", ctx.addr), ("task", &task)],
    )
}

/// `!reel` of a wormhole cast: the quest item at the fair rate, or consoling detritus.
pub(super) fn resolve_wormhole_reel(
    ctx: &Ctx,
    state: &mut State,
    key: &str,
    rng: &mut Rng,
) -> Result<(), Error> {
    let Some(quest) = state.players.get(key).and_then(|p| p.wormhole.clone()) else {
        // Defensive: a wormhole line without a quest (e.g. a hand-restored save). Eat it quietly.
        return ctx.say(
            "fishing.wormhole.gone",
            &["{user} reels in an empty hook — the wormhole has already collapsed behind them."],
            &[("user", ctx.addr)],
        );
    };
    if !target_hits(rng) {
        let item = detritus(rng, &quest.target);
        let task = quest_task_text(&quest);
        let player = state.players.get_mut(key).expect("quest holder exists");
        player.wormhole.as_mut().expect("checked above").casts += 1;
        player.xp += 5;
        season_stats_mut(player).xp_earned += 5;
        save_state(state)?;
        return ctx.say(
            "fishing.wormhole.miss",
            &["{user} reels in... {item}. Not it. The wormhole wants more: {task}. (+5 XP)"],
            &[("user", ctx.addr), ("item", item), ("task", &task)],
        );
    }

    // The item itself. Land it, pay the windfall, and go home.
    let item_text = match quest.kind {
        WormholeKind::Fish => format!("the {}", quest.target),
        WormholeKind::Junk => quest.target.clone(),
    };
    let player = state.players.get_mut(key).expect("quest holder exists");
    let grant = xp_for_next_levels(player.level, player.xp, WORMHOLE_REWARD_LEVELS);
    player.xp += grant;
    season_stats_mut(player).xp_earned += grant;
    let level_before = player.level;
    let new_level = check_level_up(player);
    player.wormhole = None;
    let level_gain = (player.level - level_before).max(0) as u64;

    let mut response = themed(
        "fishing.wormhole.win",
        &["{user} hauls in {item} — EXACTLY what the wormhole wanted! It shudders, satisfied, and spits {user} back out onto familiar water. (+{xp} XP)"],
        &[
            ("user", ctx.addr),
            ("item", &item_text),
            ("xp", &grant.to_string()),
        ],
    )?;
    response.push_str(&level_up_suffix(level_before, new_level)?);
    save_state(state)?;
    ctx.say_text("fishing.wormhole.win", &response)?;
    // Awarded only after the save that persists the completion (module contract).
    ctx.award(vec![("level", level_gain), ("wormhole_quests", 1)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rng(seed: u64) -> Rng {
        Rng(seed | 1)
    }

    #[test]
    fn quests_draw_from_both_pools() {
        let mut rng = rng(42);
        let (mut fish, mut junk) = (false, false);
        for _ in 0..200 {
            let quest = new_quest(&mut rng);
            match quest.kind {
                WormholeKind::Fish => {
                    fish = true;
                    assert!(WORMHOLE_FISH.contains(&quest.target.as_str()));
                }
                WormholeKind::Junk => {
                    junk = true;
                    assert!(WORMHOLE_JUNK.contains(&quest.target.as_str()));
                }
            }
        }
        assert!(
            fish && junk,
            "both quest kinds should appear across many rolls"
        );
    }

    #[test]
    fn task_text_reads_naturally_for_both_kinds() {
        let fish = Wormhole {
            kind: WormholeKind::Fish,
            target: "Quantum Carp".into(),
            casts: 0,
        };
        assert_eq!(quest_task_text(&fish), "catch the Quantum Carp");
        let junk = Wormhole {
            kind: WormholeKind::Junk,
            target: "a clock that runs backward".into(),
            casts: 0,
        };
        assert_eq!(quest_task_text(&junk), "find a clock that runs backward");
    }

    #[test]
    fn target_rate_hovers_near_two_in_six_and_detritus_never_is_the_target() {
        let mut rng = rng(7);
        let mut hits = 0;
        for _ in 0..6000 {
            if target_hits(&mut rng) {
                hits += 1;
            }
        }
        // 6000 rolls at 2/6 ≈ 2000; loose bounds tolerate a small-seed PRNG.
        assert!(
            (1700..=2300).contains(&hits),
            "target rate drifted: {hits}/6000"
        );
        for _ in 0..200 {
            assert_ne!(detritus(&mut rng, "Quantum Carp"), "Quantum Carp");
        }
    }

    #[test]
    fn reward_is_exactly_three_levels_worth() {
        // From a fresh level-10 save the grant carries the angler to level 13, leaving the
        // same remainder they started with.
        let grant = xp_for_next_levels(10, 0, WORMHOLE_REWARD_LEVELS);
        let mut player = Player {
            level: 10,
            xp: grant,
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut player), Some(13));
        assert_eq!(player.xp, 0);

        let grant = xp_for_next_levels(10, 5, WORMHOLE_REWARD_LEVELS);
        let mut player = Player {
            level: 10,
            xp: 5 + grant,
            ..Default::default()
        };
        assert_eq!(check_level_up(&mut player), Some(13));
        assert_eq!(player.xp, 0);
    }
}
