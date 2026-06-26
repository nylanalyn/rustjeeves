//! SQLite persistence behind an actor task.
//!
//! `rusqlite` is synchronous, so a single [`rusqlite::Connection`] lives on a dedicated OS thread
//! and is the *only* thing that touches the database. Async callers talk to it through
//! [`DbHandle`], which sends [`DbRequest`]s over a channel and awaits a oneshot reply.

use crate::config::ServerConfig;
use crate::log_bus::LogEvent;
use anyhow::{anyhow, Result};
use jeeves_abi::{Category, Level};
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

    /// All configured server profiles, ordered by id.
    pub async fn load_servers(&self) -> Result<Vec<ServerConfig>> {
        self.call(DbRequest::LoadServers).await
    }

    /// Insert (id == 0) or update a server profile; returns its row id.
    pub async fn upsert_server(&self, cfg: ServerConfig) -> Result<i64> {
        self.call(|reply| DbRequest::UpsertServer(Box::new(cfg), reply)).await
    }

    /// Delete a server profile and its associated sasl/channels rows. (Used by the TUI.)
    #[allow(dead_code)]
    pub async fn delete_server(&self, id: i64) -> Result<()> {
        self.call(|reply| DbRequest::DeleteServer(id, reply)).await
    }

    /// Convenience for the (still single-server) TUI form: the first profile, or a placeholder.
    pub async fn load_first_server(&self) -> Result<ServerConfig> {
        Ok(self.load_servers().await?.into_iter().next().unwrap_or_else(ServerConfig::placeholder))
    }

    pub async fn append_log(&self, ev: LogEvent) -> Result<()> {
        self.call(|reply| DbRequest::AppendLog(ev, reply)).await
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
    }
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
            accept_invalid_certs INTEGER NOT NULL DEFAULT 0
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
    // Give any pre-existing rows a unique non-empty label.
    let _ = conn.execute("UPDATE servers SET label = 'server' || id WHERE label = '' OR label IS NULL", []);
    Ok(())
}

fn load_servers(conn: &Connection) -> Result<Vec<ServerConfig>> {
    let mut servers = {
        let mut stmt = conn.prepare(
            "SELECT id, label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs
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
            "INSERT INTO servers (label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64,
            ],
        )?;
        conn.last_insert_rowid()
    } else {
        conn.execute(
            "UPDATE servers SET label=?2, enabled=?3, host=?4, port=?5, tls=?6,
                nick=?7, username=?8, realname=?9, accept_invalid_certs=?10 WHERE id=?1",
            rusqlite::params![
                cfg.id, label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64,
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
