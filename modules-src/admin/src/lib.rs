//! Admin module for rustjeeves.
//!
//! Parses bot commands from PRIVMSGs and drives privileged host functions. Demonstrates the full
//! module contract: guest exports (`init`, `on_message`) and host-function imports.
//!
//! Commands: `!ping`, `!help [module [command]]`, `!reload`, `!refresh`, `!shutdown`.

use extism_pdk::*;
use jeeves_abi::{
    Category, CommandInfo, CommandManifest, CommandSpec, Event, EventEnvelope, Level, LogReq, Role,
    SendMessage, ThemeReq, COMMAND_MANIFEST_VERSION,
};

#[host_fn]
extern "ExtismHost" {
    fn send_message(input: String) -> String;
    fn log(input: String) -> String;
    fn theme(input: String) -> String;
    fn commands_list(input: String) -> String;
    fn bot_reload(input: String) -> String;
    fn bot_refresh(input: String) -> String;
    fn bot_shutdown(input: String) -> String;
}

#[plugin_fn]
pub fn commands(_: String) -> FnResult<String> {
    let command = |name: &str, description: &str, usage: &str| CommandSpec {
        name: name.into(),
        description: description.into(),
        usage: usage.into(),
        ..Default::default()
    };
    Ok(serde_json::to_string(&CommandManifest {
        version: COMMAND_MANIFEST_VERSION,
        commands: vec![
            command("ping", "Check whether Jeeves is responsive.", "!ping"),
            command(
                "help",
                "List modules, module commands, or command detail.",
                "!help [module [command]]",
            ),
            command("reload", "Reload WASM modules (admin).", "!reload"),
            command("refresh", "Reconnect enabled networks (admin).", "!refresh"),
            command("shutdown", "Shut down Jeeves (super-admin).", "!shutdown"),
        ],
    })?)
}

fn themed(key: &str, defaults: &[&str], vars: &[(&str, &str)]) -> Result<String, Error> {
    let req = ThemeReq {
        key: key.to_string(),
        default: defaults.iter().map(|s| s.to_string()).collect(),
        vars: vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    };
    Ok(unsafe { theme(serde_json::to_string(&req)?)? })
}

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

fn all_commands() -> Result<Vec<CommandInfo>, Error> {
    let raw = unsafe { commands_list(String::new())? };
    Ok(serde_json::from_str(&raw).unwrap_or_default())
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

    let dest = if msg.is_private {
        msg.nick.as_str()
    } else {
        msg.target.as_str()
    };
    let cmd = text.split_whitespace().next().unwrap_or("");

    let required = match cmd {
        "!reload" | "!refresh" => Some(Role::Admin),
        "!shutdown" => Some(Role::SuperAdmin),
        _ => None,
    };
    let who = if msg.display.is_empty() {
        msg.nick.as_str()
    } else {
        msg.display.as_str()
    };
    if let Some(required) = required {
        let allowed = msg.role.is_some_and(|r| r.satisfies(required));
        if !allowed {
            command_log(&format!(
                "[{server}] DENIED {} -> {cmd} (role={:?})",
                msg.nick, msg.role
            ))?;
            reply(
                &server,
                dest,
                &themed(
                    "denied",
                    &["I'm afraid I can't allow that, {user}."],
                    &[("user", who)],
                )?,
            )?;
            return Ok(());
        }
    }

    match cmd {
        "!ping" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(
                &server,
                dest,
                &themed(
                    "pong",
                    &["Pong.", "At your service, {user}.", "Indeed."],
                    &[("user", who)],
                )?,
            )?;
        }
        "!help" => {
            let arg = text["!help".len()..].trim();
            cmd_help(&server, dest, arg)?;
        }
        "!reload" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(
                &server,
                dest,
                &themed("reload", &["Reloading modules, {user}."], &[("user", who)])?,
            )?;
            unsafe { bot_reload(String::new())? };
        }
        "!refresh" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(&server, dest, &themed("refresh", &["Refreshing."], &[])?)?;
            unsafe { bot_refresh(String::new())? };
        }
        "!shutdown" => {
            command_log(&format!("[{server}] {} ran {cmd}", msg.nick))?;
            reply(
                &server,
                dest,
                &themed(
                    "shutdown",
                    &["Very good. Shutting down. Goodbye."],
                    &[("user", who)],
                )?,
            )?;
            unsafe { bot_shutdown(String::new())? };
        }
        _ => {}
    }

    Ok(())
}

fn cmd_help(server: &str, dest: &str, arg: &str) -> Result<(), Error> {
    let all = all_commands()?;

    if arg.is_empty() {
        // Level 1: list all module names.
        let mut modules: Vec<&str> = all.iter().map(|c| c.module.as_str()).collect();
        modules.dedup(); // already sorted by module in the registry
        modules.sort_unstable();
        modules.dedup();
        let list = modules.join(", ");
        let count = modules.len().to_string();
        reply(
            server,
            dest,
            &themed(
                "help.modules",
                &["Modules ({count}): {list} — !help <module> for commands"],
                &[("count", &count), ("list", &list)],
            )?,
        )?;
        return Ok(());
    }

    let mut parts = arg.splitn(2, char::is_whitespace);
    let module_arg = parts.next().unwrap_or("").to_ascii_lowercase();
    let command_arg = parts.next().unwrap_or("").trim().to_ascii_lowercase();

    let module_cmds: Vec<&CommandInfo> = all
        .iter()
        .filter(|c| c.module == module_arg)
        .collect();

    if module_cmds.is_empty() {
        reply(
            server,
            dest,
            &themed(
                "help.unknown_module",
                &["No module named '{module}'. Try !help for the list."],
                &[("module", &module_arg)],
            )?,
        )?;
        return Ok(());
    }

    if command_arg.is_empty() {
        // Level 2: list commands for a module.
        let entries: Vec<String> = module_cmds
            .iter()
            .map(|c| {
                if c.aliases.is_empty() {
                    c.usage.clone()
                } else {
                    format!("{} [{}]", c.usage, c.aliases.iter().map(|a| format!("!{a}")).collect::<Vec<_>>().join(", "))
                }
            })
            .collect();
        let cmds_str = entries.join(" · ");
        reply(
            server,
            dest,
            &themed(
                "help.module",
                &["{module}: {commands}"],
                &[("module", &module_arg), ("commands", &cmds_str)],
            )?,
        )?;
        return Ok(());
    }

    // Level 3: detail for a specific command.
    let info = module_cmds
        .iter()
        .find(|c| c.name == command_arg || c.aliases.iter().any(|a| a == &command_arg));

    match info {
        Some(c) => {
            let aliases = if c.aliases.is_empty() {
                String::new()
            } else {
                format!(" [{}]", c.aliases.iter().map(|a| format!("!{a}")).collect::<Vec<_>>().join(", "))
            };
            let detail = format!("{}{} — {}", c.usage, aliases, c.description);
            reply(
                server,
                dest,
                &themed(
                    "help.command",
                    &["{detail}"],
                    &[("detail", &detail)],
                )?,
            )?;
        }
        None => {
            reply(
                server,
                dest,
                &themed(
                    "help.unknown_command",
                    &["No command '{command}' in module '{module}'. Try !help {module}."],
                    &[("command", &command_arg), ("module", &module_arg)],
                )?,
            )?;
        }
    }

    Ok(())
}
