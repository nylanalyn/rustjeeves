//! The IRC actor: owns the `irc` client, drives connection + IRCv3 CAP/SASL negotiation, streams
//! server messages out as [`jeeves_abi::Event`]s, and executes [`IrcAction`]s.

use crate::action::IrcAction;
use crate::config::ServerConfig;
use crate::log_bus::LogBus;
use anyhow::{anyhow, Result};
use base64::Engine;
use futures_util::StreamExt;
use irc::client::prelude::{Capability, Client, Command, Config, Prefix, Response};
use irc::proto::CapSubCommand;
use jeeves_abi::{Event, EventEnvelope, MessagePayload};
use std::collections::VecDeque;
use tokio::sync::mpsc;

// Outbound rate-limit: burst of 4 messages, then 1 per 500 ms.
const RATE_BURST: f64 = 4.0;
const RATE_MS_PER_TOKEN: f64 = 500.0;
// Messages queued while rate-limited; overflow is dropped with an error log.
const MAX_PENDING: usize = 64;
// Maximum byte length for a PRIVMSG/NOTICE text after stripping control chars.
// IRC max message is 512 bytes total; 450 leaves generous room for the wire prefix.
const MAX_MSG_BYTES: usize = 450;

/// Run the IRC actor until the connection ends or a fatal error occurs.
///
/// * `cfg` — connection settings.
/// * `log` — log bus for status/errors/messages.
/// * `actions` — inbound actions to execute against the connection.
/// * `events` — outbound IRC events (for the module host). May be a dropped receiver in headless
///   mode with no modules; sends are best-effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunExit {
    Disconnected,
    StopRequested,
}

pub async fn run(
    cfg: ServerConfig,
    log: LogBus,
    actions: &mut mpsc::Receiver<IrcAction>,
    events: mpsc::Sender<EventEnvelope>,
) -> Result<RunExit> {
    if cfg.host.is_empty() {
        return Err(anyhow!(
            "no IRC server configured — set one in the TUI (interactive mode) first"
        ));
    }

    let irc_config = build_config(&cfg);
    log.info(
        "irc",
        format!(
            "[{}] connecting to {}:{} (tls={})",
            cfg.label, cfg.host, cfg.port, cfg.tls
        ),
    );

    let mut client = Client::from_config(irc_config).await?;
    let mut stream = client.stream()?;
    let sender = client.sender();

    // Begin registration by requesting capabilities ourselves (so we control CAP END timing for
    // SASL). We request `account-tag` (for permission resolution) plus `sasl` when configured.
    let mut neg = Neg {
        sasl_pending: cfg.sasl_enabled(),
        retried: false,
        ended: false,
    };
    let mut caps = vec![Capability::AccountTag];
    if neg.sasl_pending {
        caps.push(Capability::Sasl);
    }
    log.info(
        "irc",
        format!(
            "[{}] negotiating caps{}",
            cfg.label,
            if neg.sasl_pending {
                " + SASL PLAIN"
            } else {
                ""
            }
        ),
    );
    sender.send_cap_req(&caps)?;
    sender.send(Command::NICK(cfg.nick.clone()))?;
    sender.send(Command::USER(
        cfg.username.clone(),
        "0".into(),
        cfg.realname.clone(),
    ))?;

    let mut pending: VecDeque<IrcAction> = VecDeque::new();
    let mut rate = RateLimiter::new(RATE_BURST, RATE_MS_PER_TOKEN);

    loop {
        tokio::select! {
            // Inbound actions. QUIT bypasses the rate limiter; everything else is queued or
            // sent immediately depending on token availability.
            maybe_action = actions.recv() => {
                match maybe_action {
                    Some(action) => {
                        if matches!(action, IrcAction::Quit(_)) {
                            execute(&sender, action, &log, &cfg.label);
                            // Give the writer task a brief opportunity to flush QUIT.
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                            let _ = events.send(EventEnvelope {
                                server: cfg.label.clone(),
                                event: Event::Disconnected,
                            }).await;
                            return Ok(RunExit::StopRequested);
                        }
                        submit_action(
                            &sender,
                            action,
                            &mut pending,
                            &mut rate,
                            &log,
                            &cfg.label,
                        );
                    }
                    None => return Ok(RunExit::StopRequested),
                }
            }

            // Drain the pending queue when tokens are available.
            _ = tokio::time::sleep(rate.next_token_in()), if !pending.is_empty() => {
                while !pending.is_empty() {
                    if rate.try_consume() {
                        let action = pending.pop_front().unwrap();
                        execute(&sender, action, &log, &cfg.label);
                    } else {
                        break;
                    }
                }
            }

            // Server messages.
            maybe_msg = stream.next() => {
                match maybe_msg {
                    Some(Ok(message)) => {
                        if let Some(action) =
                            handle_message(&cfg, &sender, &log, &events, &mut neg, message).await
                        {
                            submit_action(
                                &sender,
                                action,
                                &mut pending,
                                &mut rate,
                                &log,
                                &cfg.label,
                            );
                        }
                    }
                    Some(Err(e)) => {
                        log.error("irc", format!("stream error: {e}"));
                        return Err(e.into());
                    }
                    None => {
                        log.error("irc", format!("[{}] disconnected", cfg.label));
                        let _ = events.send(EventEnvelope { server: cfg.label.clone(), event: Event::Disconnected }).await;
                        return Ok(RunExit::Disconnected);
                    }
                }
            }
        }
    }
}

fn build_config(cfg: &ServerConfig) -> Config {
    let (channels, keys): (Vec<String>, std::collections::HashMap<String, String>) = {
        let mut chans = Vec::new();
        let mut keys = std::collections::HashMap::new();
        for (name, key) in &cfg.channels {
            chans.push(name.clone());
            if let Some(k) = key {
                keys.insert(name.clone(), k.clone());
            }
        }
        (chans, keys)
    };

    Config {
        nickname: Some(cfg.nick.clone()),
        username: Some(cfg.username.clone()),
        realname: Some(cfg.realname.clone()),
        server: Some(cfg.host.clone()),
        port: Some(cfg.port),
        use_tls: Some(cfg.tls),
        dangerously_accept_invalid_certs: Some(cfg.accept_invalid_certs),
        umodes: cfg.umodes.clone(),
        channels,
        channel_keys: keys,
        // Used by the crate's NickServ-message fallback (auto-IDENTIFY on end of MOTD).
        // Left as None when SASL is in use so we don't identify twice.
        nick_password: if cfg.sasl_enabled() {
            None
        } else {
            cfg.nick_password.clone()
        },
        ..Config::default()
    }
}

fn execute(sender: &irc::client::Sender, action: IrcAction, log: &LogBus, label: &str) {
    let result = match &action {
        IrcAction::Privmsg { target, text } => {
            let clean = sanitize_outbound(text, log, label);
            sender.send_privmsg(target.as_str(), clean.as_str())
        }
        IrcAction::Notice { target, text } => {
            let clean = sanitize_outbound(text, log, label);
            sender.send_notice(target.as_str(), clean.as_str())
        }
        IrcAction::Join(chan) => sender.send_join(chan),
        IrcAction::Part(chan) => sender.send_part(chan),
        IrcAction::Quit(msg) => sender.send_quit(msg.clone().unwrap_or_default()),
    };
    if let Err(e) = result {
        log.error(
            "irc",
            format!("[{label}] failed to execute {action:?}: {e}"),
        );
    }
}

fn submit_action(
    sender: &irc::client::Sender,
    action: IrcAction,
    pending: &mut VecDeque<IrcAction>,
    rate: &mut RateLimiter,
    log: &LogBus,
    label: &str,
) {
    // Once anything is queued, preserve FIFO order instead of allowing a newer action to consume
    // a freshly-refilled token ahead of it.
    if pending.is_empty() && rate.try_consume() {
        execute(sender, action, log, label);
    } else if pending.len() < MAX_PENDING {
        pending.push_back(action);
    } else {
        log.error(
            "irc",
            format!("[{label}] outbound queue full; message dropped"),
        );
    }
}

async fn handle_message(
    cfg: &ServerConfig,
    sender: &irc::client::Sender,
    log: &LogBus,
    events: &mpsc::Sender<EventEnvelope>,
    neg: &mut Neg,
    message: irc::proto::Message,
) -> Option<IrcAction> {
    let (nick, user, host) = match &message.prefix {
        Some(Prefix::Nickname(n, u, h)) => (n.clone(), u.clone(), h.clone()),
        _ => (String::new(), String::new(), String::new()),
    };
    match &message.command {
        // --- Capability negotiation ---
        // The acked capability list can land in either CAP field depending on whether it was sent
        // as a trailing (":sasl") parameter, so check both.
        Command::CAP(_, CapSubCommand::ACK, mid, trailing) => {
            if neg.sasl_pending && cap_acks_sasl(mid, trailing) {
                // Caps acked and we need SASL: begin it. CAP END happens after SASL completes.
                if let Err(e) = sender.send_sasl_plain() {
                    log.error("irc", format!("send AUTHENTICATE PLAIN failed: {e}"));
                }
            } else if !neg.sasl_pending {
                // Caps acked and there is no SASL to do — finish negotiation.
                end_caps(neg, sender);
            }
        }
        Command::CAP(_, CapSubCommand::NAK, _, _) => {
            if neg.sasl_pending && !neg.retried {
                // The combined REQ (account-tag + sasl) was rejected (likely account-tag
                // unsupported). Retry SASL alone so authentication still happens.
                neg.retried = true;
                log.debug(
                    "irc",
                    format!("[{}] cap REQ rejected; retrying SASL alone", cfg.label),
                );
                let _ = sender.send_cap_req(&[Capability::Sasl]);
            } else {
                neg.sasl_pending = false;
                end_caps(neg, sender);
            }
        }
        Command::AUTHENTICATE(data) if neg.sasl_pending => {
            if data == "+" {
                match sasl_plain_payload(cfg) {
                    Some(payload) => {
                        if let Err(e) = sender.send_sasl(payload) {
                            log.error("irc", format!("send SASL payload failed: {e}"));
                        }
                    }
                    None => log.error("irc", "SASL enabled but credentials missing"),
                }
            }
        }
        Command::Response(Response::RPL_SASLSUCCESS, _)
        | Command::Response(Response::RPL_LOGGEDIN, _) => {
            if neg.sasl_pending {
                log.info(
                    "irc",
                    format!("[{}] SASL authentication succeeded", cfg.label),
                );
                neg.sasl_pending = false;
                end_caps(neg, sender);
            }
        }
        Command::Response(Response::ERR_SASLFAIL, _) => {
            log.error(
                "irc",
                format!(
                    "[{}] SASL authentication failed; registering anyway",
                    cfg.label
                ),
            );
            neg.sasl_pending = false;
            end_caps(neg, sender);
        }

        // --- Registration complete ---
        Command::Response(Response::RPL_WELCOME, _) => {
            log.info("irc", format!("[{}] registered with server", cfg.label));
            emit(events, &cfg.label, Event::Connected).await;
        }

        // --- Channel lifecycle (the crate auto-joins configured channels) ---
        Command::JOIN(chan, _, _) => {
            if nick.eq_ignore_ascii_case(&cfg.nick) {
                log.info("irc", format!("[{}] joined {chan}", cfg.label));
                emit(
                    events,
                    &cfg.label,
                    Event::Joined {
                        channel: chan.clone(),
                    },
                )
                .await;
            }
        }
        Command::PART(chan, _) => {
            if nick.eq_ignore_ascii_case(&cfg.nick) {
                emit(
                    events,
                    &cfg.label,
                    Event::Parted {
                        channel: chan.clone(),
                    },
                )
                .await;
            }
        }
        Command::NICK(new_nick) => {
            let account = message
                .tags
                .as_ref()
                .and_then(|tags| tags.iter().find(|t| t.0 == "account"))
                .and_then(|t| t.1.clone())
                .filter(|a| !a.is_empty() && a != "*");
            if !nick.is_empty() {
                emit(
                    events,
                    &cfg.label,
                    Event::NickChanged {
                        old_nick: nick,
                        new_nick: new_nick.clone(),
                        account,
                    },
                )
                .await;
            }
        }

        // --- Messages ---
        Command::PRIVMSG(target, text) => {
            // Intercept CTCP before forwarding to modules.
            if let Some(ctcp) = parse_ctcp(text) {
                return handle_ctcp(&nick, &ctcp, &cfg.label, log);
            }
            let is_private = !is_channel(target);
            log.message("irc", format!("[{}] <{nick}> [{target}] {text}", cfg.label));
            let payload = MessagePayload {
                user_id: String::new(),
                display: nick.clone(),
                nick,
                user,
                host,
                target: target.clone(),
                text: text.clone(),
                is_private,
                tags: message
                    .tags
                    .as_ref()
                    .map(|tags| tags.iter().map(|t| (t.0.clone(), t.1.clone())).collect())
                    .unwrap_or_default(),
                role: None,
            };
            emit(events, &cfg.label, Event::Message(payload)).await;
        }

        // --- Everything else: forward as raw, log at debug ---
        other => {
            let rendered = message.to_string();
            let rendered = rendered.trim();
            log.debug("irc", format!("[{}] {rendered}", cfg.label));
            emit(
                events,
                &cfg.label,
                Event::Raw {
                    command: format!("{other:?}"),
                    args: vec![rendered.to_string()],
                },
            )
            .await;
        }
    }
    None
}

async fn emit(events: &mpsc::Sender<EventEnvelope>, server: &str, event: Event) {
    let _ = events
        .send(EventEnvelope {
            server: server.to_string(),
            event,
        })
        .await;
}

/// Capability-negotiation state for one connection.
struct Neg {
    /// We still intend to authenticate via SASL.
    sasl_pending: bool,
    /// We already retried the CAP REQ with SASL alone (after a combined-REQ NAK).
    retried: bool,
    /// CAP END has been sent.
    ended: bool,
}

/// Send `CAP END` exactly once to finish negotiation and let registration proceed.
fn end_caps(neg: &mut Neg, sender: &irc::client::Sender) {
    if !neg.ended {
        neg.ended = true;
        let _ = sender.send(Command::CAP(None, CapSubCommand::END, None, None));
    }
}

/// base64( authzid \0 authcid \0 passwd ) for SASL PLAIN. authzid is left empty.
fn sasl_plain_payload(cfg: &ServerConfig) -> Option<String> {
    let account = cfg.sasl_account.as_ref()?;
    let password = cfg.sasl_password.as_ref()?;
    let raw = format!("\0{account}\0{password}");
    Some(base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()))
}

fn is_channel(target: &str) -> bool {
    target.starts_with('#') || target.starts_with('&')
}

/// Whether a `CAP ... ACK` acknowledges the `sasl` capability. The acked capability list can land
/// in either CAP parameter field depending on whether the server sent it as a trailing (`:sasl`)
/// parameter, so check both. (Regression guard: ergo sends it in the middle field.)
fn cap_acks_sasl(mid: &Option<String>, trailing: &Option<String>) -> bool {
    mid.as_deref().unwrap_or("").contains("sasl")
        || trailing.as_deref().unwrap_or("").contains("sasl")
}

// ---------------------------------------------------------------------------
// Token-bucket rate limiter
// ---------------------------------------------------------------------------

struct RateLimiter {
    tokens: f64,
    burst: f64,
    ms_per_token: f64,
    last: std::time::Instant,
}

impl RateLimiter {
    fn new(burst: f64, ms_per_token: f64) -> Self {
        Self {
            tokens: burst,
            burst,
            ms_per_token,
            last: std::time::Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = std::time::Instant::now();
        let elapsed_ms = now.duration_since(self.last).as_secs_f64() * 1000.0;
        self.last = now;
        self.tokens = (self.tokens + elapsed_ms / self.ms_per_token).min(self.burst);
    }

    fn try_consume(&mut self) -> bool {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until we have at least one token.
    fn next_token_in(&mut self) -> std::time::Duration {
        self.refill();
        if self.tokens >= 1.0 {
            std::time::Duration::ZERO
        } else {
            let ms_needed = (1.0 - self.tokens) * self.ms_per_token;
            std::time::Duration::from_millis(ms_needed.ceil() as u64)
        }
    }
}

// ---------------------------------------------------------------------------
// Output sanitisation
// ---------------------------------------------------------------------------

/// Strip CR/LF and truncate to at most [`MAX_MSG_BYTES`] bytes at a UTF-8 boundary.
fn sanitize_outbound(text: &str, log: &LogBus, label: &str) -> String {
    let clean: String = text.chars().filter(|&c| c != '\r' && c != '\n').collect();
    if clean.len() <= MAX_MSG_BYTES {
        return clean;
    }
    let mut end = MAX_MSG_BYTES;
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    log.error(
        "irc",
        format!("[{label}] outbound message truncated to {end} bytes"),
    );
    clean[..end].to_string()
}

// ---------------------------------------------------------------------------
// CTCP
// ---------------------------------------------------------------------------

/// Extract the CTCP payload from `\x01COMMAND[ args]\x01`, or return `None`.
fn parse_ctcp(text: &str) -> Option<String> {
    let inner = text.strip_prefix('\x01')?.strip_suffix('\x01')?;
    Some(inner.to_string())
}

/// Handle a decoded CTCP payload: reply to VERSION and PING; silently ignore others.
fn handle_ctcp(nick: &str, ctcp: &str, label: &str, log: &LogBus) -> Option<IrcAction> {
    let (cmd, param) = ctcp.split_once(' ').unwrap_or((ctcp, ""));
    let text = match cmd {
        "VERSION" => Some("\x01VERSION rustjeeves 0.1\x01".to_string()),
        "PING" => Some(format!("\x01PING {param}\x01")),
        other => {
            log.debug(
                "irc",
                format!("[{label}] CTCP {other} from {nick} (ignored)"),
            );
            None
        }
    }?;
    Some(IrcAction::Notice {
        target: nick.to_string(),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::{cap_acks_sasl, handle_ctcp, parse_ctcp, sanitize_outbound, MAX_MSG_BYTES};
    use crate::action::IrcAction;
    use crate::log_bus::LogBus;

    fn dummy_log() -> LogBus {
        LogBus::new(16)
    }

    #[test]
    fn detects_sasl_ack_in_either_cap_field() {
        assert!(cap_acks_sasl(&Some("sasl".into()), &None));
        assert!(cap_acks_sasl(&None, &Some("sasl".into())));
        assert!(cap_acks_sasl(&None, &Some("sasl message-tags".into())));
        assert!(!cap_acks_sasl(&Some("message-tags".into()), &None));
        assert!(!cap_acks_sasl(&None, &None));
    }

    #[test]
    fn parse_ctcp_extracts_payload() {
        assert_eq!(parse_ctcp("\x01VERSION\x01").as_deref(), Some("VERSION"));
        assert_eq!(
            parse_ctcp("\x01PING 12345\x01").as_deref(),
            Some("PING 12345")
        );
        assert!(parse_ctcp("hello").is_none());
        assert!(parse_ctcp("\x01noeol").is_none());
    }

    #[test]
    fn ctcp_reply_is_returned_as_a_rate_limited_action() {
        let log = dummy_log();
        let action = handle_ctcp("alice", "PING 12345", "test", &log).unwrap();
        assert!(matches!(
            action,
            IrcAction::Notice { target, text }
                if target == "alice" && text == "\x01PING 12345\x01"
        ));
        assert!(handle_ctcp("alice", "TIME", "test", &log).is_none());
    }

    #[test]
    fn sanitize_outbound_strips_crlf() {
        let log = dummy_log();
        assert_eq!(
            sanitize_outbound("hello\r\nworld", &log, "test"),
            "helloworld"
        );
    }

    #[test]
    fn sanitize_outbound_truncates_long_message() {
        let log = dummy_log();
        let long = "a".repeat(MAX_MSG_BYTES + 100);
        let out = sanitize_outbound(&long, &log, "test");
        assert_eq!(out.len(), MAX_MSG_BYTES);
    }

    #[test]
    fn sanitize_outbound_truncates_at_utf8_boundary() {
        let log = dummy_log();
        // Each '€' is 3 bytes; build a string where a naive byte-cut would land mid-char.
        let s = "€".repeat(160); // 480 bytes > 450
        let out = sanitize_outbound(&s, &log, "test");
        assert!(out.len() <= MAX_MSG_BYTES);
        // Must be valid UTF-8 (String::from_utf8 would panic on invalid).
        assert!(std::str::from_utf8(out.as_bytes()).is_ok());
    }
}
