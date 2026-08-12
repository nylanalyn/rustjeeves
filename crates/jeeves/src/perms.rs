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
pub fn spawn(
    db: DbHandle,
    log: LogBus,
    out: mpsc::Sender<EventEnvelope>,
) -> mpsc::Sender<EventEnvelope> {
    let (tx, mut rx) = mpsc::channel::<EventEnvelope>(256);
    tokio::spawn(async move {
        while let Some(mut env) = rx.recv().await {
            if let Event::NickChanged {
                old_nick,
                new_nick,
                account,
            } = &env.event
            {
                if let Err(e) = db
                    .profile_bind_nick(&env.server, old_nick, new_nick, account.clone(), now_secs())
                    .await
                {
                    log.error("profiles", format!("nick alias update failed: {e}"));
                }
            } else if let Event::Message(msg) = &mut env.event {
                let account = msg
                    .tags
                    .iter()
                    .find(|(k, _)| k == "account")
                    .and_then(|(_, v)| v.clone())
                    .filter(|a| !a.is_empty());
                let hostmask = format!("{}!{}@{}", msg.nick, msg.user, msg.host);
                // How to address them: "{title} {nick}" if a title is set, else just the nick.
                let profile = match db
                    .profile_resolve(&env.server, &msg.nick, account.clone(), now_secs())
                    .await
                {
                    Ok(p) => {
                        msg.user_id = p.id.clone();
                        msg.display =
                            match p.title.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
                                Some(title) => format!("{title} {}", msg.nick),
                                None => msg.nick.clone(),
                            };
                        Some(p)
                    }
                    Err(e) => {
                        log.error("perms", format!("profile resolution failed: {e}"));
                        msg.display = msg.nick.clone();
                        None
                    }
                };

                if let Some(profile) = &profile {
                    match db.profile_is_ignored(&env.server, &profile.id).await {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(e) => log.error("perms", format!("ignore lookup failed: {e}")),
                    }
                }

                match db
                    .resolve_role(&env.server, &msg.nick, &hostmask, account)
                    .await
                {
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

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
