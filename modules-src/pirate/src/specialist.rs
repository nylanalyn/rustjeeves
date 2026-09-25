//! Career milestones and once-per-season specialist switching.

use crate::model::{Player, Specialist};

pub(crate) fn progress(player: &Player, specialist: Specialist) -> (i64, i64) {
    match specialist {
        Specialist::RaidLeader => (player.career_raids_won.max(0), 5),
        Specialist::Defense => (player.career_defenses_won.max(0), 5),
        Specialist::StrategicAlcoholic => (player.career_rum_collected.max(0), 30),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecruitError {
    Locked,
    AlreadyActive,
    SwitchUsed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecruitResult {
    First,
    Switched,
}

pub(crate) fn recruit(
    player: &mut Player,
    specialist: Specialist,
) -> Result<RecruitResult, RecruitError> {
    if player.specialist == Some(specialist) {
        return Err(RecruitError::AlreadyActive);
    }
    let (current, required) = progress(player, specialist);
    if current < required {
        return Err(RecruitError::Locked);
    }
    if !player.specialist_recruited {
        player.specialist = Some(specialist);
        player.specialist_recruited = true;
        return Ok(RecruitResult::First);
    }
    if player.specialist_switched_this_season {
        return Err(RecruitError::SwitchUsed);
    }
    player.specialist = Some(specialist);
    player.specialist_switched_this_season = true;
    Ok(RecruitResult::Switched)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn career_progress_unlocks_roles_and_switch_is_limited_to_once_per_season() {
        let mut player = Player {
            career_raids_won: 5,
            career_defenses_won: 5,
            career_rum_collected: 30,
            ..Default::default()
        };
        assert_eq!(
            recruit(&mut player, Specialist::RaidLeader),
            Ok(RecruitResult::First)
        );
        assert_eq!(
            recruit(&mut player, Specialist::Defense),
            Ok(RecruitResult::Switched)
        );
        assert_eq!(
            recruit(&mut player, Specialist::StrategicAlcoholic),
            Err(RecruitError::SwitchUsed)
        );
        assert_eq!(player.specialist, Some(Specialist::Defense));
    }

    #[test]
    fn legacy_career_totals_unlock_roles_without_special_migration() {
        let player = Player {
            career_raids_won: 4,
            career_defenses_won: 5,
            career_rum_collected: 29,
            ..Default::default()
        };
        assert_eq!(progress(&player, Specialist::RaidLeader), (4, 5));
        assert_eq!(progress(&player, Specialist::Defense), (5, 5));
        assert_eq!(progress(&player, Specialist::StrategicAlcoholic), (29, 30));

        let legacy: Player = serde_json::from_value(serde_json::json!({
            "career_raids_won": 5,
            "career_defenses_won": 5,
            "career_rum_collected": 30
        }))
        .expect("legacy player defaults new specialist fields");
        assert_eq!(progress(&legacy, Specialist::RaidLeader), (5, 5));
        assert_eq!(legacy.specialist, None);
        assert!(!legacy.specialist_recruited);
    }
}
