//! Pure export and deletion planning for the module's single JSON state blob.

use crate::model::State;
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

fn belongs_to(profile_id: &str, nick: &str, request: &ModuleDataRequest) -> bool {
    profile_id == request.subject.profile_id || request.aliases.iter().any(|alias| alias == nick)
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
            .filter(|(uuid, player)| belongs_to(uuid, &player.nick_cache, request))
            .map(|(uuid, player)| (uuid.clone(), player.clone()))
            .collect::<HashMap<_, _>>();
        let blockades = game
            .players
            .iter()
            .filter(|(_, target)| {
                target.player_blockade.as_ref().is_some_and(|blockade| {
                    blockade.blockader_uuid == request.subject.profile_id
                        || request
                            .aliases
                            .iter()
                            .any(|alias| alias == &blockade.blockader_nick)
                })
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
            .filter(|(uuid, player)| belongs_to(uuid, &player.nick_cache, request))
            .map(|(uuid, _)| uuid.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            continue;
        }
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
        for id in &ids {
            game.players.remove(id);
        }
        game.voyages.retain(|voyage| {
            !ids.contains(&voyage.owner_uuid)
                && !voyage
                    .target_uuid
                    .as_ref()
                    .is_some_and(|target| ids.contains(target))
        });
        game.prisoners.retain(|prisoner| {
            !ids.contains(&prisoner.holder_uuid) && !ids.contains(&prisoner.origin_uuid)
        });
        game.ransoms.retain(|ransom| {
            !ids.contains(&ransom.holder_uuid) && !ids.contains(&ransom.target_uuid)
        });
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
}
