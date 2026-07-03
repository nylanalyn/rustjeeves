//! rustjeeves — an IRCv3 bot framework. Binary entrypoint.

mod action;
mod adminapi;
mod ai;
mod backup;
mod casemapping;
mod commands;
mod config;
mod data_lifecycle;
mod db;
mod deepl;
mod dictionary;
mod geo;
mod irc;
mod local_time;
mod log_bus;
mod modules;
mod perms;
mod runtime;
mod scheduler;
mod search;
mod settings;
mod theme;
mod tui;
mod weather;
mod youtube;

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

    /// Export host-owned profile data as JSON and exit, using SERVER:NICK.
    #[arg(long, value_name = "SERVER:NICK")]
    export_profile: Option<String>,

    /// Directory used by --export-profile (created with private permissions).
    #[arg(long, default_value = "data-exports")]
    export_dir: String,

    /// Decrypt a client-encrypted .rjb backup and exit. Reads the key from
    /// RUSTJEEVES_BACKUP_ENCRYPTION_KEY.
    #[arg(long, value_name = "FILE", requires = "decrypt_output")]
    decrypt_backup: Option<String>,

    /// Destination for --decrypt-backup. It must not already exist.
    #[arg(long, value_name = "FILE")]
    decrypt_output: Option<String>,

    /// Open, migrate, and integrity-check a SQLite backup, then exit.
    #[arg(long, value_name = "FILE", conflicts_with = "decrypt_backup")]
    verify_backup: Option<String>,

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

    /// Start the TUI without connecting to IRC. Edit settings, then press Ctrl-R to connect.
    /// Implies --interactive.
    #[arg(long, conflicts_with = "headless")]
    no_connect: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let interactive = cli.interactive || cli.no_connect || !cli.headless;

    if let Some(input) = &cli.decrypt_backup {
        let output = cli.decrypt_output.as_deref().expect("clap requires output");
        let key = std::env::var("RUSTJEEVES_BACKUP_ENCRYPTION_KEY").map_err(|_| {
            anyhow::anyhow!("RUSTJEEVES_BACKUP_ENCRYPTION_KEY is required to decrypt a backup")
        })?;
        backup::decrypt_file(
            std::path::Path::new(input),
            std::path::Path::new(output),
            &key,
        )?;
        let verification = db::verify_backup_file(std::path::Path::new(output))?;
        println!(
            "restored {} (schema {}, integrity {})",
            output, verification.schema_version, verification.integrity_check
        );
        return Ok(());
    }
    if let Some(path) = &cli.verify_backup {
        let verification = db::verify_backup_file(std::path::Path::new(path))?;
        println!(
            "verified {} (schema {}, integrity {})",
            path, verification.schema_version, verification.integrity_check
        );
        return Ok(());
    }

    let export_subject = cli
        .export_profile
        .as_deref()
        .map(|subject| {
            subject
                .split_once(':')
                .filter(|(server, nick)| !server.is_empty() && !nick.is_empty())
                .ok_or_else(|| anyhow::anyhow!("--export-profile must be SERVER:NICK"))
        })
        .transpose()?;

    let db = DbHandle::open(&cli.db)?;

    if let Some((server, nick)) = export_subject {
        let path = data_lifecycle::export_profile(
            &db,
            server,
            nick,
            std::path::Path::new(&cli.export_dir),
        )
        .await?;
        println!("{}", path.display());
        return Ok(());
    }

    let log = LogBus::new(1024);
    let paths = runtime::RuntimePaths {
        modules: &cli.modules,
        theme: &cli.theme,
        capabilities: &cli.module_capabilities,
        exports: &cli.export_dir,
    };

    // The admin API is enabled only when a token is provided (flag or env).
    let admin_token = cli
        .admin_token
        .or_else(|| std::env::var("RUSTJEEVES_ADMIN_TOKEN").ok());
    let admin = admin_token.map(|t| (cli.admin_bind.clone(), t));

    if interactive {
        runtime::run_interactive(db, log, paths, admin, cli.no_connect).await
    } else {
        runtime::run_headless(db, log, paths, admin).await
    }
}
