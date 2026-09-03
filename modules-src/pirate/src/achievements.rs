//! Achievement manifest and the pure, idempotent backfill over career stats in the blob.

use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest,
    AchievementSetMax, AchievementSpec, AchievementStat, PrestigeSpec,
    ACHIEVEMENT_MANIFEST_VERSION,
};

pub(crate) const STATS: &[&str] = &[
    "voyages",
    "raids_won",
    "defenses_won",
    "gold_plundered",
    "prisoners_taken",
    "prisoners_marooned",
    "seasons_played",
    "rum_collected",
];

pub(crate) fn manifest() -> AchievementManifest {
    let spec = |id: &str,
                name: &str,
                description: String,
                stat: &str,
                threshold: u64,
                optional: bool,
                secret: bool| {
        AchievementSpec {
            id: id.into(),
            name: name.into(),
            description,
            stat: stat.into(),
            threshold,
            optional,
            secret,
        }
    };
    let achievements = vec![
        spec(
            "first_voyage",
            "Cast Off",
            "Complete your first voyage.".into(),
            "voyages",
            1,
            false,
            false,
        ),
        spec(
            "seasoned_sailor",
            "Seasoned Sailor",
            "Complete 25 voyages.".into(),
            "voyages",
            25,
            false,
            false,
        ),
        spec(
            "first_blood",
            "First Blood",
            "Win your first raid.".into(),
            "raids_won",
            1,
            false,
            false,
        ),
        spec(
            "raider",
            "Raider",
            "Win 10 raids.".into(),
            "raids_won",
            10,
            false,
            false,
        ),
        spec(
            "terror_of_the_seas",
            "Terror of the Seas",
            "Win 50 raids.".into(),
            "raids_won",
            50,
            false,
            false,
        ),
        spec(
            "plunderer",
            "Plunderer",
            "Plunder 1,000 gold from rival captains.".into(),
            "gold_plundered",
            1000,
            false,
            false,
        ),
        spec(
            "gold_hoarder",
            "Gold Hoarder",
            "Plunder 10,000 gold from rival captains.".into(),
            "gold_plundered",
            10000,
            false,
            false,
        ),
        spec(
            "hold_the_line",
            "Hold the Line",
            "Successfully defend your isle.".into(),
            "defenses_won",
            1,
            false,
            false,
        ),
        spec(
            "fortress",
            "The Fortress",
            "Successfully defend your isle 10 times.".into(),
            "defenses_won",
            10,
            false,
            false,
        ),
        spec(
            "man_overboard",
            "Man Overboard",
            "Maroon a prisoner. Brutal. Permanent.".into(),
            "prisoners_marooned",
            1,
            true,
            true,
        ),
        spec(
            "pressganger",
            "Pressganger",
            "Take 5 prisoners.".into(),
            "prisoners_taken",
            5,
            true,
            false,
        ),
        spec(
            "old_hand",
            "Old Hand",
            "Play 3 seasons.".into(),
            "seasons_played",
            3,
            false,
            false,
        ),
        spec(
            "rum_baron",
            "Rum Baron",
            "Collect 100 rum from voyages.".into(),
            "rum_collected",
            100,
            true,
            false,
        ),
    ];
    AchievementManifest {
        version: ACHIEVEMENT_MANIFEST_VERSION,
        stats: STATS
            .iter()
            .map(|id| AchievementStat {
                id: (*id).into(),
                description: (*id).into(),
            })
            .collect(),
        achievements,
        prestige: vec![PrestigeSpec {
            id: "pirate_king".into(),
            name: "Pirate King".into(),
            stat: "gold_plundered".into(),
            first_threshold: 5000,
            every: 5000,
        }],
        catalog_version: 1,
    }
}

/// Pure, idempotent backfill: absolute `set_max` values from each player's career stats.
pub(crate) fn backfill(
    request: AchievementBackfillRequest,
) -> Result<AchievementBackfillResponse, extism_pdk::Error> {
    let Some(entry) = request.entries.iter().find(|entry| entry.key == "data") else {
        return Ok(AchievementBackfillResponse::default());
    };
    let state: crate::model::State = serde_json::from_str(&entry.value)?;
    let mut state = state;
    // The blob may predate the serverwide migration; fold it so game keys are plain servers.
    crate::model::migrate_state(&mut state, 0);
    let mut values = Vec::new();
    for (game_key, game) in &state.games {
        if game_key != &request.server {
            continue;
        }
        for (uuid, player) in &game.players {
            let stats = [
                ("voyages", player.career_voyages.max(0) as u64),
                ("raids_won", player.career_raids_won.max(0) as u64),
                ("defenses_won", player.career_defenses_won.max(0) as u64),
                ("gold_plundered", player.career_gold_plundered.max(0) as u64),
                (
                    "prisoners_taken",
                    player.career_prisoners_taken.max(0) as u64,
                ),
                (
                    "prisoners_marooned",
                    player.career_prisoners_marooned.max(0) as u64,
                ),
                ("seasons_played", player.seasons_played as u64),
                ("rum_collected", player.career_rum_collected.max(0) as u64),
            ];
            for (stat, value) in stats {
                values.push(AchievementSetMax {
                    profile_id: uuid.clone(),
                    stat: stat.into(),
                    value,
                });
            }
        }
    }
    Ok(AchievementBackfillResponse { values })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Game, Player, State};
    use jeeves_abi::ModuleKvEntry;

    fn request(server: &str, state: &State) -> AchievementBackfillRequest {
        AchievementBackfillRequest {
            server: server.into(),
            entries: vec![ModuleKvEntry {
                key: "data".into(),
                value: serde_json::to_string(state).unwrap(),
            }],
            previous_version: 0,
            catalog_version: 1,
        }
    }

    #[test]
    fn manifest_is_consistent() {
        let manifest = manifest();
        assert_eq!(manifest.version, ACHIEVEMENT_MANIFEST_VERSION);
        assert_eq!(manifest.catalog_version, 1);
        let stats: Vec<&str> = manifest.stats.iter().map(|s| s.id.as_str()).collect();
        for achievement in &manifest.achievements {
            assert!(
                stats.contains(&achievement.stat.as_str()),
                "{} references unknown stat {}",
                achievement.id,
                achievement.stat
            );
        }
        assert!(
            manifest.achievements.iter().any(|a| a.secret && a.optional),
            "at least one secret optional achievement"
        );
        assert_eq!(manifest.prestige[0].stat, "gold_plundered");
    }

    #[test]
    fn backfill_is_idempotent_and_scoped_to_the_server() {
        let mut state = State::default();
        let mut game = Game::default();
        game.players.insert(
            "uuid-a".into(),
            Player {
                career_raids_won: 7,
                career_gold_plundered: 1234,
                seasons_played: 2,
                ..Default::default()
            },
        );
        state.games.insert("net".into(), game);
        state.games.insert("other".into(), Game::default());

        let first = backfill(request("net", &state)).unwrap();
        let second = backfill(request("net", &state)).unwrap();
        assert_eq!(first, second, "backfill must be idempotent");
        assert_eq!(first.values.len(), STATS.len());
        let raids = first.values.iter().find(|v| v.stat == "raids_won").unwrap();
        assert_eq!(raids.profile_id, "uuid-a");
        assert_eq!(raids.value, 7);
        let seasons = first
            .values
            .iter()
            .find(|v| v.stat == "seasons_played")
            .unwrap();
        assert_eq!(seasons.value, 2);
    }

    #[test]
    fn backfill_without_data_is_empty() {
        let request = AchievementBackfillRequest {
            server: "net".into(),
            entries: vec![],
            previous_version: 0,
            catalog_version: 1,
        };
        assert!(backfill(request).unwrap().values.is_empty());
    }
}
