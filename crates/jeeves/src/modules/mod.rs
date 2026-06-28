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
use crate::commands::{canonicalized_event, CommandRegistry, SharedCommandRegistry};
use crate::db::DbHandle;
use crate::log_bus::LogBus;
use crate::scheduler::{ScheduledDelivery, SchedulerHandle};
use crate::settings::{SettingRegistry, SharedSettingRegistry};
use crate::theme::ThemeHandle;
use anyhow::Result;
use extism::{Manifest, PluginBuilder, UserData, Wasm, PTR};
use jeeves_abi::{
    CommandManifest, CommandSpec, Event, EventEnvelope, SettingSpec, SettingsManifest,
    COMMAND_MANIFEST_VERSION, SETTINGS_MANIFEST_VERSION,
};
use std::collections::HashMap;
use std::collections::HashSet;
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
    pub settings: SharedSettingRegistry,
    pub scheduler: SchedulerHandle,
    pub commands: SharedCommandRegistry,
    capabilities: Arc<HashSet<String>>,
}

impl HostCtx {
    pub fn require(&self, capability: &str) -> Result<()> {
        if self.capabilities.contains(capability) {
            Ok(())
        } else {
            anyhow::bail!("module '{}' lacks capability '{capability}'", self.module)
        }
    }
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
    Event(Box<EventEnvelope>),
    Scheduled(Box<ScheduledDelivery>),
    Reload,
    Shutdown,
}

fn reject_scheduled_error(error: std::sync::mpsc::TrySendError<ModMsg>) {
    let message = match error {
        std::sync::mpsc::TrySendError::Full(message)
        | std::sync::mpsc::TrySendError::Disconnected(message) => message,
    };
    if let ModMsg::Scheduled(delivery) = message {
        let _ = delivery.accepted.try_send(false);
    }
}

/// Handles returned to the runtime for feeding the module host.
pub struct ModuleHost {
    /// Send IRC events here; they are dispatched to all loaded modules.
    pub events: mpsc::Sender<EventEnvelope>,
    /// Send reload/shutdown signals to the module thread.
    pub control: mpsc::Sender<ModuleControl>,
    /// Names of currently-loaded modules, updated on each (re)load. Read by the admin API.
    pub names: Arc<Mutex<Vec<String>>>,
    /// Metadata and effective aliases for currently loaded module commands.
    pub commands: SharedCommandRegistry,
    /// Metadata for operator-configurable settings advertised by loaded modules.
    pub settings: SharedSettingRegistry,
    /// Handle to the durable job scheduler; usable from blocking threads (e.g. the TUI).
    pub scheduler: SchedulerHandle,
}

/// Spawn the module host: a forwarder task plus the dedicated plugin thread.
pub fn spawn(
    modules_dir: impl Into<PathBuf>,
    registry: ServerRegistry,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
    theme: ThemeHandle,
    capabilities_path: impl Into<PathBuf>,
) -> ModuleHost {
    let modules_dir = modules_dir.into();
    let (events_tx, mut events_rx) = mpsc::channel::<EventEnvelope>(256);
    let (modctl_tx, mut modctl_rx) = mpsc::channel::<ModuleControl>(16);
    let (scheduled_tx, mut scheduled_rx) = mpsc::channel::<ScheduledDelivery>(64);
    let scheduler = SchedulerHandle::spawn(db.clone(), scheduled_tx, log.clone());
    let scheduler_for_host = scheduler.clone();

    // Bridge async channels -> a single std channel the blocking thread drains.
    let (std_tx, std_rx) = std::sync::mpsc::sync_channel::<ModMsg>(512);
    let watch_tx = std_tx.clone();
    let forward_log = log.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                ev = events_rx.recv() => match ev {
                    Some(ev) => {
                        if let Err(e) = std_tx.try_send(ModMsg::Event(Box::new(ev))) {
                            if matches!(e, std::sync::mpsc::TrySendError::Disconnected(_)) { break; }
                            forward_log.error("modules", "module dispatcher queue full; event dropped");
                        }
                    }
                    None => break,
                },
                ctl = modctl_rx.recv() => match ctl {
                    Some(ModuleControl::Reload) => { let _ = std_tx.try_send(ModMsg::Reload); }
                    Some(ModuleControl::Shutdown) | None => { let _ = std_tx.try_send(ModMsg::Shutdown); break; }
                },
                delivery = scheduled_rx.recv() => match delivery {
                    Some(delivery) => {
                        if let Err(error) = std_tx.try_send(ModMsg::Scheduled(Box::new(delivery))) {
                            reject_scheduled_error(error);
                        }
                    }
                    None => break,
                },
            }
        }
    });

    // Auto-reload: watch the modules directory and feed debounced Reload signals into the same
    // channel the plugin thread drains. Best-effort — if watching fails, !reload still works.
    spawn_watcher(&modules_dir, watch_tx, &log);

    let names: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let commands = CommandRegistry::shared();
    let settings = SettingRegistry::shared();
    let base = ModuleBase {
        registry,
        control,
        db,
        log,
        theme,
        names: names.clone(),
        commands: commands.clone(),
        settings: settings.clone(),
        scheduler,
        capabilities_path: capabilities_path.into(),
    };
    std::thread::Builder::new()
        .name("jeeves-modules".into())
        .spawn(move || module_thread(modules_dir, base, std_rx))
        .expect("spawn module thread");

    ModuleHost {
        events: events_tx,
        control: modctl_tx,
        names,
        commands,
        settings,
        scheduler: scheduler_for_host,
    }
}

/// Watch `dir` for `*.wasm` changes and send a debounced [`ModMsg::Reload`] on activity.
fn spawn_watcher(dir: &Path, reload_tx: std::sync::mpsc::SyncSender<ModMsg>, log: &LogBus) {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::{channel, RecvTimeoutError};
    use std::time::Duration;

    let (raw_tx, raw_rx) = channel::<()>();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(
                    ev.kind,
                    notify::EventKind::Create(_)
                        | notify::EventKind::Modify(_)
                        | notify::EventKind::Remove(_)
                ) {
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
        log.info(
            "modules",
            format!("not watching {} for changes ({e})", dir.display()),
        );
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
    names: Arc<Mutex<Vec<String>>>,
    commands: SharedCommandRegistry,
    settings: SharedSettingRegistry,
    scheduler: SchedulerHandle,
    capabilities_path: PathBuf,
}

struct Worker {
    name: String,
    commands: Vec<CommandSpec>,
    settings: Vec<SettingSpec>,
    tx: std::sync::mpsc::SyncSender<WorkerMsg>,
}

enum WorkerMsg {
    Event(Arc<EventEnvelope>),
    Shutdown,
}

fn module_thread(dir: PathBuf, base: ModuleBase, rx: std::sync::mpsc::Receiver<ModMsg>) {
    let mut workers = load_all(&dir, &base);
    publish_names(&base, &workers);
    publish_commands(&base, &workers);
    publish_settings(&base, &workers);
    base.log.info(
        "modules",
        format!("started {} module worker(s)", workers.len()),
    );

    while let Ok(msg) = rx.recv() {
        match msg {
            ModMsg::Event(env) => dispatch(&workers, &base, &env),
            ModMsg::Scheduled(delivery) => dispatch_scheduled(&workers, &base, *delivery),
            ModMsg::Reload => {
                base.log.info("modules", "reloading modules");
                let next = load_all(&dir, &base);
                for worker in workers.drain(..) {
                    let _ = worker.tx.try_send(WorkerMsg::Shutdown);
                }
                workers = next;
                publish_names(&base, &workers);
                publish_commands(&base, &workers);
                publish_settings(&base, &workers);
                base.log.info(
                    "modules",
                    format!("restarted {} module worker(s)", workers.len()),
                );
            }
            ModMsg::Shutdown => {
                for worker in workers.drain(..) {
                    let _ = worker.tx.try_send(WorkerMsg::Shutdown);
                }
                break;
            }
        }
    }
}

/// Publish the current loaded-module names to the shared list read by the admin API.
fn publish_names(base: &ModuleBase, plugins: &[Worker]) {
    let mut names = base.names.lock().unwrap();
    *names = plugins.iter().map(|p| p.name.clone()).collect();
    names.sort();
}

fn publish_commands(base: &ModuleBase, workers: &[Worker]) {
    let specs = workers
        .iter()
        .flat_map(|worker| {
            worker
                .commands
                .iter()
                .cloned()
                .map(|spec| (worker.name.clone(), spec))
        })
        .collect();
    let overrides = match base.db.load_alias_overrides_blocking() {
        Ok(overrides) => overrides,
        Err(error) => {
            base.log
                .error("modules", format!("cannot load command aliases: {error}"));
            Default::default()
        }
    };
    let warnings = base
        .commands
        .lock()
        .unwrap()
        .replace_specs(specs, overrides);
    for warning in warnings {
        base.log.error("modules", warning);
    }
}

fn publish_settings(base: &ModuleBase, workers: &[Worker]) {
    let mut specs = Vec::new();
    for worker in workers {
        if !worker
            .settings
            .iter()
            .any(|spec| spec.key.eq_ignore_ascii_case("enabled"))
        {
            specs.push((
                worker.name.clone(),
                SettingSpec {
                    key: "enabled".into(),
                    description: "Whether this module receives events in the selected scope."
                        .into(),
                    default: "true".into(),
                    kind: jeeves_abi::SettingKind::Boolean,
                    scopes: vec![
                        jeeves_abi::SettingScope::Global,
                        jeeves_abi::SettingScope::Network,
                        jeeves_abi::SettingScope::Channel,
                    ],
                    applies_immediately: true,
                },
            ));
        }
        specs.extend(
            worker
                .settings
                .iter()
                .cloned()
                .map(|spec| (worker.name.clone(), spec)),
        );
    }
    let overrides = match base.db.load_setting_overrides_blocking() {
        Ok(overrides) => overrides,
        Err(error) => {
            base.log
                .error("modules", format!("cannot load module settings: {error}"));
            Vec::new()
        }
    };
    let mut registry = base.settings.lock().unwrap();
    let warnings = registry.replace_specs(specs);
    registry.replace_overrides(overrides);
    drop(registry);
    for warning in warnings {
        base.log.error("modules", warning);
    }
}

/// (Re)load every `*.wasm` in `dir`.
fn load_all(dir: &Path, base: &ModuleBase) -> Vec<Worker> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            base.log.info(
                "modules",
                format!("no modules directory at {}", dir.display()),
            );
            return out;
        }
    };
    let mut paths = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("wasm"))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("module")
            .to_string();
        if let Some(worker) = spawn_worker(path, name, base.clone()) {
            out.push(worker);
        }
    }
    out
}

fn load_one(path: &Path, name: &str, base: &ModuleBase) -> Result<extism::Plugin> {
    let capabilities = Arc::new(load_capabilities(&base.capabilities_path, name, &base.log));
    // Extism can interrupt runaway guest execution. Host calls such as weather are synchronous,
    // so allow enough time for their explicit network timeouts while still bounding guest code.
    let manifest =
        Manifest::new([Wasm::file(path)]).with_timeout(std::time::Duration::from_secs(20));
    let ud = UserData::new(HostCtx {
        module: name.to_string(),
        registry: base.registry.clone(),
        control: base.control.clone(),
        db: base.db.clone(),
        log: base.log.clone(),
        theme: base.theme.clone(),
        settings: base.settings.clone(),
        scheduler: base.scheduler.clone(),
        commands: base.commands.clone(),
        capabilities,
    });

    let plugin = PluginBuilder::new(manifest)
        .with_wasi(true)
        .with_function(
            "send_message",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::send_message,
        )
        .with_function(
            "send_notice",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::send_notice,
        )
        .with_function("join", [PTR], [PTR], ud.clone(), host_fns::join)
        .with_function("part", [PTR], [PTR], ud.clone(), host_fns::part)
        .with_function("kv_get", [PTR], [PTR], ud.clone(), host_fns::kv_get)
        .with_function("kv_set", [PTR], [PTR], ud.clone(), host_fns::kv_set)
        .with_function(
            "setting_get",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::setting_get,
        )
        .with_function(
            "schedule_set",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::schedule_set,
        )
        .with_function(
            "schedule_cancel",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::schedule_cancel,
        )
        .with_function(
            "schedule_list",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::schedule_list,
        )
        .with_function(
            "random_bytes",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::random_bytes,
        )
        .with_function("log", [PTR], [PTR], ud.clone(), host_fns::log)
        .with_function("now", [PTR], [PTR], ud.clone(), host_fns::now)
        .with_function("theme", [PTR], [PTR], ud.clone(), host_fns::theme)
        .with_function(
            "profile_ensure",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::profile_ensure,
        )
        .with_function(
            "profile_get",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::profile_get,
        )
        .with_function(
            "profile_set",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::profile_set,
        )
        .with_function(
            "profile_clear",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::profile_clear,
        )
        .with_function("geocode", [PTR], [PTR], ud.clone(), host_fns::geocode)
        .with_function("weather", [PTR], [PTR], ud.clone(), host_fns::weather)
        .with_function("local_time", [PTR], [PTR], ud.clone(), host_fns::local_time)
        .with_function("web_search", [PTR], [PTR], ud.clone(), host_fns::web_search)
        .with_function("translate", [PTR], [PTR], ud.clone(), host_fns::translate)
        .with_function(
            "commands_list",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::commands_list,
        )
        .with_function("bot_reload", [PTR], [PTR], ud.clone(), host_fns::bot_reload)
        .with_function(
            "bot_refresh",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::bot_refresh,
        )
        .with_function(
            "bot_shutdown",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::bot_shutdown,
        )
        .build()?;

    Ok(plugin)
}

fn load_capabilities(path: &Path, module: &str, log: &LogBus) -> HashSet<String> {
    let safe_defaults = || {
        ["log", "theme", "now", "setting_get"]
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) => {
            log.error(
                "modules",
                format!(
                    "cannot read capability policy {} ({e}); '{module}' gets safe defaults only",
                    path.display()
                ),
            );
            return safe_defaults();
        }
    };
    let doc = match text.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(e) => {
            log.error(
                "modules",
                format!(
                    "invalid capability policy {} ({e}); '{module}' gets safe defaults only",
                    path.display()
                ),
            );
            return safe_defaults();
        }
    };
    let Some(array) = doc
        .get(module)
        .and_then(|section| section.get("capabilities"))
        .and_then(|item| item.as_array())
    else {
        log.info(
            "modules",
            format!("no capability policy for '{module}'; safe defaults only"),
        );
        return safe_defaults();
    };
    array
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn spawn_worker(path: PathBuf, name: String, base: ModuleBase) -> Option<Worker> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<WorkerMsg>(64);
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let startup_log = base.log.clone();
    let worker_name = name.clone();
    std::thread::Builder::new()
        .name(format!("jeeves-module-{name}"))
        .spawn(move || {
            let mut plugin = match load_one(&path, &worker_name, &base) {
                Ok(plugin) => plugin,
                Err(e) => {
                    base.log
                        .error("modules", format!("failed to load {}: {e}", path.display()));
                    let _ = ready_tx.send(Err(e.to_string()));
                    return;
                }
            };
            if plugin.function_exists("init") {
                if let Err(e) = plugin.call::<&str, &str>("init", "") {
                    base.log
                        .error("modules", format!("{worker_name}: init failed: {e}"));
                }
            }
            let commands = match read_command_manifest(&mut plugin) {
                Ok(commands) => commands,
                Err(error) => {
                    base.log.error(
                        "modules",
                        format!("{worker_name}: command metadata ignored: {error}"),
                    );
                    Vec::new()
                }
            };
            let settings = match read_settings_manifest(&mut plugin) {
                Ok(settings) => settings,
                Err(error) => {
                    base.log.error(
                        "modules",
                        format!("{worker_name}: settings metadata ignored: {error}"),
                    );
                    Vec::new()
                }
            };
            let _ = ready_tx.send(Ok((commands, settings)));
            base.log
                .info("modules", format!("loaded module '{worker_name}'"));
            while let Ok(msg) = rx.recv() {
                match msg {
                    WorkerMsg::Event(env) => dispatch_one(&mut plugin, &base, &worker_name, &env),
                    WorkerMsg::Shutdown => break,
                }
            }
        })
        .unwrap_or_else(|e| panic!("spawn worker for {name}: {e}"));
    match ready_rx.recv_timeout(std::time::Duration::from_secs(25)) {
        Ok(Ok((commands, settings))) => Some(Worker {
            name,
            commands,
            settings,
            tx,
        }),
        Ok(Err(error)) => {
            startup_log.error(
                "modules",
                format!("module '{name}' failed to start: {error}"),
            );
            None
        }
        Err(error) => {
            startup_log.error(
                "modules",
                format!("module '{name}' startup handshake failed: {error}"),
            );
            None
        }
    }
}

fn read_command_manifest(plugin: &mut extism::Plugin) -> Result<Vec<CommandSpec>> {
    if !plugin.function_exists("commands") {
        return Ok(Vec::new());
    }
    let raw = plugin.call::<&str, &str>("commands", "")?;
    let manifest: CommandManifest = serde_json::from_str(raw)?;
    if manifest.version != COMMAND_MANIFEST_VERSION {
        anyhow::bail!(
            "unsupported command manifest version {} (host supports {})",
            manifest.version,
            COMMAND_MANIFEST_VERSION
        );
    }
    Ok(manifest.commands)
}

fn read_settings_manifest(plugin: &mut extism::Plugin) -> Result<Vec<SettingSpec>> {
    if !plugin.function_exists("settings") {
        return Ok(Vec::new());
    }
    let raw = plugin.call::<&str, &str>("settings", "")?;
    let manifest: SettingsManifest = serde_json::from_str(raw)?;
    if manifest.version != SETTINGS_MANIFEST_VERSION {
        anyhow::bail!(
            "unsupported settings manifest version {} (host supports {})",
            manifest.version,
            SETTINGS_MANIFEST_VERSION
        );
    }
    Ok(manifest.settings)
}

/// Dispatch without blocking the other modules. A full worker queue means only that module is
/// behind; its event is dropped and clearly logged.
fn dispatch(plugins: &[Worker], base: &ModuleBase, env: &EventEnvelope) {
    let target = match &env.event {
        Event::Message(message) => message
            .text
            .split_whitespace()
            .next()
            .and_then(|token| base.commands.lock().unwrap().resolve(token)),
        _ => None,
    };
    let original = Arc::new(env.clone());
    let canonical = target
        .as_ref()
        .map(|target| Arc::new(canonicalized_event(env, &target.canonical)));
    for worker in plugins {
        let channel = match &env.event {
            Event::Message(message) if !message.is_private => Some(message.target.as_str()),
            _ => None,
        };
        let enabled = base
            .settings
            .lock()
            .unwrap()
            .effective(&worker.name, "enabled", Some(&env.server), channel)
            .is_none_or(|value| value == "true");
        if !enabled {
            continue;
        }
        let event = match (&target, &canonical) {
            (Some(target), Some(canonical)) if target.module == worker.name => canonical.clone(),
            _ => original.clone(),
        };
        if let Err(e) = worker.tx.try_send(WorkerMsg::Event(event)) {
            base.log
                .error("modules", format!("{}: event dropped ({e})", worker.name));
        }
    }
}

fn dispatch_scheduled(workers: &[Worker], base: &ModuleBase, delivery: ScheduledDelivery) {
    let channel = match &delivery.envelope.event {
        Event::Timer { channel, .. } => Some(channel.as_str()),
        _ => None,
    };
    let enabled = base
        .settings
        .lock()
        .unwrap()
        .effective(
            &delivery.module,
            "enabled",
            Some(&delivery.envelope.server),
            channel,
        )
        .is_none_or(|value| value == "true");
    let accepted = workers
        .iter()
        .find(|worker| worker.name == delivery.module)
        .filter(|_| enabled)
        .is_some_and(|worker| {
            worker
                .tx
                .try_send(WorkerMsg::Event(Arc::new(delivery.envelope)))
                .is_ok()
        });
    let _ = delivery.accepted.try_send(accepted);
}

fn dispatch_one(plugin: &mut extism::Plugin, base: &ModuleBase, name: &str, env: &EventEnvelope) {
    let hook = match env.event {
        Event::Message(_) => "on_message",
        _ => "on_event",
    };
    let payload = match serde_json::to_string(env) {
        Ok(p) => p,
        Err(e) => {
            base.log
                .error("modules", format!("event serialize failed: {e}"));
            return;
        }
    };
    if plugin.function_exists(hook) {
        if let Err(e) = plugin.call::<&str, &str>(hook, &payload) {
            base.log
                .error("modules", format!("{name}: {hook} failed: {e}"));
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
                user_id: String::new(),
                nick: "tester".into(),
                display: "tester".into(),
                user: "u".into(),
                host: "h".into(),
                target: if is_private {
                    "jeeves".into()
                } else {
                    "#chan".into()
                },
                text: text.into(),
                is_private,
                tags: Vec::new(),
                role: Some(jeeves_abi::Role::SuperAdmin),
            }),
        }
    }

    fn test_scheduler(db: &DbHandle, log: &LogBus) -> SchedulerHandle {
        let (deliveries, _rx) = mpsc::channel(1);
        SchedulerHandle::spawn(db.clone(), deliveries, log.clone())
    }

    #[test]
    fn capability_policy_keeps_privileged_controls_admin_only() {
        let path = std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../module-capabilities.toml"
        ));
        let log = LogBus::new(8);
        let admin = load_capabilities(path, "admin", &log);
        let fishing = load_capabilities(path, "fishing", &log);
        let memos = load_capabilities(path, "memos", &log);
        assert!(admin.contains("bot_shutdown"));
        assert!(!fishing.contains("bot_shutdown"));
        assert!(fishing.contains("theme"));
        assert_eq!(
            memos,
            [
                "send_message",
                "theme",
                "kv_get",
                "kv_set",
                "profile_get",
                "now",
                "setting_get",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
    }

    #[test]
    fn aliases_are_canonicalized_only_for_the_owning_module() {
        let commands = CommandRegistry::shared();
        commands.lock().unwrap().replace_specs(
            vec![(
                "weather".into(),
                CommandSpec {
                    name: "weather".into(),
                    aliases: vec!["w".into()],
                    ..Default::default()
                },
            )],
            Default::default(),
        );
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(8);
        let (control, _) = mpsc::channel(1);
        let scheduler = test_scheduler(&db, &log);
        let base = ModuleBase {
            registry: Arc::new(Mutex::new(HashMap::new())),
            control,
            db,
            log,
            theme: crate::theme::ThemeStore::open("/tmp/jeeves-alias-test-theme.toml"),
            names: Arc::new(Mutex::new(Vec::new())),
            commands,
            settings: SettingRegistry::shared(),
            scheduler,
            capabilities_path: PathBuf::new(),
        };
        let (weather_tx, weather_rx) = std::sync::mpsc::sync_channel(1);
        let (history_tx, history_rx) = std::sync::mpsc::sync_channel(1);
        let workers = vec![
            Worker {
                name: "weather".into(),
                commands: Vec::new(),
                settings: Vec::new(),
                tx: weather_tx,
            },
            Worker {
                name: "history".into(),
                commands: Vec::new(),
                settings: Vec::new(),
                tx: history_tx,
            },
        ];

        dispatch(&workers, &base, &envelope("net", "!w New York", false));
        let WorkerMsg::Event(weather) = weather_rx.recv().unwrap() else {
            panic!("expected weather event")
        };
        let WorkerMsg::Event(history) = history_rx.recv().unwrap() else {
            panic!("expected history event")
        };
        let Event::Message(weather) = &weather.event else {
            panic!("expected weather message")
        };
        let Event::Message(history) = &history.event else {
            panic!("expected history message")
        };
        assert_eq!(weather.text, "!weather New York");
        assert_eq!(history.text, "!w New York");
    }

    #[test]
    fn scoped_enabled_setting_blocks_only_matching_channel_dispatch() {
        let settings = SettingRegistry::shared();
        settings.lock().unwrap().replace_specs(vec![(
            "weather".into(),
            SettingSpec {
                key: "enabled".into(),
                description: String::new(),
                default: "true".into(),
                kind: jeeves_abi::SettingKind::Boolean,
                scopes: vec![
                    jeeves_abi::SettingScope::Global,
                    jeeves_abi::SettingScope::Network,
                    jeeves_abi::SettingScope::Channel,
                ],
                applies_immediately: true,
            },
        )]);
        settings.lock().unwrap().set_override(
            "weather",
            "enabled",
            jeeves_abi::SettingScope::Channel,
            "net",
            "#quiet",
            Some("false".into()),
        );
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(8);
        let (control, _) = mpsc::channel(1);
        let scheduler = test_scheduler(&db, &log);
        let base = ModuleBase {
            registry: Arc::new(Mutex::new(HashMap::new())),
            control,
            db,
            log,
            theme: crate::theme::ThemeStore::open("/tmp/jeeves-settings-test-theme.toml"),
            names: Arc::new(Mutex::new(Vec::new())),
            commands: CommandRegistry::shared(),
            settings,
            scheduler,
            capabilities_path: PathBuf::new(),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let workers = vec![Worker {
            name: "weather".into(),
            commands: Vec::new(),
            settings: Vec::new(),
            tx,
        }];

        dispatch(&workers, &base, &envelope("net", "hello", false));
        assert!(rx.try_recv().is_ok(), "#chan remains enabled");
        let mut quiet = envelope("net", "hello", false);
        let Event::Message(message) = &mut quiet.event else {
            unreachable!()
        };
        message.target = "#quiet".into();
        dispatch(&workers, &base, &quiet);
        assert!(rx.try_recv().is_err(), "#quiet override blocks dispatch");
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
        registry
            .lock()
            .unwrap()
            .insert("testnet".to_string(), actions_tx);
        let theme_path =
            std::env::temp_dir().join(format!("jeeves-modtest-theme-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&theme_path);
        let theme = crate::theme::ThemeStore::open(&theme_path);
        let capabilities = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../module-capabilities.toml"
        );
        let host = spawn(dir, registry, control_tx, db, log, theme, capabilities);

        // !ping -> reply "pong" to the channel on the originating network.
        host.events
            .send(envelope("testnet", "!ping", false))
            .await
            .unwrap();
        let act = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
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
        let registered = host.commands.lock().unwrap().snapshot();
        assert!(registered.iter().any(|command| {
            command.module == "weather"
                && command.name == "weather"
                && command.aliases.iter().any(|alias| alias == "w")
        }));
        let registered_settings = host.settings.lock().unwrap().snapshot();
        assert!(registered_settings.iter().any(|setting| {
            setting.module == "memos" && setting.spec.key == "retention_seconds"
        }));
        assert!(registered_settings
            .iter()
            .any(|setting| setting.module == "clock" && setting.spec.key == "enabled"));
        assert!(registered
            .iter()
            .any(|command| command.module == "memos" && command.name == "tell"));
        assert!(registered.iter().any(|command| {
            command.module == "clock"
                && command.name == "time"
                && command.aliases.iter().any(|alias| alias == "clock")
        }));
        assert!(registered
            .iter()
            .any(|command| command.module == "reminders" && command.name == "remind"));

        // The fishing module must route even its static help output through theme.toml.
        if std::path::Path::new(dir).join("fishing.wasm").exists() {
            let mut fishing = envelope("testnet", "!fish help", false);
            if let Event::Message(msg) = &mut fishing.event {
                msg.user_id = "00000000-0000-4000-8000-000000000001".into();
            }
            host.events.send(fishing).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
                .await
                .expect("timed out waiting for fishing help")
                .unwrap();
            assert!(matches!(reply, IrcAction::Privmsg { .. }));
            let written = std::fs::read_to_string(&theme_path).unwrap();
            assert!(written.contains("[fishing]"), "theme file: {written}");
            assert!(written.contains("help"), "theme file: {written}");
        }

        if std::path::Path::new(dir).join("history.wasm").exists() {
            let mut original = envelope("testnet", "I mistyped ctas", false);
            if let Event::Message(message) = &mut original.event {
                message.user_id = "00000000-0000-4000-8000-000000000002".into();
            }
            host.events.send(original).await.unwrap();
            let mut correction = envelope("testnet", "s/ctas/cats/", false);
            if let Event::Message(message) = &mut correction.event {
                message.user_id = "00000000-0000-4000-8000-000000000002".into();
            }
            host.events.send(correction).await.unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
                .await
                .expect("timed out waiting for sed correction")
                .unwrap();
            let IrcAction::Privmsg { text, .. } = reply else {
                panic!("expected sed correction reply")
            };
            assert!(text.contains("cats"), "sed reply: {text}");
        }

        if std::path::Path::new(dir).join("reminders.wasm").exists() {
            let mut reminder = envelope(
                "testnet",
                "!remind me in 1 second to verify durable timers",
                false,
            );
            if let Event::Message(message) = &mut reminder.event {
                message.user_id = "00000000-0000-4000-8000-000000000003".into();
            }
            host.events.send(reminder).await.unwrap();
            let confirmation = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
                .await
                .expect("timed out waiting for reminder confirmation")
                .unwrap();
            assert!(matches!(confirmation, IrcAction::Privmsg { .. }));
            let delivery = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
                .await
                .expect("timed out waiting for durable reminder delivery")
                .unwrap();
            let IrcAction::Privmsg { target, text } = delivery else {
                panic!("expected reminder delivery")
            };
            assert_eq!(target, "#chan");
            assert!(text.contains("verify durable timers"), "delivery: {text}");
        }

        // !shutdown -> reply, then a Control::Shutdown.
        host.events
            .send(envelope("testnet", "!shutdown", true))
            .await
            .unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(15), actions_rx.recv())
            .await
            .expect("timed out waiting for shutdown reply")
            .unwrap();
        assert!(matches!(reply, IrcAction::Privmsg { .. }));

        let ctl = tokio::time::timeout(Duration::from_secs(15), control_rx.recv())
            .await
            .expect("timed out waiting for shutdown control")
            .unwrap();
        assert!(
            matches!(ctl, Control::Shutdown),
            "expected Shutdown, got {ctl:?}"
        );
        let _ = std::fs::remove_file(theme_path);
    }
}
