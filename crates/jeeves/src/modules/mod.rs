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
use anyhow::Result;
use extism::{Manifest, PluginBuilder, UserData, Wasm, PTR};
use jeeves_abi::Event;
use std::path::{Path, PathBuf};
use tokio::sync::mpsc;

/// Shared context handed to every host-function invocation. `module` is per-plugin (for KV
/// namespacing); the rest are shared clones.
#[derive(Clone)]
pub struct HostCtx {
    pub module: String,
    pub actions: mpsc::Sender<IrcAction>,
    pub control: mpsc::Sender<Control>,
    pub db: DbHandle,
    pub log: LogBus,
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
    Event(Event),
    Reload,
    Shutdown,
}

/// Handles returned to the runtime for feeding the module host.
pub struct ModuleHost {
    /// Send IRC events here; they are dispatched to all loaded modules.
    pub events: mpsc::Sender<Event>,
    /// Send reload/shutdown signals to the module thread.
    pub control: mpsc::Sender<ModuleControl>,
}

/// Spawn the module host: a forwarder task plus the dedicated plugin thread.
pub fn spawn(
    modules_dir: impl Into<PathBuf>,
    actions: mpsc::Sender<IrcAction>,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
) -> ModuleHost {
    let modules_dir = modules_dir.into();
    let (events_tx, mut events_rx) = mpsc::channel::<Event>(256);
    let (modctl_tx, mut modctl_rx) = mpsc::channel::<ModuleControl>(16);

    // Bridge async channels -> a single std channel the blocking thread drains.
    let (std_tx, std_rx) = std::sync::mpsc::channel::<ModMsg>();
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

    let base = ModuleBase { actions, control, db, log };
    std::thread::Builder::new()
        .name("jeeves-modules".into())
        .spawn(move || module_thread(modules_dir, base, std_rx))
        .expect("spawn module thread");

    ModuleHost { events: events_tx, control: modctl_tx }
}

/// The shared, plugin-independent half of [`HostCtx`].
#[derive(Clone)]
struct ModuleBase {
    actions: mpsc::Sender<IrcAction>,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
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
            ModMsg::Event(ev) => dispatch(&mut plugins, &base, &ev),
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
        actions: base.actions.clone(),
        control: base.control.clone(),
        db: base.db.clone(),
        log: base.log.clone(),
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
        .with_function("bot_reload", [PTR], [PTR], ud.clone(), host_fns::bot_reload)
        .with_function("bot_refresh", [PTR], [PTR], ud.clone(), host_fns::bot_refresh)
        .with_function("bot_shutdown", [PTR], [PTR], ud.clone(), host_fns::bot_shutdown)
        .build()?;

    Ok(Loaded { name: name.to_string(), plugin })
}

/// Dispatch one event to every module that exports the relevant hook.
fn dispatch(plugins: &mut [Loaded], base: &ModuleBase, ev: &Event) {
    let hook = match ev {
        Event::Message(_) => "on_message",
        _ => "on_event",
    };
    let payload = match serde_json::to_string(ev) {
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

    fn message(text: &str, is_private: bool) -> Event {
        Event::Message(MessagePayload {
            nick: "tester".into(),
            target: if is_private { "jeeves".into() } else { "#chan".into() },
            text: text.into(),
            is_private,
            tags: Vec::new(),
        })
    }

    /// Load the real admin.wasm and confirm commands drive host functions: `!ping` produces a
    /// reply action, `!shutdown` produces a reply plus a Control::Shutdown.
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
        let host = spawn(dir, actions_tx, control_tx, db, log);

        // !ping -> reply "pong" to the channel.
        host.events.send(message("!ping", false)).await.unwrap();
        let act = tokio::time::timeout(Duration::from_secs(5), actions_rx.recv())
            .await
            .expect("timed out waiting for ping reply")
            .unwrap();
        match act {
            IrcAction::Privmsg { target, text } => {
                assert_eq!(target, "#chan");
                assert_eq!(text, "pong");
            }
            other => panic!("expected pong privmsg, got {other:?}"),
        }

        // !shutdown -> reply, then a Control::Shutdown.
        host.events.send(message("!shutdown", true)).await.unwrap();
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
