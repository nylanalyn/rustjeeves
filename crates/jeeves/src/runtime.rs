//! Runtime supervisors for the two modes. Wires together the DB, log bus, IRC actor, module host,
//! and (in interactive mode) the TUI.

use crate::action::{AppRequest, Control, IrcAction};
use crate::config::ServerConfig;
use crate::db::DbHandle;
use crate::irc;
use crate::log_bus::LogBus;
use crate::modules::{self, ModuleControl, ModuleHost};
use crate::tui;
use anyhow::Result;
use jeeves_abi::{Category, Event, Level};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the IRC actor for `cfg`, returning its task handle and an action sender.
fn spawn_irc(
    cfg: ServerConfig,
    log: LogBus,
    events: mpsc::Sender<Event>,
) -> (JoinHandle<()>, mpsc::Sender<IrcAction>) {
    let (action_tx, action_rx) = mpsc::channel(64);
    let handle = tokio::spawn(async move {
        if let Err(e) = irc::run(cfg, log.clone(), action_rx, events).await {
            log.error("irc", format!("connection ended: {e}"));
        }
    });
    (handle, action_tx)
}

/// A stable [`IrcAction`] inlet that forwards to whichever IRC actor is currently connected.
/// Returns the durable sender and a way to point it at a new actor on (re)connect.
fn spawn_action_relay() -> (mpsc::Sender<IrcAction>, mpsc::Sender<mpsc::Sender<IrcAction>>) {
    let (relay_tx, mut relay_rx) = mpsc::channel::<IrcAction>(64);
    let (curr_tx, mut curr_rx) = mpsc::channel::<mpsc::Sender<IrcAction>>(4);
    tokio::spawn(async move {
        let mut current: Option<mpsc::Sender<IrcAction>> = None;
        loop {
            tokio::select! {
                a = relay_rx.recv() => match a {
                    Some(a) => { if let Some(s) = &current { let _ = s.try_send(a); } }
                    None => break,
                },
                s = curr_rx.recv() => match s {
                    Some(s) => current = Some(s),
                    None => break,
                },
            }
        }
    });
    (relay_tx, curr_tx)
}

/// Subscribe to the log bus and persist every event to the DB.
fn spawn_db_sink(log: &LogBus, db: DbHandle) {
    let mut rx = log.subscribe();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            let _ = db.append_log(ev).await;
        }
    });
}

/// Shared wiring used by both modes.
struct Core {
    db: DbHandle,
    log: LogBus,
    modhost: ModuleHost,
    control_rx: mpsc::Receiver<Control>,
    curr_tx: mpsc::Sender<mpsc::Sender<IrcAction>>,
    current: Option<JoinHandle<()>>,
}

impl Core {
    fn new(db: DbHandle, log: LogBus, modules_dir: &str) -> Self {
        spawn_db_sink(&log, db.clone());
        let (relay_tx, curr_tx) = spawn_action_relay();
        let (control_tx, control_rx) = mpsc::channel::<Control>(32);
        let modhost = modules::spawn(modules_dir, relay_tx, control_tx, db.clone(), log.clone());
        Core { db, log, modhost, control_rx, curr_tx, current: None }
    }

    /// (Re)connect using the latest saved config; routes module actions to the new actor.
    async fn connect(&mut self) {
        if let Some(h) = self.current.take() {
            h.abort();
        }
        match self.db.load_server().await {
            Ok(cfg) if cfg.host.is_empty() => {
                self.log.info("main", "no server configured — set one in Settings, then connect")
            }
            Ok(cfg) => {
                let (handle, action_tx) = spawn_irc(cfg, self.log.clone(), self.modhost.events.clone());
                let _ = self.curr_tx.send(action_tx).await;
                self.current = Some(handle);
            }
            Err(e) => self.log.error("main", format!("config load failed: {e}")),
        }
    }
}

/// Headless: connect and run, logging to stdout and the DB until ctrl-c, disconnect, or a module
/// requests shutdown.
pub async fn run_headless(db: DbHandle, log: LogBus, modules_dir: &str) -> Result<()> {
    // Stdout sink.
    {
        let mut rx = log.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                println!(
                    "[{}] {:<7} {:<8} {}: {}",
                    ev.ts,
                    level_str(ev.level),
                    cat_str(ev.category),
                    ev.source,
                    ev.message
                );
            }
        });
    }

    let mut core = Core::new(db, log.clone(), modules_dir);
    core.connect().await;

    loop {
        tokio::select! {
            ctl = core.control_rx.recv() => {
                match ctl {
                    Some(Control::Reload) => { let _ = core.modhost.control.send(ModuleControl::Reload).await; }
                    Some(Control::Refresh) => core.connect().await,
                    Some(Control::Shutdown) | None => {
                        log.info("main", "shutting down (module request)");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                log.info("main", "shutting down (ctrl-c)");
                break;
            }
        }
    }
    Ok(())
}

/// Interactive: launch the TUI and supervise the IRC connection + modules in the background.
pub async fn run_interactive(db: DbHandle, log: LogBus, modules_dir: &str) -> Result<()> {
    // Bridge log bus -> TUI (std channel the blocking TUI thread can drain).
    let (tui_log_tx, tui_log_rx) = std::sync::mpsc::channel();
    {
        let mut rx = log.subscribe();
        tokio::spawn(async move {
            while let Ok(ev) = rx.recv().await {
                if tui_log_tx.send(ev).is_err() {
                    break;
                }
            }
        });
    }

    let initial = db.load_server().await?;
    let (app_tx, mut app_rx) = mpsc::channel::<AppRequest>(32);

    let tui_handle = {
        let app_tx = app_tx.clone();
        let initial = initial.clone();
        tokio::task::spawn_blocking(move || tui::run(initial, tui_log_rx, app_tx))
    };

    let mut core = Core::new(db.clone(), log.clone(), modules_dir);
    if !initial.host.is_empty() {
        core.connect().await;
    } else {
        log.info("main", "no server configured yet — set one in Settings and press Ctrl-R");
    }

    loop {
        tokio::select! {
            req = app_rx.recv() => match req {
                Some(AppRequest::SaveConfig(cfg)) => match db.save_server(cfg).await {
                    Ok(()) => log.info("tui", "configuration saved"),
                    Err(e) => log.error("tui", format!("save failed: {e}")),
                },
                Some(AppRequest::Reconnect) => core.connect().await,
                Some(AppRequest::Shutdown) | None => break,
            },
            ctl = core.control_rx.recv() => match ctl {
                Some(Control::Reload) => { let _ = core.modhost.control.send(ModuleControl::Reload).await; }
                Some(Control::Refresh) => core.connect().await,
                Some(Control::Shutdown) => break,
                None => {}
            },
        }
    }

    let _ = tui_handle.await;
    Ok(())
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Error => "ERROR",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    }
}

fn cat_str(c: Category) -> &'static str {
    match c {
        Category::Error => "ERROR",
        Category::Debug => "DEBUG",
        Category::Message => "MESSAGE",
        Category::Command => "COMMAND",
    }
}
