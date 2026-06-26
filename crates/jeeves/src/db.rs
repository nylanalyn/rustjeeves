//! SQLite persistence behind an actor task.
//!
//! `rusqlite` is synchronous, so a single [`rusqlite::Connection`] lives on a dedicated OS thread
//! and is the *only* thing that touches the database. Async callers talk to it through
//! [`DbHandle`], which sends [`DbRequest`]s over a channel and awaits a oneshot reply.

use crate::config::ServerConfig;
use crate::log_bus::LogEvent;
use anyhow::{anyhow, Result};
use jeeves_abi::{Category, Level};
use rusqlite::Connection;
use tokio::sync::{mpsc, oneshot};

/// Requests the DB actor understands. Each carries a oneshot sender for its reply.
enum DbRequest {
    LoadServer(oneshot::Sender<Result<ServerConfig>>),
    SaveServer(ServerConfig, oneshot::Sender<Result<()>>),
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

    pub async fn load_server(&self) -> Result<ServerConfig> {
        self.call(DbRequest::LoadServer).await
    }

    pub async fn save_server(&self, cfg: ServerConfig) -> Result<()> {
        self.call(|reply| DbRequest::SaveServer(cfg, reply)).await
    }

    /// Async KV get. Currently modules use the blocking variant; kept for async callers.
    #[allow(dead_code)]
    pub async fn kv_get(&self, module: &str, key: &str) -> Result<Option<String>> {
        let (module, key) = (module.to_string(), key.to_string());
        self.call(|reply| DbRequest::KvGet { module, key, reply }).await
    }

    /// Async KV set. Currently modules use the blocking variant; kept for async callers.
    #[allow(dead_code)]
    pub async fn kv_set(&self, module: &str, key: &str, value: &str) -> Result<()> {
        let (module, key, value) = (module.to_string(), key.to_string(), value.to_string());
        self.call(|reply| DbRequest::KvSet { module, key, value, reply }).await
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
        DbRequest::LoadServer(reply) => {
            let _ = reply.send(load_server(conn));
        }
        DbRequest::SaveServer(cfg, reply) => {
            let _ = reply.send(save_server(conn, &cfg));
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
    // Defensive migration for databases created before this column existed.
    let _ = conn.execute(
        "ALTER TABLE servers ADD COLUMN accept_invalid_certs INTEGER NOT NULL DEFAULT 0",
        [],
    );
    Ok(())
}

/// The bot is single-server for now; server row id is always 1.
const SERVER_ID: i64 = 1;

fn load_server(conn: &Connection) -> Result<ServerConfig> {
    let base = conn.query_row(
        "SELECT host, port, tls, nick, username, realname, accept_invalid_certs
         FROM servers WHERE id = ?1",
        [SERVER_ID],
        |row| {
            Ok(ServerConfig {
                host: row.get(0)?,
                port: row.get::<_, i64>(1)? as u16,
                tls: row.get::<_, i64>(2)? != 0,
                nick: row.get(3)?,
                username: row.get(4)?,
                realname: row.get(5)?,
                accept_invalid_certs: row.get::<_, i64>(6)? != 0,
                sasl_account: None,
                sasl_password: None,
                nick_password: None,
                channels: Vec::new(),
            })
        },
    );

    let mut cfg = match base {
        Ok(c) => c,
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(ServerConfig::placeholder()),
        Err(e) => return Err(e.into()),
    };

    if let Ok((account, password, nick_password)) = conn.query_row(
        "SELECT account, password, nick_password FROM sasl WHERE server_id = ?1",
        [SERVER_ID],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        },
    ) {
        cfg.sasl_account = account.filter(|s| !s.is_empty());
        cfg.sasl_password = password.filter(|s| !s.is_empty());
        cfg.nick_password = nick_password.filter(|s| !s.is_empty());
    }

    let mut stmt = conn.prepare("SELECT name, key FROM channels WHERE server_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map([SERVER_ID], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    for r in rows {
        cfg.channels.push(r?);
    }

    Ok(cfg)
}

fn save_server(conn: &Connection, cfg: &ServerConfig) -> Result<()> {
    conn.execute(
        "INSERT INTO servers (id, host, port, tls, nick, username, realname, accept_invalid_certs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(id) DO UPDATE SET
            host=excluded.host, port=excluded.port, tls=excluded.tls,
            nick=excluded.nick, username=excluded.username, realname=excluded.realname,
            accept_invalid_certs=excluded.accept_invalid_certs",
        rusqlite::params![
            SERVER_ID,
            cfg.host,
            cfg.port as i64,
            cfg.tls as i64,
            cfg.nick,
            cfg.username,
            cfg.realname,
            cfg.accept_invalid_certs as i64,
        ],
    )?;

    conn.execute(
        "INSERT INTO sasl (server_id, mechanism, account, password, nick_password)
         VALUES (?1, 'PLAIN', ?2, ?3, ?4)
         ON CONFLICT(server_id) DO UPDATE SET
            account=excluded.account, password=excluded.password, nick_password=excluded.nick_password",
        rusqlite::params![
            SERVER_ID,
            cfg.sasl_account,
            cfg.sasl_password,
            cfg.nick_password,
        ],
    )?;

    conn.execute("DELETE FROM channels WHERE server_id = ?1", [SERVER_ID])?;
    for (name, key) in &cfg.channels {
        conn.execute(
            "INSERT OR REPLACE INTO channels (server_id, name, key) VALUES (?1, ?2, ?3)",
            rusqlite::params![SERVER_ID, name, key],
        )?;
    }

    Ok(())
}

fn kv_get(conn: &Connection, module: &str, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row(
            "SELECT value FROM module_kv WHERE module = ?1 AND key = ?2",
            rusqlite::params![module, key],
            |row| row.get::<_, Option<String>>(0),
        )
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
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
