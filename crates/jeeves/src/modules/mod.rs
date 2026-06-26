//! WASM module host (extism). Loads every `*.wasm` in the modules directory, dispatches IRC
//! events to each module's exported hooks, and exposes host functions modules call for side
//! effects (the "base" capability API).
//!
//! extism plugins are synchronous and not async-friendly, so all plugins live on a dedicated
//! OS thread. IRC events and reload/shutdown signals reach that thread through a std channel fed
//! by a small async forwarder task. Host functions invoked during a plugin call run on that same
//! thread and reach the rest of the system over channels (and the DB actor's blocking API).

mod host_fns;

use crate::action::{Control, IrcAction};
use crate::db::DbHandle;
use crate::log_bus::LogBus;
use crate::theme::ThemeHandle;
use anyhow::Result;
use extism::{Manifest, PluginBuilder, UserData, Wasm, PTR};
use jeeves_abi::{Event, EventEnvelope};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Maps a server label to the live action sender for its IRC actor. Updated by the runtime
/// supervisor on (re)connect/disconnect; read by server-aware host functions.
pub type ServerRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<IrcAction>>>>;

/// Shared context handed to every host-function invocation. `module` is per-plugin (for KV
/// namespacing); the rest are shared clones.
#[derive(Clone)]
pub struct HostCtx {
    pub module: String,
    pub registry: ServerRegistry,
    pub control: mpsc::Sender<Control>,
    pub db: DbHandle,
    pub log: LogBus,
    pub theme: ThemeHandle,
}

/// Runtime -> module-thread control signals.
pub enum ModuleControl {
    Reload,
    /// Explicit shutdown. Normally the thread also stops when its channels close.
    #[allow(dead_code)]
    Shutdown,
}

/// Messages the module thread processes off its std channel.
enum ModMsg {
    Event(EventEnvelope),
    Reload,
    Shutdown,
}

/// Handles returned to the runtime for feeding the module host.
pub struct ModuleHost {
    /// Send IRC events here; they are dispatched to all loaded modules.
    pub events: mpsc::Sender<EventEnvelope>,
    /// Send reload/shutdown signals to the module thread.
    pub control: mpsc::Sender<ModuleControl>,
}

/// Spawn the module host: a forwarder task plus the dedicated plugin thread.
pub fn spawn(
    modules_dir: impl Into<PathBuf>,
    registry: ServerRegistry,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
    theme: ThemeHandle,
) -> ModuleHost {
    let modules_dir = modules_dir.into();
    let (events_tx, mut events_rx) = mpsc::channel::<EventEnvelope>(256);
    let (modctl_tx, mut modctl_rx) = mpsc::channel::<ModuleControl>(16);

    // Bridge async channels -> a single std channel the blocking thread drains.
    let (std_tx, std_rx) = std::sync::mpsc::channel::<ModMsg>();
    let watch_tx = std_tx.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = events_rx.recv() => match ev {
                    Some(ev) => { if std_tx.send(ModMsg::Event(ev)).is_err() { break; } }
                    None => break,
                },
                ctl = modctl_rx.recv() => match ctl {
                    Some(ModuleControl::Reload) => { let _ = std_tx.send(ModMsg::Reload); }
                    Some(ModuleControl::Shutdown) | None => { let _ = std_tx.send(ModMsg::Shutdown); break; }
                },
            }
        }
    });

    // Auto-reload: watch the modules directory and feed debounced Reload signals into the same
    // channel the plugin thread drains. Best-effort — if watching fails, !reload still works.
    spawn_watcher(&modules_dir, watch_tx, &log);

    let base = ModuleBase { registry, control, db, log, theme };
    std::thread::Builder::new()
        .name("jeeves-modules".into())
        .spawn(move || module_thread(modules_dir, base, std_rx))
        .expect("spawn module thread");

    ModuleHost { events: events_tx, control: modctl_tx }
}

/// Watch `dir` for `*.wasm` changes and send a debounced [`ModMsg::Reload`] on activity.
fn spawn_watcher(dir: &Path, reload_tx: std::sync::mpsc::Sender<ModMsg>, log: &LogBus) {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration;

    let (raw_tx, raw_rx) = channel::<()>();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if matches!(ev.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_)) {
                let _ = raw_tx.send(());
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            log.error("modules", format!("module watcher unavailable: {e}"));
            return;
        }
    };
    if let Err(e) = watcher.watch(dir, RecursiveMode::NonRecursive) {
        log.info("modules", format!("not watching {} for changes ({e})", dir.display()));
        return;
    }

    let log = log.clone();
    std::thread::Builder::new()
        .name("jeeves-mod-watch".into())
        .spawn(move || {
            let _watcher = watcher; // keep the watcher alive for the thread's lifetime
            loop {
                // Block for the first change, then coalesce a burst (e.g. a multi-write copy).
                if raw_rx.recv().is_err() {
                    break;
                }
                loop {
                    match raw_rx.recv_timeout(Duration::from_millis(500)) {
                        Ok(()) => continue,
                        Err(RecvTimeoutError::Timeout) => break,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                log.info("modules", "module directory changed — auto-reloading");
                if reload_tx.send(ModMsg::Reload).is_err() {
                    break;
                }
            }
        })
        .ok();
}

/// The shared, plugin-independent half of [`HostCtx`].
#[derive(Clone)]
struct ModuleBase {
    registry: ServerRegistry,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
    theme: ThemeHandle,
}

struct Loaded {
    name: String,
    plugin: extism::Plugin,
}

fn module_thread(dir: PathBuf, base: ModuleBase, rx: std::sync::mpsc::Receiver<ModMsg>) {
    let mut plugins = load_all(&dir, &base);
    base.log.info("modules", format!("loaded {} module(s)", plugins.len()));

    while let Ok(msg) = rx.recv() {
        match msg {
            ModMsg::Event(env) => dispatch(&mut plugins, &base, &env),
            ModMsg::Reload => {
                base.log.info("modules", "reloading modules");
                plugins = load_all(&dir, &base);
                base.log.info("modules", format!("reloaded {} module(s)", plugins.len()));
            }
            ModMsg::Shutdown => break,
        }
    }
}

/// (Re)load every `*.wasm` in `dir`.
fn load_all(dir: &Path, base: &ModuleBase) -> Vec<Loaded> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            base.log.info("modules", format!("no modules directory at {}", dir.display()));
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wasm") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("module").to_string();
        match load_one(&path, &name, base) {
            Ok(mut loaded) => {
                if loaded.plugin.function_exists("init") {
                    if let Err(e) = loaded.plugin.call::<&str, &str>("init", "") {
                        base.log.error("modules", format!("{name}: init failed: {e}"));
                    }
                }
                base.log.info("modules", format!("loaded module '{name}'"));
                out.push(loaded);
            }
            Err(e) => base.log.error("modules", format!("failed to load {}: {e}", path.display())),
        }
    }
    out
}

fn load_one(path: &Path, name: &str, base: &ModuleBase) -> Result<Loaded> {
    let manifest = Manifest::new([Wasm::file(path)]);
    let ud = UserData::new(HostCtx {
        module: name.to_string(),
        registry: base.registry.clone(),
        control: base.control.clone(),
        db: base.db.clone(),
        log: base.log.clone(),
        theme: base.theme.clone(),
    });

    let plugin = PluginBuilder::new(manifest)
        .with_wasi(true)
        .with_function("send_message", [PTR], [PTR], ud.clone(), host_fns::send_message)
        .with_function("send_notice", [PTR], [PTR], ud.clone(), host_fns::send_notice)
        .with_function("join", [PTR], [PTR], ud.clone(), host_fns::join)
        .with_function("part", [PTR], [PTR], ud.clone(), host_fns::part)
        .with_function("kv_get", [PTR], [PTR], ud.clone(), host_fns::kv_get)
        .with_function("kv_set", [PTR], [PTR], ud.clone(), host_fns::kv_set)
        .with_function("log", [PTR], [PTR], ud.clone(), host_fns::log)
        .with_function("theme", [PTR], [PTR], ud.clone(), host_fns::theme)
        .with_function("profile_ensure", [PTR], [PTR], ud.clone(), host_fns::profile_ensure)
        .with_function("profile_get", [PTR], [PTR], ud.clone(), host_fns::profile_get)
        .with_function("profile_set", [PTR], [PTR], ud.clone(), host_fns::profile_set)
        .with_function("profile_clear", [PTR], [PTR], ud.clone(), host_fns::profile_clear)
        .with_function("geocode", [PTR], [PTR], ud.clone(), host_fns::geocode)
        .with_function("weather", [PTR], [PTR], ud.clone(), host_fns::weather)
        .with_function("bot_reload", [PTR], [PTR], ud.clone(), host_fns::bot_reload)
        .with_function("bot_refresh", [PTR], [PTR], ud.clone(), host_fns::bot_refresh)
        .with_function("bot_shutdown", [PTR], [PTR], ud.clone(), host_fns::bot_shutdown)
        .build()?;

    Ok(Loaded { name: name.to_string(), plugin })
}

/// Dispatch one event envelope to every module that exports the relevant hook.
fn dispatch(plugins: &mut [Loaded], base: &ModuleBase, env: &EventEnvelope) {
    let hook = match env.event {
        Event::Message(_) => "on_message",
        _ => "on_event",
    };
    let payload = match serde_json::to_string(env) {
        Ok(p) => p,
        Err(e) => {
            base.log.error("modules", format!("event serialize failed: {e}"));
            return;
        }
    };
    for m in plugins.iter_mut() {
        if m.plugin.function_exists(hook) {
            if let Err(e) = m.plugin.call::<&str, &str>(hook, &payload) {
                base.log.error("modules", format!("{}: {hook} failed: {e}", m.name));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::MessagePayload;
    use std::time::Duration;

    fn envelope(server: &str, text: &str, is_private: bool) -> EventEnvelope {
        EventEnvelope {
            server: server.into(),
            event: Event::Message(MessagePayload {
                nick: "tester".into(),
                user: "u".into(),
                host: "h".into(),
                target: if is_private { "jeeves".into() } else { "#chan".into() },
                text: text.into(),
                is_private,
                tags: Vec::new(),
                role: Some(jeeves_abi::Role::SuperAdmin),
            }),
        }
    }

    /// Load the real admin.wasm and confirm commands drive host functions on the right server:
    /// `!ping` produces a reply action on that network, `!shutdown` produces a reply plus a
    /// Control::Shutdown.
    #[tokio::test]
    async fn admin_commands_drive_host_functions() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../modules");
        if !std::path::Path::new(dir).join("admin.wasm").exists() {
            eprintln!("skipping: modules/admin.wasm not built");
            return;
        }

        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(64);
        let (actions_tx, mut actions_rx) = mpsc::channel::<IrcAction>(16);
        let (control_tx, mut control_rx) = mpsc::channel::<Control>(16);
        let registry: ServerRegistry = Arc::new(Mutex::new(HashMap::new()));
        registry.lock().unwrap().insert("testnet".to_string(), actions_tx);
        let theme = crate::theme::ThemeStore::open(
            std::env::temp_dir().join(format!("jeeves-modtest-theme-{}.toml", std::process::id())),
        );
        let host = spawn(dir, registry, control_tx, db, log, theme);

        // !ping -> reply "pong" to the channel on the originating network.
        host.events.send(envelope("testnet", "!ping", false)).await.unwrap();
        let act = tokio::time::timeout(Duration::from_secs(5), actions_rx.recv())
            .await
            .expect("timed out waiting for ping reply")
            .unwrap();
        match act {
            IrcAction::Privmsg { target, text } => {
                assert_eq!(target, "#chan");
                // Reply text now comes from the theme (a random default variant) — just confirm
                // a non-empty themed string was produced on the right network.
                assert!(!text.is_empty(), "expected a themed pong reply");
            }
            other => panic!("expected pong privmsg, got {other:?}"),
        }

        // !shutdown -> reply, then a Control::Shutdown.
        host.events.send(envelope("testnet", "!shutdown", true)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), actions_rx.recv())
            .await
            .expect("timed out waiting for shutdown reply")
            .unwrap();
        assert!(matches!(reply, IrcAction::Privmsg { .. }));

        let ctl = tokio::time::timeout(Duration::from_secs(5), control_rx.recv())
            .await
            .expect("timed out waiting for shutdown control")
            .unwrap();
        assert!(matches!(ctl, Control::Shutdown), "expected Shutdown, got {ctl:?}");
    }
}
