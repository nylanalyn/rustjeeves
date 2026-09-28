//! Server-wide achievement views: leaders, rarest achievements, and recent unlocks.
//!
//! Only the DB actor calls this (it takes its connection). Unlocks are matched against the
//! currently loaded catalogues, so achievements from retired modules don't count, and derived
//! "meta" collection milestones are left out. Secret achievement names are masked.

use anyhow::Result;
use jeeves_abi::{
    AchievementBoardRequest, AchievementBoardResponse, AchievementLeader, AchievementManifest,
    AchievementRarity, AchievementRecentUnlock,
};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet, HashMap};

const MAX_LIMIT: u32 = 200;
const SECRET_NAME: &str = "Secret";

struct Unlock {
    profile_id: String,
    nick: String,
    module: String,
    id: String,
    unlocked_at: i64,
}

pub fn board(
    conn: &Connection,
    request: &AchievementBoardRequest,
    manifests: &[(String, AchievementManifest)],
) -> Result<AchievementBoardResponse> {
    let (server, module, limit) = match request {
        AchievementBoardRequest::Top {
            server,
            module,
            limit,
        }
        | AchievementBoardRequest::Rare {
            server,
            module,
            limit,
        } => (server, module.as_deref(), *limit),
        AchievementBoardRequest::Unlocks { server, limit, .. } => (server, None, *limit),
    };
    let limit = limit.clamp(1, MAX_LIMIT) as usize;
    // (module, id) → (name, secret) for every achievement in scope.
    let catalogue = manifests
        .iter()
        .filter(|(name, _)| module.is_none_or(|wanted| name.eq_ignore_ascii_case(wanted)))
        .flat_map(|(name, manifest)| {
            manifest.achievements.iter().map(move |item| {
                (
                    (name.clone(), item.id.clone()),
                    (item.name.clone(), item.secret),
                )
            })
        })
        .collect::<HashMap<_, _>>();
    let mut stmt = conn.prepare(
        "SELECT u.profile_id, p.nick, u.module, u.achievement_id, u.unlocked_at
         FROM achievement_unlocks u
         JOIN profiles p ON p.server=u.server AND p.id=u.profile_id
         WHERE u.server=?1 AND p.achievements_opt_out=0 AND u.module != 'meta'",
    )?;
    let unlocks = stmt
        .query_map([server], |row| {
            Ok(Unlock {
                profile_id: row.get(0)?,
                nick: row.get(1)?,
                module: row.get(2)?,
                id: row.get(3)?,
                unlocked_at: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|unlock| catalogue.contains_key(&(unlock.module.clone(), unlock.id.clone())))
        .collect::<Vec<_>>();
    let mut response = AchievementBoardResponse {
        collectors: unlocks
            .iter()
            .map(|unlock| unlock.profile_id.as_str())
            .collect::<BTreeSet<_>>()
            .len() as u64,
        available: catalogue.len() as u64,
        ..AchievementBoardResponse::default()
    };
    let display_name = |module: &str, id: &str| {
        let (name, secret) = &catalogue[&(module.to_string(), id.to_string())];
        if *secret {
            (SECRET_NAME.to_string(), true)
        } else {
            (name.clone(), false)
        }
    };
    match request {
        AchievementBoardRequest::Top { .. } => {
            // profile → (nick, earned, latest unlock); ties go to whoever got there first.
            let mut leaders = BTreeMap::<&str, (&str, u64, i64)>::new();
            for unlock in &unlocks {
                let entry =
                    leaders
                        .entry(&unlock.profile_id)
                        .or_insert((&unlock.nick, 0, i64::MIN));
                entry.1 += 1;
                entry.2 = entry.2.max(unlock.unlocked_at);
            }
            let mut leaders = leaders.into_iter().collect::<Vec<_>>();
            leaders
                .sort_by(|(_, left), (_, right)| right.1.cmp(&left.1).then(left.2.cmp(&right.2)));
            response.top = leaders
                .into_iter()
                .take(limit)
                .map(|(profile_id, (nick, earned, _))| AchievementLeader {
                    profile_id: profile_id.into(),
                    nick: nick.into(),
                    earned,
                })
                .collect();
        }
        AchievementBoardRequest::Rare { .. } => {
            let mut holders = BTreeMap::<(&str, &str), u64>::new();
            for unlock in &unlocks {
                *holders.entry((&unlock.module, &unlock.id)).or_default() += 1;
            }
            let mut rare = holders
                .into_iter()
                .map(|((module, id), holders)| {
                    let (name, secret) = display_name(module, id);
                    AchievementRarity {
                        module: module.into(),
                        id: id.into(),
                        name,
                        holders,
                        secret,
                    }
                })
                .collect::<Vec<_>>();
            rare.sort_by(|left, right| {
                left.holders
                    .cmp(&right.holders)
                    .then(left.secret.cmp(&right.secret))
                    .then_with(|| left.name.cmp(&right.name))
            });
            rare.truncate(limit);
            response.rare = rare;
        }
        AchievementBoardRequest::Unlocks { since, .. } => {
            let mut recent = unlocks
                .iter()
                .filter(|unlock| unlock.unlocked_at >= *since)
                .map(|unlock| {
                    let (name, secret) = display_name(&unlock.module, &unlock.id);
                    AchievementRecentUnlock {
                        profile_id: unlock.profile_id.clone(),
                        nick: unlock.nick.clone(),
                        module: unlock.module.clone(),
                        id: unlock.id.clone(),
                        name,
                        unlocked_at: unlock.unlocked_at,
                        secret,
                    }
                })
                .collect::<Vec<_>>();
            recent.sort_by_key(|unlock| std::cmp::Reverse(unlock.unlocked_at));
            recent.truncate(limit);
            response.unlocks = recent;
        }
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::AchievementSpec;

    fn manifest() -> Vec<(String, AchievementManifest)> {
        let spec = |id: &str, secret: bool| AchievementSpec {
            id: id.into(),
            name: format!("{id} name"),
            description: String::new(),
            stat: "wins".into(),
            threshold: 1,
            optional: secret,
            secret,
        };
        vec![(
            "game".into(),
            AchievementManifest {
                achievements: vec![
                    spec("first", false),
                    spec("second", false),
                    spec("hidden", true),
                ],
                ..AchievementManifest::default()
            },
        )]
    }

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE profiles (server TEXT, id TEXT, nick TEXT, achievements_opt_out INTEGER);
             CREATE TABLE achievement_unlocks (server TEXT, profile_id TEXT, module TEXT,
                 achievement_id TEXT, unlocked_at INTEGER);
             INSERT INTO profiles VALUES ('net','a','alice',0), ('net','b','bob',0),
                 ('net','c','carol',1), ('other','d','dan',0);
             INSERT INTO achievement_unlocks VALUES
                 ('net','a','game','first',10), ('net','a','game','second',20),
                 ('net','b','game','first',5), ('net','b','game','hidden',30),
                 ('net','b','retired','gone',1), ('net','c','game','first',1),
                 ('other','d','game','first',1);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn leaders_rarity_and_unlocks_respect_scope_and_secrets() {
        let conn = setup();
        let top = board(
            &conn,
            &AchievementBoardRequest::Top {
                server: "net".into(),
                module: None,
                limit: 10,
            },
            &manifest(),
        )
        .unwrap();
        // alice and bob both hold two current achievements; alice finished first (20 < 30).
        assert_eq!(
            top.top
                .iter()
                .map(|leader| leader.nick.as_str())
                .collect::<Vec<_>>(),
            ["alice", "bob"]
        );
        assert_eq!((top.collectors, top.available), (2, 3));

        let rare = board(
            &conn,
            &AchievementBoardRequest::Rare {
                server: "net".into(),
                module: Some("GAME".into()),
                limit: 10,
            },
            &manifest(),
        )
        .unwrap();
        assert_eq!(rare.rare[0].name, "second name");
        assert!(rare.rare[1].secret && rare.rare[1].name == SECRET_NAME);
        assert_eq!(rare.rare.last().unwrap().holders, 2);

        let recent = board(
            &conn,
            &AchievementBoardRequest::Unlocks {
                server: "net".into(),
                since: 10,
                limit: 10,
            },
            &manifest(),
        )
        .unwrap();
        assert_eq!(
            recent
                .unlocks
                .iter()
                .map(|unlock| unlock.unlocked_at)
                .collect::<Vec<_>>(),
            [30, 20, 10]
        );
        assert_eq!(recent.unlocks[0].name, SECRET_NAME);
    }
}
