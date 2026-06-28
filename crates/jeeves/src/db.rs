//! SQLite persistence behind an actor task.
//!
//! `rusqlite` is synchronous, so a single [`rusqlite::Connection`] lives on a dedicated OS thread
//! and is the *only* thing that touches the database. Async callers talk to it through
//! [`DbHandle`], which sends [`DbRequest`]s over a channel and awaits a oneshot reply.

use crate::commands::AliasOverrides;
use crate::config::{AdminEntry, ServerConfig};
use crate::log_bus::LogEvent;
use anyhow::{anyhow, Result};
use jeeves_abi::{Category, Level, Profile, ProfileUpdate, Role};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot};

/// Requests the DB actor understands. Each carries a oneshot sender for its reply.
enum DbRequest {
    ConfigGet {
        key: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    ConfigSet {
        key: String,
        value: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    LoadAliasOverrides(oneshot::Sender<Result<AliasOverrides>>),
    SetAliasOverride {
        module: String,
        command: String,
        aliases: Option<Vec<String>>,
        reply: oneshot::Sender<Result<()>>,
    },
    LoadServers(oneshot::Sender<Result<Vec<ServerConfig>>>),
    UpsertServer(Box<ServerConfig>, oneshot::Sender<Result<i64>>),
    DeleteServer(i64, oneshot::Sender<Result<()>>),
    KvGet {
        module: String,
        key: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    KvSet {
        module: String,
        key: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    AppendLog(LogEvent, oneshot::Sender<Result<()>>),
    ResolveRole {
        server_label: String,
        nick: String,
        hostmask: String,
        account: Option<String>,
        reply: oneshot::Sender<Result<Option<Role>>>,
    },
    LoadAdmins(i64, oneshot::Sender<Result<Vec<AdminEntry>>>),
    UpsertAdmin(i64, AdminEntry, oneshot::Sender<Result<()>>),
    DeleteAdmin(i64, String, oneshot::Sender<Result<()>>),
    ProfileEnsure {
        server: String,
        nick: String,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ProfileResolve {
        server: String,
        nick: String,
        account: Option<String>,
        now: i64,
        reply: oneshot::Sender<Result<Profile>>,
    },
    ProfileBindNick {
        server: String,
        old_nick: String,
        new_nick: String,
        account: Option<String>,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ProfileGet {
        server: String,
        nick: String,
        reply: oneshot::Sender<Result<Option<Profile>>>,
    },
    ProfileSet(Box<ProfileUpdate>, oneshot::Sender<Result<()>>),
    ProfileClear {
        server: String,
        nick: String,
        field: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Cloneable async handle to the DB actor.
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::Sender<DbRequest>,
}

impl DbHandle {
    /// Open (creating + migrating) the database at `path` and spawn its actor thread.
    pub fn open(path: &str) -> Result<DbHandle> {
        let conn = Connection::open(path)?;
        migrate(&conn)?;
        let (tx, mut rx) = mpsc::channel::<DbRequest>(64);

        std::thread::Builder::new()
            .name("jeeves-db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Some(req) = rx.blocking_recv() {
                    handle(&mut conn, req);
                }
            })?;

        Ok(DbHandle { tx })
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Synchronous variant of [`Self::call`] for the blocking TUI thread. Must NOT be called from
    /// within the async runtime.
    fn call_blocking<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(make(reply))
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }

    // --- Blocking accessors used by the TUI thread ---

    pub fn config_get_blocking(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.call_blocking(|reply| DbRequest::ConfigGet { key, reply })
    }

    pub fn config_set_blocking(&self, key: &str, value: Option<&str>) -> Result<()> {
        let key = key.to_string();
        let value = value.map(str::to_string);
        self.call_blocking(|reply| DbRequest::ConfigSet { key, value, reply })
    }

    pub fn load_alias_overrides_blocking(&self) -> Result<AliasOverrides> {
        self.call_blocking(DbRequest::LoadAliasOverrides)
    }

    pub fn set_alias_override_blocking(
        &self,
        module: &str,
        command: &str,
        aliases: Option<&[String]>,
    ) -> Result<()> {
        let module = module.to_string();
        let command = command.to_string();
        let aliases = aliases.map(<[String]>::to_vec);
        self.call_blocking(|reply| DbRequest::SetAliasOverride {
            module,
            command,
            aliases,
            reply,
        })
    }

    pub fn load_servers_blocking(&self) -> Result<Vec<ServerConfig>> {
        self.call_blocking(DbRequest::LoadServers)
    }

    pub fn upsert_server_blocking(&self, cfg: ServerConfig) -> Result<i64> {
        self.call_blocking(|reply| DbRequest::UpsertServer(Box::new(cfg), reply))
    }

    pub fn delete_server_blocking(&self, id: i64) -> Result<()> {
        self.call_blocking(|reply| DbRequest::DeleteServer(id, reply))
    }

    pub fn load_admins_blocking(&self, server_id: i64) -> Result<Vec<AdminEntry>> {
        self.call_blocking(|reply| DbRequest::LoadAdmins(server_id, reply))
    }

    pub fn upsert_admin_blocking(&self, server_id: i64, entry: AdminEntry) -> Result<()> {
        self.call_blocking(|reply| DbRequest::UpsertAdmin(server_id, entry, reply))
    }

    pub fn delete_admin_blocking(&self, server_id: i64, nick: &str) -> Result<()> {
        let nick = nick.to_string();
        self.call_blocking(|reply| DbRequest::DeleteAdmin(server_id, nick, reply))
    }

    // --- Profiles (blocking; called from the module-host thread) ---

    pub fn profile_ensure_blocking(&self, server: &str, nick: &str, now: i64) -> Result<()> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call_blocking(|reply| DbRequest::ProfileEnsure {
            server,
            nick,
            now,
            reply,
        })
    }

    pub fn profile_get_blocking(&self, server: &str, nick: &str) -> Result<Option<Profile>> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call_blocking(|reply| DbRequest::ProfileGet {
            server,
            nick,
            reply,
        })
    }

    pub fn profile_set_blocking(&self, update: ProfileUpdate) -> Result<()> {
        self.call_blocking(|reply| DbRequest::ProfileSet(Box::new(update), reply))
    }

    pub fn profile_clear_blocking(&self, server: &str, nick: &str, field: &str) -> Result<()> {
        let (server, nick, field) = (server.to_string(), nick.to_string(), field.to_string());
        self.call_blocking(|reply| DbRequest::ProfileClear {
            server,
            nick,
            field,
            reply,
        })
    }

    /// All configured server profiles, ordered by id.
    pub async fn load_servers(&self) -> Result<Vec<ServerConfig>> {
        self.call(DbRequest::LoadServers).await
    }

    pub async fn append_log(&self, ev: LogEvent) -> Result<()> {
        self.call(|reply| DbRequest::AppendLog(ev, reply)).await
    }

    /// Resolve a sender's permission role on `server_label`, performing trust-on-first-use binding
    /// as a side effect. Returns `None` if the sender is not a configured admin or identity check
    /// fails.
    pub async fn resolve_role(
        &self,
        server_label: &str,
        nick: &str,
        hostmask: &str,
        account: Option<String>,
    ) -> Result<Option<Role>> {
        let (server_label, nick, hostmask) = (
            server_label.to_string(),
            nick.to_string(),
            hostmask.to_string(),
        );
        self.call(|reply| DbRequest::ResolveRole {
            server_label,
            nick,
            hostmask,
            account,
            reply,
        })
        .await
    }

    /// Resolve or create the stable profile for an observed IRC identity.
    pub async fn profile_resolve(
        &self,
        server: &str,
        nick: &str,
        account: Option<String>,
        now: i64,
    ) -> Result<Profile> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call(|reply| DbRequest::ProfileResolve {
            server,
            nick,
            account,
            now,
            reply,
        })
        .await
    }

    /// Record a NICK change as another alias of the same stable profile.
    pub async fn profile_bind_nick(
        &self,
        server: &str,
        old_nick: &str,
        new_nick: &str,
        account: Option<String>,
        now: i64,
    ) -> Result<()> {
        let (server, old_nick, new_nick) = (
            server.to_string(),
            old_nick.to_string(),
            new_nick.to_string(),
        );
        self.call(|reply| DbRequest::ProfileBindNick {
            server,
            old_nick,
            new_nick,
            account,
            now,
            reply,
        })
        .await
    }

    /// Blocking KV get for use from the synchronous module-host thread. Must NOT be called from
    /// within the async runtime.
    pub fn kv_get_blocking(&self, module: &str, key: &str) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(DbRequest::KvGet {
                module: module.to_string(),
                key: key.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Blocking KV set for use from the synchronous module-host thread.
    pub fn kv_set_blocking(&self, module: &str, key: &str, value: &str) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(DbRequest::KvSet {
                module: module.to_string(),
                key: key.to_string(),
                value: value.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }
}

fn handle(conn: &mut Connection, req: DbRequest) {
    match req {
        DbRequest::ConfigGet { key, reply } => {
            let _ = reply.send(config_get(conn, &key));
        }
        DbRequest::ConfigSet { key, value, reply } => {
            let _ = reply.send(config_set(conn, &key, value.as_deref()));
        }
        DbRequest::LoadAliasOverrides(reply) => {
            let _ = reply.send(load_alias_overrides(conn));
        }
        DbRequest::SetAliasOverride {
            module,
            command,
            aliases,
            reply,
        } => {
            let _ = reply.send(set_alias_override(
                conn,
                &module,
                &command,
                aliases.as_deref(),
            ));
        }
        DbRequest::LoadServers(reply) => {
            let _ = reply.send(load_servers(conn));
        }
        DbRequest::UpsertServer(cfg, reply) => {
            let _ = reply.send(upsert_server(conn, &cfg));
        }
        DbRequest::DeleteServer(id, reply) => {
            let _ = reply.send(delete_server(conn, id));
        }
        DbRequest::KvGet { module, key, reply } => {
            let _ = reply.send(kv_get(conn, &module, &key));
        }
        DbRequest::KvSet {
            module,
            key,
            value,
            reply,
        } => {
            let _ = reply.send(kv_set(conn, &module, &key, &value));
        }
        DbRequest::AppendLog(ev, reply) => {
            let _ = reply.send(append_log(conn, &ev));
        }
        DbRequest::ResolveRole {
            server_label,
            nick,
            hostmask,
            account,
            reply,
        } => {
            let _ = reply.send(resolve_role(
                conn,
                &server_label,
                &nick,
                &hostmask,
                account.as_deref(),
            ));
        }
        DbRequest::LoadAdmins(server_id, reply) => {
            let _ = reply.send(load_admins(conn, server_id));
        }
        DbRequest::UpsertAdmin(server_id, entry, reply) => {
            let _ = reply.send(upsert_admin(conn, server_id, &entry));
        }
        DbRequest::DeleteAdmin(server_id, nick, reply) => {
            let _ = reply.send(delete_admin(conn, server_id, &nick));
        }
        DbRequest::ProfileEnsure {
            server,
            nick,
            now,
            reply,
        } => {
            let _ = reply.send(profile_ensure(conn, &server, &nick, now));
        }
        DbRequest::ProfileResolve {
            server,
            nick,
            account,
            now,
            reply,
        } => {
            let _ = reply.send(profile_resolve(
                conn,
                &server,
                &nick,
                account.as_deref(),
                now,
            ));
        }
        DbRequest::ProfileBindNick {
            server,
            old_nick,
            new_nick,
            account,
            now,
            reply,
        } => {
            let _ = reply.send(profile_bind_nick(
                conn,
                &server,
                &old_nick,
                &new_nick,
                account.as_deref(),
                now,
            ));
        }
        DbRequest::ProfileGet {
            server,
            nick,
            reply,
        } => {
            let _ = reply.send(profile_get(conn, &server, &nick));
        }
        DbRequest::ProfileSet(update, reply) => {
            let _ = reply.send(profile_set(conn, &update));
        }
        DbRequest::ProfileClear {
            server,
            nick,
            field,
            reply,
        } => {
            let _ = reply.send(profile_clear(conn, &server, &nick, &field));
        }
    }
}

fn role_to_str(r: Role) -> &'static str {
    match r {
        Role::Admin => "admin",
        Role::SuperAdmin => "superadmin",
    }
}

fn role_from_str(s: &str) -> Option<Role> {
    match s {
        "admin" => Some(Role::Admin),
        "superadmin" => Some(Role::SuperAdmin),
        _ => None,
    }
}

/// Resolve a sender's role on a network, applying trust-on-first-use binding. See the permission
/// rules in `perms.rs` / SPEC.md.
fn resolve_role(
    conn: &Connection,
    server_label: &str,
    nick: &str,
    hostmask: &str,
    account: Option<&str>,
) -> Result<Option<Role>> {
    let sid: Option<i64> = conn
        .query_row(
            "SELECT id FROM servers WHERE label = ?1",
            [server_label],
            |r| r.get(0),
        )
        .optional()?;
    let Some(sid) = sid else { return Ok(None) };

    let row = conn
        .query_row(
            "SELECT role, account, bound_hostmask, bound_account
             FROM admins WHERE server_id = ?1 AND nick = ?2 COLLATE NOCASE",
            rusqlite::params![sid, nick],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((role_s, cfg_account, bound_hostmask, bound_account)) = row else {
        return Ok(None);
    };
    let Some(role) = role_from_str(&role_s) else {
        return Ok(None);
    };

    let observed_account = account.filter(|a| !a.is_empty());

    // 1. Operator pinned an explicit account: must match.
    if let Some(want) = cfg_account.as_deref().filter(|a| !a.is_empty()) {
        return Ok((observed_account == Some(want)).then_some(role));
    }
    // 2. Previously bound to an account: must match.
    if let Some(bound) = bound_account.as_deref() {
        return Ok((observed_account == Some(bound)).then_some(role));
    }
    // 3. Previously bound to a hostmask: must match.
    if let Some(bound) = bound_hostmask.as_deref() {
        return Ok((hostmask == bound).then_some(role));
    }
    // 4. First contact — bind the strongest identity available (prefer account).
    if let Some(acct) = observed_account {
        conn.execute(
            "UPDATE admins SET bound_account = ?3 WHERE server_id = ?1 AND nick = ?2 COLLATE NOCASE",
            rusqlite::params![sid, nick, acct],
        )?;
    } else {
        conn.execute(
            "UPDATE admins SET bound_hostmask = ?3 WHERE server_id = ?1 AND nick = ?2 COLLATE NOCASE",
            rusqlite::params![sid, nick, hostmask],
        )?;
    }
    Ok(Some(role))
}

fn load_admins(conn: &Connection, server_id: i64) -> Result<Vec<AdminEntry>> {
    let mut stmt = conn.prepare(
        "SELECT nick, role, account, bound_hostmask, bound_account
         FROM admins WHERE server_id = ?1 ORDER BY nick",
    )?;
    let rows = stmt.query_map([server_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (nick, role_s, account, bound_hostmask, bound_account) = r?;
        if let Some(role) = role_from_str(&role_s) {
            out.push(AdminEntry {
                nick,
                role,
                account,
                bound_hostmask,
                bound_account,
            });
        }
    }
    Ok(out)
}

fn upsert_admin(conn: &Connection, server_id: i64, entry: &AdminEntry) -> Result<()> {
    // Re-inserting an admin clears any stale binding so the next contact re-binds.
    conn.execute(
        "INSERT INTO admins (server_id, nick, role, account, bound_hostmask, bound_account)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL)
         ON CONFLICT(server_id, nick) DO UPDATE SET
            role=excluded.role, account=excluded.account,
            bound_hostmask=NULL, bound_account=NULL",
        rusqlite::params![
            server_id,
            entry.nick,
            role_to_str(entry.role),
            entry.account
        ],
    )?;
    Ok(())
}

fn delete_admin(conn: &Connection, server_id: i64, nick: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM admins WHERE server_id = ?1 AND nick = ?2 COLLATE NOCASE",
        rusqlite::params![server_id, nick],
    )?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS config (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
        CREATE TABLE IF NOT EXISTS command_alias_overrides (
            module  TEXT NOT NULL,
            command TEXT NOT NULL,
            aliases TEXT NOT NULL,
            PRIMARY KEY (module, command)
        );
        CREATE TABLE IF NOT EXISTS servers (
            id       INTEGER PRIMARY KEY,
            label    TEXT NOT NULL DEFAULT '',
            enabled  INTEGER NOT NULL DEFAULT 1,
            host     TEXT NOT NULL DEFAULT '',
            port     INTEGER NOT NULL DEFAULT 6697,
            tls      INTEGER NOT NULL DEFAULT 1,
            nick     TEXT NOT NULL DEFAULT 'jeeves',
            username TEXT NOT NULL DEFAULT 'jeeves',
            realname TEXT NOT NULL DEFAULT 'rustjeeves',
            accept_invalid_certs INTEGER NOT NULL DEFAULT 0,
            umodes TEXT
        );
        CREATE TABLE IF NOT EXISTS sasl (
            server_id INTEGER PRIMARY KEY,
            mechanism TEXT NOT NULL DEFAULT 'PLAIN',
            account   TEXT,
            password  TEXT,
            nick_password TEXT
        );
        CREATE TABLE IF NOT EXISTS channels (
            server_id INTEGER NOT NULL,
            name      TEXT NOT NULL,
            key       TEXT,
            PRIMARY KEY (server_id, name)
        );
        CREATE TABLE IF NOT EXISTS admins (
            server_id      INTEGER NOT NULL,
            nick           TEXT NOT NULL,
            role           TEXT NOT NULL,
            account        TEXT,
            bound_hostmask TEXT,
            bound_account  TEXT,
            PRIMARY KEY (server_id, nick)
        );
        CREATE TABLE IF NOT EXISTS profiles (
            id                 TEXT,
            server             TEXT NOT NULL,
            nick               TEXT NOT NULL COLLATE NOCASE,
            created            INTEGER NOT NULL,
            last_seen          INTEGER NOT NULL,
            title              TEXT,
            birthday           TEXT,
            pronoun_subject    TEXT,
            pronoun_object     TEXT,
            pronoun_possessive TEXT,
            location_display   TEXT,
            location_label     TEXT,
            lat                REAL,
            lon                REAL,
            PRIMARY KEY (server, nick)
        );
        CREATE TABLE IF NOT EXISTS module_kv (
            module TEXT NOT NULL,
            key    TEXT NOT NULL,
            value  TEXT,
            PRIMARY KEY (module, key)
        );
        CREATE TABLE IF NOT EXISTS logs (
            id       INTEGER PRIMARY KEY,
            ts       INTEGER NOT NULL,
            level    TEXT NOT NULL,
            category TEXT NOT NULL,
            source   TEXT NOT NULL,
            message  TEXT NOT NULL
        );
        "#,
    )?;
    // Defensive migrations for databases created before these columns existed.
    let _ = conn.execute(
        "ALTER TABLE servers ADD COLUMN accept_invalid_certs INTEGER NOT NULL DEFAULT 0",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE servers ADD COLUMN label TEXT NOT NULL DEFAULT ''",
        [],
    );
    let _ = conn.execute(
        "ALTER TABLE servers ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
        [],
    );
    let _ = conn.execute("ALTER TABLE servers ADD COLUMN umodes TEXT", []);
    let _ = conn.execute("ALTER TABLE profiles ADD COLUMN id TEXT", []);
    // Give any pre-existing rows a unique non-empty label.
    let _ = conn.execute(
        "UPDATE servers SET label = 'server' || id WHERE label = '' OR label IS NULL",
        [],
    );
    // Existing nick-keyed profiles receive stable IDs. Alias/account tables retain all future
    // identity information without destructively rewriting the original profile table.
    let mut missing =
        conn.prepare("SELECT server, nick FROM profiles WHERE id IS NULL OR id = ''")?;
    let rows = missing
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(missing);
    for (server, nick) in rows {
        conn.execute(
            "UPDATE profiles SET id=?3 WHERE server=?1 AND nick=?2",
            rusqlite::params![server, nick, uuid::Uuid::new_v4().to_string()],
        )?;
    }
    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS profiles_id_idx ON profiles(id);
        CREATE INDEX IF NOT EXISTS logs_ts_idx ON logs(ts);
        CREATE INDEX IF NOT EXISTS logs_category_idx ON logs(category);
        CREATE TABLE IF NOT EXISTS profile_aliases (
            server TEXT NOT NULL,
            nick TEXT NOT NULL COLLATE NOCASE,
            profile_id TEXT NOT NULL,
            last_seen INTEGER NOT NULL,
            PRIMARY KEY(server, nick)
        );
        CREATE TABLE IF NOT EXISTS profile_accounts (
            server TEXT NOT NULL,
            account TEXT NOT NULL COLLATE NOCASE,
            profile_id TEXT NOT NULL,
            PRIMARY KEY(server, account)
        );
        INSERT OR IGNORE INTO profile_aliases(server, nick, profile_id, last_seen)
            SELECT server, nick, id, last_seen FROM profiles WHERE id IS NOT NULL;
        "#,
    )?;
    Ok(())
}

fn load_servers(conn: &Connection) -> Result<Vec<ServerConfig>> {
    let mut servers = {
        let mut stmt = conn.prepare(
            "SELECT id, label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs, umodes
             FROM servers ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ServerConfig {
                id: row.get(0)?,
                label: row.get(1)?,
                enabled: row.get::<_, i64>(2)? != 0,
                host: row.get(3)?,
                port: row.get::<_, i64>(4)? as u16,
                tls: row.get::<_, i64>(5)? != 0,
                nick: row.get(6)?,
                username: row.get(7)?,
                realname: row.get(8)?,
                accept_invalid_certs: row.get::<_, i64>(9)? != 0,
                umodes: row.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty()),
                sasl_account: None,
                sasl_password: None,
                nick_password: None,
                channels: Vec::new(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for cfg in &mut servers {
        load_sasl_and_channels(conn, cfg)?;
    }
    Ok(servers)
}

fn load_sasl_and_channels(conn: &Connection, cfg: &mut ServerConfig) -> Result<()> {
    if let Some((account, password, nick_password)) = conn
        .query_row(
            "SELECT account, password, nick_password FROM sasl WHERE server_id = ?1",
            [cfg.id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
    {
        cfg.sasl_account = account.filter(|s| !s.is_empty());
        cfg.sasl_password = password.filter(|s| !s.is_empty());
        cfg.nick_password = nick_password.filter(|s| !s.is_empty());
    }

    let mut stmt =
        conn.prepare("SELECT name, key FROM channels WHERE server_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map([cfg.id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    cfg.channels = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(())
}

fn upsert_server(conn: &mut Connection, cfg: &ServerConfig) -> Result<i64> {
    let tx = conn.transaction()?;
    let label = if cfg.label.trim().is_empty() {
        if cfg.host.is_empty() {
            "default".to_string()
        } else {
            cfg.host.clone()
        }
    } else {
        cfg.label.trim().to_string()
    };

    // Enforce label uniqueness across distinct rows.
    let conflict: Option<i64> = tx
        .query_row(
            "SELECT id FROM servers WHERE label = ?1 AND id <> ?2",
            rusqlite::params![label, cfg.id],
            |r| r.get(0),
        )
        .optional()?;
    if conflict.is_some() {
        return Err(anyhow!("server label '{label}' is already in use"));
    }

    let id = if cfg.id == 0 {
        tx.execute(
            "INSERT INTO servers (label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs, umodes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        tx.last_insert_rowid()
    } else {
        let changed = tx.execute(
            "UPDATE servers SET label=?2, enabled=?3, host=?4, port=?5, tls=?6,
                nick=?7, username=?8, realname=?9, accept_invalid_certs=?10, umodes=?11 WHERE id=?1",
            rusqlite::params![
                cfg.id, label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        if changed == 0 {
            return Err(anyhow!("server id {} does not exist", cfg.id));
        }
        cfg.id
    };

    tx.execute(
        "INSERT INTO sasl (server_id, mechanism, account, password, nick_password)
         VALUES (?1, 'PLAIN', ?2, ?3, ?4)
         ON CONFLICT(server_id) DO UPDATE SET
            account=excluded.account, password=excluded.password, nick_password=excluded.nick_password",
        rusqlite::params![id, cfg.sasl_account, cfg.sasl_password, cfg.nick_password],
    )?;

    tx.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    for (name, key) in &cfg.channels {
        tx.execute(
            "INSERT OR REPLACE INTO channels (server_id, name, key) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, key],
        )?;
    }

    tx.commit()?;
    Ok(id)
}

fn delete_server(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM sasl WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM admins WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM servers WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

fn profile_ensure(conn: &Connection, server: &str, nick: &str, now: i64) -> Result<()> {
    let _ = profile_resolve(conn, server, nick, None, now)?;
    Ok(())
}

fn profile_get(conn: &Connection, server: &str, nick: &str) -> Result<Option<Profile>> {
    let Some(id) = profile_id_for(conn, server, nick, None)? else {
        return Ok(None);
    };
    let stored_alias = conn
        .query_row(
            "SELECT nick FROM profile_aliases WHERE server=?1 AND nick=?2",
            rusqlite::params![server, nick],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| nick.to_string());
    profile_get_by_id(conn, &id, &stored_alias)
}

fn profile_get_by_id(conn: &Connection, id: &str, observed_nick: &str) -> Result<Option<Profile>> {
    let p = conn
        .query_row(
            "SELECT id, server, nick, created, last_seen, title, birthday,
                    pronoun_subject, pronoun_object, pronoun_possessive,
                    location_display, location_label, lat, lon
             FROM profiles WHERE id = ?1",
            [id],
            |row| {
                Ok(Profile {
                    id: row.get(0)?,
                    server: row.get(1)?,
                    nick: row.get(2)?,
                    created: row.get(3)?,
                    last_seen: row.get(4)?,
                    title: row.get(5)?,
                    birthday: row.get(6)?,
                    pronoun_subject: row.get(7)?,
                    pronoun_object: row.get(8)?,
                    pronoun_possessive: row.get(9)?,
                    location_display: row.get(10)?,
                    location_label: row.get(11)?,
                    lat: row.get(12)?,
                    lon: row.get(13)?,
                })
            },
        )
        .optional()?;
    Ok(p.map(|mut p| {
        p.nick = observed_nick.to_string();
        p
    }))
}

fn profile_id_for(
    conn: &Connection,
    server: &str,
    nick: &str,
    account: Option<&str>,
) -> Result<Option<String>> {
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        let id = conn
            .query_row(
                "SELECT profile_id FROM profile_accounts WHERE server=?1 AND account=?2",
                rusqlite::params![server, account],
                |r| r.get(0),
            )
            .optional()?;
        if id.is_some() {
            return Ok(id);
        }
    }
    let alias = conn
        .query_row(
            "SELECT profile_id FROM profile_aliases WHERE server=?1 AND nick=?2",
            rusqlite::params![server, nick],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(alias_id) = alias {
        // A different authenticated account reusing a nick must not inherit the old account's
        // profile. An unclaimed nick alias may be upgraded to its first services account.
        if account.is_some() {
            let already_account_backed: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM profile_accounts WHERE server=?1 AND profile_id=?2)",
                rusqlite::params![server, alias_id],
                |r| r.get(0),
            )?;
            if already_account_backed {
                return Ok(None);
            }
        }
        return Ok(Some(alias_id));
    }
    conn.query_row(
        "SELECT id FROM profiles WHERE server=?1 AND nick=?2",
        rusqlite::params![server, nick],
        |r| r.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn profile_resolve(
    conn: &Connection,
    server: &str,
    nick: &str,
    account: Option<&str>,
    now: i64,
) -> Result<Profile> {
    let id = match profile_id_for(conn, server, nick, account)? {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let occupied: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM profiles WHERE server=?1 AND nick=?2)",
                rusqlite::params![server, nick],
                |r| r.get(0),
            )?;
            let stored_nick = if occupied {
                format!("{nick}~{}", &id[..8])
            } else {
                nick.to_string()
            };
            conn.execute(
                "INSERT INTO profiles (id, server, nick, created, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![id, server, stored_nick, now],
            )?;
            id
        }
    };
    conn.execute(
        "INSERT INTO profile_aliases(server, nick, profile_id, last_seen) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(server, nick) DO UPDATE SET profile_id=excluded.profile_id, last_seen=excluded.last_seen",
        rusqlite::params![server, nick, id, now],
    )?;
    if let Some(account) = account.filter(|a| !a.is_empty() && *a != "*") {
        conn.execute(
            "INSERT INTO profile_accounts(server, account, profile_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(server, account) DO UPDATE SET profile_id=excluded.profile_id",
            rusqlite::params![server, account, id],
        )?;
    }
    conn.execute(
        "UPDATE OR IGNORE profiles SET last_seen=?2, nick=?3 WHERE id=?1",
        rusqlite::params![id, now, nick],
    )?;
    profile_get_by_id(conn, &id, nick)?.ok_or_else(|| anyhow!("resolved profile disappeared"))
}

fn profile_bind_nick(
    conn: &Connection,
    server: &str,
    old_nick: &str,
    new_nick: &str,
    account: Option<&str>,
    now: i64,
) -> Result<()> {
    let profile = profile_resolve(conn, server, old_nick, account, now)?;
    conn.execute(
        "INSERT INTO profile_aliases(server, nick, profile_id, last_seen) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(server, nick) DO UPDATE SET profile_id=excluded.profile_id, last_seen=excluded.last_seen",
        rusqlite::params![server, new_nick, profile.id, now],
    )?;
    // Keep the latest nick as profile information. OR IGNORE avoids merging two legacy rows that
    // already occupy the same (server, nick) primary key; the alias still resolves correctly.
    conn.execute(
        "UPDATE OR IGNORE profiles SET nick=?2 WHERE id=?1",
        rusqlite::params![profile.id, new_nick],
    )?;
    Ok(())
}

/// Merge the `Some` fields of `u` into the profile row (creating a skeleton first if needed).
/// `COALESCE(?, col)` keeps the existing value when the bound parameter is NULL (field is `None`).
fn profile_set(conn: &Connection, u: &ProfileUpdate) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let p = profile_resolve(conn, &u.server, &u.nick, None, now)?;
    conn.execute(
        "UPDATE profiles SET
            title              = COALESCE(?3, title),
            birthday           = COALESCE(?4, birthday),
            pronoun_subject    = COALESCE(?5, pronoun_subject),
            pronoun_object     = COALESCE(?6, pronoun_object),
            pronoun_possessive = COALESCE(?7, pronoun_possessive),
            location_display   = COALESCE(?8, location_display),
            location_label     = COALESCE(?9, location_label),
            lat                = COALESCE(?10, lat),
            lon                = COALESCE(?11, lon)
         WHERE id = ?1",
        rusqlite::params![
            p.id,
            u.nick,
            u.title,
            u.birthday,
            u.pronoun_subject,
            u.pronoun_object,
            u.pronoun_possessive,
            u.location_display,
            u.location_label,
            u.lat,
            u.lon,
        ],
    )?;
    Ok(())
}

/// Clear a field group on a profile by setting its column(s) to NULL. `field` is whitelisted.
fn profile_clear(conn: &Connection, server: &str, nick: &str, field: &str) -> Result<()> {
    let Some(id) = profile_id_for(conn, server, nick, None)? else {
        return Ok(());
    };
    let sql = match field {
        "title" => "UPDATE profiles SET title=NULL WHERE id=?1",
        "birthday" => "UPDATE profiles SET birthday=NULL WHERE id=?1",
        "pronouns" => "UPDATE profiles SET pronoun_subject=NULL, pronoun_object=NULL, pronoun_possessive=NULL WHERE id=?1",
        "location" => "UPDATE profiles SET location_display=NULL, location_label=NULL, lat=NULL, lon=NULL WHERE id=?1",
        other => return Err(anyhow!("unknown profile field '{other}'")),
    };
    conn.execute(sql, [id])?;
    Ok(())
}

fn kv_get(conn: &Connection, module: &str, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row(
            "SELECT value FROM module_kv WHERE module = ?1 AND key = ?2",
            rusqlite::params![module, key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(v)
}

fn config_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM config WHERE key = ?1", [key], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

fn config_set(conn: &Connection, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            conn.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
        }
        None => {
            conn.execute("DELETE FROM config WHERE key = ?1", [key])?;
        }
    }
    Ok(())
}

fn load_alias_overrides(conn: &Connection) -> Result<AliasOverrides> {
    let mut stmt = conn.prepare(
        "SELECT module, command, aliases FROM command_alias_overrides ORDER BY module, command",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut overrides = AliasOverrides::new();
    for row in rows {
        let (module, command, aliases) = row?;
        overrides.insert((module, command), serde_json::from_str(&aliases)?);
    }
    Ok(overrides)
}

fn set_alias_override(
    conn: &Connection,
    module: &str,
    command: &str,
    aliases: Option<&[String]>,
) -> Result<()> {
    match aliases {
        Some(aliases) => {
            let aliases = serde_json::to_string(aliases)?;
            conn.execute(
                "INSERT INTO command_alias_overrides(module, command, aliases)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(module, command) DO UPDATE SET aliases=excluded.aliases",
                rusqlite::params![module, command, aliases],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM command_alias_overrides WHERE module=?1 AND command=?2",
                rusqlite::params![module, command],
            )?;
        }
    }
    Ok(())
}

fn kv_set(conn: &Connection, module: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO module_kv (module, key, value) VALUES (?1, ?2, ?3)",
        rusqlite::params![module, key, value],
    )?;
    Ok(())
}

fn append_log(conn: &Connection, ev: &LogEvent) -> Result<()> {
    conn.execute(
        "INSERT INTO logs (ts, level, category, source, message) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            ev.ts,
            level_str(ev.level),
            category_str(ev.category),
            ev.source,
            ev.message,
        ],
    )?;
    // Amortized retention: keep 30 days and cap the table at the newest 100,000 rows.
    // Running this every 256 inserts avoids turning every log write into a cleanup scan.
    if conn.last_insert_rowid() % 256 == 0 {
        let cutoff = ev.ts.saturating_sub(30 * 24 * 60 * 60);
        conn.execute("DELETE FROM logs WHERE ts < ?1", [cutoff])?;
        conn.execute(
            "DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT 100000)",
            [],
        )?;
    }
    Ok(())
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Error => "ERROR",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    }
}

fn category_str(c: Category) -> &'static str {
    match c {
        Category::Error => "ERROR",
        Category::Debug => "DEBUG",
        Category::Message => "MESSAGE",
        Category::Command => "COMMAND",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO servers (id, label, host) VALUES (1, 'net', 'irc.example')",
            [],
        )
        .unwrap();
        conn
    }

    #[test]
    fn config_roundtrip_and_clear() {
        let conn = setup();
        assert_eq!(config_get(&conn, "tavily_api_key").unwrap(), None);
        config_set(&conn, "tavily_api_key", Some("secret")).unwrap();
        assert_eq!(
            config_get(&conn, "tavily_api_key").unwrap().as_deref(),
            Some("secret")
        );
        config_set(&conn, "tavily_api_key", None).unwrap();
        assert_eq!(config_get(&conn, "tavily_api_key").unwrap(), None);
    }

    #[test]
    fn alias_override_roundtrip_distinguishes_empty_from_default() {
        let conn = setup();
        assert!(load_alias_overrides(&conn).unwrap().is_empty());
        set_alias_override(&conn, "weather", "weather", Some(&[])).unwrap();
        assert_eq!(
            load_alias_overrides(&conn)
                .unwrap()
                .get(&("weather".into(), "weather".into())),
            Some(&Vec::<String>::new())
        );
        let aliases = vec!["w".to_string(), "weath".to_string()];
        set_alias_override(&conn, "weather", "weather", Some(&aliases)).unwrap();
        assert_eq!(
            load_alias_overrides(&conn)
                .unwrap()
                .get(&("weather".into(), "weather".into())),
            Some(&aliases)
        );
        set_alias_override(&conn, "weather", "weather", None).unwrap();
        assert!(load_alias_overrides(&conn).unwrap().is_empty());
    }

    #[test]
    fn legacy_nick_keyed_profile_is_migrated_to_uuid_and_alias() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE profiles (
                server TEXT NOT NULL,
                nick TEXT NOT NULL COLLATE NOCASE,
                created INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                title TEXT, birthday TEXT,
                pronoun_subject TEXT, pronoun_object TEXT, pronoun_possessive TEXT,
                location_display TEXT, location_label TEXT, lat REAL, lon REAL,
                PRIMARY KEY(server, nick)
             );
             INSERT INTO profiles(server, nick, created, last_seen, title)
             VALUES ('net', 'LegacyNick', 10, 20, 'Captain');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let profile = profile_get(&conn, "net", "legacynick").unwrap().unwrap();
        assert!(uuid::Uuid::parse_str(&profile.id).is_ok());
        assert_eq!(profile.nick, "LegacyNick");
        assert_eq!(profile.title.as_deref(), Some("Captain"));
    }

    #[test]
    fn unknown_nick_has_no_role() {
        let conn = setup();
        let r = resolve_role(&conn, "net", "nobody", "nobody!u@h", None).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn explicit_account_must_match() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role, account) VALUES (1, 'boss', 'superadmin', 'bossacct')",
            [],
        )
        .unwrap();
        // Wrong / missing account -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", None).unwrap(),
            None
        );
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("other")).unwrap(),
            None
        );
        // Correct account -> superadmin.
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("bossacct")).unwrap(),
            Some(Role::SuperAdmin)
        );
    }

    #[test]
    fn hostmask_tofu_binds_then_enforces() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role) VALUES (1, 'op', 'admin')",
            [],
        )
        .unwrap();
        // First contact (no account) binds the hostmask and grants.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-a", None).unwrap(),
            Some(Role::Admin)
        );
        // Same hostmask -> still granted.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-a", None).unwrap(),
            Some(Role::Admin)
        );
        // Different hostmask (spoof attempt) -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-b", None).unwrap(),
            None
        );
    }

    #[test]
    fn account_tofu_preferred_over_hostmask() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role) VALUES (1, 'op', 'admin')",
            [],
        )
        .unwrap();
        // First contact carries an account -> binds account (not hostmask).
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h1", Some("opacct")).unwrap(),
            Some(Role::Admin)
        );
        // Same account from a different host -> still granted (account beats host).
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h2", Some("opacct")).unwrap(),
            Some(Role::Admin)
        );
        // Different account -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h1", Some("evil")).unwrap(),
            None
        );
    }

    #[test]
    fn profile_ensure_get_set_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // First contact creates a skeleton.
        profile_ensure(&conn, "net", "Alice", 1000).unwrap();
        let p = profile_get(&conn, "net", "alice").unwrap().unwrap(); // case-insensitive
        assert_eq!(p.nick, "Alice");
        assert_eq!(p.created, 1000);
        assert_eq!(p.title, None);

        // Partial update merges; untouched fields are preserved.
        let mut u = ProfileUpdate {
            server: "net".into(),
            nick: "alice".into(),
            ..Default::default()
        };
        u.title = Some("Queen".into());
        u.lat = Some(51.5);
        u.lon = Some(-0.05);
        u.location_display = Some("Hackney, England".into());
        profile_set(&conn, &u).unwrap();

        let p = profile_get(&conn, "net", "Alice").unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Queen"));
        assert_eq!(p.location_display.as_deref(), Some("Hackney, England"));
        assert_eq!(p.lat, Some(51.5));
        assert_eq!(p.created, 1000, "created preserved across update");

        // A second update touching only birthday leaves the title intact.
        let u2 = ProfileUpdate {
            server: "net".into(),
            nick: "alice".into(),
            birthday: Some("03-14".into()),
            ..Default::default()
        };
        profile_set(&conn, &u2).unwrap();
        let p = profile_get(&conn, "net", "alice").unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Queen"));
        assert_eq!(p.birthday.as_deref(), Some("03-14"));
    }

    #[test]
    fn stable_profile_survives_nick_change_and_account_lookup() {
        let conn = setup();
        let first = profile_resolve(&conn, "net", "Alice", Some("alice_account"), 100).unwrap();
        profile_bind_nick(
            &conn,
            "net",
            "Alice",
            "AliceAway",
            Some("alice_account"),
            200,
        )
        .unwrap();
        let renamed = profile_get(&conn, "net", "AliceAway").unwrap().unwrap();
        let account = profile_resolve(
            &conn,
            "net",
            "CompletelyDifferentNick",
            Some("alice_account"),
            300,
        )
        .unwrap();
        assert_eq!(first.id, renamed.id);
        assert_eq!(first.id, account.id);
        assert_eq!(account.nick, "CompletelyDifferentNick");
    }

    #[test]
    fn different_accounts_reusing_a_nick_do_not_merge_profiles() {
        let conn = setup();
        let first = profile_resolve(&conn, "net", "SharedNick", Some("account_a"), 100).unwrap();
        let second = profile_resolve(&conn, "net", "SharedNick", Some("account_b"), 200).unwrap();
        assert_ne!(first.id, second.id);
        let first_again =
            profile_resolve(&conn, "net", "AnotherNick", Some("account_a"), 300).unwrap();
        assert_eq!(first.id, first_again.id);
    }

    #[test]
    fn nick_match_is_case_insensitive() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role, account) VALUES (1, 'Boss', 'admin', 'a')",
            [],
        )
        .unwrap();
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("a")).unwrap(),
            Some(Role::Admin)
        );
    }
}
