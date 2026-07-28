use serde::{Deserialize, Serialize};

use crate::{format_elapsed, load_state, now_secs, save_state, Ctx};

pub(crate) const CONFIRM_SECS: i64 = 60;
pub(crate) const RECOVERY_SECS: i64 = 3 * 86_400;

const LIMBS: &[(u8, &str)] = &[
    (1 << 0, "left arm"),
    (1 << 1, "right arm"),
    (1 << 2, "left leg"),
    (1 << 3, "right leg"),
];

const WEAPONS: &[&str] = &[
    "damp revolver",
    "rusty bayonet",
    "tackle-box shotgun",
    "ceremonial trout cannon",
    "aggressively sharpened landing net",
    "flare gun of uncertain provenance",
];

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub(crate) struct DangerState {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default)]
    pending_until: Option<i64>,
    #[serde(default)]
    missing_limbs: u8,
    #[serde(default)]
    banned_until: Option<i64>,
    #[serde(default)]
    weapon: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BeginResult {
    Warning,
    AlreadyPending,
    AlreadyEnabled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AnswerResult {
    Enlisted,
    BackedOut,
    NoPendingQuestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SafetyResult {
    Ceasefire,
    AlreadySafe,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CatchOutcome {
    Quiet,
    Weapon {
        weapon: String,
    },
    Injury {
        limb: &'static str,
        banned_until: Option<i64>,
    },
}

impl DangerState {
    pub(crate) fn begin(&mut self, now: i64) -> BeginResult {
        self.expire_pending(now);
        if self.enabled {
            return BeginResult::AlreadyEnabled;
        }
        if self.pending_until.is_some() {
            return BeginResult::AlreadyPending;
        }
        self.pending_until = Some(now + CONFIRM_SECS);
        BeginResult::Warning
    }

    pub(crate) fn answer(&mut self, yes: bool, now: i64) -> AnswerResult {
        self.expire_pending(now);
        if self.pending_until.take().is_none() {
            return AnswerResult::NoPendingQuestion;
        }
        if yes {
            self.enabled = true;
            self.weapon
                .get_or_insert_with(|| "questionably licensed starter pistol".into());
            AnswerResult::Enlisted
        } else {
            AnswerResult::BackedOut
        }
    }

    pub(crate) fn stand_down(&mut self) -> SafetyResult {
        self.pending_until = None;
        if std::mem::take(&mut self.enabled) {
            SafetyResult::Ceasefire
        } else {
            SafetyResult::AlreadySafe
        }
    }

    pub(crate) fn active_ban(&mut self, now: i64) -> Option<i64> {
        self.settle_recovery(now);
        self.banned_until.filter(|&until| now < until)
    }

    pub(crate) fn settle_recovery(&mut self, now: i64) -> bool {
        if self.banned_until.is_some_and(|until| now >= until) {
            self.banned_until = None;
            self.missing_limbs = 0;
            return true;
        }
        false
    }

    pub(crate) fn weapon(&self) -> &str {
        self.weapon
            .as_deref()
            .unwrap_or("questionably licensed starter pistol")
    }

    pub(crate) fn missing_limbs(&self) -> Vec<&'static str> {
        LIMBS
            .iter()
            .filter_map(|(bit, name)| (self.missing_limbs & bit != 0).then_some(*name))
            .collect()
    }

    pub(crate) fn resolve_catch(
        &mut self,
        now: i64,
        event_roll: f64,
        choice: usize,
    ) -> CatchOutcome {
        if !self.enabled || self.active_ban(now).is_some() {
            return CatchOutcome::Quiet;
        }

        // DANGER MODE is still fishing: incidents should remain punctuation, not the core loop.
        if event_roll < 0.10 {
            let attached = LIMBS
                .iter()
                .filter(|(bit, _)| self.missing_limbs & bit == 0)
                .copied()
                .collect::<Vec<_>>();
            let Some((bit, limb)) = attached.get(choice % attached.len().max(1)).copied() else {
                return CatchOutcome::Quiet;
            };
            self.missing_limbs |= bit;
            let banned_until = if self.missing_limbs.count_ones() == LIMBS.len() as u32 {
                let until = now + RECOVERY_SECS;
                self.banned_until = Some(until);
                Some(until)
            } else {
                None
            };
            return CatchOutcome::Injury { limb, banned_until };
        }

        if event_roll < 0.18 {
            let weapon = WEAPONS[choice % WEAPONS.len()].to_string();
            self.weapon = Some(weapon.clone());
            return CatchOutcome::Weapon { weapon };
        }

        CatchOutcome::Quiet
    }

    fn expire_pending(&mut self, now: i64) {
        if self.pending_until.is_some_and(|until| now >= until) {
            self.pending_until = None;
        }
    }
}

pub(crate) fn cmd_danger(ctx: &Ctx) -> Result<(), extism_pdk::Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let player = state.players.entry(key).or_default();
    player.nick = ctx.nick.to_string();
    player.danger.settle_recovery(now);
    let result = player.danger.begin(now);
    let weapon = player.danger.weapon().to_string();
    save_state(&state)?;

    match result {
        BeginResult::Warning => ctx.say(
            "fishing.danger.warning",
            &[
                "I would STRONGLY advise against this, {user}. Are you certain? Type !yes within 60 seconds to risk war against fishdom, or !no to retain the use of diplomacy.",
            ],
            &[("user", ctx.addr)],
        ),
        BeginResult::AlreadyPending => ctx.say(
            "fishing.danger.warning_pending",
            &[
                "{user}, the declaration of war is awaiting your signature. Type !yes or !no.",
            ],
            &[("user", ctx.addr)],
        ),
        BeginResult::AlreadyEnabled => ctx.say(
            "fishing.danger.already_enabled",
            &[
                "{user}, DANGER MODE is already enabled. Your current weapon is the {weapon}. The fish remember.",
            ],
            &[("user", ctx.addr), ("weapon", &weapon)],
        ),
    }
}

pub(crate) fn cmd_answer(ctx: &Ctx, yes: bool) -> Result<(), extism_pdk::Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let player = state.players.entry(key).or_default();
    player.nick = ctx.nick.to_string();
    let result = player.danger.answer(yes, now_secs());
    save_state(&state)?;

    match result {
        AnswerResult::Enlisted => {
            ctx.say(
                "fishing.danger.enlisted",
                &[
                    "Very well, {user}. Your next of kin will be notified as a courtesy. DANGER MODE ENABLED.",
                ],
                &[("user", ctx.addr)],
            )?;
            ctx.award(vec![("danger_enlistments", 1)])
        }
        AnswerResult::BackedOut => {
            ctx.say(
                "fishing.danger.backed_out",
                &[
                    "A wise decision, {user}. The lake has been informed that the incident is closed.",
                ],
                &[("user", ctx.addr)],
            )?;
            ctx.award(vec![("danger_backouts", 1)])
        }
        AnswerResult::NoPendingQuestion => ctx.say(
            "fishing.danger.no_question",
            &["{user}, I admire the confidence, but I have not asked you anything."],
            &[("user", ctx.addr)],
        ),
    }
}

pub(crate) fn cmd_safety(ctx: &Ctx) -> Result<(), extism_pdk::Error> {
    let mut state = load_state()?;
    let key = ctx.key();
    let player = state.players.entry(key).or_default();
    player.nick = ctx.nick.to_string();
    let result = player.danger.stand_down();
    save_state(&state)?;

    match result {
        SafetyResult::Ceasefire => ctx.say(
            "fishing.danger.ceasefire",
            &["{user}, the fish cautiously accept your ceasefire."],
            &[("user", ctx.addr)],
        ),
        SafetyResult::AlreadySafe => ctx.say(
            "fishing.danger.already_safe",
            &["{user}, you are not currently at war with fishdom."],
            &[("user", ctx.addr)],
        ),
    }
}

pub(crate) fn cmd_limbs(ctx: &Ctx) -> Result<(), extism_pdk::Error> {
    let mut state = load_state()?;
    let now = now_secs();
    let key = ctx.key();
    let Some(player) = state.players.get_mut(&key) else {
        return ctx.say(
            "fishing.danger.limbs_intact",
            &["{user}, all four limbs are present and no weapon has been issued."],
            &[("user", ctx.addr)],
        );
    };

    let recovered = player.danger.settle_recovery(now);
    let missing = player.danger.missing_limbs();
    let weapon = player.danger.weapon().to_string();
    let enabled = player.danger.enabled;
    let ban = player.danger.active_ban(now);
    if recovered {
        save_state(&state)?;
    }

    if let Some(until) = ban {
        let remaining = format_elapsed(until - now);
        return ctx.say(
            "fishing.danger.limbs_banned",
            &[
                "{user}, you have no operational limbs. Rehabilitation concludes in {remaining}. Your {weapon} has been placed somewhere you cannot reach it.",
            ],
            &[
                ("user", ctx.addr),
                ("remaining", &remaining),
                ("weapon", &weapon),
            ],
        );
    }
    if missing.is_empty() {
        let mode = if enabled { "DANGER" } else { "ordinary" };
        return ctx.say(
            "fishing.danger.limbs_intact_equipped",
            &[
                "{user}, all four limbs are present. Current weapon: {weapon}. Fishing posture: {mode}.",
            ],
            &[("user", ctx.addr), ("weapon", &weapon), ("mode", mode)],
        );
    }

    let missing = missing.join(", ");
    ctx.say(
        "fishing.danger.limbs_missing",
        &[
            "{user}, missing: {missing}. Current weapon: {weapon}. This has no practical effect, somehow.",
        ],
        &[
            ("user", ctx.addr),
            ("missing", &missing),
            ("weapon", &weapon),
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirmation_is_explicit_and_expires() {
        let mut state = DangerState::default();
        assert_eq!(state.begin(100), BeginResult::Warning);
        assert_eq!(
            state.answer(true, 100 + CONFIRM_SECS),
            AnswerResult::NoPendingQuestion
        );
        assert!(!state.enabled);
    }

    #[test]
    fn backing_out_never_enables_danger() {
        let mut state = DangerState::default();
        assert_eq!(state.begin(100), BeginResult::Warning);
        assert_eq!(state.answer(false, 101), AnswerResult::BackedOut);
        assert!(!state.enabled);
    }

    #[test]
    fn fourth_injury_bans_then_restores_every_limb() {
        let mut state = DangerState::default();
        state.begin(100);
        state.answer(true, 101);

        for index in 0..3 {
            let CatchOutcome::Injury { banned_until, .. } =
                state.resolve_catch(200 + index, 0.0, 0)
            else {
                panic!("expected an injury");
            };
            assert_eq!(banned_until, None);
        }
        let CatchOutcome::Injury { banned_until, .. } = state.resolve_catch(204, 0.0, 0) else {
            panic!("expected the fourth injury");
        };
        assert_eq!(banned_until, Some(204 + RECOVERY_SECS));
        assert_eq!(state.missing_limbs().len(), 4);
        assert_eq!(state.active_ban(204), banned_until);

        assert_eq!(state.active_ban(204 + RECOVERY_SECS), None);
        assert!(state.missing_limbs().is_empty());
        assert!(state.enabled);
    }

    #[test]
    fn weapon_drops_replace_the_current_loadout() {
        let mut state = DangerState::default();
        state.begin(100);
        state.answer(true, 101);
        let CatchOutcome::Weapon { weapon } = state.resolve_catch(200, 0.15, 2) else {
            panic!("expected a weapon");
        };
        assert_eq!(weapon, "tackle-box shotgun");
        assert_eq!(state.weapon(), weapon);
    }
}
