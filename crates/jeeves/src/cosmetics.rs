//! Host-owned cosmetics: which badges and flourishes each profile owns and wears.
//!
//! Only the DB actor calls these functions (they take its connection). Modules reach them through
//! the `cosmetics` capability (grant, list, wear) and the read-only `cosmetics_read` capability
//! (what several profiles are wearing, for decorating leaderboards and wins).

use anyhow::{anyhow, Result};
use jeeves_abi::{
    Cosmetic, CosmeticGrantRequest, CosmeticGrantResponse, CosmeticInventory, CosmeticKind,
    CosmeticWearResponse, WornCosmetics,
};
use rusqlite::{Connection, OptionalExtension};

const MAX_ID_CHARS: usize = 48;
const MAX_NAME_CHARS: usize = 60;
const MAX_BADGE_CHARS: usize = 16;
const MAX_FLOURISH_CHARS: usize = 40;
/// Bound on one `cosmetics_worn` lookup.
pub const MAX_WORN_LOOKUP: usize = 100;

pub fn create_tables(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS cosmetics_owned (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, kind TEXT NOT NULL, item_id TEXT NOT NULL,
            name TEXT NOT NULL, value TEXT NOT NULL, module TEXT NOT NULL, event_id TEXT NOT NULL,
            acquired_at INTEGER NOT NULL,
            PRIMARY KEY(server, profile_id, kind, item_id)
        );
        CREATE TABLE IF NOT EXISTS cosmetics_worn (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, kind TEXT NOT NULL, item_id TEXT NOT NULL,
            PRIMARY KEY(server, profile_id, kind)
        );
        "#,
    )?;
    Ok(())
}

fn kind_from_str(value: &str) -> Option<CosmeticKind> {
    CosmeticKind::parse(value)
}

fn validate(cosmetic: &Cosmetic) -> Result<()> {
    let id_ok = !cosmetic.id.is_empty()
        && cosmetic.id.chars().count() <= MAX_ID_CHARS
        && cosmetic
            .id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_');
    let text_ok = |text: &str, max: usize| {
        let count = text.chars().count();
        count > 0 && count <= max && !text.chars().any(char::is_control) && text.trim() == text
    };
    let value_max = match cosmetic.kind {
        CosmeticKind::Badge => MAX_BADGE_CHARS,
        CosmeticKind::Flourish => MAX_FLOURISH_CHARS,
    };
    if !id_ok || !text_ok(&cosmetic.name, MAX_NAME_CHARS) || !text_ok(&cosmetic.value, value_max) {
        return Err(anyhow!("invalid cosmetic {:?}", cosmetic.id));
    }
    Ok(())
}

pub fn grant(
    conn: &Connection,
    module: &str,
    request: &CosmeticGrantRequest,
    now: i64,
) -> Result<CosmeticGrantResponse> {
    validate(&request.cosmetic)?;
    if request.event_id.is_empty() || request.event_id.len() > 200 {
        return Err(anyhow!("cosmetic grants need a bounded event id"));
    }
    let kind = request.cosmetic.kind.as_str();
    let existing: Option<String> = conn
        .query_row(
            "SELECT event_id FROM cosmetics_owned
             WHERE server=?1 AND profile_id=?2 AND kind=?3 AND item_id=?4",
            (
                &request.server,
                &request.profile_id,
                kind,
                &request.cosmetic.id,
            ),
            |row| row.get(0),
        )
        .optional()?;
    if let Some(event_id) = existing {
        let replay = event_id == request.event_id;
        return Ok(CosmeticGrantResponse {
            granted: replay,
            duplicate: !replay,
        });
    }
    conn.execute(
        "INSERT INTO cosmetics_owned(server,profile_id,kind,item_id,name,value,module,event_id,acquired_at)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![
            request.server,
            request.profile_id,
            kind,
            request.cosmetic.id,
            request.cosmetic.name,
            request.cosmetic.value,
            module,
            request.event_id,
            now
        ],
    )?;
    Ok(CosmeticGrantResponse {
        granted: true,
        duplicate: false,
    })
}

fn row_to_cosmetic(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Cosmetic>> {
    let kind: String = row.get(0)?;
    Ok(kind_from_str(&kind).map(|kind| Cosmetic {
        kind,
        id: row.get(1).unwrap_or_default(),
        name: row.get(2).unwrap_or_default(),
        value: row.get(3).unwrap_or_default(),
        module: row.get(4).unwrap_or_default(),
        acquired_at: row.get(5).unwrap_or_default(),
    }))
}

pub fn list(conn: &Connection, server: &str, profile_id: &str) -> Result<CosmeticInventory> {
    let mut stmt = conn.prepare(
        "SELECT kind,item_id,name,value,module,acquired_at FROM cosmetics_owned
         WHERE server=?1 AND profile_id=?2 ORDER BY kind, acquired_at, item_id",
    )?;
    let owned = stmt
        .query_map((server, profile_id), row_to_cosmetic)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut worn_stmt =
        conn.prepare("SELECT kind,item_id FROM cosmetics_worn WHERE server=?1 AND profile_id=?2")?;
    let worn = worn_stmt
        .query_map((server, profile_id), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let wearing = |kind: CosmeticKind| {
        worn.iter()
            .find(|(worn_kind, _)| worn_kind == kind.as_str())
            .and_then(|(_, id)| {
                owned
                    .iter()
                    .find(|item| item.kind == kind && &item.id == id)
                    .cloned()
            })
    };
    Ok(CosmeticInventory {
        badge: wearing(CosmeticKind::Badge),
        flourish: wearing(CosmeticKind::Flourish),
        owned,
    })
}

pub fn wear(
    conn: &Connection,
    server: &str,
    profile_id: &str,
    kind: CosmeticKind,
    id: Option<&str>,
) -> Result<CosmeticWearResponse> {
    let Some(id) = id else {
        conn.execute(
            "DELETE FROM cosmetics_worn WHERE server=?1 AND profile_id=?2 AND kind=?3",
            (server, profile_id, kind.as_str()),
        )?;
        return Ok(CosmeticWearResponse {
            ok: true,
            worn: None,
        });
    };
    let inventory = list(conn, server, profile_id)?;
    let Some(item) = inventory
        .owned
        .into_iter()
        .find(|item| item.kind == kind && item.id == id)
    else {
        return Ok(CosmeticWearResponse::default());
    };
    conn.execute(
        "INSERT INTO cosmetics_worn(server,profile_id,kind,item_id) VALUES(?1,?2,?3,?4)
         ON CONFLICT(server,profile_id,kind) DO UPDATE SET item_id=excluded.item_id",
        (server, profile_id, kind.as_str(), id),
    )?;
    Ok(CosmeticWearResponse {
        ok: true,
        worn: Some(item),
    })
}

/// The worn badge and flourish values for each requested profile, in request order. Profiles
/// wearing nothing are still returned, with both values empty.
pub fn worn(conn: &Connection, server: &str, profile_ids: &[String]) -> Result<Vec<WornCosmetics>> {
    let mut stmt = conn.prepare(
        "SELECT w.kind, o.value FROM cosmetics_worn w
         JOIN cosmetics_owned o ON o.server=w.server AND o.profile_id=w.profile_id
             AND o.kind=w.kind AND o.item_id=w.item_id
         WHERE w.server=?1 AND w.profile_id=?2",
    )?;
    let mut result = Vec::new();
    for profile_id in profile_ids.iter().take(MAX_WORN_LOOKUP) {
        let mut entry = WornCosmetics {
            profile_id: profile_id.clone(),
            ..WornCosmetics::default()
        };
        let rows = stmt
            .query_map((server, profile_id), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (kind, value) in rows {
            match kind_from_str(&kind) {
                Some(CosmeticKind::Badge) => entry.badge = Some(value),
                Some(CosmeticKind::Flourish) => entry.flourish = Some(value),
                None => {}
            }
        }
        result.push(entry);
    }
    Ok(result)
}

/// Erasure: everything the profile owns and wears on this server.
pub fn delete_profile(conn: &Connection, server: &str, profile_id: &str) -> Result<()> {
    for table in ["cosmetics_owned", "cosmetics_worn"] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE server=?1 AND profile_id=?2"),
            (server, profile_id),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn badge(id: &str, value: &str) -> Cosmetic {
        Cosmetic {
            kind: CosmeticKind::Badge,
            id: id.into(),
            name: format!("{id} badge"),
            value: value.into(),
            module: String::new(),
            acquired_at: 0,
        }
    }

    fn grant_request(profile: &str, cosmetic: Cosmetic, event: &str) -> CosmeticGrantRequest {
        CosmeticGrantRequest {
            server: "net".into(),
            profile_id: profile.into(),
            cosmetic,
            event_id: event.into(),
        }
    }

    #[test]
    fn grants_are_idempotent_per_event_and_report_duplicates() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        let first = grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("owl", "🦉"), "e1"),
            5,
        );
        assert_eq!(
            first.unwrap(),
            CosmeticGrantResponse {
                granted: true,
                duplicate: false
            }
        );
        let replay = grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("owl", "🦉"), "e1"),
            6,
        );
        assert!(
            replay.unwrap().granted,
            "a retried event is still the grant"
        );
        let again = grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("owl", "🦉"), "e2"),
            7,
        );
        assert!(again.unwrap().duplicate);
        assert!(grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("Bad Id", "x"), "e3"),
            7
        )
        .is_err());
        assert!(grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("long", &"x".repeat(17)), "e4"),
            7
        )
        .is_err());
    }

    #[test]
    fn wearing_requires_ownership_and_shows_everywhere() {
        let conn = Connection::open_in_memory().unwrap();
        create_tables(&conn).unwrap();
        grant(
            &conn,
            "gacha",
            &grant_request("p1", badge("owl", "🦉"), "e1"),
            5,
        )
        .unwrap();
        assert!(
            !wear(&conn, "net", "p1", CosmeticKind::Badge, Some("cat"))
                .unwrap()
                .ok
        );
        let worn_now = wear(&conn, "net", "p1", CosmeticKind::Badge, Some("owl")).unwrap();
        assert_eq!(worn_now.worn.unwrap().value, "🦉");
        assert_eq!(list(&conn, "net", "p1").unwrap().badge.unwrap().id, "owl");
        let decorated = worn(&conn, "net", &["p1".into(), "p2".into()]).unwrap();
        assert_eq!(decorated[0].badge.as_deref(), Some("🦉"));
        assert_eq!(
            decorated[1],
            WornCosmetics {
                profile_id: "p2".into(),
                ..Default::default()
            }
        );
        wear(&conn, "net", "p1", CosmeticKind::Badge, None).unwrap();
        assert!(list(&conn, "net", "p1").unwrap().badge.is_none());
        delete_profile(&conn, "net", "p1").unwrap();
        assert!(list(&conn, "net", "p1").unwrap().owned.is_empty());
    }
}
