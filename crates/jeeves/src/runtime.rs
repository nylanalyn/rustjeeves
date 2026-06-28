//! Runtime supervisors for the two modes. Wires together the DB, log bus, IRC actors (one per
//! network), module host, and (in interactive mode) the TUI.

use crate::action::{AppRequest, Control, IrcAction};
use crate::adminapi::{self, AdminState, EventLog};
use crate::config::ServerConfig;
use crate::db::DbHandle;
use crate::irc;
use crate::log_bus::LogBus;
use crate::modules::{self, ModuleControl, ModuleHost, ServerRegistry};
use crate::perms;
use crate::theme::ThemeStore;
use crate::tui;
use anyhow::Result;
use jeeves_abi::{Category, EventEnvelope, Level};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Spawn the IRC actor for `cfg`, returning its task handle and an action sender.
fn spawn_irc(
    cfg: ServerConfig,
    log: LogBus,
    events: mpsc::Sender<EventEnvelope>,
) -> (JoinHandle<()>, mpsc::Sender<IrcAction>) {
    let (action_tx, action_rx) = mpsc::channel(64);
    let label = cfg.label.clone();
    let handle = tokio::spawn(async move {
        let mut action_rx = action_rx;
        let mut delay = Duration::from_secs(1);
        loop {
            let started = std::time::Instant::now();
            match irc::run(cfg.clone(), log.clone(), &mut action_rx, events.clone()).await {
                Ok(irc::RunExit::StopRequested) => break,
                Ok(irc::RunExit::Disconnected) => {}
                Err(e) => log.error("irc", format!("[{label}] connection ended: {e}")),
            }
            let _ = events
                .send(EventEnvelope {
                    server: label.clone(),
                    event: jeeves_abi::Event::Disconnected,
                })
                .await;
            if started.elapsed() >= Duration::from_secs(60) {
                delay = Duration::from_secs(1);
            }
            log.info(
                "irc",
                format!("[{label}] reconnecting in {}s", delay.as_secs()),
            );
            let deadline = tokio::time::Instant::now() + delay;
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    action = action_rx.recv() => match action {
                        Some(IrcAction::Quit(_)) | None => return,
                        Some(action) => log.debug(
                            "irc",
                            format!("[{label}] dropped action while disconnected: {action:?}"),
                        ),
                    },
                }
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    });
    (handle, action_tx)
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
    control_tx: mpsc::Sender<Control>,
    registry: ServerRegistry,
    /// Inlet for IRC events: the permission resolver, which enriches messages with the sender's
    /// role and forwards to the module host.
    events_in: mpsc::Sender<EventEnvelope>,
    handles: HashMap<String, JoinHandle<()>>,
}

impl Core {
    fn new(
        db: DbHandle,
        log: LogBus,
        modules_dir: &str,
        theme_path: &str,
        capabilities_path: &str,
    ) -> Self {
        spawn_db_sink(&log, db.clone());
        let registry: ServerRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (control_tx, control_rx) = mpsc::channel::<Control>(32);
        let theme = ThemeStore::open(theme_path);
        let modhost = modules::spawn(
            modules_dir,
            registry.clone(),
            control_tx.clone(),
            db.clone(),
            log.clone(),
            theme,
            capabilities_path,
        );
        let events_in = perms::spawn(db.clone(), log.clone(), modhost.events.clone());
        Core {
            db,
            log,
            modhost,
            control_rx,
            control_tx,
            registry,
            events_in,
            handles: HashMap::new(),
        }
    }

    /// Start the Discord/admin HTTP API if a token was configured. Surfaces ERROR + COMMAND log
    /// events to the API's `/v1/events` buffer.
    fn start_admin(&self, admin: Option<(String, String)>) {
        let Some((bind, token)) = admin else {
            return;
        };
        let events = Arc::new(Mutex::new(EventLog::default()));
        {
            let events = events.clone();
            let mut rx = self.log.subscribe();
            tokio::spawn(async move {
                while let Ok(ev) = rx.recv().await {
                    if matches!(ev.level, Level::Error) || matches!(ev.category, Category::Command)
                    {
                        events
                            .lock()
                            .unwrap()
                            .push(format!("[{}] {}", ev.source, ev.message));
                    }
                }
            });
        }
        let state = AdminState {
            registry: self.registry.clone(),
            control: self.control_tx.clone(),
            modules: self.modhost.names.clone(),
            events,
        };
        adminapi::serve(bind, token, state, self.log.clone());
    }

    /// (Re)connect every enabled server profile. Rebuilds the action registry so module sends
    /// reach the live actors.
    async fn connect_all(&mut self) {
        let servers = match self.db.load_servers().await {
            Ok(s) => s,
            Err(e) => {
                self.log.error("main", format!("config load failed: {e}"));
                return;
            }
        };
        let enabled: Vec<ServerConfig> = servers
            .into_iter()
            .filter(|s| s.enabled && !s.host.is_empty())
            .collect();
        if enabled.is_empty() {
            self.log.info(
                "main",
                "no enabled servers configured — add one in Settings",
            );
            return;
        }
        for cfg in enabled {
            let label = cfg.label.clone();
            let (handle, action_tx) = spawn_irc(cfg, self.log.clone(), self.events_in.clone());
            self.registry
                .lock()
                .unwrap()
                .insert(label.clone(), action_tx);
            self.handles.insert(label, handle);
        }
    }

    async fn reconnect_all(&mut self) {
        self.quit_all().await;
        self.connect_all().await;
    }

    /// Gracefully stop all IRC actors: send QUIT to each, then wait for the connection to close
    /// (flushing the QUIT), aborting any that linger past a short grace period.
    async fn quit_all(&mut self) {
        {
            let reg = self.registry.lock().unwrap();
            for tx in reg.values() {
                let _ = tx.try_send(IrcAction::Quit(Some("rustjeeves shutting down".into())));
            }
        } // drop the std Mutex guard before awaiting

        for (label, mut handle) in self.handles.drain() {
            if tokio::time::timeout(Duration::from_millis(2000), &mut handle)
                .await
                .is_err()
            {
                self.log
                    .debug("main", format!("[{label}] did not QUIT in time; aborting"));
                handle.abort();
            }
        }
        self.registry.lock().unwrap().clear();
    }
}

/// Headless: connect and run, logging to stdout and the DB until ctrl-c, disconnect, or a module
/// requests shutdown.
pub async fn run_headless(
    db: DbHandle,
    log: LogBus,
    modules_dir: &str,
    theme_path: &str,
    capabilities_path: &str,
    admin: Option<(String, String)>,
) -> Result<()> {
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

    let mut core = Core::new(db, log.clone(), modules_dir, theme_path, capabilities_path);
    core.start_admin(admin);
    core.connect_all().await;

    loop {
        tokio::select! {
            ctl = core.control_rx.recv() => {
                match ctl {
                    Some(Control::Reload) => { let _ = core.modhost.control.send(ModuleControl::Reload).await; }
                    Some(Control::Refresh) => core.reconnect_all().await,
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
    core.quit_all().await;
    Ok(())
}

/// Interactive: launch the TUI and supervise the IRC connections + modules in the background.
pub async fn run_interactive(
    db: DbHandle,
    log: LogBus,
    modules_dir: &str,
    theme_path: &str,
    capabilities_path: &str,
    admin: Option<(String, String)>,
) -> Result<()> {
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

    let (app_tx, mut app_rx) = mpsc::channel::<AppRequest>(32);

    let mut core = Core::new(
        db.clone(),
        log.clone(),
        modules_dir,
        theme_path,
        capabilities_path,
    );

    let tui_handle = {
        let app_tx = app_tx.clone();
        let db = db.clone();
        let commands = core.modhost.commands.clone();
        let settings = core.modhost.settings.clone();
        tokio::task::spawn_blocking(move || tui::run(db, tui_log_rx, app_tx, commands, settings))
    };
    core.start_admin(admin);
    core.connect_all().await;

    loop {
        tokio::select! {
            req = app_rx.recv() => match req {
                Some(AppRequest::Reconnect) => core.reconnect_all().await,
                Some(AppRequest::Shutdown) | None => break,
            },
            ctl = core.control_rx.recv() => match ctl {
                Some(Control::Reload) => { let _ = core.modhost.control.send(ModuleControl::Reload).await; }
                Some(Control::Refresh) => core.reconnect_all().await,
                Some(Control::Shutdown) => break,
                None => {}
            },
        }
    }

    core.quit_all().await;
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
