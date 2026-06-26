//! Localhost HTTP admin API — the bot side of the shared Discord admin router
//! (`ircbot_core/discord_admin.py`). Implements the same contract used by the other bots:
//!
//! * `GET  /health`            -> `{"ok":true}` (no auth)
//! * `POST /v1/command`        -> `{"messages":[...]}`  (Bearer auth; body `{"command","args"}`)
//! * `GET  /v1/events?since=N` -> `{"events":[{"id","message"}]}` (Bearer auth)
//!
//! Runs on a dedicated thread (like the DB / module-host threads) and reaches the async runtime via
//! the shared [`ServerRegistry`] (for `say`/`join`/`part`) and a [`Control`] channel
//! (`reload`/`shutdown`). Enabled only when a token is configured; binds localhost by default.

use crate::action::{Control, IrcAction};
use crate::log_bus::LogBus;
use crate::modules::ServerRegistry;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Request, Response, Server};
use tokio::sync::mpsc;

/// Ring buffer of events surfaced to Discord via `/v1/events`.
#[derive(Default)]
pub struct EventLog {
    items: Vec<(i64, String)>,
    next_id: i64,
}

impl EventLog {
    pub fn push(&mut self, message: String) {
        self.next_id += 1;
        self.items.push((self.next_id, message));
        let overflow = self.items.len().saturating_sub(500);
        if overflow > 0 {
            self.items.drain(0..overflow);
        }
    }

    fn since(&self, since: i64) -> Vec<(i64, String)> {
        self.items.iter().filter(|(id, _)| *id > since).cloned().collect()
    }
}

/// Shared state the admin API needs to act on the running bot.
#[derive(Clone)]
pub struct AdminState {
    pub registry: ServerRegistry,
    pub control: mpsc::Sender<Control>,
    pub modules: Arc<Mutex<Vec<String>>>,
    pub events: Arc<Mutex<EventLog>>,
}

/// Start the admin API server on its own thread. No-op-with-error-log if the bind fails.
pub fn serve(bind: String, token: String, state: AdminState, log: LogBus) {
    std::thread::Builder::new()
        .name("jeeves-adminapi".into())
        .spawn(move || {
            let server = match Server::http(&bind) {
                Ok(s) => s,
                Err(e) => {
                    log.error("adminapi", format!("failed to bind {bind}: {e}"));
                    return;
                }
            };
            log.info("adminapi", format!("admin API listening on http://{bind}"));
            for request in server.incoming_requests() {
                handle(request, &token, &state, &log);
            }
        })
        .ok();
}

fn handle(mut req: Request, token: &str, state: &AdminState, log: &LogBus) {
    let (path, query) = split_url(req.url());

    // Health is unauthenticated.
    if req.method() == &Method::Get && path == "/health" {
        let _ = req.respond(json_response(200, r#"{"ok":true}"#));
        return;
    }

    if !authorized(&req, token) {
        let _ = req.respond(json_response(401, r#"{"error":"unauthorized"}"#));
        return;
    }

    match (req.method(), path.as_str()) {
        (&Method::Get, "/v1/events") => {
            let since = query_param(&query, "since").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let events = state.events.lock().unwrap().since(since);
            let body = serde_json::json!({
                "events": events.iter().map(|(id, m)| serde_json::json!({"id": id, "message": m})).collect::<Vec<_>>()
            });
            let _ = req.respond(json_response(200, &body.to_string()));
        }
        (&Method::Post, "/v1/command") => {
            let mut body = String::new();
            if req.as_reader().read_to_string(&mut body).is_err() {
                let _ = req.respond(json_response(400, r#"{"error":"unreadable body"}"#));
                return;
            }
            let parsed: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(_) => {
                    let _ = req.respond(json_response(400, r#"{"error":"invalid json"}"#));
                    return;
                }
            };
            let command = parsed.get("command").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            let args = parsed.get("args").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
            if command.is_empty() {
                let _ = req.respond(json_response(400, r#"{"error":"command is required"}"#));
                return;
            }
            log.log(jeeves_abi::Level::Info, jeeves_abi::Category::Command, "adminapi", format!("discord: {command} {args}"));
            let messages = dispatch(state, &command, &args);
            let body = serde_json::json!({ "messages": messages });
            let _ = req.respond(json_response(200, &body.to_string()));
        }
        _ => {
            let _ = req.respond(json_response(404, r#"{"error":"not found"}"#));
        }
    }
}

/// Route a command to bot actions and return human-readable result lines. Pure except for the
/// channel sends it triggers — unit-tested via a fake [`AdminState`].
pub fn dispatch(state: &AdminState, command: &str, args: &str) -> Vec<String> {
    match command.to_lowercase().as_str() {
        "help" => vec![
            "commands: status, modules, reload, refresh, shutdown,".into(),
            "say <server> <target> <message>, join <server> <#chan>, part <server> <#chan>".into(),
            "(<server> may be omitted when only one network is connected)".into(),
        ],
        "status" => {
            let nets = network_list(state);
            let mods = state.modules.lock().unwrap();
            vec![
                format!("networks ({}): {}", nets.len(), join_or_none(&nets)),
                format!("modules ({}): {}", mods.len(), join_or_none(&mods)),
            ]
        }
        "modules" => {
            let mods = state.modules.lock().unwrap();
            vec![format!("modules ({}): {}", mods.len(), join_or_none(&mods))]
        }
        "reload" => {
            let _ = state.control.try_send(Control::Reload);
            vec!["reloading modules…".into()]
        }
        "refresh" => {
            let _ = state.control.try_send(Control::Refresh);
            vec!["reconnecting all networks…".into()]
        }
        "shutdown" | "kill" => {
            let _ = state.control.try_send(Control::Shutdown);
            vec!["shutting down. goodbye.".into()]
        }
        "say" => cmd_say(state, args),
        "join" => cmd_chan(state, args, true),
        "part" => cmd_chan(state, args, false),
        other => vec![format!("unknown command '{other}'. try 'help'.")],
    }
}

fn cmd_say(state: &AdminState, args: &str) -> Vec<String> {
    let nets = network_list(state);
    let Some((server, rest)) = resolve_server(&nets, args) else {
        return vec![format!("specify a network. connected: {}", join_or_none(&nets))];
    };
    let mut it = rest.trim().splitn(2, char::is_whitespace);
    let target = it.next().unwrap_or("").trim();
    let message = it.next().unwrap_or("").trim();
    if target.is_empty() || message.is_empty() {
        return vec!["usage: say <server> <target> <message>".into()];
    }
    match send_action(state, &server, IrcAction::Privmsg { target: target.into(), text: message.into() }) {
        Ok(()) => vec![format!("sent to {target} on {server}.")],
        Err(e) => vec![e],
    }
}

fn cmd_chan(state: &AdminState, args: &str, join: bool) -> Vec<String> {
    let nets = network_list(state);
    let Some((server, rest)) = resolve_server(&nets, args) else {
        return vec![format!("specify a network. connected: {}", join_or_none(&nets))];
    };
    let channel = rest.trim();
    if channel.is_empty() {
        return vec![format!("usage: {} <server> <#channel>", if join { "join" } else { "part" })];
    }
    let action = if join { IrcAction::Join(channel.into()) } else { IrcAction::Part(channel.into()) };
    let verb = if join { "joining" } else { "parting" };
    match send_action(state, &server, action) {
        Ok(()) => vec![format!("{verb} {channel} on {server}.")],
        Err(e) => vec![e],
    }
}

/// Split the leading `<server>` token from `args` when it names a connected network; otherwise, if
/// exactly one network is connected, use it and treat all of `args` as the remainder.
fn resolve_server(networks: &[String], args: &str) -> Option<(String, String)> {
    let mut it = args.trim().splitn(2, char::is_whitespace);
    let first = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("");
    if networks.iter().any(|n| n == first) {
        Some((first.to_string(), rest.to_string()))
    } else if networks.len() == 1 {
        Some((networks[0].clone(), args.trim().to_string()))
    } else {
        None
    }
}

fn send_action(state: &AdminState, server: &str, action: IrcAction) -> Result<(), String> {
    let reg = state.registry.lock().unwrap();
    match reg.get(server) {
        Some(tx) => tx.try_send(action).map_err(|_| format!("send queue full for '{server}'")),
        None => Err(format!("not connected to '{server}'")),
    }
}

fn network_list(state: &AdminState) -> Vec<String> {
    let mut nets: Vec<String> = state.registry.lock().unwrap().keys().cloned().collect();
    nets.sort();
    nets
}

fn join_or_none(items: &[String]) -> String {
    if items.is_empty() { "(none)".into() } else { items.join(", ") }
}

// ---- HTTP helpers ----

fn authorized(req: &Request, token: &str) -> bool {
    let expected = format!("Bearer {token}");
    for h in req.headers() {
        if h.field.as_str().as_str().eq_ignore_ascii_case("authorization") {
            return constant_time_eq(h.value.as_str().as_bytes(), expected.as_bytes());
        }
    }
    false
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn split_url(url: &str) -> (String, String) {
    match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url.to_string(), String::new()),
    }
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

fn json_response(status: u16, body: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    Response::from_string(body).with_status_code(status).with_header(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn state_with(networks: &[&str]) -> (AdminState, HashMap<String, mpsc::Receiver<IrcAction>>, mpsc::Receiver<Control>) {
        let mut registry = HashMap::new();
        let mut receivers = HashMap::new();
        for n in networks {
            let (tx, rx) = mpsc::channel(8);
            registry.insert(n.to_string(), tx);
            receivers.insert(n.to_string(), rx);
        }
        let (ctl_tx, ctl_rx) = mpsc::channel(8);
        let state = AdminState {
            registry: Arc::new(Mutex::new(registry)),
            control: ctl_tx,
            modules: Arc::new(Mutex::new(vec!["admin".into(), "users".into()])),
            events: Arc::new(Mutex::new(EventLog::default())),
        };
        (state, receivers, ctl_rx)
    }

    #[test]
    fn say_enqueues_privmsg_on_named_server() {
        let (state, mut rx, _ctl) = state_with(&["libera", "ergo"]);
        let out = dispatch(&state, "say", "ergo #chan hello there");
        assert!(out[0].contains("sent to #chan on ergo"), "{out:?}");
        match rx.get_mut("ergo").unwrap().try_recv().unwrap() {
            IrcAction::Privmsg { target, text } => {
                assert_eq!(target, "#chan");
                assert_eq!(text, "hello there");
            }
            other => panic!("expected privmsg, got {other:?}"),
        }
    }

    #[test]
    fn say_omits_server_when_single_network() {
        let (state, mut rx, _ctl) = state_with(&["only"]);
        let out = dispatch(&state, "say", "#room hi");
        assert!(out[0].contains("on only"), "{out:?}");
        assert!(matches!(rx.get_mut("only").unwrap().try_recv().unwrap(), IrcAction::Privmsg { .. }));
    }

    #[test]
    fn say_requires_server_when_multiple() {
        let (state, _rx, _ctl) = state_with(&["a", "b"]);
        let out = dispatch(&state, "say", "#room hi");
        assert!(out[0].contains("specify a network"), "{out:?}");
    }

    #[test]
    fn control_commands_signal() {
        let (state, _rx, mut ctl) = state_with(&["x"]);
        dispatch(&state, "reload", "");
        assert!(matches!(ctl.try_recv().unwrap(), Control::Reload));
        dispatch(&state, "shutdown", "");
        assert!(matches!(ctl.try_recv().unwrap(), Control::Shutdown));
    }

    #[test]
    fn status_lists_networks_and_modules() {
        let (state, _rx, _ctl) = state_with(&["libera"]);
        let out = dispatch(&state, "status", "");
        assert!(out.iter().any(|l| l.contains("libera")));
        assert!(out.iter().any(|l| l.contains("admin")));
    }

    #[test]
    fn unknown_command_is_friendly() {
        let (state, _rx, _ctl) = state_with(&["x"]);
        assert!(dispatch(&state, "frobnicate", "")[0].contains("unknown command"));
    }

    #[test]
    fn event_log_since() {
        let mut log = EventLog::default();
        log.push("a".into());
        log.push("b".into());
        let after_first = log.since(1);
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].1, "b");
    }
}
