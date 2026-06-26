//! rustjeeves — an IRCv3 bot framework. Binary entrypoint.

mod action;
mod config;
mod db;
mod irc;
mod log_bus;
mod modules;
mod runtime;
mod tui;

use anyhow::Result;
use clap::Parser;
use db::DbHandle;
use log_bus::LogBus;

#[derive(Parser, Debug)]
#[command(name = "jeeves", version, about = "An IRCv3 bot framework in Rust")]
struct Cli {
    /// Run without a TUI (logs to stdout). Mutually exclusive with --interactive.
    #[arg(long)]
    headless: bool,

    /// Launch the interactive TUI (default).
    #[arg(long)]
    interactive: bool,

    /// Path to the SQLite database file.
    #[arg(long, default_value = "bot.db")]
    db: String,

    /// Directory scanned for `*.wasm` modules.
    #[arg(long, default_value = "modules")]
    modules: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let interactive = !cli.headless;

    let db = DbHandle::open(&cli.db)?;
    let log = LogBus::new(1024);

    if interactive {
        runtime::run_interactive(db, log, &cli.modules).await
    } else {
        runtime::run_headless(db, log, &cli.modules).await
    }
}
