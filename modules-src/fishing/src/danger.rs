use serde::{Deserialize, Serialize};

use crate::{fishing_settings, format_elapsed, load_state, now_secs, save_state, Ctx};

pub(crate) const CONFIRM_SECS: i64 = 60;
pub(crate) const RECOVERY_SECS: i64 = 3 * 86_400;

const SERIOUS_INJURY_CHANCE: f64 = 0.15;
const MINOR_INJURY_CUTOFF: f64 = 0.30;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum MinorInjuryKind {
    Arm,
    Leg,
}

impl MinorInjuryKind {
    pub(crate) fn status_flavor(self) -> &'static str {
        match self {
            Self::Arm => "Your shooting arm is still complaining from the lake's return fire.",
            Self::Leg => "You set your feet carefully; the lake's return fire left you limping.",
        }
    }

    pub(crate) fn reel_flavor(self) -> &'static str {
        match self {
            Self::Arm => "The lake fires back and clips your arm. Your aim is now mostly theoretical.",
            Self::Leg => "The lake fires back and clips your leg. You leave the battlefield with a slight limp.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct MinorInjury {
    kind: MinorInjuryKind,
    until: i64,
}

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
    /// A temporary cosmetic injury from return fire. It never affects game mechanics.
    #[serde(default)]
    minor_injury: Option<MinorInjury>,
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
    Injury {
        limb: &'static str,
        banned_until: Option<i64>,
    },
    MinorInjury {
        kind: MinorInjuryKind,
    },
}

impl DangerState {
    pub(crate) fn begin(&mut self, now: i64, confirm_seconds: i64) -> BeginResult {
        self.expire_pending(now);
        if self.enabled {
            return BeginResult::AlreadyEnabled;
        }
        if self.pending_until.is_some() {
            return BeginResult::AlreadyPending;
        }
        self.pending_until = Some(now + confirm_seconds);
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

    pub(crate) fn settle_minor_injury(&mut self, now: i64) -> bool {
        if self.minor_injury.is_some_and(|injury| now >= injury.until) {
            self.minor_injury = None;
            return true;
        }
        false
    }

    pub(crate) fn minor_injury_kind(&self) -> Option<MinorInjuryKind> {
        self.minor_injury.map(|injury| injury.kind)
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

    pub(crate) fn missing_limb_count(&self) -> i64 {
        self.missing_limbs.count_ones() as i64
    }

    pub(crate) fn heal_missing_limbs(&mut self) {
        self.missing_limbs = 0;
        self.banned_until = None;
    }

    pub(crate) fn resolve_catch(
        &mut self,
        now: i64,
        event_roll: f64,
        choice: usize,
        recovery_seconds: i64,
        minor_injury_seconds: i64,
    ) -> CatchOutcome {
        if !self.enabled || self.active_ban(now).is_some() {
            return CatchOutcome::Quiet;
        }

        // DANGER MODE is still fishing: incidents should remain punctuation, not the core loop.
        if event_roll < SERIOUS_INJURY_CHANCE {
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
                let until = now + recovery_seconds;
                self.banned_until = Some(until);
                Some(until)
            } else {
                None
            };
            return CatchOutcome::Injury { limb, banned_until };
        }

        if event_roll < MINOR_INJURY_CUTOFF {
            let kind = if choice.is_multiple_of(2) {
                MinorInjuryKind::Arm
            } else {
                MinorInjuryKind::Leg
            };
            self.minor_injury = Some(MinorInjury {
                kind,
                until: now + minor_injury_seconds,
            });
            return CatchOutcome::MinorInjury { kind };
        }

        CatchOutcome::Quiet
    }

    /// Roll an independent harmless weapon swap after a non-serious danger incident. The caller
    /// controls when this runs so serious injuries can take precedence in the narration.
    pub(crate) fn maybe_weapon_drop(
        &mut self,
        roll: f64,
        choice: usize,
        chance_percent: i64,
    ) -> Option<String> {
        if !self.enabled || chance_percent <= 0 || roll >= chance_percent as f64 / 100.0 {
            return None;
        }
        let current = self.weapon.as_deref();
        let choices = WEAPONS
            .iter()
            .copied()
            .filter(|weapon| Some(*weapon) != current)
            .collect::<Vec<_>>();
        let weapon = choices.get(choice % choices.len())?.to_string();
        self.weapon = Some(weapon.clone());
        Some(weapon)
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
    let settings = fishing_settings(ctx.server);
    let key = ctx.key();
    let player = state.players.entry(key).or_default();
    player.nick = ctx.nick.to_string();
    player.danger.settle_recovery(now);
    let result = player.danger.begin(now, settings.danger_confirm_seconds);
    let weapon = player.danger.weapon().to_string();
    save_state(&state)?;

    match result {
        BeginResult::Warning => ctx.say(
            "fishing.danger.warning",
            &[
                "I would STRONGLY advise against this, {user}. Are you certain? Type !yes within {seconds} seconds to risk war against fishdom, or !no to retain the use of diplomacy.",
            ],
            &[
                ("user", ctx.addr),
                ("seconds", &settings.danger_confirm_seconds.to_string()),
            ],
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

    const MINOR_INJURY_SECS: i64 = 2 * 86_400;

    #[test]
    fn confirmation_is_explicit_and_expires() {
        let mut state = DangerState::default();
        assert_eq!(state.begin(100, CONFIRM_SECS), BeginResult::Warning);
        assert_eq!(
            state.answer(true, 100 + CONFIRM_SECS),
            AnswerResult::NoPendingQuestion
        );
        assert!(!state.enabled);
    }

    #[test]
    fn backing_out_never_enables_danger() {
        let mut state = DangerState::default();
        assert_eq!(state.begin(100, CONFIRM_SECS), BeginResult::Warning);
        assert_eq!(state.answer(false, 101), AnswerResult::BackedOut);
        assert!(!state.enabled);
    }

    #[test]
    fn fourth_injury_bans_then_restores_every_limb() {
        let mut state = DangerState::default();
        state.begin(100, CONFIRM_SECS);
        state.answer(true, 101);

        for index in 0..3 {
            let CatchOutcome::Injury { banned_until, .. } =
                state.resolve_catch(200 + index, 0.0, 0, RECOVERY_SECS, MINOR_INJURY_SECS)
            else {
                panic!("expected an injury");
            };
            assert_eq!(banned_until, None);
        }
        let CatchOutcome::Injury { banned_until, .. } =
            state.resolve_catch(204, 0.0, 0, RECOVERY_SECS, MINOR_INJURY_SECS)
        else {
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
    fn paid_healing_clears_missing_limbs_and_ban_without_disabling_danger() {
        let mut state = DangerState::default();
        state.enabled = true;
        state.missing_limbs = 0b0101;
        state.banned_until = Some(9_999);

        assert_eq!(state.missing_limb_count(), 2);
        state.heal_missing_limbs();

        assert_eq!(state.missing_limb_count(), 0);
        assert_eq!(state.active_ban(100), None);
        assert!(state.enabled);
    }

    #[test]
    fn weapon_drops_replace_the_current_loadout_without_duplicates() {
        let mut state = DangerState::default();
        state.begin(100, CONFIRM_SECS);
        state.answer(true, 101);
        let weapon = state
            .maybe_weapon_drop(0.10, 2, 25)
            .expect("expected a weapon");
        assert_eq!(weapon, "tackle-box shotgun");
        assert_eq!(state.weapon(), weapon);
        assert_ne!(state.maybe_weapon_drop(0.10, 2, 25), Some(weapon));
    }

    #[test]
    fn weapon_roll_is_separate_from_injury_roll() {
        let mut state = DangerState::default();
        state.begin(100, CONFIRM_SECS);
        state.answer(true, 101);
        assert_eq!(
            state.resolve_catch(200, 0.20, 0, RECOVERY_SECS, MINOR_INJURY_SECS),
            CatchOutcome::MinorInjury {
                kind: MinorInjuryKind::Arm
            }
        );
        assert_eq!(
            state.maybe_weapon_drop(0.10, 0, 25),
            Some("damp revolver".into())
        );
        assert_eq!(state.minor_injury_kind(), Some(MinorInjuryKind::Arm));
    }

    #[test]
    fn minor_injuries_are_cosmetic_and_expire() {
        let mut state = DangerState::default();
        state.begin(100, CONFIRM_SECS);
        state.answer(true, 101);

        assert_eq!(
            state.resolve_catch(200, 0.20, 0, RECOVERY_SECS, MINOR_INJURY_SECS),
            CatchOutcome::MinorInjury {
                kind: MinorInjuryKind::Arm
            }
        );
        assert_eq!(state.minor_injury_kind(), Some(MinorInjuryKind::Arm));
        let restored: DangerState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.minor_injury_kind(), Some(MinorInjuryKind::Arm));
        assert!(!state.settle_minor_injury(200 + MINOR_INJURY_SECS - 1));
        assert!(state.settle_minor_injury(200 + MINOR_INJURY_SECS));
        assert_eq!(state.minor_injury_kind(), None);
    }

    #[test]
    fn minor_injury_can_switch_between_arm_and_leg() {
        let mut state = DangerState::default();
        state.begin(100, CONFIRM_SECS);
        state.answer(true, 101);

        assert_eq!(
            state.resolve_catch(200, 0.20, 0, RECOVERY_SECS, MINOR_INJURY_SECS),
            CatchOutcome::MinorInjury {
                kind: MinorInjuryKind::Arm
            }
        );
        assert_eq!(
            state.resolve_catch(201, 0.20, 1, RECOVERY_SECS, MINOR_INJURY_SECS),
            CatchOutcome::MinorInjury {
                kind: MinorInjuryKind::Leg
            }
        );
        assert_eq!(state.minor_injury_kind(), Some(MinorInjuryKind::Leg));
    }
}
