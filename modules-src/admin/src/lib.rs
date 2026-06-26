//! Admin module for rustjeeves.
//!
//! Parses bot commands from PRIVMSGs and drives privileged host functions. Demonstrates the full
//! module contract: guest exports (`init`, `on_message`) and host-function imports.
//!
//! Commands: `!ping`, `!help`, `!reload`, `!refresh`, `!shutdown`.

use extism_pdk::*;
use jeeves_abi::{Category, Event, EventEnvelope, Level, LogReq, Role, SendMessage};

// Host functions provided by jeeves (the "base" capability API). Default namespace
// "extism:host/user" matches what the host registers.
#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn log(input: String) -> String;
    fn bot_reload(input: String) -> String;
    fn bot_refresh(input: String) -> String;
    fn bot_shutdown(input: String) -> String;
}

/// Emit a COMMAND-category log line through the host (this is what lights up the COMMAND filter in
/// the TUI logs screen).
fn command_log(message: &str) -> Result<(), Error> {
    let req = LogReq {
        level: Level::Info,
        category: Category::Command,
        message: message.to_string(),
    };
    unsafe { log(serde_json::to_string(&req)?)? };
    Ok(())
}

fn reply(server: &str, target: &str, text: &str) -> Result<(), Error> {
    let req = SendMessage {
        server: server.to_string(),
        target: target.to_string(),
        text: text.to_string(),
    };
    unsafe { send_message(serde_json::to_string(&req)?)? };
    Ok(())
}

#[plugin_fn]
pub fn init() -> FnResult<()> {
    command_log("admin module loaded (commands: !ping !help !reload !refresh !shutdown)")?;
    Ok(())
}

#[plugin_fn]
pub fn on_message(input: String) -> FnResult<()> {
    let env: EventEnvelope = serde_json::from_str(&input)?;
    let server = env.server;
    let Event::Message(msg) = env.event else {
        return Ok(());
    };

    let text = msg.text.trim();
    if !text.starts_with('!') {
        return Ok(());
    }

    // Where to reply: the nick for a PM, otherwise the channel.
    let dest = if msg.is_private { msg.nick.as_str() } else { msg.target.as_str() };
    let cmd = text.split_whitespace().next().unwrap_or("");

    // Required role per command (None = open to everyone).
    let required = match cmd {
        "!reload" | "!refresh" => Some(Role::Admin),
        "!shutdown" => Some(Role::SuperAdmin),
        _ => None,
    };
    if let Some(required) = required {
        let allowed = msg.role.is_some_and(|r| r.satisfies(required));
        if !allowed {
            command_log(&format!("[{server}] DENIED {} -> {cmd} (role={:?})", msg.nick, msg.role))?;
            reply(&server, dest, "permission denied")?;
            return Ok(());
        }
    }

    match cmd {
        "!ping" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(&server, dest, "pong")?;
        }
        "!help" => {
            reply(&server, dest, "commands: !ping !help !reload(admin) !refresh(admin) !shutdown(superadmin)")?;
        }
        "!reload" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(&server, dest, "reloading modules…")?;
            unsafe { bot_reload(String::new())? };
        }
        "!refresh" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(&server, dest, "refreshing…")?;
            unsafe { bot_refresh(String::new())? };
        }
        "!shutdown" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(&server, dest, "shutting down. goodbye.")?;
            unsafe { bot_shutdown(String::new())? };
        }
        _ => {}
    }

    Ok(())
}
