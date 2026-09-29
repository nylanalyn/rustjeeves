//! Staged tips: fishing's deeper features, introduced one at a time as a player levels up.
//!
//! After a reel, at most one tip: the first not yet shown whose level the player has reached. A
//! tip for something the player can *do* (bait, choosing a location, lures, chum) is marked used
//! when they do it; if a week passes without that, a gentle reminder follows, at most once a week
//! across all tips and twice per tip. `!fish tips off` stops them. Players who were already past a
//! tip's level when tips arrived are never taught what they already know.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const WEEK: i64 = 7 * 24 * 60 * 60;
const MAX_REMINDERS_PER_TIP: u8 = 2;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(super) struct TipState {
    /// Set on a player's first reel after tips existed; see [`initialize`].
    #[serde(default)]
    initialized: bool,
    /// Tip id → when it was shown.
    #[serde(default)]
    shown: BTreeMap<String, i64>,
    #[serde(default)]
    used: BTreeSet<String>,
    #[serde(default)]
    reminded: BTreeMap<String, u8>,
    #[serde(default)]
    last_reminder: i64,
    #[serde(default)]
    pub(super) off: bool,
}

pub(super) struct Tip {
    pub(super) id: &'static str,
    level: i64,
    /// Whether using it is something to notice (and remind about).
    actionable: bool,
    pub(super) key: &'static str,
    pub(super) default: &'static str,
}

pub(super) const TIPS: &[Tip] = &[
    Tip {
        id: "locations",
        level: 2,
        actionable: true,
        key: "fishing.tip.locations",
        default: "Tip: !fishinfo lists the waters you can fish and what lives there; !cast <place> picks one.",
    },
    Tip {
        id: "bait",
        level: 3,
        actionable: true,
        key: "fishing.tip.bait",
        default: "Tip: !cast bait 300 spends XP to tilt the odds toward rarer fish (100 XP per hour of virtual waiting).",
    },
    Tip {
        id: "lure",
        level: 5,
        actionable: true,
        key: "fishing.tip.lure",
        default: "Tip: !lure ({lure_cost} XP) rigs a mystery lure — rarity or size — for your next catch.",
    },
    Tip {
        id: "chum",
        level: 7,
        actionable: true,
        key: "fishing.tip.chum",
        default: "Tip: !chum ({chum_cost} XP) brings better fish to everyone's lines here for a while.",
    },
    Tip {
        id: "mastery",
        level: 9,
        actionable: false,
        key: "fishing.tip.mastery",
        default: "Tip: !mastery shows the species you've mastered, and !records your heaviest catches.",
    },
];

/// Tips below a veteran's current level count as seen and used, so arriving tips only teach what
/// lies ahead.
fn initialize(state: &mut TipState, level: i64, now: i64) {
    if state.initialized {
        return;
    }
    state.initialized = true;
    for tip in TIPS.iter().filter(|tip| tip.level < level) {
        state.shown.insert(tip.id.into(), now);
        state.used.insert(tip.id.into());
    }
}

pub(super) fn mark_used(state: &mut TipState, id: &str) {
    state.used.insert(id.into());
}

/// The tip to show after this reel, if any; `true` when it's a reminder.
pub(super) fn next_tip(state: &mut TipState, level: i64, now: i64) -> Option<(&'static Tip, bool)> {
    initialize(state, level, now);
    if state.off {
        return None;
    }
    if let Some(tip) = TIPS
        .iter()
        .find(|tip| tip.level <= level && !state.shown.contains_key(tip.id))
    {
        state.shown.insert(tip.id.into(), now);
        return Some((tip, false));
    }
    if now - state.last_reminder < WEEK {
        return None;
    }
    let tip = TIPS.iter().find(|tip| {
        tip.actionable
            && !state.used.contains(tip.id)
            && state
                .shown
                .get(tip.id)
                .is_some_and(|shown| now - shown >= WEEK)
            && state.reminded.get(tip.id).copied().unwrap_or(0) < MAX_REMINDERS_PER_TIP
    })?;
    *state.reminded.entry(tip.id.into()).or_default() += 1;
    state.last_reminder = now;
    Some((tip, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_arrive_one_per_reel_as_levels_are_reached() {
        let mut state = TipState::default();
        assert!(
            next_tip(&mut state, 1, 0).is_none(),
            "level 1 has nothing to learn yet"
        );
        let (tip, reminder) = next_tip(&mut state, 3, 10).unwrap();
        assert_eq!((tip.id, reminder), ("locations", false));
        assert_eq!(next_tip(&mut state, 3, 11).unwrap().0.id, "bait");
        assert!(next_tip(&mut state, 3, 12).is_none());
    }

    #[test]
    fn veterans_skip_what_they_know_and_off_means_off() {
        let mut state = TipState::default();
        assert!(
            next_tip(&mut state, 8, 0).is_none(),
            "tips below level 8 count as known"
        );
        assert_eq!(next_tip(&mut state, 9, 1).unwrap().0.id, "mastery");
        let mut state = TipState::default();
        assert!(
            next_tip(&mut state, 30, 0).is_none(),
            "a level 30 player has seen it all"
        );
        let mut state = TipState {
            off: true,
            ..TipState::default()
        };
        assert!(next_tip(&mut state, 5, 0).is_none());
    }

    #[test]
    fn unused_features_get_weekly_reminders_twice_at_most() {
        let mut state = TipState::default();
        next_tip(&mut state, 2, 0); // locations shown
        assert!(
            next_tip(&mut state, 2, WEEK - 1).is_none(),
            "not yet a week"
        );
        let (tip, reminder) = next_tip(&mut state, 2, WEEK).unwrap();
        assert_eq!((tip.id, reminder), ("locations", true));
        assert!(next_tip(&mut state, 2, WEEK + 10).is_none(), "once a week");
        assert!(next_tip(&mut state, 2, 2 * WEEK).is_some());
        assert!(
            next_tip(&mut state, 2, 3 * WEEK).is_none(),
            "two reminders at most"
        );
        let mut state = TipState::default();
        next_tip(&mut state, 2, 0);
        mark_used(&mut state, "locations");
        assert!(
            next_tip(&mut state, 2, WEEK).is_none(),
            "used features aren't nagged"
        );
    }
}
