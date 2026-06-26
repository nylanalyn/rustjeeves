//! Permission resolver stage.
//!
//! Sits between the IRC actors and the module host. For each incoming message it resolves the
//! sender's role (via the DB, which also performs trust-on-first-use binding) and stamps it onto
//! the message before forwarding to the modules. Modules enforce access by checking `msg.role`.
//!
//! Identity preference: the verified services account (IRCv3 `account-tag`) when present, else the
//! `nick!user@host` hostmask bound on first contact. The actual policy lives in `db::resolve_role`.

use crate::db::DbHandle;
use crate::log_bus::LogBus;
use jeeves_abi::{Event, EventEnvelope};
use tokio::sync::mpsc;

/// Spawn the resolver. Returns the inlet the IRC actors should send events to; resolved events are
/// forwarded to `out` (the module host).
pub fn spawn(db: DbHandle, log: LogBus, out: mpsc::Sender<EventEnvelope>) -> mpsc::Sender<EventEnvelope> {
    let (tx, mut rx) = mpsc::channel::<EventEnvelope>(256);
    tokio::spawn(async move {
        while let Some(mut env) = rx.recv().await {
            if let Event::Message(msg) = &mut env.event {
                let account = msg
                    .tags
                    .iter()
                    .find(|(k, _)| k == "account")
                    .and_then(|(_, v)| v.clone())
                    .filter(|a| !a.is_empty());
                let hostmask = format!("{}!{}@{}", msg.nick, msg.user, msg.host);
                match db.resolve_role(&env.server, &msg.nick, &hostmask, account).await {
                    Ok(role) => msg.role = role,
                    Err(e) => log.error("perms", format!("role resolution failed: {e}")),
                }
            }
            if out.send(env).await.is_err() {
                break;
            }
        }
    });
    tx
}
