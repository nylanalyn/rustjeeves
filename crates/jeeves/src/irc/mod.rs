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
use jeeves_abi::{Event, MessagePayload};
use tokio::sync::mpsc;

/// Run the IRC actor until the connection ends or a fatal error occurs.
///
/// * `cfg` — connection settings.
/// * `log` — log bus for status/errors/messages.
/// * `actions` — inbound actions to execute against the connection.
/// * `events` — outbound IRC events (for the module host). May be a dropped receiver in headless
///   mode with no modules; sends are best-effort.
pub async fn run(
    cfg: ServerConfig,
    log: LogBus,
    mut actions: mpsc::Receiver<IrcAction>,
    events: mpsc::Sender<Event>,
) -> Result<()> {
    if cfg.host.is_empty() {
        return Err(anyhow!(
            "no IRC server configured — set one in the TUI (interactive mode) first"
        ));
    }

    let irc_config = build_config(&cfg);
    log.info("irc", format!("connecting to {}:{} (tls={})", cfg.host, cfg.port, cfg.tls));

    let mut client = Client::from_config(irc_config).await?;
    let mut stream = client.stream()?;
    let sender = client.sender();

    // Begin registration. With SASL we drive CAP/AUTHENTICATE manually and must NOT call
    // identify() (which would send CAP END before authentication completes).
    let mut sasl_pending = cfg.sasl_enabled();
    if sasl_pending {
        log.info("irc", "negotiating SASL PLAIN");
        sender.send_cap_req(&[Capability::Sasl])?;
        sender.send(Command::NICK(cfg.nick.clone()))?;
        sender.send(Command::USER(
            cfg.username.clone(),
            "0".into(),
            cfg.realname.clone(),
        ))?;
    } else {
        client.identify()?;
    }

    loop {
        tokio::select! {
            // Inbound actions to execute.
            maybe_action = actions.recv() => {
                match maybe_action {
                    Some(action) => execute(&sender, action, &log),
                    None => {
                        // All action senders dropped — keep the connection alive anyway.
                        // (Drain by awaiting only the stream from here on.)
                    }
                }
            }

            // Server messages.
            maybe_msg = stream.next() => {
                match maybe_msg {
                    Some(Ok(message)) => {
                        handle_message(&cfg, &sender, &log, &events, &mut sasl_pending, message).await;
                    }
                    Some(Err(e)) => {
                        log.error("irc", format!("stream error: {e}"));
                        return Err(e.into());
                    }
                    None => {
                        log.error("irc", "disconnected");
                        let _ = events.try_send(Event::Disconnected);
                        return Ok(());
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

fn execute(sender: &irc::client::Sender, action: IrcAction, log: &LogBus) {
    let result = match &action {
        IrcAction::Privmsg { target, text } => sender.send_privmsg(target, text),
        IrcAction::Notice { target, text } => sender.send_notice(target, text),
        IrcAction::Join(chan) => sender.send_join(chan),
        IrcAction::Part(chan) => sender.send_part(chan),
        IrcAction::Quit(msg) => sender.send_quit(msg.clone().unwrap_or_default()),
    };
    if let Err(e) = result {
        log.error("irc", format!("failed to execute {action:?}: {e}"));
    }
}

async fn handle_message(
    cfg: &ServerConfig,
    sender: &irc::client::Sender,
    log: &LogBus,
    events: &mpsc::Sender<Event>,
    sasl_pending: &mut bool,
    message: irc::proto::Message,
) {
    let nick = match &message.prefix {
        Some(Prefix::Nickname(n, _, _)) => n.clone(),
        _ => String::new(),
    };

    match &message.command {
        // --- SASL negotiation ---
        // The acked capability list can land in either CAP field depending on whether it was sent
        // as a trailing (":sasl") parameter, so check both.
        Command::CAP(_, CapSubCommand::ACK, mid, trailing) => {
            if *sasl_pending && cap_acks_sasl(mid, trailing) {
                if let Err(e) = sender.send_sasl_plain() {
                    log.error("irc", format!("send AUTHENTICATE PLAIN failed: {e}"));
                }
            }
        }
        Command::AUTHENTICATE(data) if *sasl_pending => {
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
        Command::Response(Response::RPL_SASLSUCCESS, _) | Command::Response(Response::RPL_LOGGEDIN, _) => {
            if *sasl_pending {
                log.info("irc", "SASL authentication succeeded");
                *sasl_pending = false;
                // End capability negotiation so registration can complete.
                let _ = sender.send(Command::CAP(None, CapSubCommand::END, None, None));
            }
        }
        Command::Response(Response::ERR_SASLFAIL, _) => {
            log.error("irc", "SASL authentication failed; aborting and registering anyway");
            *sasl_pending = false;
            let _ = sender.send(Command::CAP(None, CapSubCommand::END, None, None));
        }

        // --- Registration complete ---
        Command::Response(Response::RPL_WELCOME, _) => {
            log.info("irc", "registered with server");
            let _ = events.try_send(Event::Connected);
        }

        // --- Channel lifecycle (the crate auto-joins configured channels) ---
        Command::JOIN(chan, _, _) => {
            if nick.eq_ignore_ascii_case(&cfg.nick) {
                log.info("irc", format!("joined {chan}"));
                let _ = events.try_send(Event::Joined { channel: chan.clone() });
            }
        }
        Command::PART(chan, _) => {
            if nick.eq_ignore_ascii_case(&cfg.nick) {
                let _ = events.try_send(Event::Parted { channel: chan.clone() });
            }
        }

        // --- Messages ---
        Command::PRIVMSG(target, text) => {
            let is_private = !is_channel(target);
            log.message("irc", format!("<{nick}> [{target}] {text}"));
            let payload = MessagePayload {
                nick,
                target: target.clone(),
                text: text.clone(),
                is_private,
                tags: message
                    .tags
                    .as_ref()
                    .map(|tags| tags.iter().map(|t| (t.0.clone(), t.1.clone())).collect())
                    .unwrap_or_default(),
            };
            let _ = events.try_send(Event::Message(payload));
        }

        // --- Everything else: forward as raw, log at debug ---
        other => {
            let rendered = message.to_string();
            let rendered = rendered.trim();
            log.debug("irc", rendered.to_string());
            let _ = events.try_send(Event::Raw {
                command: format!("{other:?}"),
                args: vec![rendered.to_string()],
            });
        }
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

#[cfg(test)]
mod tests {
    use super::cap_acks_sasl;

    #[test]
    fn detects_sasl_ack_in_either_cap_field() {
        // ergo: "CAP * ACK :sasl" parses with the cap name in the middle field.
        assert!(cap_acks_sasl(&Some("sasl".into()), &None));
        // Other servers: cap name in the trailing field.
        assert!(cap_acks_sasl(&None, &Some("sasl".into())));
        // Multiple caps acked together.
        assert!(cap_acks_sasl(&None, &Some("sasl message-tags".into())));
        // No sasl -> not acked.
        assert!(!cap_acks_sasl(&Some("message-tags".into()), &None));
        assert!(!cap_acks_sasl(&None, &None));
    }
}
