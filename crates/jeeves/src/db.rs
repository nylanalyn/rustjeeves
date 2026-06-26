//! SQLite persistence behind an actor task.
//!
//! `rusqlite` is synchronous, so a single [`rusqlite::Connection`] lives on a dedicated OS thread
//! and is the *only* thing that touches the database. Async callers talk to it through
//! [`DbHandle`], which sends [`DbRequest`]s over a channel and awaits a oneshot reply.

use crate::config::{AdminEntry, ServerConfig};
use crate::log_bus::LogEvent;
use anyhow::{anyhow, Result};
use jeeves_abi::{Category, Level, Profile, ProfileUpdate, Role};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot};

/// Requests the DB actor understands. Each carries a oneshot sender for its reply.
enum DbRequest {
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
                while let Some(req) = rx.blocking_recv() {
                    handle(&conn, req);
                }
            })?;

        Ok(DbHandle { tx })
    }

    async fn call<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Synchronous variant of [`Self::call`] for the blocking TUI thread. Must NOT be called from
    /// within the async runtime.
    fn call_blocking<T>(&self, make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(make(reply))
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv().map_err(|_| anyhow!("db actor dropped reply"))?
    }

    // --- Blocking accessors used by the TUI thread ---

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
        self.call_blocking(|reply| DbRequest::ProfileEnsure { server, nick, now, reply })
    }

    pub fn profile_get_blocking(&self, server: &str, nick: &str) -> Result<Option<Profile>> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call_blocking(|reply| DbRequest::ProfileGet { server, nick, reply })
    }

    pub fn profile_set_blocking(&self, update: ProfileUpdate) -> Result<()> {
        self.call_blocking(|reply| DbRequest::ProfileSet(Box::new(update), reply))
    }

    pub fn profile_clear_blocking(&self, server: &str, nick: &str, field: &str) -> Result<()> {
        let (server, nick, field) = (server.to_string(), nick.to_string(), field.to_string());
        self.call_blocking(|reply| DbRequest::ProfileClear { server, nick, field, reply })
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
        let (server_label, nick, hostmask) =
            (server_label.to_string(), nick.to_string(), hostmask.to_string());
        self.call(|reply| DbRequest::ResolveRole { server_label, nick, hostmask, account, reply })
            .await
    }

    /// Async profile lookup (used by the message-enrichment resolver).
    pub async fn profile_get(&self, server: &str, nick: &str) -> Result<Option<Profile>> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call(|reply| DbRequest::ProfileGet { server, nick, reply }).await
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
        rx.blocking_recv().map_err(|_| anyhow!("db actor dropped reply"))?
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
        rx.blocking_recv().map_err(|_| anyhow!("db actor dropped reply"))?
    }
}

fn handle(conn: &Connection, req: DbRequest) {
    match req {
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
        DbRequest::KvSet { module, key, value, reply } => {
            let _ = reply.send(kv_set(conn, &module, &key, &value));
        }
        DbRequest::AppendLog(ev, reply) => {
            let _ = reply.send(append_log(conn, &ev));
        }
        DbRequest::ResolveRole { server_label, nick, hostmask, account, reply } => {
            let _ = reply.send(resolve_role(conn, &server_label, &nick, &hostmask, account.as_deref()));
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
        DbRequest::ProfileEnsure { server, nick, now, reply } => {
            let _ = reply.send(profile_ensure(conn, &server, &nick, now));
        }
        DbRequest::ProfileGet { server, nick, reply } => {
            let _ = reply.send(profile_get(conn, &server, &nick));
        }
        DbRequest::ProfileSet(update, reply) => {
            let _ = reply.send(profile_set(conn, &update));
        }
        DbRequest::ProfileClear { server, nick, field, reply } => {
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
        .query_row("SELECT id FROM servers WHERE label = ?1", [server_label], |r| r.get(0))
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
    let Some(role) = role_from_str(&role_s) else { return Ok(None) };

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
            out.push(AdminEntry { nick, role, account, bound_hostmask, bound_account });
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
        rusqlite::params![server_id, entry.nick, role_to_str(entry.role), entry.account],
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
    let _ = conn.execute("ALTER TABLE servers ADD COLUMN accept_invalid_certs INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE servers ADD COLUMN label TEXT NOT NULL DEFAULT ''", []);
    let _ = conn.execute("ALTER TABLE servers ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1", []);
    let _ = conn.execute("ALTER TABLE servers ADD COLUMN umodes TEXT", []);
    // Give any pre-existing rows a unique non-empty label.
    let _ = conn.execute("UPDATE servers SET label = 'server' || id WHERE label = '' OR label IS NULL", []);
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

    let mut stmt = conn.prepare("SELECT name, key FROM channels WHERE server_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map([cfg.id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    cfg.channels = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(())
}

fn upsert_server(conn: &Connection, cfg: &ServerConfig) -> Result<i64> {
    let label = if cfg.label.trim().is_empty() {
        if cfg.host.is_empty() { "default".to_string() } else { cfg.host.clone() }
    } else {
        cfg.label.trim().to_string()
    };

    // Enforce label uniqueness across distinct rows.
    let conflict: Option<i64> = conn
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
        conn.execute(
            "INSERT INTO servers (label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs, umodes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        conn.last_insert_rowid()
    } else {
        conn.execute(
            "UPDATE servers SET label=?2, enabled=?3, host=?4, port=?5, tls=?6,
                nick=?7, username=?8, realname=?9, accept_invalid_certs=?10, umodes=?11 WHERE id=?1",
            rusqlite::params![
                cfg.id, label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        cfg.id
    };

    conn.execute(
        "INSERT INTO sasl (server_id, mechanism, account, password, nick_password)
         VALUES (?1, 'PLAIN', ?2, ?3, ?4)
         ON CONFLICT(server_id) DO UPDATE SET
            account=excluded.account, password=excluded.password, nick_password=excluded.nick_password",
        rusqlite::params![id, cfg.sasl_account, cfg.sasl_password, cfg.nick_password],
    )?;

    conn.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    for (name, key) in &cfg.channels {
        conn.execute(
            "INSERT OR REPLACE INTO channels (server_id, name, key) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, key],
        )?;
    }

    Ok(id)
}

fn delete_server(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    conn.execute("DELETE FROM sasl WHERE server_id = ?1", [id])?;
    conn.execute("DELETE FROM admins WHERE server_id = ?1", [id])?;
    conn.execute("DELETE FROM servers WHERE id = ?1", [id])?;
    Ok(())
}

fn profile_ensure(conn: &Connection, server: &str, nick: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO profiles (server, nick, created, last_seen) VALUES (?1, ?2, ?3, ?3)",
        rusqlite::params![server, nick, now],
    )?;
    conn.execute(
        "UPDATE profiles SET last_seen = ?3 WHERE server = ?1 AND nick = ?2",
        rusqlite::params![server, nick, now],
    )?;
    Ok(())
}

fn profile_get(conn: &Connection, server: &str, nick: &str) -> Result<Option<Profile>> {
    let p = conn
        .query_row(
            "SELECT server, nick, created, last_seen, title, birthday,
                    pronoun_subject, pronoun_object, pronoun_possessive,
                    location_display, location_label, lat, lon
             FROM profiles WHERE server = ?1 AND nick = ?2",
            rusqlite::params![server, nick],
            |row| {
                Ok(Profile {
                    server: row.get(0)?,
                    nick: row.get(1)?,
                    created: row.get(2)?,
                    last_seen: row.get(3)?,
                    title: row.get(4)?,
                    birthday: row.get(5)?,
                    pronoun_subject: row.get(6)?,
                    pronoun_object: row.get(7)?,
                    pronoun_possessive: row.get(8)?,
                    location_display: row.get(9)?,
                    location_label: row.get(10)?,
                    lat: row.get(11)?,
                    lon: row.get(12)?,
                })
            },
        )
        .optional()?;
    Ok(p)
}

/// Merge the `Some` fields of `u` into the profile row (creating a skeleton first if needed).
/// `COALESCE(?, col)` keeps the existing value when the bound parameter is NULL (field is `None`).
fn profile_set(conn: &Connection, u: &ProfileUpdate) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO profiles (server, nick, created, last_seen)
         VALUES (?1, ?2, CAST(strftime('%s','now') AS INTEGER), CAST(strftime('%s','now') AS INTEGER))",
        rusqlite::params![u.server, u.nick],
    )?;
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
         WHERE server = ?1 AND nick = ?2",
        rusqlite::params![
            u.server,
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
    let sql = match field {
        "title" => "UPDATE profiles SET title=NULL WHERE server=?1 AND nick=?2",
        "birthday" => "UPDATE profiles SET birthday=NULL WHERE server=?1 AND nick=?2",
        "pronouns" => "UPDATE profiles SET pronoun_subject=NULL, pronoun_object=NULL, pronoun_possessive=NULL WHERE server=?1 AND nick=?2",
        "location" => "UPDATE profiles SET location_display=NULL, location_label=NULL, lat=NULL, lon=NULL WHERE server=?1 AND nick=?2",
        other => return Err(anyhow!("unknown profile field '{other}'")),
    };
    conn.execute(sql, rusqlite::params![server, nick])?;
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
        assert_eq!(resolve_role(&conn, "net", "boss", "boss!u@h", None).unwrap(), None);
        assert_eq!(resolve_role(&conn, "net", "boss", "boss!u@h", Some("other")).unwrap(), None);
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
        assert_eq!(resolve_role(&conn, "net", "op", "op!user@host-b", None).unwrap(), None);
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
        assert_eq!(resolve_role(&conn, "net", "op", "op!u@h1", Some("evil")).unwrap(), None);
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
        let mut u = ProfileUpdate { server: "net".into(), nick: "alice".into(), ..Default::default() };
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
