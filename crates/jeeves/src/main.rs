//! rustjeeves — an IRCv3 bot framework. Binary entrypoint.

mod action;
mod adminapi;
mod config;
mod db;
mod deepl;
mod geo;
mod irc;
mod log_bus;
mod modules;
mod perms;
mod runtime;
mod search;
mod theme;
mod tui;
mod weather;

use anyhow::Result;
use clap::Parser;
use db::DbHandle;
use log_bus::LogBus;

#[derive(Parser, Debug)]
#[command(name = "jeeves", version, about = "An IRCv3 bot framework in Rust")]
struct Cli {
    /// Run without a TUI (logs to stdout). Mutually exclusive with --interactive.
    #[arg(long, conflicts_with = "interactive")]
    headless: bool,

    /// Launch the interactive TUI (default).
    #[arg(long, conflicts_with = "headless")]
    interactive: bool,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "bot.db")]
    db: String,

    /// Directory scanned for `*.wasm` modules.
    #[arg(long, default_value = "modules")]
    modules: String,

    /// Path to the themable strings file (created with defaults on first use).
    #[arg(long, default_value = "theme.toml")]
    theme: String,

    /// Operator-owned per-module host capability policy.
    #[arg(long, default_value = "module-capabilities.toml")]
    module_capabilities: String,

    /// Address for the Discord/admin HTTP API (localhost recommended).
    #[arg(long, default_value = "127.0.0.1:9110")]
    admin_bind: String,

    /// Bearer token enabling the admin HTTP API. If unset, falls back to RUSTJEEVES_ADMIN_TOKEN;
    /// if still unset, the admin API stays disabled.
    #[arg(long)]
    admin_token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let interactive = cli.interactive || !cli.headless;

    let db = DbHandle::open(&cli.db)?;
    let log = LogBus::new(1024);

    // The admin API is enabled only when a token is provided (flag or env).
    let admin_token = cli
        .admin_token
        .or_else(|| std::env::var("RUSTJEEVES_ADMIN_TOKEN").ok());
    let admin = admin_token.map(|t| (cli.admin_bind.clone(), t));

    if interactive {
        runtime::run_interactive(
            db,
            log,
            &cli.modules,
            &cli.theme,
            &cli.module_capabilities,
            admin,
        )
        .await
    } else {
        runtime::run_headless(
            db,
            log,
            &cli.modules,
            &cli.theme,
            &cli.module_capabilities,
            admin,
        )
        .await
    }
}
