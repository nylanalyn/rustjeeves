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

pub(crate) fn data_export(request: &ModuleDataRequest) -> Result<String, Error> {
    let Some(raw) = data_entry(request) else {
        return Ok(serde_json::to_string(&ModuleDataResponse {
            version: DATA_LIFECYCLE_VERSION,
            data: serde_json::Value::Null,
        })?);
    };
    let state: State = serde_json::from_str(raw)?;
    let prefix = format!("{}/", request.subject.server);
    let mut games = HashMap::new();
    for (key, game) in state.games {
        if !key.starts_with(&prefix) {
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
    let data = if games.is_empty() && sessions.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "games": games, "pm_sessions": sessions })
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
    let mut state: State = serde_json::from_str(raw)?;
    let prefix = format!("{}/", request.subject.server);
    let mut removed = Vec::new();
    for (key, game) in state.games.iter_mut() {
        if !key.starts_with(&prefix) {
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
    state.pm_sessions.retain(|key, _| {
        key != &format!("{}/{}", request.subject.server, request.subject.profile_id)
    });
    let mutations = if removed.is_empty() {
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
