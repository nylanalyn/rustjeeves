//! Pure export and deletion planning for the module's single JSON state blob.

use crate::model::{Game, State};
use extism_pdk::Error;
use jeeves_abi::{
    ModuleDataDeletePlan, ModuleDataRequest, ModuleDataResponse, ModuleKvMutation,
    DATA_LIFECYCLE_VERSION,
};
use std::collections::HashMap;

fn data_entry(request: &ModuleDataRequest) -> Option<&str> {
    request
        .entries
        .iter()
        .find(|entry| entry.key == "data")
        .map(|entry| entry.value.as_str())
}

/// Pirate state has always been keyed by the host-stamped profile UUID, so ownership is the UUID
/// alone. Matching the subject's legacy nick aliases against `nick_cache` would be wrong: IRC nicks
/// are reused, and another captain now wearing one of those nicks would be exported or erased.
fn belongs_to(profile_id: &str, request: &ModuleDataRequest) -> bool {
    profile_id == request.subject.profile_id
}

/// Parse the blob and fold any legacy per-channel layout into the serverwide one, so both hooks
/// always see plain `"{server}"` game keys regardless of when the export runs. Pure: no host
/// calls, nothing persisted.
fn parsed_state(raw: &str) -> Result<State, Error> {
    let mut state: State = serde_json::from_str(raw)?;
    crate::model::migrate_state(&mut state, 0);
    Ok(state)
}

pub(crate) fn data_export(request: &ModuleDataRequest) -> Result<String, Error> {
    let Some(raw) = data_entry(request) else {
        return Ok(serde_json::to_string(&ModuleDataResponse {
            version: DATA_LIFECYCLE_VERSION,
            data: serde_json::Value::Null,
        })?);
    };
    let state = parsed_state(raw)?;
    let mut games = HashMap::new();
    let mut blockade_data = HashMap::new();
    for (key, game) in state.games {
        if key != request.subject.server {
            continue;
        }
        let players = game
            .players
            .iter()
            .filter(|(uuid, _)| belongs_to(uuid, request))
            .map(|(uuid, player)| (uuid.clone(), player.clone()))
            .collect::<HashMap<_, _>>();
        let blockades = game
            .players
            .iter()
            .filter(|(_, target)| {
                target
                    .player_blockade
                    .as_ref()
                    .is_some_and(|blockade| belongs_to(&blockade.blockader_uuid, request))
            })
            .map(|(target_uuid, target)| serde_json::json!({"target_uuid": target_uuid, "blockade": target.player_blockade}))
            .collect::<Vec<_>>();
        if !blockades.is_empty() {
            blockade_data.insert(key.clone(), blockades);
        }
        if players.is_empty() && !blockade_data.contains_key(&key) {
            continue;
        }
        games.insert(key, serde_json::json!({ "players": players }));
    }
    let sessions = state
        .pm_sessions
        .into_iter()
        .filter(|(key, _)| {
            key == &format!("{}/{}", request.subject.server, request.subject.profile_id)
        })
        .map(|(key, value)| {
            (
                key,
                serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<HashMap<_, _>>();
    let voyage_offers = state
        .voyage_offers
        .into_iter()
        .filter(|(key, _)| {
            key == &format!("{}/{}", request.subject.server, request.subject.profile_id)
        })
        .map(|(key, value)| {
            (
                key,
                serde_json::to_value(value).unwrap_or(serde_json::Value::Null),
            )
        })
        .collect::<HashMap<_, _>>();
    let data = if games.is_empty()
        && blockade_data.is_empty()
        && sessions.is_empty()
        && voyage_offers.is_empty()
    {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "games": games, "player_blockades": blockade_data, "pm_sessions": sessions, "voyage_offers": voyage_offers })
    };
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data,
    })?)
}

/// Remove captains from one game and everything that hangs off them: blockades settle, voyages
/// against them sail home, and their prisoners, ransoms, and intel go. Pure; shared by data
/// deletion and NPC retirement.
pub(crate) fn remove_captains(game: &mut Game, ids: &[String]) {
    // If the target is erased, settle as expiry so escrow goes to the blockader. If the
    // blockader is erased, break each blockade so escrow returns to the target.
    let blockade_targets: Vec<(String, bool)> = game
        .players
        .iter()
        .filter_map(|(target, p)| {
            let b = p.player_blockade.as_ref()?;
            if ids.contains(target) {
                Some((target.clone(), false))
            } else if ids.contains(&b.blockader_uuid) {
                Some((target.clone(), true))
            } else {
                None
            }
        })
        .collect();
    for (target, broken) in blockade_targets {
        crate::blockade::settle(game, &target, broken);
    }
    for id in ids {
        game.players.remove(id);
    }
    // Other captains' crews sent against the erased isle sail home rather than vanishing
    // with it. Their scheduler jobs then find no voyage and no-op.
    game.voyages.retain(|voyage| {
        if ids.contains(&voyage.owner_uuid) {
            return false;
        }
        let against_erased = voyage
            .target_uuid
            .as_ref()
            .is_some_and(|target| ids.contains(target));
        if !against_erased {
            return true;
        }
        if !voyage.resolved {
            if let Some(owner) = game.players.get_mut(&voyage.owner_uuid) {
                owner.crew_regular += voyage.crew_regular;
                owner.crew_loyal += voyage.crew_loyal;
            }
        }
        false
    });
    game.navy_harassments.retain(|sortie| {
        if ids.contains(&sortie.owner_uuid) {
            return false;
        }
        if !ids.contains(&sortie.target_uuid) {
            return true;
        }
        if !sortie.resolved {
            if let Some(owner) = game.players.get_mut(&sortie.owner_uuid) {
                owner.crew_regular += sortie.crew_regular;
                owner.crew_loyal += sortie.crew_loyal;
            }
        }
        false
    });
    if game
        .navy_pending_target
        .as_ref()
        .is_some_and(|target| ids.contains(target))
    {
        game.navy_pending_target = None;
        game.navy_pending_hit_at = 0;
    }
    for player in game.players.values_mut() {
        if player
            .raid_intel
            .as_ref()
            .is_some_and(|intel| ids.contains(&intel.target_uuid))
        {
            player.raid_intel = None;
        }
    }
    game.prisoners.retain(|prisoner| {
        !ids.contains(&prisoner.holder_uuid) && !ids.contains(&prisoner.origin_uuid)
    });
    game.ransoms
        .retain(|ransom| !ids.contains(&ransom.holder_uuid) && !ids.contains(&ransom.target_uuid));
}

pub(crate) fn data_delete(request: &ModuleDataRequest) -> Result<String, Error> {
    let Some(raw) = data_entry(request) else {
        return Ok(serde_json::to_string(&ModuleDataDeletePlan {
            version: DATA_LIFECYCLE_VERSION,
            mutations: Vec::new(),
        })?);
    };
    let mut state = parsed_state(raw)?;
    let mut removed = Vec::new();
    for (key, game) in state.games.iter_mut() {
        if key != &request.subject.server {
            continue;
        }
        let ids = game
            .players
            .iter()
            .filter(|(uuid, _)| belongs_to(uuid, request))
            .map(|(uuid, _)| uuid.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            continue;
        }
        remove_captains(game, &ids);
        removed.extend(ids);
    }
    let sessions_before = state.pm_sessions.len();
    state.pm_sessions.retain(|key, _| {
        key != &format!("{}/{}", request.subject.server, request.subject.profile_id)
    });
    let offers_before = state.voyage_offers.len();
    state.voyage_offers.retain(|key, _| {
        key != &format!("{}/{}", request.subject.server, request.subject.profile_id)
    });
    let session_data_removed =
        sessions_before != state.pm_sessions.len() || offers_before != state.voyage_offers.len();
    let mutations = if removed.is_empty() && !session_data_removed {
        Vec::new()
    } else {
        vec![ModuleKvMutation {
            key: "data".into(),
            value: Some(serde_json::to_string(&state)?),
        }]
    };
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Game, Player, PlayerBlockade};
    use crate::voyage::VoyageOption;
    use jeeves_abi::{DataSubject, ModuleKvEntry};

    fn request(state: &State) -> ModuleDataRequest {
        ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: DataSubject {
                server: "net".into(),
                profile_id: "player".into(),
            },
            aliases: Vec::new(),
            entries: vec![ModuleKvEntry {
                key: "data".into(),
                value: serde_json::to_string(state).unwrap(),
            }],
        }
    }

    fn state_with_personal_menu_data() -> State {
        let mut state = State::default();
        state
            .pm_sessions
            .insert("net/player".into(), Default::default());
        state.voyage_offers.insert(
            "net/player".into(),
            vec![VoyageOption {
                kind: crate::model::VoyageKind::Merchant,
                target_uuid: None,
                target_nick: None,
            }],
        );
        state
    }

    #[test]
    fn voyage_offers_export_and_delete_without_a_matching_player() {
        let state = state_with_personal_menu_data();
        let export: ModuleDataResponse =
            serde_json::from_str(&data_export(&request(&state)).unwrap()).unwrap();
        assert_eq!(
            export.data["voyage_offers"]["net/player"][0]["kind"],
            "merchant"
        );

        let plan: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete(&request(&state)).unwrap()).unwrap();
        let rewritten: State =
            serde_json::from_str(plan.mutations[0].value.as_deref().expect("state rewrite"))
                .unwrap();
        assert!(!rewritten.pm_sessions.contains_key("net/player"));
        assert!(!rewritten.voyage_offers.contains_key("net/player"));
    }

    #[test]
    fn deleting_blockader_returns_escrow_to_target_before_removal() {
        let mut state = State::default();
        let mut game = Game::default();
        game.players.insert("player".into(), Player::default());
        game.players.insert(
            "target".into(),
            Player {
                gold: 9,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "player".into(),
                    escrow_gold: 31,
                    escrow_rum: 5,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        state.games.insert("net".into(), game);
        let plan: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete(&request(&state)).unwrap()).unwrap();
        let rewritten: State =
            serde_json::from_str(plan.mutations[0].value.as_deref().unwrap()).unwrap();
        assert_eq!(rewritten.games["net"].players["target"].gold, 40);
        assert_eq!(rewritten.games["net"].players["target"].rum, 5);
        assert!(rewritten.games["net"].players["target"]
            .player_blockade
            .is_none());
        assert!(!rewritten.games["net"].players.contains_key("player"));
    }

    #[test]
    fn blockader_export_includes_only_its_external_commitment() {
        let mut state = State::default();
        let mut game = Game::default();
        game.players.insert("player".into(), Player::default());
        game.players.insert(
            "target".into(),
            Player {
                gold: 999,
                player_blockade: Some(PlayerBlockade {
                    blockader_uuid: "player".into(),
                    escrow_gold: 20,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        state.games.insert("net".into(), game);
        let export: ModuleDataResponse =
            serde_json::from_str(&data_export(&request(&state)).unwrap()).unwrap();
        assert_eq!(
            export.data["player_blockades"]["net"][0]["blockade"]["escrow_gold"],
            20
        );
        assert!(export.data["games"]["net"]["players"]
            .get("target")
            .is_none());
    }

    fn deleted_state(request: &ModuleDataRequest) -> State {
        let plan: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete(request).unwrap()).unwrap();
        serde_json::from_str(plan.mutations[0].value.as_deref().unwrap()).unwrap()
    }

    #[test]
    fn a_reused_nick_alias_never_touches_another_captains_isle() {
        let mut state = State::default();
        let mut game = Game::default();
        game.players.insert("player".into(), Player::default());
        game.players.insert(
            "someone-else".into(),
            Player {
                nick_cache: "Bob".into(),
                gold: 999,
                ..Default::default()
            },
        );
        state.games.insert("net".into(), game);
        let mut request = request(&state);
        // The subject once used the nick "Bob"; another captain wears it now.
        request.aliases = vec!["Bob".into()];

        let export: ModuleDataResponse =
            serde_json::from_str(&data_export(&request).unwrap()).unwrap();
        assert!(!export.data.to_string().contains("someone-else"));

        let after = deleted_state(&request);
        assert!(!after.games["net"].players.contains_key("player"));
        assert_eq!(after.games["net"].players["someone-else"].gold, 999);
    }

    #[test]
    fn deleting_a_target_sends_attackers_and_allies_home_and_frees_the_navy() {
        let mut state = State::default();
        let mut game = Game::default();
        game.players.insert("player".into(), Player::default());
        game.players.insert("raider".into(), Player::default());
        game.voyages.push(crate::model::Voyage {
            id: 7,
            owner_uuid: "raider".into(),
            kind: crate::model::VoyageKind::Raid,
            target_uuid: Some("player".into()),
            crew_regular: 4,
            crew_loyal: 1,
            ..Default::default()
        });
        game.navy_harassments.push(crate::model::NavyHarassment {
            id: 8,
            owner_uuid: "raider".into(),
            target_uuid: "player".into(),
            crew_regular: 2,
            ..Default::default()
        });
        game.navy_pending_target = Some("player".into());
        game.navy_pending_hit_at = 50;
        state.games.insert("net".into(), game);

        let after = deleted_state(&request(&state));
        let game = &after.games["net"];
        assert!(game.voyages.is_empty() && game.navy_harassments.is_empty());
        assert_eq!(
            (
                game.players["raider"].crew_regular,
                game.players["raider"].crew_loyal
            ),
            (6, 1),
            "the raid's and the sortie's crews came home"
        );
        assert!(game.navy_pending_target.is_none());
    }
}
