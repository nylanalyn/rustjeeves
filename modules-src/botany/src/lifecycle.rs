use crate::model::{GardenBook, STATS};
use extism_pdk::*;
use jeeves_abi::*;
use jeeves_guest::encode;

fn decode(value: &str) -> Result<String, Error> {
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return Err(Error::msg("invalid botany key"));
    }
    let bytes = (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(String::from_utf8(bytes)?)
}

// No nick-keyed state is created. Accept old alias ownership if an imported record has one.
fn fold(nick: &str) -> String {
    nick.to_ascii_lowercase()
        .chars()
        .map(|c| match c {
            '[' => '{',
            ']' => '}',
            '\\' => '|',
            '~' => '^',
            _ => c,
        })
        .collect()
}

fn owned(request: &ModuleDataRequest, key: &str, kind: &str) -> Result<bool, Error> {
    let prefix = format!("{kind}:{}:", encode(&request.subject.server));
    let Some(tail) = key.strip_prefix(&prefix) else {
        return Ok(false);
    };
    let id = decode(tail)?;
    Ok(id == request.subject.profile_id
        || id.strip_prefix("nick:").is_some_and(|nick| {
            request
                .aliases
                .iter()
                .any(|alias| fold(nick) == fold(alias))
        }))
}

fn relevant(request: &ModuleDataRequest) -> Result<Vec<&ModuleKvEntry>, Error> {
    if request.version != DATA_LIFECYCLE_VERSION {
        return Err(Error::msg("unsupported lifecycle version"));
    }
    let mut result = Vec::new();
    for entry in &request.entries {
        if owned(request, &entry.key, "plant")? {
            GardenBook::parse(&entry.value)?;
            result.push(entry);
        } else if owned(request, &entry.key, "cooldown")? {
            entry.value.parse::<i64>()?;
            result.push(entry);
        }
    }
    Ok(result)
}

#[cfg_attr(not(test), plugin_fn)]
pub fn data_export(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    let entries = relevant(&request)?;
    Ok(serde_json::to_string(&ModuleDataResponse {
        version: DATA_LIFECYCLE_VERSION,
        data: if entries.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({ "version": 1, "entries": entries })
        },
    })?)
}

#[cfg_attr(not(test), plugin_fn)]
pub fn data_delete(input: String) -> FnResult<String> {
    let request: ModuleDataRequest = serde_json::from_str(&input)?;
    Ok(serde_json::to_string(&ModuleDataDeletePlan {
        version: DATA_LIFECYCLE_VERSION,
        mutations: relevant(&request)?
            .into_iter()
            .map(|entry| ModuleKvMutation {
                key: entry.key.clone(),
                value: None,
            })
            .collect(),
    })?)
}

#[cfg_attr(not(test), plugin_fn)]
pub fn achievement_backfill(input: String) -> FnResult<String> {
    let request: AchievementBackfillRequest = serde_json::from_str(&input)?;
    let prefix = format!("plant:{}:", encode(&request.server));
    let mut values = Vec::new();
    for entry in &request.entries {
        let Some(tail) = entry.key.strip_prefix(&prefix) else {
            continue;
        };
        let id = decode(tail)?;
        let book = GardenBook::parse(&entry.value)?;
        if id.is_empty() || id.starts_with("nick:") {
            continue;
        }
        for (i, stat) in STATS.iter().enumerate() {
            values.push(AchievementSetMax {
                profile_id: id.clone(),
                stat: (*stat).into(),
                value: book.totals[i],
            });
        }
    }
    Ok(serde_json::to_string(&AchievementBackfillResponse {
        values,
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book_key;
    fn request() -> ModuleDataRequest {
        ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: DataSubject {
                server: "net".into(),
                profile_id: "uuid".into(),
            },
            aliases: vec!["Old[Nick".into()],
            entries: vec![
                ModuleKvEntry {
                    key: book_key("net", "uuid"),
                    value: serde_json::to_string(&GardenBook::default()).unwrap(),
                },
                ModuleKvEntry {
                    key: book_key("other", "uuid"),
                    value: "malformed foreign record".into(),
                },
                ModuleKvEntry {
                    key: book_key("net", "someone-else"),
                    value: "malformed unrelated record".into(),
                },
                ModuleKvEntry {
                    key: book_key("net", "nick:old{nick"),
                    value: serde_json::to_string(&GardenBook::default()).unwrap(),
                },
                ModuleKvEntry {
                    key: format!("cooldown:{}:{}", encode("net"), encode("uuid")),
                    value: "123".into(),
                },
            ],
        }
    }

    #[test]
    fn lifecycle_isolated_alias_aware_and_idempotent() {
        let mut req = request();
        let exported: ModuleDataResponse =
            serde_json::from_str(&data_export(serde_json::to_string(&req).unwrap()).unwrap())
                .unwrap();
        assert_eq!(exported.data["entries"].as_array().unwrap().len(), 3);
        let plan: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete(serde_json::to_string(&req).unwrap()).unwrap())
                .unwrap();
        assert_eq!(plan.mutations.len(), 3);
        req.entries
            .retain(|entry| !plan.mutations.iter().any(|m| m.key == entry.key));
        let again: ModuleDataDeletePlan =
            serde_json::from_str(&data_delete(serde_json::to_string(&req).unwrap()).unwrap())
                .unwrap();
        assert!(again.mutations.is_empty());
    }

    #[test]
    fn malformed_owned_data_fails_both_hooks() {
        let mut req = request();
        req.entries[0].value = "broken".into();
        assert!(data_export(serde_json::to_string(&req).unwrap()).is_err());
        assert!(data_delete(serde_json::to_string(&req).unwrap()).is_err());
    }

    #[test]
    fn backfill_uses_absolute_totals_and_network_boundary() {
        let book = GardenBook {
            totals: [2, 56, 2, 1, 5, 0],
            ..Default::default()
        };
        let req = AchievementBackfillRequest {
            server: "net".into(),
            previous_version: 0,
            catalog_version: 1,
            entries: vec![
                ModuleKvEntry {
                    key: book_key("net", "uuid"),
                    value: serde_json::to_string(&book).unwrap(),
                },
                ModuleKvEntry {
                    key: book_key("other", "uuid"),
                    value: "broken".into(),
                },
            ],
        };
        let input = serde_json::to_string(&req).unwrap();
        let result: AchievementBackfillResponse =
            serde_json::from_str(&achievement_backfill(input.clone()).unwrap()).unwrap();
        assert_eq!(result.values.len(), 6);
        assert_eq!(result.values[1].value, 56);
        assert_eq!(
            achievement_backfill(input.clone()).unwrap(),
            achievement_backfill(input).unwrap()
        );
    }
}
