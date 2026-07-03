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
use crate::data_lifecycle;
use crate::db::{DataDeletionJob, DbHandle};
use crate::log_bus::LogBus;
use crate::scheduler::{ScheduledCompletion, ScheduledDelivery, SchedulerHandle};
use crate::settings::{SettingRegistry, SharedSettingRegistry};
use crate::theme::ThemeHandle;
use anyhow::Result;
use extism::{Manifest, PluginBuilder, UserData, Wasm, PTR};
use jeeves_abi::{
    AchievementBackfillRequest, AchievementBackfillResponse, AchievementManifest, CommandManifest,
    CommandSpec, DataSubject, Event, EventEnvelope, ModuleDataDeletePlan, ModuleDataExport,
    ModuleDataRequest, ModuleDataResponse, Role, SettingSpec, SettingsManifest,
    COMMAND_MANIFEST_VERSION, DATA_LIFECYCLE_VERSION, SETTINGS_MANIFEST_VERSION,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Maps a server label to the live action sender for its IRC actor. Updated by the runtime
/// supervisor on (re)connect/disconnect; read by server-aware host functions.
pub type ServerRegistry = Arc<Mutex<HashMap<String, mpsc::Sender<IrcAction>>>>;
type AchievementRegistry = Arc<Mutex<HashMap<String, AchievementManifest>>>;
type AchievementAnnouncementQueue = Arc<Mutex<HashMap<String, PendingAchievementAnnouncement>>>;

#[derive(Clone)]
struct PendingAchievementAnnouncement {
    server: String,
    target: String,
    display_name: String,
    unlocks: Vec<String>,
    prestige: Vec<String>,
    completion: bool,
}

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
    achievements: AchievementRegistry,
    achievement_announcements: AchievementAnnouncementQueue,
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
    ProfileInspect {
        server: String,
        profile_id: String,
        reply: std::sync::mpsc::SyncSender<Result<Vec<ProfileModuleData>>>,
    },
    ProfilePlanReset {
        server: String,
        profile_id: String,
        module: String,
        reply: std::sync::mpsc::SyncSender<Result<ProfileModuleResetPlan>>,
    },
    ProfileApplyReset {
        plan: ProfileModuleResetPlan,
        reply: std::sync::mpsc::SyncSender<Result<()>>,
    },
    Shutdown,
}

fn reject_scheduled_error(error: std::sync::mpsc::TrySendError<ModMsg>) {
    let message = match error {
        std::sync::mpsc::TrySendError::Full(message)
        | std::sync::mpsc::TrySendError::Disconnected(message) => message,
    };
    if let ModMsg::Scheduled(delivery) = message {
        delivery.completion.finish(false);
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
    /// Blocking operator interface for inspecting and resetting profile-owned module data.
    pub profile_admin: ProfileAdminHandle,
}

#[derive(Clone, Debug)]
pub struct ProfileModuleData {
    pub module: String,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ProfileModuleResetPlan {
    pub module: String,
    pub profile_id: String,
    expected_entries: Vec<jeeves_abi::ModuleKvEntry>,
    mutations: Vec<jeeves_abi::ModuleKvMutation>,
}

impl ProfileModuleResetPlan {
    pub fn mutation_count(&self) -> usize {
        self.mutations.len()
    }
}

#[derive(Clone)]
pub struct ProfileAdminHandle {
    tx: std::sync::mpsc::SyncSender<ModMsg>,
}

impl ProfileAdminHandle {
    pub fn inspect_blocking(
        &self,
        server: &str,
        profile_id: &str,
    ) -> Result<Vec<ProfileModuleData>> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(ModMsg::ProfileInspect {
                server: server.into(),
                profile_id: profile_id.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("module host stopped"))?;
        rx.recv_timeout(std::time::Duration::from_secs(25))
            .map_err(|error| anyhow::anyhow!("profile inspection timed out: {error}"))?
    }

    pub fn plan_reset_blocking(
        &self,
        server: &str,
        profile_id: &str,
        module: &str,
    ) -> Result<ProfileModuleResetPlan> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(ModMsg::ProfilePlanReset {
                server: server.into(),
                profile_id: profile_id.into(),
                module: module.into(),
                reply,
            })
            .map_err(|_| anyhow::anyhow!("module host stopped"))?;
        rx.recv_timeout(std::time::Duration::from_secs(25))
            .map_err(|error| anyhow::anyhow!("profile reset preview timed out: {error}"))?
    }

    pub fn apply_reset_blocking(&self, plan: ProfileModuleResetPlan) -> Result<()> {
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.tx
            .send(ModMsg::ProfileApplyReset { plan, reply })
            .map_err(|_| anyhow::anyhow!("module host stopped"))?;
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|error| anyhow::anyhow!("profile reset timed out: {error}"))?
    }
}

pub struct ModulePaths {
    pub modules_dir: PathBuf,
    pub capabilities_path: PathBuf,
    pub export_dir: PathBuf,
}

/// Spawn the module host: a forwarder task plus the dedicated plugin thread.
pub fn spawn(
    paths: ModulePaths,
    registry: ServerRegistry,
    control: mpsc::Sender<Control>,
    db: DbHandle,
    log: LogBus,
    theme: ThemeHandle,
) -> ModuleHost {
    let modules_dir = paths.modules_dir;
    let (events_tx, mut events_rx) = mpsc::channel::<EventEnvelope>(256);
    let (modctl_tx, mut modctl_rx) = mpsc::channel::<ModuleControl>(16);
    let (scheduled_tx, mut scheduled_rx) = mpsc::channel::<ScheduledDelivery>(64);
    let scheduler = SchedulerHandle::spawn(db.clone(), scheduled_tx, log.clone());
    let scheduler_for_host = scheduler.clone();

    // Bridge async channels -> a single std channel the blocking thread drains.
    let (std_tx, std_rx) = std::sync::mpsc::sync_channel::<ModMsg>(512);
    let profile_admin = ProfileAdminHandle { tx: std_tx.clone() };
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
        achievements: Arc::new(Mutex::new(HashMap::new())),
        achievement_announcements: Arc::new(Mutex::new(HashMap::new())),
        settings: settings.clone(),
        scheduler,
        capabilities_path: paths.capabilities_path,
        export_dir: paths.export_dir,
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
        profile_admin,
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
    achievements: AchievementRegistry,
    achievement_announcements: AchievementAnnouncementQueue,
    settings: SharedSettingRegistry,
    scheduler: SchedulerHandle,
    capabilities_path: PathBuf,
    export_dir: PathBuf,
}

struct Worker {
    name: String,
    commands: Vec<CommandSpec>,
    settings: Vec<SettingSpec>,
    achievements: Option<AchievementManifest>,
    lifecycle: bool,
    tx: std::sync::mpsc::SyncSender<WorkerMsg>,
}

enum WorkerMsg {
    Event(Arc<EventEnvelope>),
    Scheduled {
        envelope: Arc<EventEnvelope>,
        completion: ScheduledCompletion,
    },
    DataExport {
        request: ModuleDataRequest,
        reply: std::sync::mpsc::SyncSender<Result<ModuleDataResponse, String>>,
    },
    DataDelete {
        request: ModuleDataRequest,
        reply: std::sync::mpsc::SyncSender<Result<ModuleDataDeletePlan, String>>,
    },
    Shutdown,
}

fn module_thread(dir: PathBuf, base: ModuleBase, rx: std::sync::mpsc::Receiver<ModMsg>) {
    let mut workers = load_all(&dir, &base);
    let mut export_cooldowns = HashMap::new();
    publish_names(&base, &workers);
    publish_commands(&base, &workers);
    publish_settings(&base, &workers);
    publish_achievements(&base, &workers);
    resume_deletions(&workers, &base);
    base.log.info(
        "modules",
        format!("started {} module worker(s)", workers.len()),
    );

    while let Ok(msg) = rx.recv() {
        match msg {
            ModMsg::Event(env) => {
                if !handle_lifecycle_command(&workers, &base, &env, &mut export_cooldowns) {
                    dispatch(&workers, &base, &env);
                }
            }
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
                publish_achievements(&base, &workers);
                resume_deletions(&workers, &base);
                base.log.info(
                    "modules",
                    format!("restarted {} module worker(s)", workers.len()),
                );
            }
            ModMsg::ProfileInspect {
                server,
                profile_id,
                reply,
            } => {
                let _ = reply.send(inspect_profile_modules(
                    &workers,
                    &base,
                    &server,
                    &profile_id,
                ));
            }
            ModMsg::ProfilePlanReset {
                server,
                profile_id,
                module,
                reply,
            } => {
                let _ = reply.send(plan_profile_module_reset(
                    &workers,
                    &base,
                    &server,
                    &profile_id,
                    &module,
                ));
            }
            ModMsg::ProfileApplyReset { plan, reply } => {
                let result = base.db.kv_apply_module_checked_blocking(
                    &plan.module,
                    plan.expected_entries,
                    plan.mutations,
                );
                let _ = reply.send(result);
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
    let mut specs = workers
        .iter()
        .flat_map(|worker| {
            worker
                .commands
                .iter()
                .cloned()
                .map(|spec| (worker.name.clone(), spec))
        })
        .collect::<Vec<_>>();
    specs.push((
        "data".into(),
        CommandSpec {
            name: "mydata".into(),
            description: "Privately summarize, export, or delete your own stored data.".into(),
            usage: "!mydata [summary | export | delete | confirm <token>]".into(),
            aliases: Vec::new(),
        },
    ));
    specs.push((
        "data".into(),
        CommandSpec {
            name: "data".into(),
            description: "Privately manage stored profile data (super-admin).".into(),
            usage:
                "!data <nick> <summary | export | delete> | !data confirm <token> | !data pending"
                    .into(),
            aliases: Vec::new(),
        },
    ));
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

fn publish_achievements(base: &ModuleBase, workers: &[Worker]) {
    let catalogs = workers
        .iter()
        .filter_map(|worker| {
            worker
                .achievements
                .clone()
                .map(|manifest| (worker.name.clone(), manifest))
        })
        .collect::<HashMap<_, _>>();
    *base.achievements.lock().unwrap() = catalogs.clone();
    if let Err(error) = base
        .db
        .achievement_catalog_reconcile_blocking(catalogs.into_iter().collect(), now_secs())
    {
        base.log.error(
            "modules",
            format!("cannot reconcile achievement catalog: {error}"),
        );
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
        achievements: base.achievements.clone(),
        achievement_announcements: base.achievement_announcements.clone(),
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
        .with_function(
            "dictionary_lookup",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::dictionary_lookup,
        )
        .with_function("translate", [PTR], [PTR], ud.clone(), host_fns::translate)
        .with_function("ai_chat", [PTR], [PTR], ud.clone(), host_fns::ai_chat)
        .with_function("bot_nick", [PTR], [PTR], ud.clone(), host_fns::bot_nick)
        .with_function(
            "irc_casefold",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::irc_casefold,
        )
        .with_function(
            "youtube_lookup",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::youtube_lookup,
        )
        .with_function(
            "youtube_search",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::youtube_search,
        )
        .with_function(
            "commands_list",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::commands_list,
        )
        .with_function(
            "award_stats",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::award_stats,
        )
        .with_function(
            "achievements_get",
            [PTR],
            [PTR],
            ud.clone(),
            host_fns::achievements_get,
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
            let achievements = match read_achievement_manifest(&mut plugin, &worker_name) {
                Ok(manifest) => manifest,
                Err(error) => {
                    base.log.error(
                        "modules",
                        format!("{worker_name}: achievement metadata ignored: {error}"),
                    );
                    None
                }
            };
            if let Some(manifest) = achievements.as_ref() {
                if let Err(error) =
                    run_achievement_backfills(&mut plugin, &base, &worker_name, manifest)
                {
                    base.log.error(
                        "modules",
                        format!("{worker_name}: achievement backfill failed: {error}"),
                    );
                }
            }
            let lifecycle =
                plugin.function_exists("data_export") && plugin.function_exists("data_delete");
            if lifecycle {
                if let Err(error) = base
                    .db
                    .lifecycle_register_blocking(&worker_name, now_secs())
                {
                    base.log.error(
                        "modules",
                        format!("{worker_name}: cannot register lifecycle hooks: {error}"),
                    );
                }
            }
            let _ = ready_tx.send(Ok((commands, settings, achievements, lifecycle)));
            base.log
                .info("modules", format!("loaded module '{worker_name}'"));
            while let Ok(msg) = rx.recv() {
                match msg {
                    WorkerMsg::Event(env) => {
                        dispatch_one(&mut plugin, &base, &worker_name, &env);
                    }
                    WorkerMsg::Scheduled {
                        envelope,
                        completion,
                    } => {
                        let succeeded = dispatch_one(&mut plugin, &base, &worker_name, &envelope);
                        completion.finish(succeeded);
                    }
                    WorkerMsg::DataExport { request, reply } => {
                        let result = call_data_export(&mut plugin, &request)
                            .map_err(|_| "lifecycle export hook failed".to_string());
                        let _ = reply.send(result);
                    }
                    WorkerMsg::DataDelete { request, reply } => {
                        let result = call_data_delete(&mut plugin, &request)
                            .map_err(|_| "lifecycle deletion hook failed".to_string());
                        let _ = reply.send(result);
                    }
                    WorkerMsg::Shutdown => break,
                }
            }
        })
        .unwrap_or_else(|e| panic!("spawn worker for {name}: {e}"));
    match ready_rx.recv_timeout(std::time::Duration::from_secs(25)) {
        Ok(Ok((commands, settings, achievements, lifecycle))) => Some(Worker {
            name,
            commands,
            settings,
            achievements,
            lifecycle,
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

fn read_achievement_manifest(
    plugin: &mut extism::Plugin,
    module: &str,
) -> Result<Option<AchievementManifest>> {
    if !plugin.function_exists("achievements") {
        return Ok(None);
    }
    let raw = plugin.call::<&str, &str>("achievements", "")?;
    let manifest: AchievementManifest = serde_json::from_str(raw)?;
    if manifest.version != jeeves_abi::ACHIEVEMENT_MANIFEST_VERSION {
        anyhow::bail!(
            "unsupported achievement manifest version {}",
            manifest.version
        );
    }
    crate::db::validate_achievement_manifest(module, &manifest)?;
    Ok(Some(manifest))
}

fn run_achievement_backfills(
    plugin: &mut extism::Plugin,
    base: &ModuleBase,
    module: &str,
    manifest: &AchievementManifest,
) -> Result<()> {
    if !plugin.function_exists("achievement_backfill") {
        return Ok(());
    }
    let entries = base.db.kv_list_module_blocking(module)?;
    for server in base
        .db
        .load_servers_blocking()?
        .into_iter()
        .filter(|server| server.enabled)
    {
        let previous_version = base
            .db
            .achievement_backfill_version_blocking(&server.label, module)?;
        if previous_version >= manifest.catalog_version {
            continue;
        }
        let request = AchievementBackfillRequest {
            server: server.label.clone(),
            entries: entries.clone(),
            previous_version,
            catalog_version: manifest.catalog_version,
        };
        let input = serde_json::to_string(&request)?;
        let raw = plugin.call::<&str, &str>("achievement_backfill", &input)?;
        let response: AchievementBackfillResponse = serde_json::from_str(raw)?;
        base.db.achievement_backfill_apply_blocking(
            &server.label,
            module,
            manifest.clone(),
            response.values,
            now_secs(),
        )?;
    }
    Ok(())
}

fn call_data_export(
    plugin: &mut extism::Plugin,
    request: &ModuleDataRequest,
) -> Result<ModuleDataResponse> {
    let input = serde_json::to_string(request)?;
    let raw = plugin.call::<&str, &str>("data_export", &input)?;
    let response: ModuleDataResponse = serde_json::from_str(raw)?;
    if response.version != DATA_LIFECYCLE_VERSION {
        anyhow::bail!(
            "unsupported lifecycle response version {}",
            response.version
        );
    }
    Ok(response)
}

fn call_data_delete(
    plugin: &mut extism::Plugin,
    request: &ModuleDataRequest,
) -> Result<ModuleDataDeletePlan> {
    let input = serde_json::to_string(request)?;
    let raw = plugin.call::<&str, &str>("data_delete", &input)?;
    let response: ModuleDataDeletePlan = serde_json::from_str(raw)?;
    if response.version != DATA_LIFECYCLE_VERSION {
        anyhow::bail!(
            "unsupported lifecycle response version {}",
            response.version
        );
    }
    Ok(response)
}

fn lifecycle_request(
    base: &ModuleBase,
    worker: &Worker,
    subject: &DataSubject,
    aliases: &[String],
) -> Result<ModuleDataRequest> {
    Ok(ModuleDataRequest {
        version: DATA_LIFECYCLE_VERSION,
        subject: subject.clone(),
        aliases: aliases.to_vec(),
        entries: base.db.kv_list_module_blocking(&worker.name)?,
    })
}

fn collect_module_exports(
    workers: &[Worker],
    base: &ModuleBase,
    subject: &DataSubject,
    aliases: &[String],
) -> Result<Vec<ModuleDataExport>> {
    let mut exports = Vec::new();
    for module in base.db.lifecycle_modules_blocking()? {
        if !workers
            .iter()
            .any(|worker| worker.name == module && worker.lifecycle)
        {
            anyhow::bail!("waiting for lifecycle module '{module}'");
        }
    }
    for worker in workers.iter().filter(|worker| worker.lifecycle) {
        let request = lifecycle_request(base, worker, subject, aliases)?;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        worker
            .tx
            .send(WorkerMsg::DataExport {
                request,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("{} lifecycle worker stopped", worker.name))?;
        let response = reply_rx
            .recv_timeout(std::time::Duration::from_secs(21))
            .map_err(|error| anyhow::anyhow!("{} lifecycle export: {error}", worker.name))?
            .map_err(|error| anyhow::anyhow!("{} lifecycle export: {error}", worker.name))?;
        if !response.data.is_null() {
            exports.push(ModuleDataExport {
                module: worker.name.clone(),
                data: response.data,
            });
        }
    }
    Ok(exports)
}

fn inspect_profile_modules(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    profile_id: &str,
) -> Result<Vec<ProfileModuleData>> {
    let (aliases, _) = base
        .db
        .profile_identity_links_blocking(server, profile_id)?;
    let aliases = aliases
        .into_iter()
        .map(|alias| alias.nick)
        .collect::<Vec<_>>();
    let subject = DataSubject {
        server: server.into(),
        profile_id: profile_id.into(),
    };
    let mut modules = base.db.lifecycle_modules_blocking()?;
    modules.extend(
        workers
            .iter()
            .filter(|worker| worker.lifecycle)
            .map(|worker| worker.name.clone()),
    );
    modules.sort();
    modules.dedup();

    let mut output = Vec::with_capacity(modules.len());
    for module in modules {
        let Some(worker) = workers
            .iter()
            .find(|worker| worker.name == module && worker.lifecycle)
        else {
            output.push(ProfileModuleData {
                module,
                data: None,
                error: Some("module is not currently loaded".into()),
            });
            continue;
        };
        let request = lifecycle_request(base, worker, &subject, &aliases)?;
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        let result = worker
            .tx
            .send(WorkerMsg::DataExport {
                request,
                reply: reply_tx,
            })
            .map_err(|_| "module worker stopped".to_string())
            .and_then(|()| {
                reply_rx
                    .recv_timeout(std::time::Duration::from_secs(21))
                    .map_err(|_| "module inspection timed out".to_string())?
            });
        match result {
            Ok(response) => output.push(ProfileModuleData {
                module,
                data: (!response.data.is_null()).then_some(response.data),
                error: None,
            }),
            Err(error) => output.push(ProfileModuleData {
                module,
                data: None,
                error: Some(error),
            }),
        }
    }
    Ok(output)
}

fn plan_profile_module_reset(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    profile_id: &str,
    module: &str,
) -> Result<ProfileModuleResetPlan> {
    let worker = workers
        .iter()
        .find(|worker| worker.name == module && worker.lifecycle)
        .ok_or_else(|| anyhow::anyhow!("module '{module}' is not currently available"))?;
    let (aliases, _) = base
        .db
        .profile_identity_links_blocking(server, profile_id)?;
    let aliases = aliases
        .into_iter()
        .map(|alias| alias.nick)
        .collect::<Vec<_>>();
    let subject = DataSubject {
        server: server.into(),
        profile_id: profile_id.into(),
    };
    let request = lifecycle_request(base, worker, &subject, &aliases)?;
    let expected_entries = request.entries.clone();
    let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
    worker
        .tx
        .send(WorkerMsg::DataDelete {
            request,
            reply: reply_tx,
        })
        .map_err(|_| anyhow::anyhow!("module '{module}' worker stopped"))?;
    let plan = reply_rx
        .recv_timeout(std::time::Duration::from_secs(21))
        .map_err(|error| anyhow::anyhow!("module reset preview timed out: {error}"))?
        .map_err(|error| anyhow::anyhow!("module reset preview failed: {error}"))?;
    Ok(ProfileModuleResetPlan {
        module: module.into(),
        profile_id: profile_id.into(),
        expected_entries,
        mutations: plan.mutations,
    })
}

fn process_deletion(workers: &[Worker], base: &ModuleBase, job: &DataDeletionJob) -> Result<bool> {
    let (aliases, _) = base
        .db
        .profile_identity_links_blocking(&job.server, &job.profile_id)?;
    let aliases = aliases
        .into_iter()
        .map(|alias| alias.nick)
        .collect::<Vec<_>>();
    let subject = DataSubject {
        server: job.server.clone(),
        profile_id: job.profile_id.clone(),
    };

    for module in base.db.deletion_module_pending_blocking(&job.id)? {
        let Some(worker) = workers
            .iter()
            .find(|worker| worker.name == module && worker.lifecycle)
        else {
            anyhow::bail!("waiting for lifecycle module '{module}'");
        };
        let request = lifecycle_request(base, worker, &subject, &aliases)?;
        let allowed_keys = request
            .entries
            .iter()
            .map(|entry| entry.key.clone())
            .collect();
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        worker
            .tx
            .send(WorkerMsg::DataDelete {
                request,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("{module} lifecycle worker stopped"))?;
        let plan = reply_rx
            .recv_timeout(std::time::Duration::from_secs(21))
            .map_err(|error| anyhow::anyhow!("{module} lifecycle deletion: {error}"))?
            .map_err(|error| anyhow::anyhow!("{module} lifecycle deletion: {error}"))?;
        base.db
            .kv_apply_module_blocking(&module, allowed_keys, plan.mutations)?;
        base.db.deletion_module_done_blocking(&job.id, &module)?;
    }

    if base
        .db
        .deletion_module_pending_blocking(&job.id)?
        .is_empty()
    {
        base.db
            .deletion_finish_blocking(&job.id, &job.server, &job.profile_id, now_secs())?;
        return Ok(true);
    }
    Ok(false)
}

fn resume_deletions(workers: &[Worker], base: &ModuleBase) {
    let jobs = match base.db.deletion_pending_blocking() {
        Ok(jobs) => jobs,
        Err(error) => {
            base.log
                .error("data", format!("cannot load deletion journal: {error}"));
            return;
        }
    };
    for job in jobs {
        match process_deletion(workers, base, &job) {
            Ok(true) => base
                .log
                .info("data", format!("deletion {} completed", job.id)),
            Ok(false) => {}
            Err(error) => {
                let message = error.to_string();
                let _ = base
                    .db
                    .deletion_fail_blocking(&job.id, &message, now_secs());
                base.log.error(
                    "data",
                    format!("deletion {} remains pending: {message}", job.id),
                );
            }
        }
    }
}

fn themed_data(base: &ModuleBase, key: &str, default: &str, vars: &[(&str, &str)]) -> String {
    base.theme.lock().unwrap().resolve(
        "data",
        key,
        &[default.to_string()],
        &vars
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect::<Vec<_>>(),
    )
}

fn send_pm(base: &ModuleBase, server: &str, nick: &str, text: String) {
    let sender = base.registry.lock().unwrap().get(server).cloned();
    let Some(sender) = sender else {
        base.log.error(
            "data",
            "cannot send lifecycle response: network unavailable",
        );
        return;
    };
    if let Err(error) = sender.blocking_send(IrcAction::Privmsg {
        target: nick.to_string(),
        text,
    }) {
        base.log
            .error("data", format!("cannot send lifecycle response: {error}"));
    }
}

fn lifecycle_command(base: &ModuleBase, text: &str) -> Option<String> {
    let token = text.split_whitespace().next()?;
    let target = base.commands.lock().unwrap().resolve(token)?;
    (target.module == "data").then_some(target.canonical)
}

fn handle_lifecycle_command(
    workers: &[Worker],
    base: &ModuleBase,
    env: &EventEnvelope,
    export_cooldowns: &mut HashMap<(String, String), std::time::Instant>,
) -> bool {
    let Event::Message(message) = &env.event else {
        return false;
    };
    let Some(command) = lifecycle_command(base, message.text.trim()) else {
        return false;
    };
    let nick = message.nick.as_str();
    if !message.is_private {
        send_pm(
            base,
            &env.server,
            nick,
            themed_data(
                base,
                "private_only",
                "For privacy, please send that command to me in a private message.",
                &[],
            ),
        );
        return true;
    }

    let result = if command == "mydata" {
        handle_mydata(workers, base, env, message, export_cooldowns)
    } else if command == "data" {
        handle_admin_data(workers, base, env, message)
    } else {
        Ok(())
    };
    if let Err(error) = result {
        base.log
            .error("data", format!("lifecycle command failed: {error}"));
        send_pm(
            base,
            &env.server,
            nick,
            themed_data(
                base,
                "error",
                "I couldn't complete that data request. The operator can inspect the error log.",
                &[],
            ),
        );
    }
    true
}

fn handle_mydata(
    workers: &[Worker],
    base: &ModuleBase,
    env: &EventEnvelope,
    message: &jeeves_abi::MessagePayload,
    export_cooldowns: &mut HashMap<(String, String), std::time::Instant>,
) -> Result<()> {
    let arg = message
        .text
        .split_once(char::is_whitespace)
        .map(|(_, arg)| arg.trim())
        .unwrap_or("summary");
    match arg
        .split_whitespace()
        .next()
        .unwrap_or("summary")
        .to_ascii_lowercase()
        .as_str()
    {
        "summary" => send_data_summary(workers, base, &env.server, &message.nick, &message.nick),
        "export" => {
            let profile = base
                .db
                .profile_get_blocking(&env.server, &message.nick)?
                .ok_or_else(|| anyhow::anyhow!("unknown profile"))?;
            let key = (env.server.clone(), profile.id);
            let now = std::time::Instant::now();
            export_cooldowns
                .retain(|_, used| now.duration_since(*used) < std::time::Duration::from_secs(60));
            match export_cooldowns.entry(key) {
                std::collections::hash_map::Entry::Occupied(_) => {
                    send_pm(
                        base,
                        &env.server,
                        &message.nick,
                        themed_data(
                            base,
                            "export_cooldown",
                            "Please wait a minute before requesting another data export.",
                            &[],
                        ),
                    );
                    Ok(())
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(now);
                    send_data_export(workers, base, &env.server, &message.nick, &message.nick)
                }
            }
        }
        "delete" => initiate_deletion(
            base,
            &env.server,
            &message.nick,
            &message.user_id,
            &message.nick,
            "!mydata",
        ),
        "confirm" => {
            let token = arg.split_whitespace().nth(1).unwrap_or("");
            confirm_deletion(
                workers,
                base,
                &env.server,
                &message.nick,
                &message.user_id,
                token,
                false,
            )
        }
        _ => {
            send_pm(
                base,
                &env.server,
                &message.nick,
                themed_data(
                    base,
                    "usage",
                    "Usage: !mydata [summary | export | delete | confirm <token>]",
                    &[],
                ),
            );
            Ok(())
        }
    }
}

fn handle_admin_data(
    workers: &[Worker],
    base: &ModuleBase,
    env: &EventEnvelope,
    message: &jeeves_abi::MessagePayload,
) -> Result<()> {
    if !message
        .role
        .is_some_and(|role| role.satisfies(Role::SuperAdmin))
    {
        send_pm(
            base,
            &env.server,
            &message.nick,
            themed_data(
                base,
                "denied",
                "This command is restricted to super-admins.",
                &[],
            ),
        );
        return Ok(());
    }
    let args = message
        .text
        .split_once(char::is_whitespace)
        .map(|(_, args)| args.trim())
        .unwrap_or("");
    let parts = args.split_whitespace().collect::<Vec<_>>();
    if parts
        .first()
        .is_some_and(|part| part.eq_ignore_ascii_case("confirm"))
    {
        return confirm_deletion(
            workers,
            base,
            &env.server,
            &message.nick,
            &message.user_id,
            parts.get(1).copied().unwrap_or(""),
            true,
        );
    }
    if parts
        .first()
        .is_some_and(|part| part.eq_ignore_ascii_case("pending"))
    {
        return send_pending_deletions(base, &env.server, &message.nick);
    }
    let (Some(target), Some(action)) = (parts.first(), parts.get(1)) else {
        send_pm(
            base,
            &env.server,
            &message.nick,
            themed_data(
                base,
                "admin_usage",
                "Usage: !data <nick> <summary | export | delete> | !data confirm <token> | !data pending",
                &[],
            ),
        );
        return Ok(());
    };
    match action.to_ascii_lowercase().as_str() {
        "summary" => send_data_summary(workers, base, &env.server, target, &message.nick),
        "export" => send_data_export(workers, base, &env.server, target, &message.nick),
        "delete" => initiate_deletion(
            base,
            &env.server,
            target,
            &message.user_id,
            &message.nick,
            "!data",
        ),
        _ => {
            send_pm(
                base,
                &env.server,
                &message.nick,
                themed_data(
                    base,
                    "admin_usage",
                    "Usage: !data <nick> <summary | export | delete> | !data confirm <token> | !data pending",
                    &[],
                ),
            );
            Ok(())
        }
    }
}

fn send_pending_deletions(base: &ModuleBase, server: &str, recipient: &str) -> Result<()> {
    let jobs = base.db.deletion_pending_blocking()?;
    let mut entries = Vec::new();
    for job in jobs.iter().take(3) {
        let remaining = base.db.deletion_module_pending_blocking(&job.id)?.len();
        entries.push(format!(
            "{} ({}, {} modules remaining)",
            job.id, job.status, remaining
        ));
    }
    let detail = if entries.is_empty() {
        "none".to_string()
    } else {
        entries.join("; ")
    };
    send_pm(
        base,
        server,
        recipient,
        themed_data(
            base,
            "pending",
            "Active deletion workflows: {detail}",
            &[("detail", &detail)],
        ),
    );
    Ok(())
}

fn collect_full_export(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    target: &str,
) -> Result<jeeves_abi::ProfileDataExport> {
    let mut export = data_lifecycle::collect_profile_blocking(&base.db, server, target)?;
    let aliases = export
        .aliases
        .iter()
        .map(|alias| alias.nick.clone())
        .collect::<Vec<_>>();
    export.modules = collect_module_exports(workers, base, &export.subject, &aliases)?;
    Ok(export)
}

fn send_data_summary(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    target: &str,
    recipient: &str,
) -> Result<()> {
    let export = collect_full_export(workers, base, server, target)?;
    let fields = [
        export.profile.title.is_some(),
        export.profile.birthday.is_some(),
        export.profile.pronoun_subject.is_some(),
        export.profile.location_display.is_some(),
    ]
    .into_iter()
    .filter(|set| *set)
    .count();
    let detail = format!("{} profile fields, {} nick aliases, {} account bindings, {} scheduled items, and {} module data sections", fields, export.aliases.len(), export.accounts.len(), export.scheduled_jobs.len(), export.modules.len());
    send_pm(
        base,
        server,
        recipient,
        themed_data(
            base,
            "summary",
            "Stored data for {target}: {detail}.",
            &[("target", target), ("detail", &detail)],
        ),
    );
    Ok(())
}

fn send_data_export(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    target: &str,
    recipient: &str,
) -> Result<()> {
    let export = collect_full_export(workers, base, server, target)?;
    let path = data_lifecycle::write_private_json(&base.export_dir, &export)?;
    let path = path.display().to_string();
    send_pm(
        base,
        server,
        recipient,
        themed_data(
            base,
            "exported",
            "Your data export has been written for the operator: {path}",
            &[("path", &path)],
        ),
    );
    Ok(())
}

fn initiate_deletion(
    base: &ModuleBase,
    server: &str,
    target: &str,
    requester_id: &str,
    recipient: &str,
    confirm_command: &str,
) -> Result<()> {
    let profile = base
        .db
        .profile_get_blocking(server, target)?
        .ok_or_else(|| anyhow::anyhow!("unknown profile"))?;
    let id = uuid::Uuid::new_v4().to_string();
    let token = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
    let now = now_secs();
    base.db.deletion_create_blocking(
        DataDeletionJob {
            id,
            server: server.to_string(),
            profile_id: profile.id,
            requester_profile_id: requester_id.to_string(),
            status: "awaiting_confirmation".into(),
            confirmation_token: token.clone(),
            confirmation_expires_at: now + 10 * 60,
            created_at: now,
            updated_at: now,
            last_error: None,
        },
        base.db.lifecycle_modules_blocking()?,
    )?;
    send_pm(
        base,
        server,
        recipient,
        themed_data(
            base,
            "confirm_delete",
            "This permanently deletes live data. Confirm within 10 minutes with: {command}",
            &[("command", &format!("{confirm_command} confirm {token}"))],
        ),
    );
    Ok(())
}

fn confirm_deletion(
    workers: &[Worker],
    base: &ModuleBase,
    server: &str,
    recipient: &str,
    requester_id: &str,
    token: &str,
    allow_other_profile: bool,
) -> Result<()> {
    let Some(job) =
        base.db
            .deletion_confirm_blocking(token, requester_id, allow_other_profile, now_secs())?
    else {
        send_pm(
            base,
            server,
            recipient,
            themed_data(
                base,
                "invalid_confirmation",
                "That confirmation is invalid or expired.",
                &[],
            ),
        );
        return Ok(());
    };
    match process_deletion(workers, base, &job) {
        Ok(true) => send_pm(
            base,
            server,
            recipient,
            themed_data(
                base,
                "deleted",
                "The live profile data has been deleted.",
                &[],
            ),
        ),
        Ok(false) => send_pm(
            base,
            server,
            recipient,
            themed_data(
                base,
                "deletion_pending",
                "Deletion is recorded and will resume automatically.",
                &[],
            ),
        ),
        Err(error) => {
            base.db
                .deletion_fail_blocking(&job.id, &error.to_string(), now_secs())?;
            base.log.error(
                "data",
                format!("deletion {} remains pending: {error}", job.id),
            );
            send_pm(
                base,
                server,
                recipient,
                themed_data(
                    base,
                    "deletion_pending",
                    "Deletion is recorded and will resume automatically.",
                    &[],
                ),
            );
        }
    }
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
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
        // `enabled` gates ambient traffic, but an explicitly targeted command remains usable.
        // This lets modules such as roadtrip and youtube keep passive announcements opt-in
        // without also making their manual commands disappear.
        let is_targeted_command = target
            .as_ref()
            .is_some_and(|target| target.module == worker.name);
        if !enabled && !is_targeted_command {
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
    let worker = workers
        .iter()
        .find(|worker| worker.name == delivery.module)
        .filter(|_| enabled);
    let Some(worker) = worker else {
        delivery.completion.finish(false);
        return;
    };
    let message = WorkerMsg::Scheduled {
        envelope: Arc::new(delivery.envelope),
        completion: delivery.completion,
    };
    if let Err(
        std::sync::mpsc::TrySendError::Full(WorkerMsg::Scheduled { completion, .. })
        | std::sync::mpsc::TrySendError::Disconnected(WorkerMsg::Scheduled { completion, .. }),
    ) = worker.tx.try_send(message)
    {
        completion.finish(false);
    }
}

fn dispatch_one(
    plugin: &mut extism::Plugin,
    base: &ModuleBase,
    name: &str,
    env: &EventEnvelope,
) -> bool {
    let hook = match env.event {
        Event::Message(_) => "on_message",
        _ => "on_event",
    };
    let payload = match serde_json::to_string(env) {
        Ok(p) => p,
        Err(e) => {
            base.log
                .error("modules", format!("event serialize failed: {e}"));
            return false;
        }
    };
    if !plugin.function_exists(hook) {
        return false;
    }
    match plugin.call::<&str, &str>(hook, &payload) {
        Ok(_) => true,
        Err(error) => {
            base.log
                .error("modules", format!("{name}: {hook} failed: {error}"));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::MessagePayload;
    use std::fs;
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

    fn lifecycle_test_base() -> (ModuleBase, mpsc::Receiver<IrcAction>) {
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(8);
        let (control, _) = mpsc::channel(1);
        let (actions, rx) = mpsc::channel(8);
        let registry = Arc::new(Mutex::new(HashMap::from([("net".into(), actions)])));
        let scheduler = test_scheduler(&db, &log);
        let base = ModuleBase {
            registry,
            control,
            db,
            log,
            theme: crate::theme::ThemeStore::open(
                std::env::temp_dir()
                    .join(format!("jeeves-data-theme-{}.toml", uuid::Uuid::new_v4())),
            ),
            names: Arc::new(Mutex::new(Vec::new())),
            commands: CommandRegistry::shared(),
            achievements: Arc::new(Mutex::new(HashMap::new())),
            achievement_announcements: Arc::new(Mutex::new(HashMap::new())),
            settings: SettingRegistry::shared(),
            scheduler,
            capabilities_path: PathBuf::new(),
            export_dir: std::env::temp_dir()
                .join(format!("jeeves-lifecycle-test-{}", uuid::Uuid::new_v4())),
        };
        publish_commands(&base, &[]);
        (base, rx)
    }

    #[test]
    fn lifecycle_commands_leave_channels_and_reply_privately() {
        let (base, mut actions) = lifecycle_test_base();
        assert!(handle_lifecycle_command(
            &[],
            &base,
            &envelope("net", "!mydata", false),
            &mut HashMap::new(),
        ));
        let IrcAction::Privmsg { target, text } = actions.blocking_recv().unwrap() else {
            panic!("expected private response")
        };
        assert_eq!(target, "tester");
        assert!(text.contains("private message"));
    }

    #[test]
    fn admin_lifecycle_command_requires_super_admin() {
        let (base, mut actions) = lifecycle_test_base();
        let mut event = envelope("net", "!data Alice summary", true);
        let Event::Message(message) = &mut event.event else {
            unreachable!()
        };
        message.role = Some(Role::Admin);

        assert!(handle_lifecycle_command(
            &[],
            &base,
            &event,
            &mut HashMap::new(),
        ));
        let IrcAction::Privmsg { target, text } = actions.blocking_recv().unwrap() else {
            panic!("expected private response")
        };
        assert_eq!(target, "tester");
        assert!(text.contains("super-admin"));
    }

    #[test]
    fn self_export_is_throttled_per_profile() {
        let (base, mut actions) = lifecycle_test_base();
        base.db.profile_ensure_blocking("net", "tester", 1).unwrap();
        let event = envelope("net", "!mydata export", true);
        let mut cooldowns = HashMap::new();

        assert!(handle_lifecycle_command(&[], &base, &event, &mut cooldowns,));
        let _first = actions.blocking_recv().unwrap();
        assert!(handle_lifecycle_command(&[], &base, &event, &mut cooldowns,));
        let IrcAction::Privmsg { text, .. } = actions.blocking_recv().unwrap() else {
            panic!("expected private response")
        };
        assert!(text.contains("wait a minute"));
        fs::remove_dir_all(&base.export_dir).unwrap();
    }

    #[test]
    fn ai_wasm_loads_and_advertises_bounded_settings() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/ai.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/ai.wasm not built");
            return;
        }
        let (base, _) = lifecycle_test_base();
        let mut plugin = load_one(&path, "ai", &base).unwrap();
        let raw = plugin.call::<&str, &str>("settings", "").unwrap();
        let manifest: SettingsManifest = serde_json::from_str(raw).unwrap();
        assert_eq!(manifest.version, SETTINGS_MANIFEST_VERSION);
        assert!(manifest.settings.iter().any(|setting| {
            setting.key == "channel_enabled"
                && setting.default == "false"
                && matches!(&setting.kind, jeeves_abi::SettingKind::Boolean)
        }));
        assert!(manifest.settings.iter().any(|setting| {
            setting.key == "max_tokens"
                && matches!(
                    &setting.kind,
                    jeeves_abi::SettingKind::Integer {
                        min: 16,
                        max: 1_024
                    }
                )
        }));
    }

    #[test]
    fn youtube_wasm_loads_and_keeps_passive_announcements_opt_in() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/youtube.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/youtube.wasm not built");
            return;
        }
        let (base, _) = lifecycle_test_base();
        let mut plugin = load_one(&path, "youtube", &base).unwrap();

        let raw = plugin.call::<&str, &str>("commands", "").unwrap();
        let commands: CommandManifest = serde_json::from_str(raw).unwrap();
        assert!(commands.commands.iter().any(|command| {
            command.name == "yt" && command.aliases.iter().any(|alias| alias == "youtube")
        }));

        let raw = plugin.call::<&str, &str>("settings", "").unwrap();
        let settings: SettingsManifest = serde_json::from_str(raw).unwrap();
        assert!(settings.settings.iter().any(|setting| {
            setting.key == "enabled"
                && setting.default == "false"
                && matches!(&setting.kind, jeeves_abi::SettingKind::Boolean)
        }));
    }

    #[test]
    fn banter_wasm_handles_crow_and_sailor_triggers_independently() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/banter.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/banter.wasm not built");
            return;
        }
        let (mut base, mut actions) = lifecycle_test_base();
        base.capabilities_path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../module-capabilities.toml"
        ));
        let worker = spawn_worker(path, "banter".into(), base.clone()).unwrap();
        base.settings.lock().unwrap().replace_specs(
            worker
                .settings
                .iter()
                .cloned()
                .map(|setting| (worker.name.clone(), setting))
                .collect(),
        );
        base.settings.lock().unwrap().set_override(
            "banter",
            "enabled",
            jeeves_abi::SettingScope::Channel,
            "net",
            "#chan",
            Some("true".into()),
        );

        dispatch(
            std::slice::from_ref(&worker),
            &base,
            &envelope("net", "a most definite CAW!", false),
        );
        let IrcAction::Privmsg { target, text } = actions.blocking_recv().unwrap() else {
            panic!("expected crow banter")
        };
        assert_eq!(target, "#chan");
        assert!(text.contains("tester"));

        let mut sail = envelope("net", "SAIL!", false);
        let Event::Message(message) = &mut sail.event else {
            unreachable!()
        };
        message.nick = "witeshark2".into();
        message.display = "witeshark2".into();
        dispatch(std::slice::from_ref(&worker), &base, &sail);
        let IrcAction::Privmsg { target, text } = actions.blocking_recv().unwrap() else {
            panic!("expected sailing banter")
        };
        assert_eq!(target, "#chan");
        assert!(text.contains("witeshark2"));

        let _ = worker.tx.try_send(WorkerMsg::Shutdown);
    }

    #[test]
    fn profile_admin_inspects_and_plans_scoped_module_reset() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/ai.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/ai.wasm not built");
            return;
        }
        let (base, _) = lifecycle_test_base();
        base.db
            .profile_ensure_blocking("net", "Alice", 100)
            .unwrap();
        let profile = base
            .db
            .profile_get_blocking("net", "Alice")
            .unwrap()
            .unwrap();
        let key = format!("cooldown:{}:{}", hex("net"), hex(&profile.id));
        base.db.kv_set_blocking("ai", &key, "123").unwrap();
        let worker = spawn_worker(path, "ai".into(), base.clone()).unwrap();

        let inspected =
            inspect_profile_modules(std::slice::from_ref(&worker), &base, "net", &profile.id)
                .unwrap();
        let ai = inspected.iter().find(|entry| entry.module == "ai").unwrap();
        assert!(ai.error.is_none());
        assert!(ai.data.is_some());

        let plan = plan_profile_module_reset(
            std::slice::from_ref(&worker),
            &base,
            "net",
            &profile.id,
            "ai",
        )
        .unwrap();
        assert_eq!(plan.mutation_count(), 1);
        base.db
            .kv_apply_module_checked_blocking(&plan.module, plan.expected_entries, plan.mutations)
            .unwrap();
        assert_eq!(base.db.kv_get_blocking("ai", &key).unwrap(), None);
        let _ = worker.tx.try_send(WorkerMsg::Shutdown);
    }

    #[test]
    fn history_lifecycle_hook_rejects_malformed_state_and_isolates_networks() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/history.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/history.wasm not built");
            return;
        }
        let (base, _) = lifecycle_test_base();
        let mut plugin = load_one(&path, "history", &base).unwrap();
        let subject = DataSubject {
            server: "net".into(),
            profile_id: "profile-id".into(),
        };
        let malformed = ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: subject.clone(),
            aliases: vec!["Alice".into()],
            entries: vec![jeeves_abi::ModuleKvEntry {
                key: format!("quotes:{}:room:book", hex("net")),
                value: "not-json".into(),
            }],
        };
        assert!(call_data_delete(&mut plugin, &malformed).is_err());

        let other_network = ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject,
            aliases: vec!["Alice".into()],
            entries: vec![jeeves_abi::ModuleKvEntry {
                key: format!("seen:{}:room:profile", hex("othernet")),
                value: serde_json::json!({
                    "user_id": "profile-id", "nick": "Alice", "display": "Alice",
                    "text": "private", "timestamp": 100
                })
                .to_string(),
            }],
        };
        assert!(call_data_delete(&mut plugin, &other_network)
            .unwrap()
            .mutations
            .is_empty());

        let legacy_alias = ModuleDataRequest {
            version: DATA_LIFECYCLE_VERSION,
            subject: DataSubject {
                server: "net".into(),
                profile_id: "profile-id".into(),
            },
            aliases: vec!["Alice".into(), "AliceAway".into()],
            entries: vec![jeeves_abi::ModuleKvEntry {
                key: format!("seen:{}:room:legacy", hex("net")),
                value: serde_json::json!({
                    "user_id": "Alice", "nick": "Alice", "display": "Alice",
                    "text": "legacy", "timestamp": 100
                })
                .to_string(),
            }],
        };
        assert_eq!(
            call_data_delete(&mut plugin, &legacy_alias)
                .unwrap()
                .mutations
                .len(),
            1
        );
    }

    #[test]
    fn deletion_resumes_when_an_absent_module_returns() {
        let path = PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../modules/reminders.wasm"
        ));
        if !path.exists() {
            eprintln!("skipping: modules/reminders.wasm not built");
            return;
        }
        let (base, _) = lifecycle_test_base();
        base.db
            .profile_ensure_blocking("net", "Alice", 100)
            .unwrap();
        let profile = base
            .db
            .profile_get_blocking("net", "Alice")
            .unwrap()
            .unwrap();
        base.db
            .kv_set_blocking("reminders", &format!("sequence:net:{}", profile.id), "4")
            .unwrap();
        let job = DataDeletionJob {
            id: "resume-job".into(),
            server: "net".into(),
            profile_id: profile.id.clone(),
            requester_profile_id: profile.id,
            status: "pending".into(),
            confirmation_token: "resume-token".into(),
            confirmation_expires_at: 200,
            created_at: 100,
            updated_at: 100,
            last_error: None,
        };
        base.db
            .deletion_create_blocking(job.clone(), vec!["reminders".into()])
            .unwrap();

        assert!(process_deletion(&[], &base, &job).is_err());
        let worker = spawn_worker(path, "reminders".into(), base.clone()).unwrap();
        assert!(process_deletion(std::slice::from_ref(&worker), &base, &job).unwrap());
        assert!(base
            .db
            .profile_get_blocking("net", "Alice")
            .unwrap()
            .is_none());
        let _ = worker.tx.send(WorkerMsg::Shutdown);
    }

    fn hex(value: &str) -> String {
        value.bytes().map(|byte| format!("{byte:02x}")).collect()
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
        assert!(fishing.contains("irc_casefold"));
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
                "log",
                "irc_casefold",
                "award_stats",
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
            achievements: Arc::new(Mutex::new(HashMap::new())),
            achievement_announcements: Arc::new(Mutex::new(HashMap::new())),
            settings: SettingRegistry::shared(),
            scheduler,
            capabilities_path: PathBuf::new(),
            export_dir: std::env::temp_dir(),
        };
        let (weather_tx, weather_rx) = std::sync::mpsc::sync_channel(1);
        let (history_tx, history_rx) = std::sync::mpsc::sync_channel(1);
        let workers = vec![
            Worker {
                name: "weather".into(),
                commands: Vec::new(),
                settings: Vec::new(),
                achievements: None,
                lifecycle: false,
                tx: weather_tx,
            },
            Worker {
                name: "history".into(),
                commands: Vec::new(),
                settings: Vec::new(),
                achievements: None,
                lifecycle: false,
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
        let commands = CommandRegistry::shared();
        commands.lock().unwrap().replace_specs(
            vec![(
                "weather".into(),
                CommandSpec {
                    name: "weather".into(),
                    description: "Weather lookup.".into(),
                    usage: "!weather <place>".into(),
                    aliases: Vec::new(),
                },
            )],
            Default::default(),
        );
        let base = ModuleBase {
            registry: Arc::new(Mutex::new(HashMap::new())),
            control,
            db,
            log,
            theme: crate::theme::ThemeStore::open("/tmp/jeeves-settings-test-theme.toml"),
            names: Arc::new(Mutex::new(Vec::new())),
            commands,
            achievements: Arc::new(Mutex::new(HashMap::new())),
            achievement_announcements: Arc::new(Mutex::new(HashMap::new())),
            settings,
            scheduler,
            capabilities_path: PathBuf::new(),
            export_dir: std::env::temp_dir(),
        };
        let (tx, rx) = std::sync::mpsc::sync_channel(2);
        let workers = vec![Worker {
            name: "weather".into(),
            commands: Vec::new(),
            settings: Vec::new(),
            achievements: None,
            lifecycle: false,
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

        let mut command = envelope("net", "!weather New York", false);
        let Event::Message(message) = &mut command.event else {
            unreachable!()
        };
        message.target = "#quiet".into();
        dispatch(&workers, &base, &command);
        assert!(
            rx.try_recv().is_ok(),
            "a directly targeted command remains available"
        );
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
        let profile = db
            .profile_resolve(
                "testnet",
                "tester",
                Some("tester-account".into()),
                now_secs(),
            )
            .await
            .unwrap();
        let data_db = db.clone();
        let sequence_key = format!("sequence:testnet:{}", profile.id);
        let seed_db = db.clone();
        let seed_key = sequence_key.clone();
        tokio::task::spawn_blocking(move || {
            seed_db
                .kv_set_blocking("reminders", &seed_key, "7")
                .unwrap();
        })
        .await
        .unwrap();
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
        let host = spawn(
            ModulePaths {
                modules_dir: dir.into(),
                capabilities_path: capabilities.into(),
                export_dir: std::env::temp_dir(),
            },
            registry,
            control_tx,
            db,
            log,
            theme,
        );

        // !ping -> reply "pong" to the channel on the originating network.
        host.events
            .send(envelope("testnet", "!ping", false))
            .await
            .unwrap();
        let act = tokio::time::timeout(Duration::from_secs(30), actions_rx.recv())
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
        assert!(registered
            .iter()
            .any(|command| command.module == "data" && command.name == "mydata"));
        assert!(registered
            .iter()
            .any(|command| command.module == "roadtrip" && command.name == "me"));

        // The host-owned PM command calls every loaded lifecycle hook and returns only by PM.
        let mut mydata = envelope("testnet", "!mydata summary", true);
        let Event::Message(message) = &mut mydata.event else {
            unreachable!()
        };
        message.user_id = profile.id.clone();
        host.events.send(mydata).await.unwrap();
        let act = tokio::time::timeout(Duration::from_secs(25), actions_rx.recv())
            .await
            .expect("timed out waiting for data summary")
            .unwrap();
        let IrcAction::Privmsg { target, text } = act else {
            panic!("expected private data summary")
        };
        assert_eq!(target, "tester");
        assert!(text.contains("Stored data"), "response: {text}");

        // Confirmation drives all module hooks, removes owned host data, and finishes the journal.
        let mut deletion = envelope("testnet", "!mydata delete", true);
        let Event::Message(message) = &mut deletion.event else {
            unreachable!()
        };
        message.user_id = profile.id.clone();
        host.events.send(deletion).await.unwrap();
        let prompt = tokio::time::timeout(Duration::from_secs(25), actions_rx.recv())
            .await
            .expect("timed out waiting for deletion prompt")
            .unwrap();
        let IrcAction::Privmsg { text, .. } = prompt else {
            panic!("expected private deletion prompt")
        };
        let token = text.split_whitespace().last().unwrap();
        let mut confirmation = envelope("testnet", &format!("!mydata confirm {token}"), true);
        let Event::Message(message) = &mut confirmation.event else {
            unreachable!()
        };
        message.user_id = profile.id.clone();
        host.events.send(confirmation).await.unwrap();
        let completed = tokio::time::timeout(Duration::from_secs(25), actions_rx.recv())
            .await
            .expect("timed out waiting for deletion completion")
            .unwrap();
        let IrcAction::Privmsg { text, .. } = completed else {
            panic!("expected private deletion result")
        };
        assert!(text.contains("deleted"), "response: {text}");
        assert!(data_db
            .profile_get("testnet", "tester")
            .await
            .unwrap()
            .is_none());
        let check_db = data_db.clone();
        assert!(tokio::task::spawn_blocking(move || check_db
            .kv_get_blocking("reminders", &sequence_key)
            .unwrap())
        .await
        .unwrap()
        .is_none());
        let registered_settings = host.settings.lock().unwrap().snapshot();
        assert!(registered_settings.iter().any(|setting| {
            setting.module == "memos" && setting.spec.key == "retention_seconds"
        }));
        assert!(registered_settings.iter().any(|setting| {
            setting.module == "banter"
                && setting.spec.key == "sailor_nick"
                && setting.spec.default == "witeshark2"
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
