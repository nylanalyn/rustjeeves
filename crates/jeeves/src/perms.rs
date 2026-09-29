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
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Spawn the resolver. Returns the inlet the IRC actors should send events to; resolved events are
/// forwarded to `out` (the module host).
pub fn spawn(
    db: DbHandle,
    log: LogBus,
    out: mpsc::Sender<EventEnvelope>,
    connected_networks: Arc<Mutex<HashSet<String>>>,
) -> mpsc::Sender<EventEnvelope> {
    let (tx, mut rx) = mpsc::channel::<EventEnvelope>(256);
    tokio::spawn(async move {
        // Canonical spelling of each joined channel, keyed by (network, casefolded name). Clients
        // may address `#Chan` or `#chan`; modules key state and timers on the target string, so
        // one spelling per channel keeps their state from silently splitting.
        let mut channels = HashMap::<(String, String), String>::new();
        while let Some(mut env) = rx.recv().await {
            normalize_channel(&db, &mut channels, &mut env);
            match &env.event {
                Event::Connected => {
                    connected_networks
                        .lock()
                        .unwrap()
                        .insert(env.server.clone());
                }
                Event::Disconnected => {
                    connected_networks.lock().unwrap().remove(&env.server);
                }
                _ => {}
            }
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
                        msg.honorific = honorific(p.pronoun_subject.as_deref(), &msg.display);
                        Some(p)
                    }
                    Err(e) => {
                        log.error("perms", format!("profile resolution failed: {e}"));
                        msg.display = msg.nick.clone();
                        msg.honorific = msg.nick.clone();
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

/// Butler-style address from saved pronouns. Anyone without he/she pronouns is addressed by
/// name rather than guessed at.
fn honorific(pronoun_subject: Option<&str>, display: &str) -> String {
    match pronoun_subject
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("he") => "sir".into(),
        Some("she") => "madam".into(),
        _ => display.to_string(),
    }
}

/// Learn channel spellings from the bot's own JOINs and rewrite channel message targets to them.
fn normalize_channel(
    db: &DbHandle,
    channels: &mut HashMap<(String, String), String>,
    env: &mut EventEnvelope,
) {
    match &mut env.event {
        Event::Joined { channel } => {
            let folded = db.irc_casefold(&env.server, channel);
            channels.insert((env.server.clone(), folded), channel.clone());
        }
        Event::Message(message) if !message.is_private => {
            let folded = db.irc_casefold(&env.server, &message.target);
            if let Some(canonical) = channels.get(&(env.server.clone(), folded)) {
                if *canonical != message.target {
                    message.target = canonical.clone();
                }
            }
        }
        _ => {}
    }
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honorifics_follow_pronouns_and_never_guess() {
        assert_eq!(honorific(Some("he"), "aureate"), "sir");
        assert_eq!(honorific(Some("She"), "Dr kim"), "madam");
        assert_eq!(honorific(Some("they"), "Captain rae"), "Captain rae");
        assert_eq!(honorific(None, "rae"), "rae");
    }

    #[test]
    fn channel_targets_use_the_joined_spelling() {
        let db = DbHandle::open(":memory:").unwrap();
        let mut channels = HashMap::new();
        let mut joined = EventEnvelope {
            server: "net".into(),
            event: Event::Joined {
                channel: "#Games".into(),
            },
        };
        normalize_channel(&db, &mut channels, &mut joined);
        let message = |target: &str, is_private: bool| EventEnvelope {
            server: "net".into(),
            event: Event::Message(jeeves_abi::MessagePayload {
                user_id: String::new(),
                nick: "alice".into(),
                display: String::new(),
                user: String::new(),
                host: String::new(),
                target: target.into(),
                text: "hi".into(),
                is_private,
                tags: Vec::new(),
                role: None,
                honorific: String::new(),
                is_action: false,
            }),
        };
        let target = |env: &EventEnvelope| match &env.event {
            Event::Message(message) => message.target.clone(),
            _ => unreachable!(),
        };
        let mut lower = message("#games", false);
        normalize_channel(&db, &mut channels, &mut lower);
        assert_eq!(target(&lower), "#Games");
        let mut other = message("#elsewhere", false);
        normalize_channel(&db, &mut channels, &mut other);
        assert_eq!(target(&other), "#elsewhere");
        let mut private = message("jeeves", true);
        normalize_channel(&db, &mut channels, &mut private);
        assert_eq!(target(&private), "jeeves");
    }

    #[tokio::test]
    async fn tracks_network_connections() {
        let (out, mut events) = mpsc::channel(2);
        let connected = Arc::new(Mutex::new(HashSet::new()));
        let input = spawn(
            DbHandle::open(":memory:").unwrap(),
            LogBus::new(8),
            out,
            connected.clone(),
        );

        input
            .send(EventEnvelope {
                server: "libera".into(),
                event: Event::Connected,
            })
            .await
            .unwrap();
        events.recv().await.unwrap();
        assert!(connected.lock().unwrap().contains("libera"));

        input
            .send(EventEnvelope {
                server: "libera".into(),
                event: Event::Disconnected,
            })
            .await
            .unwrap();
        events.recv().await.unwrap();
        assert!(connected.lock().unwrap().is_empty());
    }
}
