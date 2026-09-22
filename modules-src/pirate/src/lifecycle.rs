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
    for (key, game) in state.games {
        if key != request.subject.server {
            continue;
        }
        let players = game
            .players
            .into_iter()
            .filter(|(uuid, player)| belongs_to(uuid, &player.nick_cache, request))
            .collect::<HashMap<_, _>>();
        if players.is_empty() {
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
    let data = if games.is_empty() && sessions.is_empty() && voyage_offers.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "games": games, "pm_sessions": sessions, "voyage_offers": voyage_offers })
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
}
