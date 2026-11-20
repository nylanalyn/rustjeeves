//! Fluxer connection actor.
//!
//! Fluxer speaks a Discord-wire-compatible API: a WebSocket gateway for events plus a REST API
//! for sending. This actor mirrors `irc::run` — it owns the connection, translates inbound
//! gateway events into `EventEnvelope`s tagged with the network label, and executes
//! `TransportAction`s over REST. Outbound text passes through `format::irc_to_markdown` so mIRC
//! formatting in themes and module output renders as Fluxer markdown.
//!
//! Identity: Fluxer user ids are stable snowflakes, so they are passed as the IRCv3 `account`
//! message tag; the permission resolver then keys profiles and roles on them unchanged.

pub mod format;

use crate::action::TransportAction;
use crate::config::{FluxerConfig, ServerConfig};
use crate::log_bus::LogBus;
use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use jeeves_abi::{Event, EventEnvelope, MessagePayload};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};

pub enum RunExit {
    Disconnected,
    StopRequested,
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Gateway opcodes (Discord-wire-compatible).
const OP_DISPATCH: u64 = 0;
const OP_HEARTBEAT: u64 = 1;
const OP_IDENTIFY: u64 = 2;
const OP_RECONNECT: u64 = 7;
const OP_INVALID_SESSION: u64 = 9;
const OP_HELLO: u64 = 10;
const OP_HEARTBEAT_ACK: u64 = 11;

pub async fn run(
    cfg: ServerConfig,
    log: LogBus,
    actions: &mut mpsc::Receiver<TransportAction>,
    events: mpsc::Sender<EventEnvelope>,
) -> Result<RunExit> {
    let fluxer = cfg
        .fluxer
        .clone()
        .ok_or_else(|| anyhow!("fluxer connection has no fluxer settings"))?;
    if fluxer.token.trim().is_empty() {
        return Err(anyhow!("fluxer connection has no bot token"));
    }
    let label = cfg.label.clone();
    log.info(
        "fluxer",
        format!("[{label}] connecting to gateway {}", fluxer.gateway()),
    );

    let (mut ws, _resp) = connect_async(fluxer.gateway().to_string()).await?;

    // First payload must be HELLO; it carries the heartbeat interval.
    let hello = next_gateway_message(&mut ws).await?;
    if hello["op"].as_u64() != Some(OP_HELLO) {
        return Err(anyhow!("expected gateway HELLO, got op {:?}", hello["op"]));
    }
    let heartbeat_ms = hello["d"]["heartbeat_interval"]
        .as_u64()
        .ok_or_else(|| anyhow!("HELLO missing heartbeat_interval"))?;

    let mut seq: Option<u64> = None;
    let mut bot_id: Option<String> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_ms.max(1000)));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Per-channel send timestamps for a simple 5-per-5s rate limit (Discord-style bucket).
    let mut send_log: HashMap<String, Vec<Instant>> = HashMap::new();

    send_gateway(
        &mut ws,
        json!({
            "op": OP_IDENTIFY,
            "d": {
                "token": fluxer.token,
                "properties": { "os": "linux", "browser": "rustjeeves", "device": "rustjeeves" },
                // Fluxer does not implement intent gating yet.
                "intents": 0,
            }
        }),
    )
    .await?;

    loop {
        tokio::select! {
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let payload: Value = match serde_json::from_str(&text) {
                            Ok(v) => v,
                            Err(e) => {
                                log.error("fluxer", format!("[{label}] bad gateway json: {e}"));
                                continue;
                            }
                        };
                        if let Some(s) = payload["s"].as_u64() {
                            seq = Some(s);
                        }
                        match payload["op"].as_u64() {
                            Some(OP_DISPATCH) => {
                                let event_name = payload["t"].as_str().unwrap_or("");
                                match event_name {
                                    "READY" => {
                                        bot_id = payload["d"]["user"]["id"].as_str().map(str::to_string);
                                        log.info("fluxer", format!(
                                            "[{label}] ready as bot id {}",
                                            bot_id.as_deref().unwrap_or("?")
                                        ));
                                        let _ = events.send(EventEnvelope {
                                            server: label.clone(),
                                            event: Event::Connected,
                                        }).await;
                                        // Announce configured channels as joined so channel
                                        // state keys on the canonical id spelling.
                                        for (channel, _) in &cfg.channels {
                                            let _ = events.send(EventEnvelope {
                                                server: label.clone(),
                                                event: Event::Joined { channel: channel.clone() },
                                            }).await;
                                        }
                                    }
                                    "MESSAGE_CREATE" => {
                                        if let Some(env) = map_message(&label, bot_id.as_deref(), &payload["d"]) {
                                            if events.send(env).await.is_err() {
                                                return Ok(RunExit::StopRequested);
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                            Some(OP_HELLO) => {} // already handled pre-loop; ignore repeats
                            Some(OP_HEARTBEAT) => {
                                send_gateway(&mut ws, json!({ "op": OP_HEARTBEAT, "d": seq })).await?;
                            }
                            Some(OP_HEARTBEAT_ACK) => {}
                            Some(OP_RECONNECT) => {
                                log.info("fluxer", format!("[{label}] gateway requested reconnect"));
                                return Ok(RunExit::Disconnected);
                            }
                            Some(OP_INVALID_SESSION) => {
                                // Re-identify fresh (we do not persist sessions across
                                // reconnects yet, so a resumable flag changes nothing here).
                                let resumable = payload["d"].as_bool().unwrap_or(false);
                                log.info("fluxer", format!(
                                    "[{label}] invalid session (resumable={resumable}); re-identifying"
                                ));
                                send_gateway(&mut ws, json!({
                                    "op": OP_IDENTIFY,
                                    "d": {
                                        "token": fluxer.token,
                                        "properties": { "os": "linux", "browser": "rustjeeves", "device": "rustjeeves" },
                                        "intents": 0,
                                    }
                                })).await?;
                            }
                            other => {
                                log.debug("fluxer", format!("[{label}] unhandled gateway op {other:?}"));
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        log.info("fluxer", format!("[{label}] gateway closed"));
                        return Ok(RunExit::Disconnected);
                    }
                    Some(Ok(_)) => {} // ping/pong/binary: tungstenite handles pings
                    Some(Err(e)) => return Err(anyhow!("gateway error: {e}")),
                }
            }
            _ = heartbeat.tick() => {
                if send_gateway(&mut ws, json!({ "op": OP_HEARTBEAT, "d": seq })).await.is_err() {
                    return Ok(RunExit::Disconnected);
                }
            }
            action = actions.recv() => {
                match action {
                    None => return Ok(RunExit::StopRequested),
                    Some(TransportAction::Quit(_)) => {
                        let _ = ws.send(Message::Close(None)).await;
                        return Ok(RunExit::StopRequested);
                    }
                    Some(action) => {
                        if let Err(e) = execute_action(&fluxer, &label, action, &log, &mut send_log).await {
                            log.error("fluxer", format!("[{label}] action failed: {e}"));
                        }
                    }
                }
            }
        }
    }
}

/// Read one text payload from the gateway, skipping non-text frames.
async fn next_gateway_message(ws: &mut Ws) -> Result<Value> {
    loop {
        match ws.next().await {
            Some(Ok(Message::Text(text))) => return Ok(serde_json::from_str(&text)?),
            Some(Ok(_)) => continue,
            Some(Err(e)) => return Err(anyhow!("gateway read error: {e}")),
            None => return Err(anyhow!("gateway closed before HELLO")),
        }
    }
}

async fn send_gateway(ws: &mut Ws, payload: Value) -> Result<()> {
    ws.send(Message::Text(payload.to_string().into())).await?;
    Ok(())
}

/// Map a gateway MESSAGE_CREATE dispatch to an event envelope. Returns `None` for messages that
/// must be ignored (other bots, the bot itself).
fn map_message(label: &str, bot_id: Option<&str>, d: &Value) -> Option<EventEnvelope> {
    let author = &d["author"];
    if author["bot"].as_bool().unwrap_or(false) {
        return None;
    }
    let id = author["id"].as_str()?;
    if bot_id == Some(id) {
        return None;
    }
    let channel_id = d["channel_id"].as_str()?.to_string();
    let is_private = d["guild_id"].as_str().is_none();
    let username = author["username"].as_str().unwrap_or("").to_string();
    // Guild nickname when present, else the username.
    let nick = d["member"]["nick"]
        .as_str()
        .filter(|n| !n.is_empty())
        .unwrap_or(&username)
        .to_string();
    let payload = MessagePayload {
        // Stamped by the permission resolver from the account tag below.
        user_id: String::new(),
        display: nick.clone(),
        nick,
        user: username,
        host: id.to_string(),
        target: channel_id,
        text: d["content"].as_str().unwrap_or("").to_string(),
        is_private,
        // The stable snowflake rides as the account tag so profile/role resolution keys on it.
        tags: vec![("account".to_string(), Some(id.to_string()))],
        role: None,
        honorific: String::new(),
        is_action: false,
    };
    Some(EventEnvelope {
        server: label.to_string(),
        event: Event::Message(payload),
    })
}

/// Execute one outbound action. Fluxer has no NOTICE/JOIN/PART/moderation equivalents, so those
/// are logged and dropped; PRIVMSG/NOTICE become REST message sends.
async fn execute_action(
    fluxer: &FluxerConfig,
    label: &str,
    action: TransportAction,
    log: &LogBus,
    send_log: &mut HashMap<String, Vec<Instant>>,
) -> Result<()> {
    match action {
        TransportAction::Privmsg { target, text } | TransportAction::Notice { target, text } => {
            send_message(fluxer, label, &target, &text, log, send_log).await
        }
        unsupported => {
            log.debug(
                "fluxer",
                format!("[{label}] action not supported on fluxer, dropping: {unsupported:?}"),
            );
            Ok(())
        }
    }
}

/// Send `text` to a channel (or, on unknown-channel, open a DM to a user id and send there).
async fn send_message(
    fluxer: &FluxerConfig,
    label: &str,
    target: &str,
    text: &str,
    log: &LogBus,
    send_log: &mut HashMap<String, Vec<Instant>>,
) -> Result<()> {
    let cleaned = format::sanitize_outbound(text);
    let markdown = format::irc_to_markdown(&cleaned);
    for chunk in format::split_outbound(&markdown) {
        wait_for_rate_limit(send_log, target).await;
        let post = {
            let fluxer = fluxer.clone();
            let target = target.to_string();
            let chunk = chunk.clone();
            tokio::task::spawn_blocking(move || rest_post_message(&fluxer, &target, &chunk))
        };
        match post.await? {
            Ok(()) => {}
            Err(e) => {
                // Unknown channel/user: if the target looks like a user id, open a DM channel
                // and retry once.
                if target.chars().all(|c| c.is_ascii_digit()) && target.len() >= 17 {
                    let dm = {
                        let fluxer = fluxer.clone();
                        let target = target.to_string();
                        tokio::task::spawn_blocking(move || rest_open_dm(&fluxer, &target))
                    };
                    match dm.await? {
                        Ok(dm_channel) => {
                            log.debug(
                                "fluxer",
                                format!("[{label}] opened DM channel {dm_channel} for {target}"),
                            );
                            let retry = {
                                let fluxer = fluxer.clone();
                                let chunk = chunk.clone();
                                tokio::task::spawn_blocking(move || {
                                    rest_post_message(&fluxer, &dm_channel, &chunk)
                                })
                            };
                            retry.await??;
                        }
                        Err(dm_err) => {
                            return Err(anyhow!(
                                "send to {target} failed ({e}); DM fallback failed ({dm_err})"
                            ));
                        }
                    }
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
}

/// Simple per-channel rate limit: at most 5 messages per rolling 5 seconds.
async fn wait_for_rate_limit(send_log: &mut HashMap<String, Vec<Instant>>, channel: &str) {
    const WINDOW: Duration = Duration::from_secs(5);
    const MAX: usize = 5;
    loop {
        let now = Instant::now();
        let entry = send_log.entry(channel.to_string()).or_default();
        entry.retain(|t| now.duration_since(*t) < WINDOW);
        if entry.len() < MAX {
            entry.push(now);
            return;
        }
        let oldest = entry[0];
        let wait = WINDOW.saturating_sub(now.duration_since(oldest));
        tokio::time::sleep(wait).await;
    }
}

/// POST /channels/{id}/messages. Blocking HTTP, so callers run this via spawn_blocking.
fn rest_post_message(fluxer: &FluxerConfig, channel_id: &str, content: &str) -> Result<()> {
    let url = format!("{}/channels/{}/messages", fluxer.api_base(), channel_id);
    let agent = ureq::Agent::new_with_defaults();
    let resp = agent
        .post(&url)
        .header("Authorization", format!("Bot {}", fluxer.token.trim()))
        .send_json(json!({ "content": content }));
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::StatusCode(code)) => Err(anyhow!("POST {url} -> {code}")),
        Err(e) => Err(anyhow!("POST {url} failed: {e}")),
    }
}

/// POST /users/@me/channels to open a DM channel with a user id; returns the channel id.
fn rest_open_dm(fluxer: &FluxerConfig, user_id: &str) -> Result<String> {
    let url = format!("{}/users/@me/channels", fluxer.api_base());
    let agent = ureq::Agent::new_with_defaults();
    let resp = agent
        .post(&url)
        .header("Authorization", format!("Bot {}", fluxer.token.trim()))
        .send_json(json!({ "recipient_id": user_id }));
    match resp {
        Ok(mut r) => {
            let body: Value = serde_json::from_str(&r.body_mut().read_to_string()?)?;
            body["id"]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("DM channel response missing id"))
        }
        Err(ureq::Error::StatusCode(code)) => Err(anyhow!("POST {url} -> {code}")),
        Err(e) => Err(anyhow!("POST {url} failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatch(d: Value) -> Value {
        json!({ "op": 0, "t": "MESSAGE_CREATE", "s": 1, "d": d })
    }

    #[test]
    fn maps_channel_message_with_snowflake_account_tag() {
        let d = json!({
            "id": "111", "channel_id": "222", "guild_id": "333",
            "content": "!weather",
            "author": { "id": "444", "username": "aureate", "bot": false },
            "member": { "nick": "Au" }
        });
        let env = map_message("flux", Some("999"), &dispatch(d)["d"]).unwrap();
        let Event::Message(msg) = env.event else {
            panic!("expected message")
        };
        assert_eq!(env.server, "flux");
        assert_eq!(msg.nick, "Au");
        assert_eq!(msg.user, "aureate");
        assert_eq!(msg.target, "222");
        assert_eq!(msg.text, "!weather");
        assert!(!msg.is_private);
        assert_eq!(
            msg.tags,
            vec![("account".to_string(), Some("444".to_string()))]
        );
    }

    #[test]
    fn dm_has_no_guild_and_is_private() {
        let d = json!({
            "id": "1", "channel_id": "555", "content": "hi",
            "author": { "id": "444", "username": "aureate" }
        });
        let env = map_message("flux", None, &d).unwrap();
        let Event::Message(msg) = env.event else {
            panic!()
        };
        assert!(msg.is_private);
    }

    #[test]
    fn ignores_bots_and_self() {
        let bot_msg = json!({
            "channel_id": "222", "content": "hi",
            "author": { "id": "444", "username": "otherbot", "bot": true }
        });
        assert!(map_message("flux", Some("999"), &bot_msg).is_none());
        let self_msg = json!({
            "channel_id": "222", "content": "hi",
            "author": { "id": "999", "username": "jeeves", "bot": true }
        });
        assert!(map_message("flux", Some("999"), &self_msg).is_none());
    }
}
