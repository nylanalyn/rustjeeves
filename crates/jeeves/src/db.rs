//! SQLite persistence behind an actor task.
//!
//! `rusqlite` is synchronous, so a single [`rusqlite::Connection`] lives on a dedicated OS thread
//! and is the *only* thing that touches the database. Async callers talk to it through
//! [`DbHandle`], which sends [`DbRequest`]s over a channel and awaits a oneshot reply.

use crate::casemapping::{CaseMapping, CaseMappingRegistry};
use crate::commands::AliasOverrides;
use crate::config::{AdminEntry, ServerConfig};
use crate::log_bus::LogEvent;
use crate::settings::{scope_name, SettingOverride};
use anyhow::{anyhow, Result};
use jeeves_abi::{
    AchievementBackfillMarker, AchievementDataExport, AchievementDedupExport, AchievementManifest,
    AchievementModuleProgress, AchievementProfileSummary, AchievementProgress, AchievementSetMax,
    AchievementStatValue, AchievementUnlock, AwardStatsRequest, AwardStatsResponse, Category,
    Level, ModuleKvEntry, ModuleKvMutation, PrestigeRank, Profile, ProfileAliasExport,
    ProfileUpdate, Role, ScheduledJob, SettingScope,
};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;
use tokio::sync::{mpsc, oneshot};

/// Requests the DB actor understands. Each carries a oneshot sender for its reply.
enum DbRequest {
    ConfigGet {
        key: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    ConfigSet {
        key: String,
        value: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    BackupTo {
        path: String,
        reply: oneshot::Sender<Result<()>>,
    },
    LoadAliasOverrides(oneshot::Sender<Result<AliasOverrides>>),
    SetAliasOverride {
        module: String,
        command: String,
        aliases: Option<Vec<String>>,
        reply: oneshot::Sender<Result<()>>,
    },
    LoadSettingOverrides(oneshot::Sender<Result<Vec<SettingOverride>>>),
    GetSettingOverride {
        module: String,
        key: String,
        scope: SettingScope,
        server: String,
        channel: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    SetSettingOverride {
        module: String,
        key: String,
        scope: SettingScope,
        server: String,
        channel: String,
        value: Option<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    LoadScheduledJobs(oneshot::Sender<Result<Vec<ScheduledJob>>>),
    SetScheduledJob(Box<ScheduledJob>, oneshot::Sender<Result<()>>),
    DeleteScheduledJob {
        module: String,
        id: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    LoadServers(oneshot::Sender<Result<Vec<ServerConfig>>>),
    UpsertServer(Box<ServerConfig>, oneshot::Sender<Result<i64>>),
    DeleteServer(i64, oneshot::Sender<Result<()>>),
    KvGet {
        module: String,
        key: String,
        reply: oneshot::Sender<Result<Option<String>>>,
    },
    KvSet {
        module: String,
        key: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    KvListModule {
        module: String,
        reply: oneshot::Sender<Result<Vec<ModuleKvEntry>>>,
    },
    KvApplyModule {
        module: String,
        allowed_keys: Vec<String>,
        mutations: Vec<ModuleKvMutation>,
        reply: oneshot::Sender<Result<()>>,
    },
    KvApplyModuleChecked {
        module: String,
        expected: Vec<ModuleKvEntry>,
        mutations: Vec<ModuleKvMutation>,
        reply: oneshot::Sender<Result<()>>,
    },
    AppendLog(LogEvent, oneshot::Sender<Result<()>>),
    ResolveRole {
        server_label: String,
        nick: String,
        hostmask: String,
        account: Option<String>,
        reply: oneshot::Sender<Result<Option<Role>>>,
    },
    LoadAdmins(i64, oneshot::Sender<Result<Vec<AdminEntry>>>),
    UpsertAdmin(i64, AdminEntry, oneshot::Sender<Result<()>>),
    DeleteAdmin(i64, String, oneshot::Sender<Result<()>>),
    ProfileEnsure {
        server: String,
        nick: String,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ProfileResolve {
        server: String,
        nick: String,
        account: Option<String>,
        now: i64,
        reply: oneshot::Sender<Result<Profile>>,
    },
    ProfileBindNick {
        server: String,
        old_nick: String,
        new_nick: String,
        account: Option<String>,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    ProfileGet {
        server: String,
        nick: String,
        reply: oneshot::Sender<Result<Option<Profile>>>,
    },
    ProfileList(oneshot::Sender<Result<Vec<Profile>>>),
    ProfileIdentityLinks {
        server: String,
        profile_id: String,
        reply: oneshot::Sender<Result<(Vec<ProfileAliasExport>, Vec<String>)>>,
    },
    ProfileSet(Box<ProfileUpdate>, oneshot::Sender<Result<()>>),
    ProfileRepair {
        profile: Box<Profile>,
        expected: Box<Profile>,
        reply: oneshot::Sender<Result<()>>,
    },
    ProfileClear {
        server: String,
        nick: String,
        field: String,
        reply: oneshot::Sender<Result<()>>,
    },
    LifecycleRegister {
        module: String,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    LifecycleModules(oneshot::Sender<Result<Vec<String>>>),
    DeletionCreate {
        job: Box<DataDeletionJob>,
        modules: Vec<String>,
        reply: oneshot::Sender<Result<()>>,
    },
    DeletionConfirm {
        token: String,
        requester_profile_id: String,
        allow_other_profile: bool,
        now: i64,
        reply: oneshot::Sender<Result<Option<DataDeletionJob>>>,
    },
    DeletionPending(oneshot::Sender<Result<Vec<DataDeletionJob>>>),
    DeletionModuleDone {
        job_id: String,
        module: String,
        reply: oneshot::Sender<Result<()>>,
    },
    DeletionModulePending {
        job_id: String,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    DeletionFail {
        job_id: String,
        error: String,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    DeletionFinish {
        job_id: String,
        server: String,
        profile_id: String,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    AchievementAward {
        module: String,
        manifest: AchievementManifest,
        catalogs: Vec<(String, AchievementManifest)>,
        request: AwardStatsRequest,
        now: i64,
        reply: oneshot::Sender<Result<AwardStatsResponse>>,
    },
    AchievementsGet {
        server: String,
        profile_id: String,
        manifests: Vec<(String, AchievementManifest)>,
        reply: oneshot::Sender<Result<AchievementProfileSummary>>,
    },
    AchievementsExport {
        server: String,
        profile_id: String,
        reply: oneshot::Sender<Result<AchievementDataExport>>,
    },
    AchievementBackfillApply {
        server: String,
        module: String,
        manifest: AchievementManifest,
        values: Vec<AchievementSetMax>,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    AchievementBackfillVersion {
        server: String,
        module: String,
        reply: oneshot::Sender<Result<u32>>,
    },
    AchievementCatalogReconcile {
        catalogs: Vec<(String, AchievementManifest)>,
        now: i64,
        reply: oneshot::Sender<Result<()>>,
    },
    AchievementOptOut {
        server: String,
        profile_id: String,
        opt_out: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    AchievementPublic {
        server: String,
        profile_id: String,
        public: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    PublicAchievementHolders(oneshot::Sender<Result<Vec<PublicAchievementHolder>>>),
}

#[derive(Debug, Clone)]
pub struct DataDeletionJob {
    pub id: String,
    pub server: String,
    pub profile_id: String,
    pub requester_profile_id: String,
    pub status: String,
    pub confirmation_token: String,
    pub confirmation_expires_at: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_error: Option<String>,
}

/// The only profile identity fields available to the public gallery.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct PublicAchievementHolder {
    pub server: String,
    pub profile_id: String,
    pub nick: String,
    #[serde(skip)]
    pub unlocks: Vec<(String, String)>,
}

/// Cloneable async handle to the DB actor.
#[derive(Clone)]
pub struct DbHandle {
    tx: mpsc::Sender<DbRequest>,
    casemappings: CaseMappingRegistry,
}

impl DbHandle {
    /// Open (creating + migrating) the database at `path` and spawn its actor thread.
    pub fn open(path: &str) -> Result<DbHandle> {
        let conn = Connection::open(path)?;
        migrate(&conn)?;
        let (tx, mut rx) = mpsc::channel::<DbRequest>(64);
        let casemappings = CaseMappingRegistry::default();
        let actor_casemappings = casemappings.clone();

        std::thread::Builder::new()
            .name("jeeves-db".into())
            .spawn(move || {
                let mut conn = conn;
                while let Some(req) = rx.blocking_recv() {
                    handle(&mut conn, &actor_casemappings, req);
                }
            })?;

        Ok(DbHandle { tx, casemappings })
    }

    pub fn casemappings(&self) -> CaseMappingRegistry {
        self.casemappings.clone()
    }

    pub fn irc_casefold(&self, server: &str, value: &str) -> String {
        self.casemappings.fold(server, value)
    }

    async fn call<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.await.map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Synchronous variant of [`Self::call`] for the blocking TUI thread. Must NOT be called from
    /// within the async runtime.
    fn call_blocking<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> DbRequest,
    ) -> Result<T> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(make(reply))
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }

    // --- Blocking accessors used by the TUI thread ---

    pub fn config_get_blocking(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.call_blocking(|reply| DbRequest::ConfigGet { key, reply })
    }

    pub fn config_set_blocking(&self, key: &str, value: Option<&str>) -> Result<()> {
        let key = key.to_string();
        let value = value.map(str::to_string);
        self.call_blocking(|reply| DbRequest::ConfigSet { key, value, reply })
    }

    pub fn backup_to_blocking(&self, path: &std::path::Path) -> Result<()> {
        let path = path
            .to_str()
            .ok_or_else(|| anyhow!("backup path is not valid UTF-8"))?
            .to_string();
        self.call_blocking(|reply| DbRequest::BackupTo { path, reply })
    }

    pub fn load_alias_overrides_blocking(&self) -> Result<AliasOverrides> {
        self.call_blocking(DbRequest::LoadAliasOverrides)
    }

    pub fn set_alias_override_blocking(
        &self,
        module: &str,
        command: &str,
        aliases: Option<&[String]>,
    ) -> Result<()> {
        let module = module.to_string();
        let command = command.to_string();
        let aliases = aliases.map(<[String]>::to_vec);
        self.call_blocking(|reply| DbRequest::SetAliasOverride {
            module,
            command,
            aliases,
            reply,
        })
    }

    pub fn load_setting_overrides_blocking(&self) -> Result<Vec<SettingOverride>> {
        self.call_blocking(DbRequest::LoadSettingOverrides)
    }

    pub fn setting_override_get_blocking(
        &self,
        module: &str,
        key: &str,
        scope: SettingScope,
        server: &str,
        channel: &str,
    ) -> Result<Option<String>> {
        let (module, key, server, channel) = (
            module.to_string(),
            key.to_string(),
            server.to_string(),
            channel.to_string(),
        );
        self.call_blocking(|reply| DbRequest::GetSettingOverride {
            module,
            key,
            scope,
            server,
            channel,
            reply,
        })
    }

    pub fn setting_override_set_blocking(
        &self,
        module: &str,
        key: &str,
        scope: SettingScope,
        server: &str,
        channel: &str,
        value: Option<&str>,
    ) -> Result<()> {
        let (module, key, server, channel) = (
            module.to_string(),
            key.to_string(),
            server.to_string(),
            channel.to_string(),
        );
        let value = value.map(str::to_string);
        self.call_blocking(|reply| DbRequest::SetSettingOverride {
            module,
            key,
            scope,
            server,
            channel,
            value,
            reply,
        })
    }

    pub fn scheduled_jobs_load_blocking(&self) -> Result<Vec<ScheduledJob>> {
        self.call_blocking(DbRequest::LoadScheduledJobs)
    }

    pub async fn scheduled_jobs_load(&self) -> Result<Vec<ScheduledJob>> {
        self.call(DbRequest::LoadScheduledJobs).await
    }

    pub fn scheduled_job_set_blocking(&self, job: ScheduledJob) -> Result<()> {
        self.call_blocking(|reply| DbRequest::SetScheduledJob(Box::new(job), reply))
    }

    #[cfg(test)]
    pub async fn scheduled_job_set(&self, job: ScheduledJob) -> Result<()> {
        self.call(|reply| DbRequest::SetScheduledJob(Box::new(job), reply))
            .await
    }

    pub fn scheduled_job_delete_blocking(&self, module: &str, id: &str) -> Result<bool> {
        let (module, id) = (module.to_string(), id.to_string());
        self.call_blocking(|reply| DbRequest::DeleteScheduledJob { module, id, reply })
    }

    pub fn load_servers_blocking(&self) -> Result<Vec<ServerConfig>> {
        self.call_blocking(DbRequest::LoadServers)
    }

    pub fn upsert_server_blocking(&self, cfg: ServerConfig) -> Result<i64> {
        self.call_blocking(|reply| DbRequest::UpsertServer(Box::new(cfg), reply))
    }

    pub fn delete_server_blocking(&self, id: i64) -> Result<()> {
        self.call_blocking(|reply| DbRequest::DeleteServer(id, reply))
    }

    pub fn load_admins_blocking(&self, server_id: i64) -> Result<Vec<AdminEntry>> {
        self.call_blocking(|reply| DbRequest::LoadAdmins(server_id, reply))
    }

    pub fn upsert_admin_blocking(&self, server_id: i64, entry: AdminEntry) -> Result<()> {
        self.call_blocking(|reply| DbRequest::UpsertAdmin(server_id, entry, reply))
    }

    pub fn delete_admin_blocking(&self, server_id: i64, nick: &str) -> Result<()> {
        let nick = nick.to_string();
        self.call_blocking(|reply| DbRequest::DeleteAdmin(server_id, nick, reply))
    }

    // --- Profiles (blocking; called from the module-host thread) ---

    pub fn profile_ensure_blocking(&self, server: &str, nick: &str, now: i64) -> Result<()> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call_blocking(|reply| DbRequest::ProfileEnsure {
            server,
            nick,
            now,
            reply,
        })
    }

    pub fn profile_get_blocking(&self, server: &str, nick: &str) -> Result<Option<Profile>> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call_blocking(|reply| DbRequest::ProfileGet {
            server,
            nick,
            reply,
        })
    }

    pub fn profile_list_blocking(&self) -> Result<Vec<Profile>> {
        self.call_blocking(DbRequest::ProfileList)
    }

    pub async fn profile_get(&self, server: &str, nick: &str) -> Result<Option<Profile>> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call(|reply| DbRequest::ProfileGet {
            server,
            nick,
            reply,
        })
        .await
    }

    pub async fn profile_identity_links(
        &self,
        server: &str,
        profile_id: &str,
    ) -> Result<(Vec<ProfileAliasExport>, Vec<String>)> {
        let (server, profile_id) = (server.to_string(), profile_id.to_string());
        self.call(|reply| DbRequest::ProfileIdentityLinks {
            server,
            profile_id,
            reply,
        })
        .await
    }

    pub fn profile_identity_links_blocking(
        &self,
        server: &str,
        profile_id: &str,
    ) -> Result<(Vec<ProfileAliasExport>, Vec<String>)> {
        let (server, profile_id) = (server.to_string(), profile_id.to_string());
        self.call_blocking(|reply| DbRequest::ProfileIdentityLinks {
            server,
            profile_id,
            reply,
        })
    }

    pub fn profile_set_blocking(&self, update: ProfileUpdate) -> Result<()> {
        self.call_blocking(|reply| DbRequest::ProfileSet(Box::new(update), reply))
    }

    pub fn profile_repair_blocking(&self, profile: Profile, expected: Profile) -> Result<()> {
        self.call_blocking(|reply| DbRequest::ProfileRepair {
            profile: Box::new(profile),
            expected: Box::new(expected),
            reply,
        })
    }

    pub fn profile_clear_blocking(&self, server: &str, nick: &str, field: &str) -> Result<()> {
        let (server, nick, field) = (server.to_string(), nick.to_string(), field.to_string());
        self.call_blocking(|reply| DbRequest::ProfileClear {
            server,
            nick,
            field,
            reply,
        })
    }

    /// All configured server profiles, ordered by id.
    pub async fn load_servers(&self) -> Result<Vec<ServerConfig>> {
        self.call(DbRequest::LoadServers).await
    }

    pub async fn append_log(&self, ev: LogEvent) -> Result<()> {
        self.call(|reply| DbRequest::AppendLog(ev, reply)).await
    }

    /// Resolve a sender's permission role on `server_label`, performing trust-on-first-use binding
    /// as a side effect. Returns `None` if the sender is not a configured admin or identity check
    /// fails.
    pub async fn resolve_role(
        &self,
        server_label: &str,
        nick: &str,
        hostmask: &str,
        account: Option<String>,
    ) -> Result<Option<Role>> {
        let (server_label, nick, hostmask) = (
            server_label.to_string(),
            nick.to_string(),
            hostmask.to_string(),
        );
        self.call(|reply| DbRequest::ResolveRole {
            server_label,
            nick,
            hostmask,
            account,
            reply,
        })
        .await
    }

    /// Resolve or create the stable profile for an observed IRC identity.
    pub async fn profile_resolve(
        &self,
        server: &str,
        nick: &str,
        account: Option<String>,
        now: i64,
    ) -> Result<Profile> {
        let (server, nick) = (server.to_string(), nick.to_string());
        self.call(|reply| DbRequest::ProfileResolve {
            server,
            nick,
            account,
            now,
            reply,
        })
        .await
    }

    /// Record a NICK change as another alias of the same stable profile.
    pub async fn profile_bind_nick(
        &self,
        server: &str,
        old_nick: &str,
        new_nick: &str,
        account: Option<String>,
        now: i64,
    ) -> Result<()> {
        let (server, old_nick, new_nick) = (
            server.to_string(),
            old_nick.to_string(),
            new_nick.to_string(),
        );
        self.call(|reply| DbRequest::ProfileBindNick {
            server,
            old_nick,
            new_nick,
            account,
            now,
            reply,
        })
        .await
    }

    /// Blocking KV get for use from the synchronous module-host thread. Must NOT be called from
    /// within the async runtime.
    pub fn kv_get_blocking(&self, module: &str, key: &str) -> Result<Option<String>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(DbRequest::KvGet {
                module: module.to_string(),
                key: key.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }

    /// Blocking KV set for use from the synchronous module-host thread.
    pub fn kv_set_blocking(&self, module: &str, key: &str, value: &str) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .blocking_send(DbRequest::KvSet {
                module: module.to_string(),
                key: key.to_string(),
                value: value.to_string(),
                reply,
            })
            .map_err(|_| anyhow!("db actor is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("db actor dropped reply"))?
    }

    pub fn kv_list_module_blocking(&self, module: &str) -> Result<Vec<ModuleKvEntry>> {
        let module = module.to_string();
        self.call_blocking(|reply| DbRequest::KvListModule { module, reply })
    }

    pub fn kv_apply_module_blocking(
        &self,
        module: &str,
        allowed_keys: Vec<String>,
        mutations: Vec<ModuleKvMutation>,
    ) -> Result<()> {
        let module = module.to_string();
        self.call_blocking(|reply| DbRequest::KvApplyModule {
            module,
            allowed_keys,
            mutations,
            reply,
        })
    }

    pub fn kv_apply_module_checked_blocking(
        &self,
        module: &str,
        expected: Vec<ModuleKvEntry>,
        mutations: Vec<ModuleKvMutation>,
    ) -> Result<()> {
        let module = module.to_string();
        self.call_blocking(|reply| DbRequest::KvApplyModuleChecked {
            module,
            expected,
            mutations,
            reply,
        })
    }

    pub fn lifecycle_register_blocking(&self, module: &str, now: i64) -> Result<()> {
        let module = module.to_string();
        self.call_blocking(|reply| DbRequest::LifecycleRegister { module, now, reply })
    }

    pub fn lifecycle_modules_blocking(&self) -> Result<Vec<String>> {
        self.call_blocking(DbRequest::LifecycleModules)
    }

    pub fn deletion_create_blocking(
        &self,
        job: DataDeletionJob,
        modules: Vec<String>,
    ) -> Result<()> {
        self.call_blocking(|reply| DbRequest::DeletionCreate {
            job: Box::new(job),
            modules,
            reply,
        })
    }

    pub fn deletion_confirm_blocking(
        &self,
        token: &str,
        requester_profile_id: &str,
        allow_other_profile: bool,
        now: i64,
    ) -> Result<Option<DataDeletionJob>> {
        let (token, requester_profile_id) = (token.to_string(), requester_profile_id.to_string());
        self.call_blocking(|reply| DbRequest::DeletionConfirm {
            token,
            requester_profile_id,
            allow_other_profile,
            now,
            reply,
        })
    }

    pub fn deletion_pending_blocking(&self) -> Result<Vec<DataDeletionJob>> {
        self.call_blocking(DbRequest::DeletionPending)
    }

    pub fn deletion_module_done_blocking(&self, job_id: &str, module: &str) -> Result<()> {
        let (job_id, module) = (job_id.to_string(), module.to_string());
        self.call_blocking(|reply| DbRequest::DeletionModuleDone {
            job_id,
            module,
            reply,
        })
    }

    pub fn deletion_module_pending_blocking(&self, job_id: &str) -> Result<Vec<String>> {
        let job_id = job_id.to_string();
        self.call_blocking(|reply| DbRequest::DeletionModulePending { job_id, reply })
    }

    pub fn deletion_fail_blocking(&self, job_id: &str, error: &str, now: i64) -> Result<()> {
        let (job_id, error) = (job_id.to_string(), error.to_string());
        self.call_blocking(|reply| DbRequest::DeletionFail {
            job_id,
            error,
            now,
            reply,
        })
    }

    pub fn deletion_finish_blocking(
        &self,
        job_id: &str,
        server: &str,
        profile_id: &str,
        now: i64,
    ) -> Result<()> {
        let (job_id, server, profile_id) = (
            job_id.to_string(),
            server.to_string(),
            profile_id.to_string(),
        );
        self.call_blocking(|reply| DbRequest::DeletionFinish {
            job_id,
            server,
            profile_id,
            now,
            reply,
        })
    }

    pub fn achievement_award_blocking(
        &self,
        module: &str,
        manifest: AchievementManifest,
        catalogs: Vec<(String, AchievementManifest)>,
        request: AwardStatsRequest,
        now: i64,
    ) -> Result<AwardStatsResponse> {
        let module = module.to_string();
        self.call_blocking(|reply| DbRequest::AchievementAward {
            module,
            manifest,
            catalogs,
            request,
            now,
            reply,
        })
    }

    #[cfg(test)]
    pub async fn achievement_award(
        &self,
        module: &str,
        manifest: AchievementManifest,
        catalogs: Vec<(String, AchievementManifest)>,
        request: AwardStatsRequest,
        now: i64,
    ) -> Result<AwardStatsResponse> {
        let module = module.to_string();
        self.call(|reply| DbRequest::AchievementAward {
            module,
            manifest,
            catalogs,
            request,
            now,
            reply,
        })
        .await
    }

    pub fn achievements_get_blocking(
        &self,
        server: &str,
        profile_id: &str,
        manifests: Vec<(String, AchievementManifest)>,
    ) -> Result<AchievementProfileSummary> {
        let (server, profile_id) = (server.to_string(), profile_id.to_string());
        self.call_blocking(|reply| DbRequest::AchievementsGet {
            server,
            profile_id,
            manifests,
            reply,
        })
    }

    pub fn achievements_export_blocking(
        &self,
        server: &str,
        profile_id: &str,
    ) -> Result<AchievementDataExport> {
        let (server, profile_id) = (server.to_string(), profile_id.to_string());
        self.call_blocking(|reply| DbRequest::AchievementsExport {
            server,
            profile_id,
            reply,
        })
    }

    pub async fn achievements_export(
        &self,
        server: &str,
        profile_id: &str,
    ) -> Result<AchievementDataExport> {
        let (server, profile_id) = (server.to_string(), profile_id.to_string());
        self.call(|reply| DbRequest::AchievementsExport {
            server,
            profile_id,
            reply,
        })
        .await
    }

    pub fn achievement_backfill_apply_blocking(
        &self,
        server: &str,
        module: &str,
        manifest: AchievementManifest,
        values: Vec<AchievementSetMax>,
        now: i64,
    ) -> Result<()> {
        let (server, module) = (server.to_string(), module.to_string());
        self.call_blocking(|reply| DbRequest::AchievementBackfillApply {
            server,
            module,
            manifest,
            values,
            now,
            reply,
        })
    }

    pub fn achievement_backfill_version_blocking(&self, server: &str, module: &str) -> Result<u32> {
        let (server, module) = (server.to_string(), module.to_string());
        self.call_blocking(|reply| DbRequest::AchievementBackfillVersion {
            server,
            module,
            reply,
        })
    }

    pub fn achievement_catalog_reconcile_blocking(
        &self,
        catalogs: Vec<(String, AchievementManifest)>,
        now: i64,
    ) -> Result<()> {
        self.call_blocking(|reply| DbRequest::AchievementCatalogReconcile {
            catalogs,
            now,
            reply,
        })
    }

    /// Atomically set a profile's achievements opt-out flag, wiping achievement rows when opting
    /// out. Safe to call from the module host thread.
    pub fn achievement_opt_out_blocking(
        &self,
        server: &str,
        profile_id: &str,
        opt_out: bool,
    ) -> Result<()> {
        self.call_blocking(|reply| DbRequest::AchievementOptOut {
            server: server.into(),
            profile_id: profile_id.into(),
            opt_out,
            reply,
        })
    }

    pub fn achievement_public_blocking(
        &self,
        server: &str,
        profile_id: &str,
        public: bool,
    ) -> Result<()> {
        self.call_blocking(|reply| DbRequest::AchievementPublic {
            server: server.into(),
            profile_id: profile_id.into(),
            public,
            reply,
        })
    }

    pub fn public_achievement_holders_blocking(&self) -> Result<Vec<PublicAchievementHolder>> {
        self.call_blocking(DbRequest::PublicAchievementHolders)
    }
}

fn handle(conn: &mut Connection, casemappings: &CaseMappingRegistry, req: DbRequest) {
    match req {
        DbRequest::ConfigGet { key, reply } => {
            let _ = reply.send(config_get(conn, &key));
        }
        DbRequest::ConfigSet { key, value, reply } => {
            let _ = reply.send(config_set(conn, &key, value.as_deref()));
        }
        DbRequest::BackupTo { path, reply } => {
            let _ = reply.send(backup_to(conn, &path));
        }
        DbRequest::LoadAliasOverrides(reply) => {
            let _ = reply.send(load_alias_overrides(conn));
        }
        DbRequest::SetAliasOverride {
            module,
            command,
            aliases,
            reply,
        } => {
            let _ = reply.send(set_alias_override(
                conn,
                &module,
                &command,
                aliases.as_deref(),
            ));
        }
        DbRequest::LoadSettingOverrides(reply) => {
            let _ = reply.send(load_setting_overrides(conn));
        }
        DbRequest::GetSettingOverride {
            module,
            key,
            scope,
            server,
            channel,
            reply,
        } => {
            let _ = reply.send(setting_override_get(
                conn, &module, &key, scope, &server, &channel,
            ));
        }
        DbRequest::SetSettingOverride {
            module,
            key,
            scope,
            server,
            channel,
            value,
            reply,
        } => {
            let _ = reply.send(setting_override_set(
                conn,
                &module,
                &key,
                scope,
                &server,
                &channel,
                value.as_deref(),
            ));
        }
        DbRequest::LoadScheduledJobs(reply) => {
            let _ = reply.send(load_scheduled_jobs(conn));
        }
        DbRequest::SetScheduledJob(job, reply) => {
            let _ = reply.send(set_scheduled_job(conn, &job));
        }
        DbRequest::DeleteScheduledJob { module, id, reply } => {
            let _ = reply.send(delete_scheduled_job(conn, &module, &id));
        }
        DbRequest::LoadServers(reply) => {
            let _ = reply.send(load_servers(conn));
        }
        DbRequest::UpsertServer(cfg, reply) => {
            let _ = reply.send(upsert_server(conn, &cfg));
        }
        DbRequest::DeleteServer(id, reply) => {
            let _ = reply.send(delete_server(conn, id));
        }
        DbRequest::KvGet { module, key, reply } => {
            let _ = reply.send(kv_get(conn, &module, &key));
        }
        DbRequest::KvSet {
            module,
            key,
            value,
            reply,
        } => {
            let _ = reply.send(kv_set(conn, &module, &key, &value));
        }
        DbRequest::KvListModule { module, reply } => {
            let _ = reply.send(kv_list_module(conn, &module));
        }
        DbRequest::KvApplyModule {
            module,
            allowed_keys,
            mutations,
            reply,
        } => {
            let _ = reply.send(kv_apply_module(conn, &module, &allowed_keys, &mutations));
        }
        DbRequest::KvApplyModuleChecked {
            module,
            expected,
            mutations,
            reply,
        } => {
            let _ = reply.send(kv_apply_module_checked(
                conn, &module, &expected, &mutations,
            ));
        }
        DbRequest::AppendLog(ev, reply) => {
            let _ = reply.send(append_log(conn, &ev));
        }
        DbRequest::ResolveRole {
            server_label,
            nick,
            hostmask,
            account,
            reply,
        } => {
            let _ = reply.send(resolve_role_mapped(
                conn,
                &server_label,
                &nick,
                &hostmask,
                account.as_deref(),
                casemappings.get(&server_label),
            ));
        }
        DbRequest::LoadAdmins(server_id, reply) => {
            let _ = reply.send(load_admins(conn, server_id));
        }
        DbRequest::UpsertAdmin(server_id, entry, reply) => {
            let _ = reply.send(upsert_admin(conn, server_id, &entry));
        }
        DbRequest::DeleteAdmin(server_id, nick, reply) => {
            let _ = reply.send(delete_admin(conn, server_id, &nick));
        }
        DbRequest::ProfileEnsure {
            server,
            nick,
            now,
            reply,
        } => {
            let _ = reply.send(profile_ensure_mapped(
                conn,
                &server,
                &nick,
                now,
                casemappings.get(&server),
            ));
        }
        DbRequest::ProfileResolve {
            server,
            nick,
            account,
            now,
            reply,
        } => {
            let _ = reply.send(profile_resolve_mapped(
                conn,
                &server,
                &nick,
                account.as_deref(),
                now,
                casemappings.get(&server),
            ));
        }
        DbRequest::ProfileBindNick {
            server,
            old_nick,
            new_nick,
            account,
            now,
            reply,
        } => {
            let _ = reply.send(profile_bind_nick_mapped(
                conn,
                &server,
                &old_nick,
                &new_nick,
                account.as_deref(),
                now,
                casemappings.get(&server),
            ));
        }
        DbRequest::ProfileGet {
            server,
            nick,
            reply,
        } => {
            let _ = reply.send(profile_get_mapped(
                conn,
                &server,
                &nick,
                casemappings.get(&server),
            ));
        }
        DbRequest::ProfileList(reply) => {
            let _ = reply.send(profile_list(conn));
        }
        DbRequest::ProfileIdentityLinks {
            server,
            profile_id,
            reply,
        } => {
            let _ = reply.send(profile_identity_links(conn, &server, &profile_id));
        }
        DbRequest::ProfileSet(update, reply) => {
            let _ = reply.send(profile_set_mapped(
                conn,
                &update,
                casemappings.get(&update.server),
            ));
        }
        DbRequest::ProfileRepair {
            profile,
            expected,
            reply,
        } => {
            let _ = reply.send(profile_repair_checked(conn, &profile, &expected));
        }
        DbRequest::ProfileClear {
            server,
            nick,
            field,
            reply,
        } => {
            let _ = reply.send(profile_clear_mapped(
                conn,
                &server,
                &nick,
                &field,
                casemappings.get(&server),
            ));
        }
        DbRequest::LifecycleRegister { module, now, reply } => {
            let _ = reply.send(lifecycle_register(conn, &module, now));
        }
        DbRequest::LifecycleModules(reply) => {
            let _ = reply.send(lifecycle_modules(conn));
        }
        DbRequest::DeletionCreate {
            job,
            modules,
            reply,
        } => {
            let _ = reply.send(deletion_create(conn, &job, &modules));
        }
        DbRequest::DeletionConfirm {
            token,
            requester_profile_id,
            allow_other_profile,
            now,
            reply,
        } => {
            let _ = reply.send(deletion_confirm(
                conn,
                &token,
                &requester_profile_id,
                allow_other_profile,
                now,
            ));
        }
        DbRequest::DeletionPending(reply) => {
            let _ = reply.send(deletion_pending(conn));
        }
        DbRequest::DeletionModuleDone {
            job_id,
            module,
            reply,
        } => {
            let _ = reply.send(deletion_module_done(conn, &job_id, &module));
        }
        DbRequest::DeletionModulePending { job_id, reply } => {
            let _ = reply.send(deletion_module_pending(conn, &job_id));
        }
        DbRequest::DeletionFail {
            job_id,
            error,
            now,
            reply,
        } => {
            let _ = reply.send(deletion_fail(conn, &job_id, &error, now));
        }
        DbRequest::DeletionFinish {
            job_id,
            server,
            profile_id,
            now,
            reply,
        } => {
            let _ = reply.send(deletion_finish(conn, &job_id, &server, &profile_id, now));
        }
        DbRequest::AchievementAward {
            module,
            manifest,
            catalogs,
            request,
            now,
            reply,
        } => {
            let _ = reply.send(achievement_award(
                conn, &module, &manifest, &catalogs, &request, now,
            ));
        }
        DbRequest::AchievementsGet {
            server,
            profile_id,
            manifests,
            reply,
        } => {
            let _ = reply.send(achievements_get(conn, &server, &profile_id, &manifests));
        }
        DbRequest::AchievementsExport {
            server,
            profile_id,
            reply,
        } => {
            let _ = reply.send(achievements_export(conn, &server, &profile_id));
        }
        DbRequest::AchievementBackfillApply {
            server,
            module,
            manifest,
            values,
            now,
            reply,
        } => {
            let _ = reply.send(achievement_backfill_apply(
                conn, &server, &module, &manifest, &values, now,
            ));
        }
        DbRequest::AchievementBackfillVersion {
            server,
            module,
            reply,
        } => {
            let result = conn
                .query_row("SELECT COALESCE(MAX(catalog_version),0) FROM achievement_catalog_versions WHERE server=?1 AND module=?2", (&server, &module), |row| row.get::<_, i64>(0))
                .map(|value| value as u32)
                .map_err(Into::into);
            let _ = reply.send(result);
        }
        DbRequest::AchievementCatalogReconcile {
            catalogs,
            now,
            reply,
        } => {
            let _ = reply.send(achievement_catalog_reconcile(conn, &catalogs, now));
        }
        DbRequest::AchievementOptOut {
            server,
            profile_id,
            opt_out,
            reply,
        } => {
            let _ = reply.send(achievement_opt_out(conn, &server, &profile_id, opt_out));
        }
        DbRequest::AchievementPublic {
            server,
            profile_id,
            public,
            reply,
        } => {
            let _ = reply.send(achievement_public(conn, &server, &profile_id, public));
        }
        DbRequest::PublicAchievementHolders(reply) => {
            let _ = reply.send(public_achievement_holders(conn));
        }
    }
}

fn role_to_str(r: Role) -> &'static str {
    match r {
        Role::Admin => "admin",
        Role::SuperAdmin => "superadmin",
    }
}

fn role_from_str(s: &str) -> Option<Role> {
    match s {
        "admin" => Some(Role::Admin),
        "superadmin" => Some(Role::SuperAdmin),
        _ => None,
    }
}

/// Resolve a sender's role on a network, applying trust-on-first-use binding. See the permission
/// rules in `perms.rs` / SPEC.md.
#[cfg(test)]
fn resolve_role(
    conn: &Connection,
    server_label: &str,
    nick: &str,
    hostmask: &str,
    account: Option<&str>,
) -> Result<Option<Role>> {
    resolve_role_mapped(
        conn,
        server_label,
        nick,
        hostmask,
        account,
        CaseMapping::default(),
    )
}

fn resolve_role_mapped(
    conn: &Connection,
    server_label: &str,
    nick: &str,
    hostmask: &str,
    account: Option<&str>,
    casemapping: CaseMapping,
) -> Result<Option<Role>> {
    let sid: Option<i64> = conn
        .query_row(
            "SELECT id FROM servers WHERE label = ?1",
            [server_label],
            |r| r.get(0),
        )
        .optional()?;
    let Some(sid) = sid else { return Ok(None) };

    let mut stmt = conn.prepare(
        "SELECT nick, role, account, bound_hostmask, bound_account
         FROM admins WHERE server_id = ?1",
    )?;
    let rows = stmt.query_map([sid], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut matched = None;
    for row in rows {
        let row = row?;
        if casemapping.equivalent(&row.0, nick) {
            matched = Some(row);
            break;
        }
    }
    let Some((stored_nick, role_s, cfg_account, bound_hostmask, bound_account)) = matched else {
        return Ok(None);
    };
    let Some(role) = role_from_str(&role_s) else {
        return Ok(None);
    };

    let observed_account = account.filter(|a| !a.is_empty());

    // 1. Operator pinned an explicit account: must match.
    if let Some(want) = cfg_account.as_deref().filter(|a| !a.is_empty()) {
        return Ok((observed_account == Some(want)).then_some(role));
    }
    // 2. Previously bound to an account: must match.
    if let Some(bound) = bound_account.as_deref() {
        return Ok((observed_account == Some(bound)).then_some(role));
    }
    // 3. Previously bound to a hostmask: must match.
    if let Some(bound) = bound_hostmask.as_deref() {
        return Ok(hostmasks_equivalent(casemapping, hostmask, bound).then_some(role));
    }
    // 4. First contact — bind the strongest identity available (prefer account).
    if let Some(acct) = observed_account {
        conn.execute(
            "UPDATE admins SET bound_account = ?3 WHERE server_id = ?1 AND nick = ?2",
            rusqlite::params![sid, stored_nick, acct],
        )?;
    } else {
        conn.execute(
            "UPDATE admins SET bound_hostmask = ?3 WHERE server_id = ?1 AND nick = ?2",
            rusqlite::params![sid, stored_nick, hostmask],
        )?;
    }
    Ok(Some(role))
}

fn hostmasks_equivalent(casemapping: CaseMapping, left: &str, right: &str) -> bool {
    match (left.split_once('!'), right.split_once('!')) {
        (Some((left_nick, left_rest)), Some((right_nick, right_rest))) => {
            casemapping.equivalent(left_nick, right_nick) && left_rest == right_rest
        }
        _ => left == right,
    }
}

fn load_admins(conn: &Connection, server_id: i64) -> Result<Vec<AdminEntry>> {
    let mut stmt = conn.prepare(
        "SELECT nick, role, account, bound_hostmask, bound_account
         FROM admins WHERE server_id = ?1 ORDER BY nick",
    )?;
    let rows = stmt.query_map([server_id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for r in rows {
        let (nick, role_s, account, bound_hostmask, bound_account) = r?;
        if let Some(role) = role_from_str(&role_s) {
            out.push(AdminEntry {
                nick,
                role,
                account,
                bound_hostmask,
                bound_account,
            });
        }
    }
    Ok(out)
}

fn upsert_admin(conn: &Connection, server_id: i64, entry: &AdminEntry) -> Result<()> {
    // Re-inserting an admin clears any stale binding so the next contact re-binds.
    conn.execute(
        "INSERT INTO admins (server_id, nick, role, account, bound_hostmask, bound_account)
         VALUES (?1, ?2, ?3, ?4, NULL, NULL)
         ON CONFLICT(server_id, nick) DO UPDATE SET
            role=excluded.role, account=excluded.account,
            bound_hostmask=NULL, bound_account=NULL",
        rusqlite::params![
            server_id,
            entry.nick,
            role_to_str(entry.role),
            entry.account
        ],
    )?;
    Ok(())
}

fn delete_admin(conn: &Connection, server_id: i64, nick: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM admins WHERE server_id = ?1 AND nick = ?2 COLLATE NOCASE",
        rusqlite::params![server_id, nick],
    )?;
    Ok(())
}

pub(crate) fn validate_achievement_manifest(
    module: &str,
    manifest: &AchievementManifest,
) -> Result<()> {
    if manifest.version != jeeves_abi::ACHIEVEMENT_MANIFEST_VERSION {
        return Err(anyhow!(
            "unsupported achievement manifest version for {module}"
        ));
    }
    let mut stats = std::collections::HashSet::new();
    for stat in &manifest.stats {
        if stat.id.is_empty()
            || stat.description.trim().is_empty()
            || !stat
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || !stats.insert(&stat.id)
        {
            return Err(anyhow!(
                "invalid or duplicate achievement stat '{}.{}'",
                module,
                stat.id
            ));
        }
    }
    let mut ids = std::collections::HashSet::new();
    let mut milestones = std::collections::HashSet::new();
    for item in &manifest.achievements {
        if item.id.is_empty()
            || !item
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || item.name.trim().is_empty()
            || item.description.trim().is_empty()
            || item.threshold == 0
            || !stats.contains(&item.stat)
            || !ids.insert(&item.id)
            || !milestones.insert((&item.stat, item.threshold))
        {
            return Err(anyhow!(
                "invalid or duplicate achievement '{}.{}'",
                module,
                item.id
            ));
        }
    }
    for item in &manifest.prestige {
        if item.id.is_empty()
            || !item
                .id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
            || item.name.trim().is_empty()
            || item.first_threshold == 0
            || item.every == 0
            || !stats.contains(&item.stat)
            || !ids.insert(&item.id)
        {
            return Err(anyhow!(
                "invalid prestige achievement '{}.{}'",
                module,
                item.id
            ));
        }
    }
    Ok(())
}

fn achievement_award(
    conn: &mut Connection,
    module: &str,
    manifest: &AchievementManifest,
    catalogs: &[(String, AchievementManifest)],
    request: &AwardStatsRequest,
    now: i64,
) -> Result<AwardStatsResponse> {
    validate_achievement_manifest(module, manifest)?;
    if request.server.is_empty() || request.profile_id.is_empty() || request.increments.is_empty() {
        return Err(anyhow!(
            "achievement award requires server, profile and increments"
        ));
    }
    if request.increments.len() > 32 {
        return Err(anyhow!("too many stat increments"));
    }
    let known = manifest
        .stats
        .iter()
        .map(|s| s.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut increments = std::collections::BTreeMap::<&str, u64>::new();
    for increment in &request.increments {
        if increment.amount == 0 || !known.contains(increment.stat.as_str()) {
            return Err(anyhow!(
                "unknown or zero achievement stat '{}.{}'",
                module,
                increment.stat
            ));
        }
        let value = increments.entry(&increment.stat).or_default();
        *value = value
            .checked_add(increment.amount)
            .ok_or_else(|| anyhow!("achievement increment overflow"))?;
    }
    let tx = conn.transaction()?;
    // Verify the profile exists AND read its opt-out flag in one query on the row we'd touch
    // anyway. An opted-out profile receives no awards at all — its own actions and other-caused
    // awards (e.g. karma received) are both suppressed, so opted-out users are invisible to the
    // achievement system.
    let opt_out: i64 = tx
        .query_row(
            "SELECT achievements_opt_out FROM profiles WHERE server=?1 AND id=?2",
            rusqlite::params![request.server, request.profile_id],
            |row| row.get(0),
        )
        .map_err(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                anyhow!("unknown profile UUID for achievement award")
            } else {
                anyhow::Error::from(e)
            }
        })?;
    if opt_out != 0 {
        // Silently drop the award: no dedup write, no stat increment, no unlock, no announcement.
        return Ok(AwardStatsResponse::default());
    }
    if let Some(id) = request.deduplication_id.as_deref() {
        if id.is_empty() || id.len() > 200 {
            return Err(anyhow!("invalid achievement deduplication id"));
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO achievement_dedup(server,profile_id,module,event_id,created_at) VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![request.server, request.profile_id, module, id, now])?;
        if inserted == 0 {
            return Ok(AwardStatsResponse {
                duplicate: true,
                ..Default::default()
            });
        }
    }
    for (stat, amount) in increments {
        let old = tx.query_row(
            "SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
            rusqlite::params![request.server, request.profile_id, module, stat], |row| row.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
        let next = old
            .checked_add(amount)
            .ok_or_else(|| anyhow!("achievement stat overflow"))?;
        if next > i64::MAX as u64 {
            return Err(anyhow!("achievement stat overflow"));
        }
        tx.execute("INSERT INTO achievement_stats(server,profile_id,module,stat,value) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(server,profile_id,module,stat) DO UPDATE SET value=excluded.value",
            rusqlite::params![request.server, request.profile_id, module, stat, next as i64])?;
    }
    let mut response = AwardStatsResponse::default();
    for item in &manifest.achievements {
        let value = tx.query_row("SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
            rusqlite::params![request.server, request.profile_id, module, item.stat], |row| row.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
        if value >= item.threshold {
            let inserted = tx.execute("INSERT OR IGNORE INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![request.server, request.profile_id, module, item.id, now])?;
            if inserted != 0 {
                response.unlocked.push(AchievementUnlock {
                    module: module.into(),
                    id: item.id.clone(),
                    name: item.name.clone(),
                    unlocked_at: now,
                });
            }
        }
    }
    for item in &manifest.prestige {
        let value = tx.query_row("SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
            rusqlite::params![request.server, request.profile_id, module, item.stat], |row| row.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
        let rank = if value < item.first_threshold {
            0
        } else {
            1 + (value - item.first_threshold) / item.every
        };
        let old = tx.query_row("SELECT max_rank FROM achievement_prestige WHERE server=?1 AND profile_id=?2 AND module=?3 AND prestige_id=?4",
            rusqlite::params![request.server, request.profile_id, module, item.id], |row| row.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
        if rank > old {
            tx.execute("INSERT INTO achievement_prestige(server,profile_id,module,prestige_id,max_rank) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(server,profile_id,module,prestige_id) DO UPDATE SET max_rank=excluded.max_rank",
                rusqlite::params![request.server, request.profile_id, module, item.id, rank as i64])?;
            response.prestige.push(PrestigeRank {
                module: module.into(),
                id: item.id.clone(),
                name: item.name.clone(),
                rank,
            });
        }
    }
    apply_meta_achievements(
        &tx,
        &request.server,
        &request.profile_id,
        catalogs,
        now,
        &mut response,
    )?;
    tx.commit()?;
    Ok(response)
}

fn apply_meta_achievements(
    tx: &rusqlite::Transaction<'_>,
    server: &str,
    profile_id: &str,
    catalogs: &[(String, AchievementManifest)],
    now: i64,
    response: &mut AwardStatsResponse,
) -> Result<()> {
    let finite_count = catalogs
        .iter()
        .flat_map(|(module, manifest)| manifest.achievements.iter().map(move |item| (module, item)))
        .filter(|(module, item)| tx.query_row("SELECT EXISTS(SELECT 1 FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module=?3 AND achievement_id=?4)", rusqlite::params![server, profile_id, module, item.id], |row| row.get::<_, bool>(0)).unwrap_or(false))
        .count() as u64;
    for (id, name, threshold) in [
        ("modest_mantelpiece", "A Modest Mantelpiece", 10),
        ("cabinet_curiosities", "Cabinet of Curiosities", 25),
        (
            "distinguished_collection",
            "A Most Distinguished Collection",
            50,
        ),
    ] {
        if finite_count >= threshold {
            let inserted = tx.execute("INSERT OR IGNORE INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,'meta',?3,?4)", rusqlite::params![server, profile_id, id, now])?;
            if inserted != 0 {
                response.unlocked.push(AchievementUnlock {
                    module: "meta".into(),
                    id: id.into(),
                    name: name.into(),
                    unlocked_at: now,
                });
            }
        }
    }
    let required = catalogs
        .iter()
        .flat_map(|(module, manifest)| {
            manifest
                .achievements
                .iter()
                .filter(|item| !item.optional)
                .map(move |item| (module, item))
        })
        .collect::<Vec<_>>();
    let complete = !required.is_empty() && required.iter().all(|(module, item)| tx.query_row("SELECT EXISTS(SELECT 1 FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module=?3 AND achievement_id=?4)", rusqlite::params![server, profile_id, module, item.id], |row| row.get::<_, bool>(0)).unwrap_or(false));
    if complete {
        let inserted = tx.execute("INSERT OR IGNORE INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,'meta','whole_shooting_match',?3)", rusqlite::params![server, profile_id, now])?;
        if inserted != 0 {
            response.unlocked.push(AchievementUnlock {
                module: "meta".into(),
                id: "whole_shooting_match".into(),
                name: "The Whole Shooting Match".into(),
                unlocked_at: now,
            });
        }
    } else {
        tx.execute("DELETE FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module='meta' AND achievement_id='whole_shooting_match'", (server, profile_id))?;
    }
    Ok(())
}

fn achievements_get(
    conn: &Connection,
    server: &str,
    profile_id: &str,
    manifests: &[(String, AchievementManifest)],
) -> Result<AchievementProfileSummary> {
    let mut result = AchievementProfileSummary::default();
    let required = manifests
        .iter()
        .flat_map(|(module, manifest)| {
            manifest
                .achievements
                .iter()
                .filter(|item| !item.optional)
                .map(move |item| (module, item))
        })
        .collect::<Vec<_>>();
    let complete = !required.is_empty() && required.iter().all(|(module, item)| conn.query_row("SELECT EXISTS(SELECT 1 FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module=?3 AND achievement_id=?4)", rusqlite::params![server, profile_id, module, item.id], |row| row.get::<_, bool>(0)).unwrap_or(false));
    if !complete {
        conn.execute("DELETE FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module='meta' AND achievement_id='whole_shooting_match'", (server, profile_id))?;
    }
    for (module, manifest) in manifests {
        validate_achievement_manifest(module, manifest)?;
        let mut progress = Vec::new();
        for item in &manifest.achievements {
            let current = conn.query_row("SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
                rusqlite::params![server, profile_id, module, item.stat], |row| row.get::<_, i64>(0)).optional()?.unwrap_or(0) as u64;
            let earned = current >= item.threshold;
            if earned {
                result.earned += 1;
            }
            result.available += 1;
            progress.push(AchievementProgress {
                module: module.clone(),
                id: item.id.clone(),
                name: if item.secret && !earned {
                    "Undiscovered secret".into()
                } else {
                    item.name.clone()
                },
                current: if item.secret && !earned { 0 } else { current },
                threshold: if item.secret && !earned {
                    0
                } else {
                    item.threshold
                },
                earned,
                secret: item.secret,
                optional: item.optional,
            });
        }
        progress.sort_by_key(|p| (p.earned, p.threshold.saturating_sub(p.current)));
        result.closest.extend(
            progress
                .iter()
                .filter(|p| !p.earned && !p.secret)
                .take(1)
                .cloned(),
        );
        let mut prestige = Vec::new();
        for item in &manifest.prestige {
            let rank = conn
                .query_row(
                    "SELECT max_rank FROM achievement_prestige WHERE server=?1 AND profile_id=?2 AND module=?3 AND prestige_id=?4",
                    rusqlite::params![server, profile_id, module, item.id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .unwrap_or(0) as u64;
            if rank > 0 {
                prestige.push(PrestigeRank {
                    module: module.clone(),
                    id: item.id.clone(),
                    name: item.name.clone(),
                    rank,
                });
            }
        }
        result.modules.push(AchievementModuleProgress {
            module: module.clone(),
            earned: progress.iter().filter(|p| p.earned).count() as u64,
            available: progress.len() as u64,
            achievements: progress,
            prestige,
        });
    }
    let meta_specs = [
        ("modest_mantelpiece", "A Modest Mantelpiece", 10),
        ("cabinet_curiosities", "Cabinet of Curiosities", 25),
        (
            "distinguished_collection",
            "A Most Distinguished Collection",
            50,
        ),
        (
            "whole_shooting_match",
            "The Whole Shooting Match",
            required.len() as u64,
        ),
    ];
    let non_meta_count = result.earned;
    let mut meta = Vec::new();
    for (id, name, threshold) in meta_specs {
        let earned: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module='meta' AND achievement_id=?3)", (server, profile_id, id), |row| row.get(0))?;
        if earned {
            result.earned += 1;
        }
        result.available += 1;
        meta.push(AchievementProgress { module: "meta".into(), id: id.into(), name: name.into(), current: if id == "whole_shooting_match" { required.iter().filter(|(module,item)| conn.query_row("SELECT EXISTS(SELECT 1 FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 AND module=?3 AND achievement_id=?4)", rusqlite::params![server,profile_id,module,item.id], |row| row.get::<_,bool>(0)).unwrap_or(false)).count() as u64 } else { non_meta_count }, threshold, earned, secret: false, optional: id != "whole_shooting_match" });
    }
    result.modules.push(AchievementModuleProgress {
        module: "meta".into(),
        earned: meta.iter().filter(|item| item.earned).count() as u64,
        available: meta.len() as u64,
        achievements: meta,
        prestige: Vec::new(),
    });
    let mut stmt = conn.prepare("SELECT module,achievement_id,unlocked_at FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 ORDER BY unlocked_at DESC LIMIT 50")?;
    result.recent = stmt
        .query_map(rusqlite::params![server, profile_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .filter_map(Result::ok)
        .filter_map(|(module, id, unlocked_at)| {
            let name = if module == "meta" {
                match id.as_str() {
                    "modest_mantelpiece" => Some("A Modest Mantelpiece".into()),
                    "cabinet_curiosities" => Some("Cabinet of Curiosities".into()),
                    "distinguished_collection" => Some("A Most Distinguished Collection".into()),
                    "whole_shooting_match" => Some("The Whole Shooting Match".into()),
                    _ => None,
                }
            } else {
                manifests
                    .iter()
                    .find(|(catalog_module, _)| catalog_module == &module)
                    .and_then(|(_, manifest)| {
                        manifest.achievements.iter().find(|item| item.id == id)
                    })
                    .map(|item| item.name.clone())
            }?;
            Some(AchievementUnlock {
                module,
                id,
                name,
                unlocked_at,
            })
        })
        .take(5)
        .collect();
    result
        .closest
        .sort_by_key(|p| p.threshold.saturating_sub(p.current));
    result.closest.truncate(3);
    Ok(result)
}

fn achievements_export(
    conn: &Connection,
    server: &str,
    profile_id: &str,
) -> Result<AchievementDataExport> {
    let mut stats_stmt = conn.prepare("SELECT module,stat,value FROM achievement_stats WHERE server=?1 AND profile_id=?2 ORDER BY module,stat")?;
    let stats = stats_stmt
        .query_map((server, profile_id), |row| {
            Ok(AchievementStatValue {
                module: row.get(0)?,
                stat: row.get(1)?,
                value: row.get::<_, i64>(2)? as u64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut unlock_stmt = conn.prepare("SELECT module,achievement_id,unlocked_at FROM achievement_unlocks WHERE server=?1 AND profile_id=?2 ORDER BY unlocked_at,module,achievement_id")?;
    let unlocks = unlock_stmt
        .query_map((server, profile_id), |row| {
            let id: String = row.get(1)?;
            Ok(AchievementUnlock {
                module: row.get(0)?,
                name: id.clone(),
                id,
                unlocked_at: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut prestige_stmt = conn.prepare("SELECT module,prestige_id,max_rank FROM achievement_prestige WHERE server=?1 AND profile_id=?2 ORDER BY module,prestige_id")?;
    let prestige = prestige_stmt
        .query_map((server, profile_id), |row| {
            let id: String = row.get(1)?;
            Ok(PrestigeRank {
                module: row.get(0)?,
                name: id.clone(),
                id,
                rank: row.get::<_, i64>(2)? as u64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut backfill_stmt = conn.prepare("SELECT module,catalog_version FROM achievement_backfills WHERE server=?1 AND profile_id=?2 ORDER BY module")?;
    let backfills = backfill_stmt
        .query_map((server, profile_id), |row| {
            Ok(AchievementBackfillMarker {
                module: row.get(0)?,
                catalog_version: row.get::<_, i64>(1)? as u32,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut dedup_stmt = conn.prepare("SELECT module,event_id,created_at FROM achievement_dedup WHERE server=?1 AND profile_id=?2 ORDER BY module,event_id")?;
    let deduplication = dedup_stmt
        .query_map((server, profile_id), |row| {
            Ok(AchievementDedupExport {
                module: row.get(0)?,
                event_id: row.get(1)?,
                created_at: row.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(AchievementDataExport {
        stats,
        unlocks,
        prestige,
        backfills,
        deduplication,
    })
}

fn achievement_backfill_apply(
    conn: &mut Connection,
    server: &str,
    module: &str,
    manifest: &AchievementManifest,
    values: &[AchievementSetMax],
    now: i64,
) -> Result<()> {
    validate_achievement_manifest(module, manifest)?;
    if values.len() > 100_000 {
        return Err(anyhow!("achievement backfill is too large"));
    }
    let known = manifest
        .stats
        .iter()
        .map(|stat| stat.id.as_str())
        .collect::<std::collections::HashSet<_>>();
    let tx = conn.transaction()?;
    let mut profiles = std::collections::BTreeSet::new();
    for value in values {
        if value.profile_id.is_empty()
            || !known.contains(value.stat.as_str())
            || value.value > i64::MAX as u64
        {
            return Err(anyhow!("malformed achievement backfill for {module}"));
        }
        let eligibility = tx
            .query_row(
                "SELECT achievements_opt_out, achievements_backfill_blocked
             FROM profiles WHERE server=?1 AND id=?2",
                (server, &value.profile_id),
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((opted_out, backfill_blocked)) = eligibility else {
            return Err(anyhow!(
                "achievement backfill references unknown profile UUID"
            ));
        };
        // Once a user resets achievement progress, historical module state must never recreate
        // it. The durable block remains set after opt-in; live awards then resume from zero.
        if opted_out != 0 || backfill_blocked != 0 {
            continue;
        }
        profiles.insert(value.profile_id.clone());
        tx.execute("INSERT INTO achievement_stats(server,profile_id,module,stat,value) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(server,profile_id,module,stat) DO UPDATE SET value=MAX(value,excluded.value)", rusqlite::params![server, value.profile_id, module, value.stat, value.value as i64])?;
        for item in manifest
            .achievements
            .iter()
            .filter(|item| item.stat == value.stat && item.threshold <= value.value)
        {
            tx.execute("INSERT OR IGNORE INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,?3,?4,?5)", rusqlite::params![server, value.profile_id, module, item.id, now])?;
        }
        tx.execute("INSERT INTO achievement_backfills(server,profile_id,module,catalog_version) VALUES(?1,?2,?3,?4) ON CONFLICT(server,profile_id,module) DO UPDATE SET catalog_version=MAX(catalog_version,excluded.catalog_version)", rusqlite::params![server, value.profile_id, module, manifest.catalog_version])?;
    }
    for profile_id in profiles {
        reconcile_module_achievements(&tx, server, &profile_id, module, manifest, now)?;
    }
    tx.execute(
        "INSERT INTO achievement_catalog_versions(server,module,catalog_version) VALUES(?1,?2,?3) ON CONFLICT(server,module) DO UPDATE SET catalog_version=MAX(catalog_version,excluded.catalog_version)",
        rusqlite::params![server, module, manifest.catalog_version],
    )?;
    tx.commit()?;
    Ok(())
}

fn reconcile_module_achievements(
    tx: &rusqlite::Transaction<'_>,
    server: &str,
    profile_id: &str,
    module: &str,
    manifest: &AchievementManifest,
    now: i64,
) -> Result<()> {
    for item in &manifest.achievements {
        let value = tx
            .query_row(
                "SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
                rusqlite::params![server, profile_id, module, item.stat],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0) as u64;
        if value >= item.threshold {
            tx.execute(
                "INSERT OR IGNORE INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,?3,?4,?5)",
                rusqlite::params![server, profile_id, module, item.id, now],
            )?;
        }
    }
    for item in &manifest.prestige {
        let value = tx
            .query_row(
                "SELECT value FROM achievement_stats WHERE server=?1 AND profile_id=?2 AND module=?3 AND stat=?4",
                rusqlite::params![server, profile_id, module, item.stat],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0) as u64;
        let rank = if value < item.first_threshold {
            0
        } else {
            1 + (value - item.first_threshold) / item.every
        };
        if rank > 0 {
            tx.execute(
                "INSERT INTO achievement_prestige(server,profile_id,module,prestige_id,max_rank) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(server,profile_id,module,prestige_id) DO UPDATE SET max_rank=MAX(max_rank,excluded.max_rank)",
                rusqlite::params![server, profile_id, module, item.id, rank as i64],
            )?;
        }
    }
    Ok(())
}

fn achievement_catalog_reconcile(
    conn: &mut Connection,
    catalogs: &[(String, AchievementManifest)],
    now: i64,
) -> Result<()> {
    for (module, manifest) in catalogs {
        validate_achievement_manifest(module, manifest)?;
    }
    let profiles = {
        let mut stmt = conn.prepare(
            "SELECT server,id FROM profiles WHERE id IS NOT NULL AND id!='' ORDER BY server,id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let tx = conn.transaction()?;
    for (server, profile_id) in profiles {
        for (module, manifest) in catalogs {
            reconcile_module_achievements(&tx, &server, &profile_id, module, manifest, now)?;
        }
        let mut silent = AwardStatsResponse::default();
        apply_meta_achievements(&tx, &server, &profile_id, catalogs, now, &mut silent)?;
    }
    tx.commit()?;
    Ok(())
}

fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS config (
            key   TEXT PRIMARY KEY,
            value TEXT
        );
        CREATE TABLE IF NOT EXISTS command_alias_overrides (
            module  TEXT NOT NULL,
            command TEXT NOT NULL,
            aliases TEXT NOT NULL,
            PRIMARY KEY (module, command)
        );
        CREATE TABLE IF NOT EXISTS module_setting_overrides (
            module  TEXT NOT NULL,
            key     TEXT NOT NULL,
            scope   TEXT NOT NULL,
            server  TEXT NOT NULL DEFAULT '',
            channel TEXT NOT NULL DEFAULT '',
            value   TEXT NOT NULL,
            PRIMARY KEY (module, key, scope, server, channel)
        );
        CREATE TABLE IF NOT EXISTS scheduled_jobs (
            module     TEXT NOT NULL,
            id         TEXT NOT NULL,
            server     TEXT NOT NULL,
            channel    TEXT NOT NULL,
            owner_profile_id TEXT,
            due_at     INTEGER NOT NULL,
            payload    TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (module, id)
        );
        CREATE INDEX IF NOT EXISTS scheduled_jobs_due_idx ON scheduled_jobs(due_at);

        CREATE TABLE IF NOT EXISTS module_lifecycle_registry (
            module     TEXT PRIMARY KEY,
            updated_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS data_deletion_jobs (
            id                       TEXT PRIMARY KEY,
            server                   TEXT,
            profile_id               TEXT,
            requester_profile_id     TEXT,
            status                   TEXT NOT NULL,
            confirmation_token       TEXT NOT NULL UNIQUE,
            confirmation_expires_at  INTEGER NOT NULL,
            created_at               INTEGER NOT NULL,
            updated_at               INTEGER NOT NULL,
            last_error               TEXT
        );

        CREATE TABLE IF NOT EXISTS data_deletion_modules (
            job_id     TEXT NOT NULL,
            module     TEXT NOT NULL,
            status     TEXT NOT NULL,
            PRIMARY KEY(job_id, module)
        );
        CREATE TABLE IF NOT EXISTS servers (
            id       INTEGER PRIMARY KEY,
            label    TEXT NOT NULL DEFAULT '',
            enabled  INTEGER NOT NULL DEFAULT 1,
            host     TEXT NOT NULL DEFAULT '',
            port     INTEGER NOT NULL DEFAULT 6697,
            tls      INTEGER NOT NULL DEFAULT 1,
            nick     TEXT NOT NULL DEFAULT 'jeeves',
            username TEXT NOT NULL DEFAULT 'jeeves',
            realname TEXT NOT NULL DEFAULT 'rustjeeves',
            accept_invalid_certs INTEGER NOT NULL DEFAULT 0,
            umodes TEXT
        );
        CREATE TABLE IF NOT EXISTS sasl (
            server_id INTEGER PRIMARY KEY,
            mechanism TEXT NOT NULL DEFAULT 'PLAIN',
            account   TEXT,
            password  TEXT,
            nick_password TEXT
        );
        CREATE TABLE IF NOT EXISTS channels (
            server_id INTEGER NOT NULL,
            name      TEXT NOT NULL,
            key       TEXT,
            PRIMARY KEY (server_id, name)
        );
        CREATE TABLE IF NOT EXISTS admins (
            server_id      INTEGER NOT NULL,
            nick           TEXT NOT NULL,
            role           TEXT NOT NULL,
            account        TEXT,
            bound_hostmask TEXT,
            bound_account  TEXT,
            PRIMARY KEY (server_id, nick)
        );
        CREATE TABLE IF NOT EXISTS profiles (
            id                 TEXT,
            server             TEXT NOT NULL,
            nick               TEXT NOT NULL COLLATE NOCASE,
            created            INTEGER NOT NULL,
            last_seen          INTEGER NOT NULL,
            title              TEXT,
            birthday           TEXT,
            pronoun_subject    TEXT,
            pronoun_object     TEXT,
            pronoun_possessive TEXT,
            location_display   TEXT,
            location_label     TEXT,
            lat                REAL,
            lon                REAL,
            timezone           TEXT,
            achievements_opt_out INTEGER NOT NULL DEFAULT 0,
            achievements_backfill_blocked INTEGER NOT NULL DEFAULT 0,
            achievements_public INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (server, nick)
        );
        CREATE TABLE IF NOT EXISTS module_kv (
            module TEXT NOT NULL,
            key    TEXT NOT NULL,
            value  TEXT,
            PRIMARY KEY (module, key)
        );
        CREATE TABLE IF NOT EXISTS logs (
            id       INTEGER PRIMARY KEY,
            ts       INTEGER NOT NULL,
            level    TEXT NOT NULL,
            category TEXT NOT NULL,
            source   TEXT NOT NULL,
            message  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS achievement_stats (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, module TEXT NOT NULL, stat TEXT NOT NULL,
            value INTEGER NOT NULL CHECK(value >= 0), PRIMARY KEY(server,profile_id,module,stat)
        );
        CREATE TABLE IF NOT EXISTS achievement_unlocks (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, module TEXT NOT NULL, achievement_id TEXT NOT NULL,
            unlocked_at INTEGER NOT NULL, PRIMARY KEY(server,profile_id,module,achievement_id)
        );
        CREATE INDEX IF NOT EXISTS achievement_unlocks_recent ON achievement_unlocks(server,profile_id,unlocked_at DESC);
        CREATE TABLE IF NOT EXISTS achievement_prestige (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, module TEXT NOT NULL, prestige_id TEXT NOT NULL,
            max_rank INTEGER NOT NULL CHECK(max_rank >= 0), PRIMARY KEY(server,profile_id,module,prestige_id)
        );
        CREATE TABLE IF NOT EXISTS achievement_backfills (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, module TEXT NOT NULL, catalog_version INTEGER NOT NULL,
            PRIMARY KEY(server,profile_id,module)
        );
        CREATE TABLE IF NOT EXISTS achievement_catalog_versions (
            server TEXT NOT NULL, module TEXT NOT NULL, catalog_version INTEGER NOT NULL,
            PRIMARY KEY(server,module)
        );
        CREATE TABLE IF NOT EXISTS achievement_dedup (
            server TEXT NOT NULL, profile_id TEXT NOT NULL, module TEXT NOT NULL, event_id TEXT NOT NULL,
            created_at INTEGER NOT NULL, PRIMARY KEY(server,profile_id,module,event_id)
        );
        "#,
    )?;
    // Defensive migrations for databases created before these columns existed. Check the schema
    // explicitly so only the expected "already present" condition is ignored.
    add_column_if_missing(
        conn,
        "servers",
        "accept_invalid_certs",
        "ALTER TABLE servers ADD COLUMN accept_invalid_certs INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "servers",
        "label",
        "ALTER TABLE servers ADD COLUMN label TEXT NOT NULL DEFAULT ''",
    )?;
    add_column_if_missing(
        conn,
        "servers",
        "enabled",
        "ALTER TABLE servers ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
    )?;
    add_column_if_missing(
        conn,
        "servers",
        "umodes",
        "ALTER TABLE servers ADD COLUMN umodes TEXT",
    )?;
    add_column_if_missing(
        conn,
        "profiles",
        "id",
        "ALTER TABLE profiles ADD COLUMN id TEXT",
    )?;
    add_column_if_missing(
        conn,
        "profiles",
        "timezone",
        "ALTER TABLE profiles ADD COLUMN timezone TEXT",
    )?;
    add_column_if_missing(
        conn,
        "profiles",
        "achievements_opt_out",
        "ALTER TABLE profiles ADD COLUMN achievements_opt_out INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "profiles",
        "achievements_backfill_blocked",
        "ALTER TABLE profiles ADD COLUMN achievements_backfill_blocked INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "profiles",
        "achievements_public",
        "ALTER TABLE profiles ADD COLUMN achievements_public INTEGER NOT NULL DEFAULT 0",
    )?;
    add_column_if_missing(
        conn,
        "scheduled_jobs",
        "owner_profile_id",
        "ALTER TABLE scheduled_jobs ADD COLUMN owner_profile_id TEXT",
    )?;
    // Give any pre-existing rows a unique non-empty label.
    conn.execute(
        "UPDATE servers SET label = 'server' || id WHERE label = '' OR label IS NULL",
        [],
    )?;
    // Existing nick-keyed profiles receive stable IDs. Alias/account tables retain all future
    // identity information without destructively rewriting the original profile table.
    let mut missing =
        conn.prepare("SELECT server, nick FROM profiles WHERE id IS NULL OR id = ''")?;
    let rows = missing
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(missing);
    for (server, nick) in rows {
        conn.execute(
            "UPDATE profiles SET id=?3 WHERE server=?1 AND nick=?2",
            rusqlite::params![server, nick, uuid::Uuid::new_v4().to_string()],
        )?;
    }
    conn.execute_batch(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS profiles_id_idx ON profiles(id);
        CREATE INDEX IF NOT EXISTS logs_ts_idx ON logs(ts);
        CREATE INDEX IF NOT EXISTS logs_category_idx ON logs(category);
        CREATE TABLE IF NOT EXISTS profile_aliases (
            server TEXT NOT NULL,
            nick TEXT NOT NULL COLLATE NOCASE,
            profile_id TEXT NOT NULL,
            last_seen INTEGER NOT NULL,
            PRIMARY KEY(server, nick)
        );
        CREATE TABLE IF NOT EXISTS profile_accounts (
            server TEXT NOT NULL,
            account TEXT NOT NULL COLLATE NOCASE,
            profile_id TEXT NOT NULL,
            PRIMARY KEY(server, account)
        );
        INSERT OR IGNORE INTO profile_aliases(server, nick, profile_id, last_seen)
            SELECT server, nick, id, last_seen FROM profiles WHERE id IS NOT NULL;
        "#,
    )?;
    Ok(())
}

fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    alter_sql: &str,
) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
        rusqlite::params![table, column],
        |row| row.get(0),
    )?;
    if !exists {
        conn.execute(alter_sql, [])?;
    }
    Ok(())
}

fn load_servers(conn: &Connection) -> Result<Vec<ServerConfig>> {
    let mut servers = {
        let mut stmt = conn.prepare(
            "SELECT id, label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs, umodes
             FROM servers ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ServerConfig {
                id: row.get(0)?,
                label: row.get(1)?,
                enabled: row.get::<_, i64>(2)? != 0,
                host: row.get(3)?,
                port: row.get::<_, i64>(4)? as u16,
                tls: row.get::<_, i64>(5)? != 0,
                nick: row.get(6)?,
                username: row.get(7)?,
                realname: row.get(8)?,
                accept_invalid_certs: row.get::<_, i64>(9)? != 0,
                umodes: row.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty()),
                sasl_account: None,
                sasl_password: None,
                nick_password: None,
                channels: Vec::new(),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    for cfg in &mut servers {
        load_sasl_and_channels(conn, cfg)?;
    }
    Ok(servers)
}

fn load_sasl_and_channels(conn: &Connection, cfg: &mut ServerConfig) -> Result<()> {
    if let Some((account, password, nick_password)) = conn
        .query_row(
            "SELECT account, password, nick_password FROM sasl WHERE server_id = ?1",
            [cfg.id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
    {
        cfg.sasl_account = account.filter(|s| !s.is_empty());
        cfg.sasl_password = password.filter(|s| !s.is_empty());
        cfg.nick_password = nick_password.filter(|s| !s.is_empty());
    }

    let mut stmt =
        conn.prepare("SELECT name, key FROM channels WHERE server_id = ?1 ORDER BY name")?;
    let rows = stmt.query_map([cfg.id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    cfg.channels = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(())
}

fn upsert_server(conn: &mut Connection, cfg: &ServerConfig) -> Result<i64> {
    let tx = conn.transaction()?;
    let label = if cfg.label.trim().is_empty() {
        if cfg.host.is_empty() {
            "default".to_string()
        } else {
            cfg.host.clone()
        }
    } else {
        cfg.label.trim().to_string()
    };

    // Enforce label uniqueness across distinct rows.
    let conflict: Option<i64> = tx
        .query_row(
            "SELECT id FROM servers WHERE label = ?1 AND id <> ?2",
            rusqlite::params![label, cfg.id],
            |r| r.get(0),
        )
        .optional()?;
    if conflict.is_some() {
        return Err(anyhow!("server label '{label}' is already in use"));
    }

    let id = if cfg.id == 0 {
        tx.execute(
            "INSERT INTO servers (label, enabled, host, port, tls, nick, username, realname, accept_invalid_certs, umodes)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        tx.last_insert_rowid()
    } else {
        let changed = tx.execute(
            "UPDATE servers SET label=?2, enabled=?3, host=?4, port=?5, tls=?6,
                nick=?7, username=?8, realname=?9, accept_invalid_certs=?10, umodes=?11 WHERE id=?1",
            rusqlite::params![
                cfg.id, label, cfg.enabled as i64, cfg.host, cfg.port as i64, cfg.tls as i64,
                cfg.nick, cfg.username, cfg.realname, cfg.accept_invalid_certs as i64, cfg.umodes,
            ],
        )?;
        if changed == 0 {
            return Err(anyhow!("server id {} does not exist", cfg.id));
        }
        cfg.id
    };

    tx.execute(
        "INSERT INTO sasl (server_id, mechanism, account, password, nick_password)
         VALUES (?1, 'PLAIN', ?2, ?3, ?4)
         ON CONFLICT(server_id) DO UPDATE SET
            account=excluded.account, password=excluded.password, nick_password=excluded.nick_password",
        rusqlite::params![id, cfg.sasl_account, cfg.sasl_password, cfg.nick_password],
    )?;

    tx.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    for (name, key) in &cfg.channels {
        tx.execute(
            "INSERT OR REPLACE INTO channels (server_id, name, key) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, name, key],
        )?;
    }

    tx.commit()?;
    Ok(id)
}

fn delete_server(conn: &mut Connection, id: i64) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM channels WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM sasl WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM admins WHERE server_id = ?1", [id])?;
    tx.execute("DELETE FROM servers WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
fn profile_ensure(conn: &Connection, server: &str, nick: &str, now: i64) -> Result<()> {
    profile_ensure_mapped(conn, server, nick, now, CaseMapping::default())
}

fn profile_ensure_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    now: i64,
    casemapping: CaseMapping,
) -> Result<()> {
    let _ = profile_resolve_mapped(conn, server, nick, None, now, casemapping)?;
    Ok(())
}

fn profile_identity_links(
    conn: &Connection,
    server: &str,
    profile_id: &str,
) -> Result<(Vec<ProfileAliasExport>, Vec<String>)> {
    let mut alias_stmt = conn.prepare(
        "SELECT nick, last_seen FROM profile_aliases
         WHERE server = ?1 AND profile_id = ?2 ORDER BY nick COLLATE NOCASE",
    )?;
    let aliases = alias_stmt
        .query_map((server, profile_id), |row| {
            Ok(ProfileAliasExport {
                nick: row.get(0)?,
                last_seen: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut account_stmt = conn.prepare(
        "SELECT account FROM profile_accounts
         WHERE server = ?1 AND profile_id = ?2 ORDER BY account COLLATE NOCASE",
    )?;
    let accounts = account_stmt
        .query_map((server, profile_id), |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok((aliases, accounts))
}

#[cfg(test)]
fn profile_get(conn: &Connection, server: &str, nick: &str) -> Result<Option<Profile>> {
    profile_get_mapped(conn, server, nick, CaseMapping::default())
}

fn profile_get_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    casemapping: CaseMapping,
) -> Result<Option<Profile>> {
    let Some(id) = profile_id_for_mapped(conn, server, nick, None, casemapping)? else {
        return Ok(None);
    };
    let stored_alias = matching_profile_alias(conn, server, nick, casemapping)?
        .map(|(stored_nick, _)| stored_nick)
        .unwrap_or_else(|| nick.to_string());
    profile_get_by_id(conn, &id, &stored_alias)
}

fn profile_list(conn: &Connection) -> Result<Vec<Profile>> {
    let mut stmt = conn.prepare(
        "SELECT id, server, nick, created, last_seen, title, birthday,
                pronoun_subject, pronoun_object, pronoun_possessive,
                location_display, location_label, lat, lon, timezone, achievements_opt_out,
                achievements_public
         FROM profiles
         WHERE id IS NOT NULL AND id != ''
         ORDER BY server COLLATE NOCASE, nick COLLATE NOCASE",
    )?;
    let profiles = stmt
        .query_map([], |row| {
            Ok(Profile {
                id: row.get(0)?,
                server: row.get(1)?,
                nick: row.get(2)?,
                created: row.get(3)?,
                last_seen: row.get(4)?,
                title: row.get(5)?,
                birthday: row.get(6)?,
                pronoun_subject: row.get(7)?,
                pronoun_object: row.get(8)?,
                pronoun_possessive: row.get(9)?,
                location_display: row.get(10)?,
                location_label: row.get(11)?,
                lat: row.get(12)?,
                lon: row.get(13)?,
                timezone: row.get(14)?,
                achievements_opt_out: profile_opt_out_flag(row.get::<_, i64>(15)?),
                achievements_public: profile_opt_out_flag(row.get::<_, i64>(16)?),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(profiles)
}

fn profile_get_by_id(conn: &Connection, id: &str, observed_nick: &str) -> Result<Option<Profile>> {
    let p = conn
        .query_row(
            "SELECT id, server, nick, created, last_seen, title, birthday,
                    pronoun_subject, pronoun_object, pronoun_possessive,
                    location_display, location_label, lat, lon, timezone, achievements_opt_out,
                    achievements_public
             FROM profiles WHERE id = ?1",
            [id],
            |row| {
                Ok(Profile {
                    id: row.get(0)?,
                    server: row.get(1)?,
                    nick: row.get(2)?,
                    created: row.get(3)?,
                    last_seen: row.get(4)?,
                    title: row.get(5)?,
                    birthday: row.get(6)?,
                    pronoun_subject: row.get(7)?,
                    pronoun_object: row.get(8)?,
                    pronoun_possessive: row.get(9)?,
                    location_display: row.get(10)?,
                    location_label: row.get(11)?,
                    lat: row.get(12)?,
                    lon: row.get(13)?,
                    timezone: row.get(14)?,
                    achievements_opt_out: profile_opt_out_flag(row.get::<_, i64>(15)?),
                    achievements_public: profile_opt_out_flag(row.get::<_, i64>(16)?),
                })
            },
        )
        .optional()?;
    Ok(p.map(|mut p| {
        p.nick = observed_nick.to_string();
        p
    }))
}

/// Map the on-disk INTEGER column to the ABI `Option<bool>` (`0` -> opted in, nonzero -> opted out).
fn profile_opt_out_flag(value: i64) -> Option<bool> {
    (value != 0).then_some(true)
}

fn profile_id_for_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    account: Option<&str>,
    casemapping: CaseMapping,
) -> Result<Option<String>> {
    if let Some(account) = account.filter(|a| !a.is_empty()) {
        let id = conn
            .query_row(
                "SELECT profile_id FROM profile_accounts WHERE server=?1 AND account=?2",
                rusqlite::params![server, account],
                |r| r.get(0),
            )
            .optional()?;
        if id.is_some() {
            return Ok(id);
        }
    }
    if let Some((_, alias_id)) = matching_profile_alias(conn, server, nick, casemapping)? {
        // A different authenticated account reusing a nick must not inherit the old account's
        // profile. An unclaimed nick alias may be upgraded to its first services account.
        if account.is_some() {
            let already_account_backed: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM profile_accounts WHERE server=?1 AND profile_id=?2)",
                rusqlite::params![server, alias_id],
                |r| r.get(0),
            )?;
            if already_account_backed {
                return Ok(None);
            }
        }
        return Ok(Some(alias_id));
    }
    let mut stmt = conn.prepare("SELECT nick, id FROM profiles WHERE server=?1")?;
    let rows = stmt.query_map([server], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (stored_nick, id) = row?;
        if casemapping.equivalent(&stored_nick, nick) {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

fn matching_profile_alias(
    conn: &Connection,
    server: &str,
    nick: &str,
    casemapping: CaseMapping,
) -> Result<Option<(String, String)>> {
    let mut stmt = conn.prepare("SELECT nick, profile_id FROM profile_aliases WHERE server=?1")?;
    let rows = stmt.query_map([server], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut matched: Option<(String, String)> = None;
    for row in rows {
        let (stored_nick, profile_id) = row?;
        if casemapping.equivalent(&stored_nick, nick) {
            if let Some((_, matched_id)) = &matched {
                if matched_id != &profile_id {
                    anyhow::bail!(
                        "multiple profiles own IRC-equivalent aliases for '{nick}' on '{server}'; repair the duplicate identities before continuing"
                    );
                }
            }
            if matched.is_none() || stored_nick == nick {
                matched = Some((stored_nick, profile_id));
            }
        }
    }
    Ok(matched)
}

fn upsert_profile_alias_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    profile_id: &str,
    now: i64,
    casemapping: CaseMapping,
) -> Result<()> {
    if let Some((stored_nick, _)) = matching_profile_alias(conn, server, nick, casemapping)? {
        conn.execute(
            "UPDATE profile_aliases SET profile_id=?3, last_seen=?4
             WHERE server=?1 AND nick=?2",
            rusqlite::params![server, stored_nick, profile_id, now],
        )?;
    } else {
        conn.execute(
            "INSERT INTO profile_aliases(server, nick, profile_id, last_seen)
             VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![server, nick, profile_id, now],
        )?;
    }
    Ok(())
}

#[cfg(test)]
fn profile_resolve(
    conn: &Connection,
    server: &str,
    nick: &str,
    account: Option<&str>,
    now: i64,
) -> Result<Profile> {
    profile_resolve_mapped(conn, server, nick, account, now, CaseMapping::default())
}

fn profile_resolve_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    account: Option<&str>,
    now: i64,
    casemapping: CaseMapping,
) -> Result<Profile> {
    let id = match profile_id_for_mapped(conn, server, nick, account, casemapping)? {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let occupied = profile_id_for_mapped(conn, server, nick, None, casemapping)?.is_some();
            let stored_nick = if occupied {
                format!("{nick}~{}", &id[..8])
            } else {
                nick.to_string()
            };
            conn.execute(
                "INSERT INTO profiles (id, server, nick, created, last_seen) VALUES (?1, ?2, ?3, ?4, ?4)",
                rusqlite::params![id, server, stored_nick, now],
            )?;
            id
        }
    };
    upsert_profile_alias_mapped(conn, server, nick, &id, now, casemapping)?;
    if let Some(account) = account.filter(|a| !a.is_empty() && *a != "*") {
        conn.execute(
            "INSERT INTO profile_accounts(server, account, profile_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(server, account) DO UPDATE SET profile_id=excluded.profile_id",
            rusqlite::params![server, account, id],
        )?;
    }
    conn.execute(
        "UPDATE OR IGNORE profiles SET last_seen=?2, nick=?3 WHERE id=?1",
        rusqlite::params![id, now, nick],
    )?;
    profile_get_by_id(conn, &id, nick)?.ok_or_else(|| anyhow!("resolved profile disappeared"))
}

#[cfg(test)]
fn profile_bind_nick(
    conn: &Connection,
    server: &str,
    old_nick: &str,
    new_nick: &str,
    account: Option<&str>,
    now: i64,
) -> Result<()> {
    profile_bind_nick_mapped(
        conn,
        server,
        old_nick,
        new_nick,
        account,
        now,
        CaseMapping::default(),
    )
}

fn profile_bind_nick_mapped(
    conn: &Connection,
    server: &str,
    old_nick: &str,
    new_nick: &str,
    account: Option<&str>,
    now: i64,
    casemapping: CaseMapping,
) -> Result<()> {
    let profile = profile_resolve_mapped(conn, server, old_nick, account, now, casemapping)?;
    upsert_profile_alias_mapped(conn, server, new_nick, &profile.id, now, casemapping)?;
    // Keep the latest nick as profile information. OR IGNORE avoids merging two legacy rows that
    // already occupy the same (server, nick) primary key; the alias still resolves correctly.
    conn.execute(
        "UPDATE OR IGNORE profiles SET nick=?2 WHERE id=?1",
        rusqlite::params![profile.id, new_nick],
    )?;
    Ok(())
}

/// Merge the `Some` fields of `u` into the profile row (creating a skeleton first if needed).
/// `COALESCE(?, col)` keeps the existing value when the bound parameter is NULL (field is `None`).
#[cfg(test)]
fn profile_set(conn: &Connection, u: &ProfileUpdate) -> Result<()> {
    profile_set_mapped(conn, u, CaseMapping::default())
}

fn profile_set_mapped(
    conn: &Connection,
    u: &ProfileUpdate,
    casemapping: CaseMapping,
) -> Result<()> {
    validate_profile_update(u)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let p = profile_resolve_mapped(conn, &u.server, &u.nick, None, now, casemapping)?;
    conn.execute(
        "UPDATE profiles SET
            title              = COALESCE(?3, title),
            birthday           = COALESCE(?4, birthday),
            pronoun_subject    = COALESCE(?5, pronoun_subject),
            pronoun_object     = COALESCE(?6, pronoun_object),
            pronoun_possessive = COALESCE(?7, pronoun_possessive),
            location_display   = COALESCE(?8, location_display),
            location_label     = COALESCE(?9, location_label),
            lat                = COALESCE(?10, lat),
            lon                = COALESCE(?11, lon),
            timezone           = COALESCE(?12, timezone)
         WHERE id = ?1",
        rusqlite::params![
            p.id,
            u.nick,
            u.title,
            u.birthday,
            u.pronoun_subject,
            u.pronoun_object,
            u.pronoun_possessive,
            u.location_display,
            u.location_label,
            u.lat,
            u.lon,
            u.timezone,
        ],
    )?;
    Ok(())
}

pub(crate) fn validate_profile_repair(profile: &Profile) -> Result<()> {
    if profile.id.trim().is_empty() || profile.server.trim().is_empty() {
        return Err(anyhow!("profile UUID and network are required"));
    }
    validate_optional_text("title", profile.title.as_deref(), 80)?;
    validate_birthday(profile.birthday.as_deref())?;
    validate_pronouns(
        profile.pronoun_subject.as_deref(),
        profile.pronoun_object.as_deref(),
        profile.pronoun_possessive.as_deref(),
        true,
    )?;
    validate_optional_text("location display", profile.location_display.as_deref(), 200)?;
    validate_optional_text("location label", profile.location_label.as_deref(), 200)?;
    validate_coordinates(profile.lat, profile.lon)?;
    validate_timezone(profile.timezone.as_deref())?;
    Ok(())
}

fn validate_profile_update(update: &ProfileUpdate) -> Result<()> {
    validate_optional_text("title", update.title.as_deref(), 80)?;
    validate_birthday(update.birthday.as_deref())?;
    validate_pronouns(
        update.pronoun_subject.as_deref(),
        update.pronoun_object.as_deref(),
        update.pronoun_possessive.as_deref(),
        false,
    )?;
    validate_optional_text("location display", update.location_display.as_deref(), 200)?;
    validate_optional_text("location label", update.location_label.as_deref(), 200)?;
    if update.lat.is_some() || update.lon.is_some() {
        validate_coordinates(update.lat, update.lon)?;
    }
    validate_timezone(update.timezone.as_deref())?;
    Ok(())
}

fn validate_optional_text(field: &str, value: Option<&str>, max_chars: usize) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    if value.chars().count() > max_chars || value.chars().any(char::is_control) {
        return Err(anyhow!(
            "{field} must contain at most {max_chars} non-control characters"
        ));
    }
    Ok(())
}

fn validate_birthday(value: Option<&str>) -> Result<()> {
    let Some(value) = value else {
        return Ok(());
    };
    let parts = value.split('-').collect::<Vec<_>>();
    if !matches!(parts.len(), 2 | 3)
        || parts[0].len() != 2
        || parts[1].len() != 2
        || !parts
            .iter()
            .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(anyhow!("birthday must be MM-DD or MM-DD-YYYY"));
    }
    let month: u32 = parts[0].parse()?;
    let day: u32 = parts[1].parse()?;
    let year = if parts.len() == 3 {
        if parts[2].len() != 4 {
            return Err(anyhow!("birthday year must use four digits"));
        }
        parts[2].parse::<i32>()?
    } else {
        2000
    };
    if chrono::NaiveDate::from_ymd_opt(year, month, day).is_none() {
        return Err(anyhow!("birthday is not a valid calendar date"));
    }
    Ok(())
}

fn validate_pronouns(
    subject: Option<&str>,
    object: Option<&str>,
    possessive: Option<&str>,
    require_complete: bool,
) -> Result<()> {
    let count = [subject, object, possessive]
        .into_iter()
        .filter(Option::is_some)
        .count();
    if require_complete && !matches!(count, 0 | 3) {
        return Err(anyhow!("pronouns must provide all three forms or none"));
    }
    for (name, value) in [
        ("pronoun subject", subject),
        ("pronoun object", object),
        ("pronoun possessive", possessive),
    ] {
        validate_optional_text(name, value, 32)?;
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(anyhow!("{name} cannot be empty"));
        }
    }
    Ok(())
}

fn validate_coordinates(lat: Option<f64>, lon: Option<f64>) -> Result<()> {
    let (Some(lat), Some(lon)) = (lat, lon) else {
        if lat.is_none() && lon.is_none() {
            return Ok(());
        }
        return Err(anyhow!(
            "latitude and longitude must be set or cleared together"
        ));
    };
    if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
        return Err(anyhow!("latitude must be between -90 and 90"));
    }
    if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
        return Err(anyhow!("longitude must be between -180 and 180"));
    }
    Ok(())
}

fn validate_timezone(value: Option<&str>) -> Result<()> {
    validate_optional_text("timezone", value, 64)?;
    if value.is_some_and(|value| {
        value.trim().is_empty()
            || !value.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'+' | b'-')
            })
    }) {
        return Err(anyhow!("timezone contains invalid characters"));
    }
    Ok(())
}

fn profile_repair(conn: &Connection, profile: &Profile) -> Result<()> {
    validate_profile_repair(profile)?;
    let changed = conn.execute(
        "UPDATE profiles SET title=?3, birthday=?4,
            pronoun_subject=?5, pronoun_object=?6, pronoun_possessive=?7,
            location_display=?8, location_label=?9, lat=?10, lon=?11, timezone=?12
         WHERE id=?1 AND server=?2",
        rusqlite::params![
            profile.id,
            profile.server,
            profile.title,
            profile.birthday,
            profile.pronoun_subject,
            profile.pronoun_object,
            profile.pronoun_possessive,
            profile.location_display,
            profile.location_label,
            profile.lat,
            profile.lon,
            profile.timezone,
        ],
    )?;
    if changed != 1 {
        return Err(anyhow!("profile no longer exists on that network"));
    }
    Ok(())
}

fn profile_editable_equal(left: &Profile, right: &Profile) -> bool {
    left.title == right.title
        && left.birthday == right.birthday
        && left.pronoun_subject == right.pronoun_subject
        && left.pronoun_object == right.pronoun_object
        && left.pronoun_possessive == right.pronoun_possessive
        && left.location_display == right.location_display
        && left.location_label == right.location_label
        && left.lat == right.lat
        && left.lon == right.lon
        && left.timezone == right.timezone
}

fn profile_repair_checked(conn: &Connection, profile: &Profile, expected: &Profile) -> Result<()> {
    if profile.id != expected.id || profile.server != expected.server {
        return Err(anyhow!("profile repair identity changed after preview"));
    }
    let current = profile_get_by_id(conn, &profile.id, &expected.nick)?
        .ok_or_else(|| anyhow!("profile no longer exists"))?;
    if !profile_editable_equal(&current, expected) {
        return Err(anyhow!(
            "profile changed after the repair preview; inspect it again before applying"
        ));
    }
    profile_repair(conn, profile)
}

/// Clear a field group on a profile by setting its column(s) to NULL. `field` is whitelisted.
fn profile_clear_mapped(
    conn: &Connection,
    server: &str,
    nick: &str,
    field: &str,
    casemapping: CaseMapping,
) -> Result<()> {
    let Some(id) = profile_id_for_mapped(conn, server, nick, None, casemapping)? else {
        return Ok(());
    };
    let sql = match field {
        "title" => "UPDATE profiles SET title=NULL WHERE id=?1",
        "birthday" => "UPDATE profiles SET birthday=NULL WHERE id=?1",
        "pronouns" => "UPDATE profiles SET pronoun_subject=NULL, pronoun_object=NULL, pronoun_possessive=NULL WHERE id=?1",
        "location" => "UPDATE profiles SET location_display=NULL, location_label=NULL, lat=NULL, lon=NULL, timezone=NULL WHERE id=?1",
        other => return Err(anyhow!("unknown profile field '{other}'")),
    };
    conn.execute(sql, [id])?;
    Ok(())
}

fn kv_get(conn: &Connection, module: &str, key: &str) -> Result<Option<String>> {
    let v = conn
        .query_row(
            "SELECT value FROM module_kv WHERE module = ?1 AND key = ?2",
            rusqlite::params![module, key],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    Ok(v)
}

fn config_get(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM config WHERE key = ?1", [key], |row| {
            row.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

fn config_set(conn: &Connection, key: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            conn.execute(
                "INSERT INTO config (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                rusqlite::params![key, value],
            )?;
        }
        None => {
            conn.execute("DELETE FROM config WHERE key = ?1", [key])?;
        }
    }
    Ok(())
}

fn backup_to(conn: &Connection, path: &str) -> Result<()> {
    let path = Path::new(path);
    if path.exists() {
        return Err(anyhow!(
            "backup destination already exists: {}",
            path.display()
        ));
    }
    conn.backup(rusqlite::MAIN_DB, path, None)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackupVerification {
    pub schema_version: i64,
    pub integrity_check: String,
}

/// Open a snapshot independently of the live DB actor, bring its schema forward if necessary,
/// and verify SQLite can read every page. This may modify an older snapshot by applying migrations.
pub(crate) fn verify_backup_file(path: &Path) -> Result<BackupVerification> {
    let conn = Connection::open(path)?;
    migrate(&conn)?;
    let integrity_check: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if !integrity_check.eq_ignore_ascii_case("ok") {
        return Err(anyhow!("backup integrity check failed: {integrity_check}"));
    }
    let schema_version = conn.query_row("PRAGMA schema_version", [], |row| row.get(0))?;
    Ok(BackupVerification {
        schema_version,
        integrity_check,
    })
}

fn load_alias_overrides(conn: &Connection) -> Result<AliasOverrides> {
    let mut stmt = conn.prepare(
        "SELECT module, command, aliases FROM command_alias_overrides ORDER BY module, command",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut overrides = AliasOverrides::new();
    for row in rows {
        let (module, command, aliases) = row?;
        overrides.insert((module, command), serde_json::from_str(&aliases)?);
    }
    Ok(overrides)
}

fn set_alias_override(
    conn: &Connection,
    module: &str,
    command: &str,
    aliases: Option<&[String]>,
) -> Result<()> {
    match aliases {
        Some(aliases) => {
            let aliases = serde_json::to_string(aliases)?;
            conn.execute(
                "INSERT INTO command_alias_overrides(module, command, aliases)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(module, command) DO UPDATE SET aliases=excluded.aliases",
                rusqlite::params![module, command, aliases],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM command_alias_overrides WHERE module=?1 AND command=?2",
                rusqlite::params![module, command],
            )?;
        }
    }
    Ok(())
}

fn load_setting_overrides(conn: &Connection) -> Result<Vec<SettingOverride>> {
    let mut stmt = conn.prepare(
        "SELECT module, key, scope, server, channel, value
         FROM module_setting_overrides ORDER BY module, key, scope, server, channel",
    )?;
    let rows = stmt.query_map([], |row| {
        let raw_scope = row.get::<_, String>(2)?;
        let scope = match raw_scope.as_str() {
            "global" => SettingScope::Global,
            "network" => SettingScope::Network,
            "channel" => SettingScope::Channel,
            _ => {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    2,
                    rusqlite::types::Type::Text,
                    format!("unknown setting scope '{raw_scope}'").into(),
                ));
            }
        };
        Ok(SettingOverride {
            module: row.get(0)?,
            key: row.get(1)?,
            scope,
            server: row.get(3)?,
            channel: row.get(4)?,
            value: row.get(5)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn setting_override_get(
    conn: &Connection,
    module: &str,
    key: &str,
    scope: SettingScope,
    server: &str,
    channel: &str,
) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM module_setting_overrides
             WHERE module=?1 AND key=?2 AND scope=?3 AND server=?4 AND channel=?5",
            rusqlite::params![module, key, scope_name(scope), server, channel],
            |row| row.get(0),
        )
        .optional()?)
}

fn setting_override_set(
    conn: &Connection,
    module: &str,
    key: &str,
    scope: SettingScope,
    server: &str,
    channel: &str,
    value: Option<&str>,
) -> Result<()> {
    match value {
        Some(value) => {
            conn.execute(
                "INSERT INTO module_setting_overrides(module, key, scope, server, channel, value)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(module, key, scope, server, channel)
                 DO UPDATE SET value=excluded.value",
                rusqlite::params![module, key, scope_name(scope), server, channel, value],
            )?;
        }
        None => {
            conn.execute(
                "DELETE FROM module_setting_overrides
                 WHERE module=?1 AND key=?2 AND scope=?3 AND server=?4 AND channel=?5",
                rusqlite::params![module, key, scope_name(scope), server, channel],
            )?;
        }
    }
    Ok(())
}

fn load_scheduled_jobs(conn: &Connection) -> Result<Vec<ScheduledJob>> {
    let mut stmt = conn.prepare(
        "SELECT module, id, server, channel, owner_profile_id, due_at, payload, created_at
         FROM scheduled_jobs ORDER BY due_at, module, id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ScheduledJob {
            module: row.get(0)?,
            id: row.get(1)?,
            server: row.get(2)?,
            channel: row.get(3)?,
            owner_profile_id: row.get(4)?,
            due_at: row.get(5)?,
            payload: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn set_scheduled_job(conn: &Connection, job: &ScheduledJob) -> Result<()> {
    conn.execute(
        "INSERT INTO scheduled_jobs(
            module, id, server, channel, owner_profile_id, due_at, payload, created_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(module, id) DO UPDATE SET server=excluded.server,
             channel=excluded.channel, owner_profile_id=excluded.owner_profile_id,
             due_at=excluded.due_at, payload=excluded.payload, created_at=excluded.created_at",
        rusqlite::params![
            job.module,
            job.id,
            job.server,
            job.channel,
            job.owner_profile_id,
            job.due_at,
            job.payload,
            job.created_at
        ],
    )?;
    Ok(())
}

fn delete_scheduled_job(conn: &Connection, module: &str, id: &str) -> Result<bool> {
    Ok(conn.execute(
        "DELETE FROM scheduled_jobs WHERE module=?1 AND id=?2",
        rusqlite::params![module, id],
    )? > 0)
}

fn kv_set(conn: &Connection, module: &str, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO module_kv (module, key, value) VALUES (?1, ?2, ?3)",
        rusqlite::params![module, key, value],
    )?;
    Ok(())
}

fn kv_list_module(conn: &Connection, module: &str) -> Result<Vec<ModuleKvEntry>> {
    let mut stmt = conn.prepare("SELECT key, value FROM module_kv WHERE module=?1 ORDER BY key")?;
    let entries = stmt
        .query_map([module], |row| {
            Ok(ModuleKvEntry {
                key: row.get(0)?,
                value: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(entries)
}

fn kv_apply_module(
    conn: &mut Connection,
    module: &str,
    allowed_keys: &[String],
    mutations: &[ModuleKvMutation],
) -> Result<()> {
    use std::collections::HashSet;
    let allowed = allowed_keys
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let mut mutated = HashSet::new();
    if mutations.iter().any(|mutation| {
        !allowed.contains(mutation.key.as_str()) || !mutated.insert(mutation.key.as_str())
    }) {
        anyhow::bail!("module lifecycle plan contains an unknown or duplicate KV mutation");
    }
    let transaction = conn.transaction()?;
    for mutation in mutations {
        match &mutation.value {
            Some(value) => {
                transaction.execute(
                    "UPDATE module_kv SET value=?3 WHERE module=?1 AND key=?2",
                    (module, &mutation.key, value),
                )?;
            }
            None => {
                transaction.execute(
                    "DELETE FROM module_kv WHERE module=?1 AND key=?2",
                    (module, &mutation.key),
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

fn kv_apply_module_checked(
    conn: &mut Connection,
    module: &str,
    expected: &[ModuleKvEntry],
    mutations: &[ModuleKvMutation],
) -> Result<()> {
    use std::collections::{HashMap, HashSet};
    let expected = expected
        .iter()
        .map(|entry| (entry.key.as_str(), entry.value.as_str()))
        .collect::<HashMap<_, _>>();
    let mut mutated = HashSet::new();
    if mutations.iter().any(|mutation| {
        !expected.contains_key(mutation.key.as_str()) || !mutated.insert(mutation.key.as_str())
    }) {
        anyhow::bail!("module repair plan contains an unknown or duplicate KV mutation");
    }
    let transaction = conn.transaction()?;
    for mutation in mutations {
        let current = transaction
            .query_row(
                "SELECT value FROM module_kv WHERE module=?1 AND key=?2",
                (module, &mutation.key),
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if current.as_deref() != expected.get(mutation.key.as_str()).copied() {
            anyhow::bail!(
                "module state changed after the repair preview; inspect it again before applying"
            );
        }
    }
    for mutation in mutations {
        match &mutation.value {
            Some(value) => {
                transaction.execute(
                    "UPDATE module_kv SET value=?3 WHERE module=?1 AND key=?2",
                    (module, &mutation.key, value),
                )?;
            }
            None => {
                transaction.execute(
                    "DELETE FROM module_kv WHERE module=?1 AND key=?2",
                    (module, &mutation.key),
                )?;
            }
        }
    }
    transaction.commit()?;
    Ok(())
}

fn lifecycle_register(conn: &Connection, module: &str, now: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO module_lifecycle_registry(module, updated_at) VALUES (?1, ?2)
         ON CONFLICT(module) DO UPDATE SET updated_at=excluded.updated_at",
        (module, now),
    )?;
    Ok(())
}

fn lifecycle_modules(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT module FROM module_lifecycle_registry
         UNION SELECT DISTINCT module FROM module_kv
         ORDER BY module",
    )?;
    let modules = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(modules)
}

fn deletion_create(conn: &mut Connection, job: &DataDeletionJob, modules: &[String]) -> Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute(
        "UPDATE data_deletion_jobs SET server=NULL, profile_id=NULL, requester_profile_id=NULL,
            status='cancelled', confirmation_token='cancelled:' || id, updated_at=?3
         WHERE server=?1 AND profile_id=?2 AND status='awaiting_confirmation'",
        (&job.server, &job.profile_id, job.created_at),
    )?;
    transaction.execute(
        "INSERT INTO data_deletion_jobs(
            id, server, profile_id, requester_profile_id, status, confirmation_token,
            confirmation_expires_at, created_at, updated_at, last_error
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            job.id,
            job.server,
            job.profile_id,
            job.requester_profile_id,
            job.status,
            job.confirmation_token,
            job.confirmation_expires_at,
            job.created_at,
            job.updated_at,
            job.last_error,
        ],
    )?;
    for module in modules {
        transaction.execute(
            "INSERT INTO data_deletion_modules(job_id, module, status) VALUES (?1, ?2, 'pending')",
            (&job.id, module),
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn deletion_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DataDeletionJob> {
    Ok(DataDeletionJob {
        id: row.get(0)?,
        server: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        profile_id: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        requester_profile_id: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        status: row.get(4)?,
        confirmation_token: row.get(5)?,
        confirmation_expires_at: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        last_error: row.get(9)?,
    })
}

const DELETION_SELECT: &str = "SELECT id, server, profile_id, requester_profile_id, status,
    confirmation_token, confirmation_expires_at, created_at, updated_at, last_error
    FROM data_deletion_jobs";

fn deletion_confirm(
    conn: &Connection,
    token: &str,
    requester_profile_id: &str,
    allow_other_profile: bool,
    now: i64,
) -> Result<Option<DataDeletionJob>> {
    let changed = conn.execute(
        "UPDATE data_deletion_jobs SET status='pending', updated_at=?3, last_error=NULL
         WHERE confirmation_token=?1 AND requester_profile_id=?2
           AND (?4 OR profile_id=requester_profile_id) AND
           ((status='awaiting_confirmation' AND confirmation_expires_at>=?3) OR status='failed')",
        (token, requester_profile_id, now, allow_other_profile),
    )?;
    if changed == 0 {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            &format!("{DELETION_SELECT} WHERE confirmation_token=?1"),
            [token],
            deletion_row,
        )
        .optional()?)
}

fn deletion_pending(conn: &Connection) -> Result<Vec<DataDeletionJob>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    conn.execute(
        "UPDATE data_deletion_jobs SET server=NULL, profile_id=NULL, requester_profile_id=NULL,
            status='expired', confirmation_token='expired:' || id, updated_at=?1
         WHERE status='awaiting_confirmation' AND confirmation_expires_at<?1",
        [now],
    )?;
    let mut stmt = conn.prepare(&format!(
        "{DELETION_SELECT} WHERE status IN ('pending', 'failed') ORDER BY created_at"
    ))?;
    let jobs = stmt
        .query_map([], deletion_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(jobs)
}

fn deletion_module_done(conn: &Connection, job_id: &str, module: &str) -> Result<()> {
    conn.execute(
        "UPDATE data_deletion_modules SET status='completed' WHERE job_id=?1 AND module=?2",
        (job_id, module),
    )?;
    Ok(())
}

fn deletion_module_pending(conn: &Connection, job_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT module FROM data_deletion_modules
         WHERE job_id=?1 AND status!='completed' ORDER BY module",
    )?;
    let modules = stmt
        .query_map([job_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(modules)
}

fn deletion_fail(conn: &Connection, job_id: &str, error: &str, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE data_deletion_jobs SET status='failed', last_error=?2, updated_at=?3 WHERE id=?1",
        (job_id, error, now),
    )?;
    Ok(())
}

fn deletion_finish(
    conn: &mut Connection,
    job_id: &str,
    server: &str,
    profile_id: &str,
    now: i64,
) -> Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute(
        "DELETE FROM scheduled_jobs WHERE server=?1 AND owner_profile_id=?2",
        (server, profile_id),
    )?;
    transaction.execute(
        "DELETE FROM profile_accounts WHERE server=?1 AND profile_id=?2",
        (server, profile_id),
    )?;
    transaction.execute(
        "DELETE FROM profile_aliases WHERE server=?1 AND profile_id=?2",
        (server, profile_id),
    )?;
    transaction.execute(
        "DELETE FROM profiles WHERE server=?1 AND id=?2",
        (server, profile_id),
    )?;
    delete_achievement_rows(&transaction, server, profile_id)?;
    transaction.execute(
        "UPDATE data_deletion_jobs SET server=NULL, profile_id=NULL, requester_profile_id=NULL,
            status='completed', confirmation_token='completed:' || id, updated_at=?2, last_error=NULL
         WHERE id=?1",
        (job_id, now),
    )?;
    transaction.commit()?;
    Ok(())
}

/// Delete every achievement row owned by `(server, profile_id)` across all five host-owned tables.
/// Used by both `deletion_finish` (full profile wipe) and the achievements opt-out path.
fn delete_achievement_rows(conn: &Connection, server: &str, profile_id: &str) -> Result<()> {
    for table in [
        "achievement_stats",
        "achievement_unlocks",
        "achievement_prestige",
        "achievement_backfills",
        "achievement_dedup",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE server=?1 AND profile_id=?2"),
            (server, profile_id),
        )?;
    }
    Ok(())
}

/// Atomically set the achievements opt-out flag for a profile and, when opting out, wipe that
/// profile's achievement rows. Opting back in (`opt_out == false`) clears the flag without wiping.
fn achievement_opt_out(
    conn: &mut Connection,
    server: &str,
    profile_id: &str,
    opt_out: bool,
) -> Result<()> {
    let tx = conn.transaction()?;
    let changed = tx.execute(
        "UPDATE profiles SET achievements_opt_out=?1,
            achievements_backfill_blocked=MAX(achievements_backfill_blocked, ?1),
            achievements_public=CASE WHEN ?1 != 0 THEN 0 ELSE achievements_public END
         WHERE server=?2 AND id=?3",
        rusqlite::params![opt_out as i64, server, profile_id],
    )?;
    if changed == 0 {
        return Err(anyhow!("unknown profile for achievements opt-out"));
    }
    if opt_out {
        delete_achievement_rows(&tx, server, profile_id)?;
    }
    tx.commit()?;
    Ok(())
}

fn achievement_public(
    conn: &Connection,
    server: &str,
    profile_id: &str,
    public: bool,
) -> Result<()> {
    let changed = conn.execute(
        "UPDATE profiles SET achievements_public=?1
         WHERE server=?2 AND id=?3 AND achievements_opt_out=0",
        rusqlite::params![public as i64, server, profile_id],
    )?;
    if changed == 0 {
        return Err(anyhow!(
            "unknown or achievement-opted-out profile for public gallery"
        ));
    }
    Ok(())
}

fn public_achievement_holders(conn: &Connection) -> Result<Vec<PublicAchievementHolder>> {
    let mut stmt = conn.prepare(
        "SELECT p.server, p.id, p.nick, u.module, u.achievement_id
         FROM profiles p
         JOIN achievement_unlocks u ON u.server=p.server AND u.profile_id=p.id
         WHERE p.achievements_public=1 AND p.achievements_opt_out=0 AND u.module != 'meta'
         ORDER BY p.server COLLATE NOCASE, p.nick COLLATE NOCASE
         LIMIT 10000",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut holders = Vec::<PublicAchievementHolder>::new();
    for (server, profile_id, nick, module, achievement_id) in rows {
        if let Some(holder) = holders
            .last_mut()
            .filter(|holder| holder.server == server && holder.profile_id == profile_id)
        {
            holder.unlocks.push((module, achievement_id));
        } else if holders.len() < 1000 {
            holders.push(PublicAchievementHolder {
                server,
                profile_id,
                nick,
                unlocks: vec![(module, achievement_id)],
            });
        }
    }
    Ok(holders)
}

fn append_log(conn: &Connection, ev: &LogEvent) -> Result<()> {
    conn.execute(
        "INSERT INTO logs (ts, level, category, source, message) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            ev.ts,
            level_str(ev.level),
            category_str(ev.category),
            ev.source,
            ev.message,
        ],
    )?;
    // Amortized retention: keep 30 days and cap the table at the newest 100,000 rows.
    // Running this every 256 inserts avoids turning every log write into a cleanup scan.
    if conn.last_insert_rowid() % 256 == 0 {
        let cutoff = ev.ts.saturating_sub(30 * 24 * 60 * 60);
        conn.execute("DELETE FROM logs WHERE ts < ?1", [cutoff])?;
        conn.execute(
            "DELETE FROM logs WHERE id NOT IN (SELECT id FROM logs ORDER BY id DESC LIMIT 100000)",
            [],
        )?;
    }
    Ok(())
}

fn level_str(l: Level) -> &'static str {
    match l {
        Level::Error => "ERROR",
        Level::Info => "INFO",
        Level::Debug => "DEBUG",
    }
}

fn category_str(c: Category) -> &'static str {
    match c {
        Category::Error => "ERROR",
        Category::Debug => "DEBUG",
        Category::Message => "MESSAGE",
        Category::Command => "COMMAND",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO servers (id, label, host) VALUES (1, 'net', 'irc.example')",
            [],
        )
        .unwrap();
        conn
    }

    fn achievement_manifest() -> AchievementManifest {
        AchievementManifest {
            version: jeeves_abi::ACHIEVEMENT_MANIFEST_VERSION,
            catalog_version: 1,
            stats: vec![jeeves_abi::AchievementStat {
                id: "wins".into(),
                description: "Wins".into(),
            }],
            achievements: vec![
                jeeves_abi::AchievementSpec {
                    id: "first".into(),
                    name: "First".into(),
                    description: "Win once".into(),
                    stat: "wins".into(),
                    threshold: 1,
                    optional: false,
                    secret: false,
                },
                jeeves_abi::AchievementSpec {
                    id: "ten".into(),
                    name: "Ten".into(),
                    description: "Win ten".into(),
                    stat: "wins".into(),
                    threshold: 10,
                    optional: false,
                    secret: false,
                },
            ],
            prestige: vec![jeeves_abi::PrestigeSpec {
                id: "master".into(),
                name: "Master".into(),
                stat: "wins".into(),
                first_threshold: 20,
                every: 10,
            }],
        }
    }

    #[test]
    fn achievement_batches_are_atomic_thresholded_and_deduplicated() {
        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        let request = |amount, id: &str| AwardStatsRequest {
            server: "net".into(),
            profile_id: "p1".into(),
            display_name: "nick".into(),
            target: "#test".into(),
            increments: vec![jeeves_abi::StatIncrement {
                stat: "wins".into(),
                amount,
            }],
            deduplication_id: Some(id.into()),
        };
        let first = achievement_award(
            &mut conn,
            "game",
            &achievement_manifest(),
            &[("game".into(), achievement_manifest())],
            &request(1, "a"),
            10,
        )
        .unwrap();
        assert_eq!(
            first
                .unlocked
                .iter()
                .map(|u| u.id.as_str())
                .collect::<Vec<_>>(),
            ["first"]
        );
        let duplicate = achievement_award(
            &mut conn,
            "game",
            &achievement_manifest(),
            &[("game".into(), achievement_manifest())],
            &request(1, "a"),
            11,
        )
        .unwrap();
        assert!(duplicate.duplicate);
        let next = achievement_award(
            &mut conn,
            "game",
            &achievement_manifest(),
            &[("game".into(), achievement_manifest())],
            &request(19, "b"),
            12,
        )
        .unwrap();
        assert_eq!(
            next.unlocked
                .iter()
                .map(|u| u.id.as_str())
                .collect::<Vec<_>>(),
            ["ten", "whole_shooting_match"]
        );
        assert_eq!(next.prestige[0].rank, 1);
        let summary = achievements_get(
            &conn,
            "net",
            "p1",
            &[("game".into(), achievement_manifest())],
        )
        .unwrap();
        assert_eq!((summary.earned, summary.available), (3, 6));
    }

    #[test]
    fn achievement_opt_out_blocks_profile_updates_and_historical_backfills() {
        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        let manifest = achievement_manifest();
        let values = [AchievementSetMax {
            profile_id: "p1".into(),
            stat: "wins".into(),
            value: 10,
        }];
        achievement_backfill_apply(&mut conn, "net", "game", &manifest, &values, 10).unwrap();
        assert!(public_achievement_holders(&conn).unwrap().is_empty());
        achievement_public(&conn, "net", "p1", true).unwrap();
        assert_eq!(public_achievement_holders(&conn).unwrap().len(), 1);

        achievement_opt_out(&mut conn, "net", "p1", true).unwrap();
        assert!(public_achievement_holders(&conn).unwrap().is_empty());
        assert!(achievement_public(&conn, "net", "p1", true).is_err());
        let update = ProfileUpdate {
            server: "net".into(),
            nick: "nick".into(),
            title: Some("Captain".into()),
            ..Default::default()
        };
        profile_set(&conn, &update).unwrap();
        let flags: (i64, i64) = conn
            .query_row(
                "SELECT achievements_opt_out, achievements_backfill_blocked FROM profiles WHERE id='p1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(flags, (1, 1));
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM achievement_stats WHERE profile_id='p1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);

        let mut next_manifest = manifest.clone();
        next_manifest.catalog_version += 1;
        achievement_backfill_apply(&mut conn, "net", "game", &next_manifest, &values, 11).unwrap();
        achievement_opt_out(&mut conn, "net", "p1", false).unwrap();
        assert!(public_achievement_holders(&conn).unwrap().is_empty());
        next_manifest.catalog_version += 1;
        achievement_backfill_apply(&mut conn, "net", "game", &next_manifest, &values, 12).unwrap();
        let rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM achievement_stats WHERE profile_id='p1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "historical progress must stay reset after opt-in");

        let response = achievement_award(
            &mut conn,
            "game",
            &next_manifest,
            &[("game".into(), next_manifest.clone())],
            &AwardStatsRequest {
                server: "net".into(),
                profile_id: "p1".into(),
                display_name: "nick".into(),
                target: "#test".into(),
                increments: vec![jeeves_abi::StatIncrement {
                    stat: "wins".into(),
                    amount: 1,
                }],
                deduplication_id: None,
            },
            13,
        )
        .unwrap();
        assert!(response.unlocked.iter().any(|item| item.id == "first"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_achievement_awards_do_not_lose_increments() {
        let db = DbHandle::open(":memory:").unwrap();
        let profile = db.profile_resolve("net", "nick", None, 0).await.unwrap();
        let mut tasks = Vec::new();
        for index in 0..20 {
            let db = db.clone();
            let profile_id = profile.id.clone();
            tasks.push(tokio::spawn(async move {
                db.achievement_award(
                    "game",
                    achievement_manifest(),
                    vec![("game".into(), achievement_manifest())],
                    AwardStatsRequest {
                        server: "net".into(),
                        profile_id,
                        display_name: "nick".into(),
                        target: "#test".into(),
                        increments: vec![jeeves_abi::StatIncrement {
                            stat: "wins".into(),
                            amount: 1,
                        }],
                        deduplication_id: Some(format!("event-{index}")),
                    },
                    index,
                )
                .await
                .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let export = db.achievements_export("net", &profile.id).await.unwrap();
        assert_eq!(export.stats[0].value, 20);
    }

    #[test]
    fn achievement_overflow_rolls_back_deduplication_and_stats() {
        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO achievement_stats(server,profile_id,module,stat,value) VALUES('net','p1','game','wins',?1)", [i64::MAX]).unwrap();
        let request = AwardStatsRequest {
            server: "net".into(),
            profile_id: "p1".into(),
            display_name: "nick".into(),
            target: "#test".into(),
            increments: vec![jeeves_abi::StatIncrement {
                stat: "wins".into(),
                amount: 1,
            }],
            deduplication_id: Some("overflow".into()),
        };
        assert!(achievement_award(
            &mut conn,
            "game",
            &achievement_manifest(),
            &[("game".into(), achievement_manifest())],
            &request,
            1
        )
        .is_err());
        let dedup: i64 = conn
            .query_row("SELECT COUNT(*) FROM achievement_dedup", [], |row| {
                row.get(0)
            })
            .unwrap();
        let value: i64 = conn
            .query_row("SELECT value FROM achievement_stats", [], |row| row.get(0))
            .unwrap();
        assert_eq!(dedup, 0);
        assert_eq!(value, i64::MAX);
    }

    #[test]
    fn achievement_lifecycle_deletion_is_complete_and_server_isolated() {
        let mut conn = setup();
        conn.execute("INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0),('p2','other','othernick',0,0)", []).unwrap();
        for (server, profile_id) in [("net", "p1"), ("other", "p2")] {
            conn.execute("INSERT INTO achievement_stats(server,profile_id,module,stat,value) VALUES(?1,?2,'game','wins',1)", (server, profile_id)).unwrap();
            conn.execute("INSERT INTO achievement_unlocks(server,profile_id,module,achievement_id,unlocked_at) VALUES(?1,?2,'game','first',1)", (server, profile_id)).unwrap();
            conn.execute("INSERT INTO achievement_prestige(server,profile_id,module,prestige_id,max_rank) VALUES(?1,?2,'game','master',1)", (server, profile_id)).unwrap();
            conn.execute("INSERT INTO achievement_backfills(server,profile_id,module,catalog_version) VALUES(?1,?2,'game',1)", (server, profile_id)).unwrap();
            conn.execute("INSERT INTO achievement_dedup(server,profile_id,module,event_id,created_at) VALUES(?1,?2,'game','event',1)", (server, profile_id)).unwrap();
        }
        let exported = achievements_export(&conn, "net", "p1").unwrap();
        assert_eq!(exported.stats.len(), 1);
        assert_eq!(exported.unlocks.len(), 1);
        assert_eq!(exported.prestige.len(), 1);
        assert_eq!(exported.backfills.len(), 1);
        assert_eq!(exported.deduplication.len(), 1);

        deletion_finish(&mut conn, "missing-job", "net", "p1", 2).unwrap();
        for table in [
            "achievement_stats",
            "achievement_unlocks",
            "achievement_prestige",
            "achievement_backfills",
            "achievement_dedup",
        ] {
            let deleted: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE server='net' AND profile_id='p1'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let retained: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} WHERE server='other' AND profile_id='p2'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(deleted, 0, "{table}");
            assert_eq!(retained, 1, "{table}");
        }
    }

    #[test]
    fn achievement_backfill_is_set_max_and_silent() {
        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        achievement_backfill_apply(
            &mut conn,
            "net",
            "game",
            &achievement_manifest(),
            &[AchievementSetMax {
                profile_id: "p1".into(),
                stat: "wins".into(),
                value: 20,
            }],
            10,
        )
        .unwrap();
        achievement_backfill_apply(
            &mut conn,
            "net",
            "game",
            &achievement_manifest(),
            &[AchievementSetMax {
                profile_id: "p1".into(),
                stat: "wins".into(),
                value: 4,
            }],
            11,
        )
        .unwrap();
        achievement_catalog_reconcile(&mut conn, &[("game".into(), achievement_manifest())], 12)
            .unwrap();
        let export = achievements_export(&conn, "net", "p1").unwrap();
        assert_eq!(export.stats[0].value, 20);
        assert_eq!(export.unlocks.len(), 3);
        assert_eq!(export.prestige[0].rank, 1);
        assert_eq!(export.backfills[0].catalog_version, 1);
        let catalog_version: i64 = conn
            .query_row(
                "SELECT catalog_version FROM achievement_catalog_versions WHERE server='net' AND module='game'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(catalog_version, 1);

        let mut empty_manifest = achievement_manifest();
        empty_manifest.catalog_version = 2;
        achievement_backfill_apply(&mut conn, "other", "game", &empty_manifest, &[], 13).unwrap();
        let empty_version: i64 = conn
            .query_row(
                "SELECT catalog_version FROM achievement_catalog_versions WHERE server='other' AND module='game'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(empty_version, 2);
    }

    #[test]
    fn catalog_expansion_silently_revokes_dynamic_completion() {
        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        achievement_backfill_apply(
            &mut conn,
            "net",
            "game",
            &achievement_manifest(),
            &[AchievementSetMax {
                profile_id: "p1".into(),
                stat: "wins".into(),
                value: 10,
            }],
            10,
        )
        .unwrap();
        let request = AwardStatsRequest {
            server: "net".into(),
            profile_id: "p1".into(),
            display_name: "nick".into(),
            target: "#test".into(),
            increments: vec![jeeves_abi::StatIncrement {
                stat: "wins".into(),
                amount: 1,
            }],
            deduplication_id: None,
        };
        let first = achievement_award(
            &mut conn,
            "game",
            &achievement_manifest(),
            &[("game".into(), achievement_manifest())],
            &request,
            11,
        )
        .unwrap();
        assert!(first
            .unlocked
            .iter()
            .any(|item| item.id == "whole_shooting_match"));
        let mut expanded = achievement_manifest();
        expanded.achievements.push(jeeves_abi::AchievementSpec {
            id: "hundred".into(),
            name: "Hundred".into(),
            description: "Win 100".into(),
            stat: "wins".into(),
            threshold: 100,
            optional: false,
            secret: false,
        });
        achievement_catalog_reconcile(&mut conn, &[("game".into(), expanded.clone())], 12).unwrap();
        let summary =
            achievements_get(&conn, "net", "p1", &[("game".into(), expanded.clone())]).unwrap();
        let whole = summary
            .modules
            .iter()
            .find(|module| module.module == "meta")
            .unwrap()
            .achievements
            .iter()
            .find(|item| item.id == "whole_shooting_match")
            .unwrap();
        assert!(!whole.earned);

        let regained = achievement_award(
            &mut conn,
            "game",
            &expanded,
            &[("game".into(), expanded.clone())],
            &AwardStatsRequest {
                increments: vec![jeeves_abi::StatIncrement {
                    stat: "wins".into(),
                    amount: 89,
                }],
                ..request
            },
            13,
        )
        .unwrap();
        assert!(regained
            .unlocked
            .iter()
            .any(|item| item.id == "whole_shooting_match"));
    }

    #[test]
    fn catalog_validation_and_secret_visibility_are_strict() {
        let mut invalid = achievement_manifest();
        invalid.achievements[1].threshold = 1;
        assert!(validate_achievement_manifest("game", &invalid).is_err());
        invalid = achievement_manifest();
        invalid.achievements[0].id = "Bad ID".into();
        assert!(validate_achievement_manifest("game", &invalid).is_err());

        let mut conn = setup();
        conn.execute(
            "INSERT INTO profiles(id,server,nick,created,last_seen) VALUES('p1','net','nick',0,0)",
            [],
        )
        .unwrap();
        let mut catalog = achievement_manifest();
        catalog.stats.push(jeeves_abi::AchievementStat {
            id: "secret".into(),
            description: "Secret facts".into(),
        });
        catalog.achievements.push(jeeves_abi::AchievementSpec {
            id: "hidden".into(),
            name: "Hidden Name".into(),
            description: "Hidden condition".into(),
            stat: "secret".into(),
            threshold: 7,
            optional: true,
            secret: true,
        });
        achievement_backfill_apply(
            &mut conn,
            "net",
            "game",
            &catalog,
            &[AchievementSetMax {
                profile_id: "p1".into(),
                stat: "wins".into(),
                value: 10,
            }],
            1,
        )
        .unwrap();
        achievement_catalog_reconcile(&mut conn, &[("game".into(), catalog.clone())], 1).unwrap();
        let summary = achievements_get(&conn, "net", "p1", &[("game".into(), catalog)]).unwrap();
        let module = summary
            .modules
            .iter()
            .find(|module| module.module == "game")
            .unwrap();
        let hidden = module
            .achievements
            .iter()
            .find(|item| item.id == "hidden")
            .unwrap();
        assert_eq!(hidden.name, "Undiscovered secret");
        assert_eq!((hidden.current, hidden.threshold), (0, 0));
        assert!(
            summary
                .modules
                .iter()
                .find(|module| module.module == "meta")
                .unwrap()
                .achievements
                .iter()
                .find(|item| item.id == "whole_shooting_match")
                .unwrap()
                .earned,
            "optional secret must not block completion"
        );
    }

    #[test]
    fn config_roundtrip_and_clear() {
        let conn = setup();
        assert_eq!(config_get(&conn, "tavily_api_key").unwrap(), None);
        config_set(&conn, "tavily_api_key", Some("secret")).unwrap();
        assert_eq!(
            config_get(&conn, "tavily_api_key").unwrap().as_deref(),
            Some("secret")
        );
        config_set(&conn, "tavily_api_key", None).unwrap();
        assert_eq!(config_get(&conn, "tavily_api_key").unwrap(), None);
    }

    #[test]
    fn alias_override_roundtrip_distinguishes_empty_from_default() {
        let conn = setup();
        assert!(load_alias_overrides(&conn).unwrap().is_empty());
        set_alias_override(&conn, "weather", "weather", Some(&[])).unwrap();
        assert_eq!(
            load_alias_overrides(&conn)
                .unwrap()
                .get(&("weather".into(), "weather".into())),
            Some(&Vec::<String>::new())
        );
        let aliases = vec!["w".to_string(), "weath".to_string()];
        set_alias_override(&conn, "weather", "weather", Some(&aliases)).unwrap();
        assert_eq!(
            load_alias_overrides(&conn)
                .unwrap()
                .get(&("weather".into(), "weather".into())),
            Some(&aliases)
        );
        set_alias_override(&conn, "weather", "weather", None).unwrap();
        assert!(load_alias_overrides(&conn).unwrap().is_empty());
    }

    #[test]
    fn setting_overrides_roundtrip_by_scope_and_survive_absent_modules() {
        let conn = setup();
        setting_override_set(
            &conn,
            "memos",
            "retention_days",
            SettingScope::Global,
            "",
            "",
            Some("45"),
        )
        .unwrap();
        setting_override_set(
            &conn,
            "memos",
            "retention_days",
            SettingScope::Channel,
            "libera",
            "#rust",
            Some("7"),
        )
        .unwrap();
        assert_eq!(
            setting_override_get(
                &conn,
                "memos",
                "retention_days",
                SettingScope::Channel,
                "libera",
                "#rust"
            )
            .unwrap()
            .as_deref(),
            Some("7")
        );
        assert_eq!(load_setting_overrides(&conn).unwrap().len(), 2);
        setting_override_set(
            &conn,
            "memos",
            "retention_days",
            SettingScope::Channel,
            "libera",
            "#rust",
            None,
        )
        .unwrap();
        assert_eq!(load_setting_overrides(&conn).unwrap().len(), 1);
    }

    #[test]
    fn scheduled_jobs_replace_and_delete_by_module() {
        let conn = setup();
        let mut job = ScheduledJob {
            module: "reminders".into(),
            id: "alice:1".into(),
            server: "net".into(),
            channel: "#room".into(),
            owner_profile_id: Some("profile-1".into()),
            due_at: 100,
            payload: "first".into(),
            created_at: 1,
        };
        set_scheduled_job(&conn, &job).unwrap();
        job.payload = "replacement".into();
        set_scheduled_job(&conn, &job).unwrap();
        let loaded = load_scheduled_jobs(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].payload, "replacement");
        assert_eq!(loaded[0].owner_profile_id.as_deref(), Some("profile-1"));
        assert!(!delete_scheduled_job(&conn, "other", "alice:1").unwrap());
        assert!(delete_scheduled_job(&conn, "reminders", "alice:1").unwrap());
        assert!(load_scheduled_jobs(&conn).unwrap().is_empty());
    }

    #[test]
    fn scheduled_job_ownership_migration_preserves_legacy_jobs() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scheduled_jobs (
                module TEXT NOT NULL,
                id TEXT NOT NULL,
                server TEXT NOT NULL,
                channel TEXT NOT NULL,
                due_at INTEGER NOT NULL,
                payload TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY(module, id)
             );
             INSERT INTO scheduled_jobs
                (module, id, server, channel, due_at, payload, created_at)
             VALUES ('reminders', 'legacy', 'net', '#room', 100, 'payload', 50);",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let jobs = load_scheduled_jobs(&conn).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "legacy");
        assert_eq!(jobs[0].owner_profile_id, None);
    }

    #[test]
    fn lifecycle_registry_includes_absent_modules_with_stored_kv() {
        let conn = setup();
        lifecycle_register(&conn, "loaded", 100).unwrap();
        kv_set(&conn, "absent", "state", "{}").unwrap();

        assert_eq!(
            lifecycle_modules(&conn).unwrap(),
            vec!["absent".to_string(), "loaded".to_string()]
        );
    }

    #[test]
    fn lifecycle_mutations_are_scoped_atomic_and_reject_duplicates() {
        let mut conn = setup();
        kv_set(&conn, "game", "one", "1").unwrap();
        kv_set(&conn, "game", "two", "2").unwrap();
        let allowed = vec!["one".to_string(), "two".to_string()];

        let duplicate = vec![
            ModuleKvMutation {
                key: "one".into(),
                value: Some("changed".into()),
            },
            ModuleKvMutation {
                key: "one".into(),
                value: None,
            },
        ];
        assert!(kv_apply_module(&mut conn, "game", &allowed, &duplicate).is_err());
        assert_eq!(kv_get(&conn, "game", "one").unwrap().as_deref(), Some("1"));

        let unknown = vec![ModuleKvMutation {
            key: "other-module-key".into(),
            value: None,
        }];
        assert!(kv_apply_module(&mut conn, "game", &allowed, &unknown).is_err());
        assert_eq!(kv_get(&conn, "game", "two").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn checked_repair_rejects_state_changed_after_preview() {
        let mut conn = setup();
        kv_set(&conn, "history", "quotes", "old").unwrap();
        let expected = vec![ModuleKvEntry {
            key: "quotes".into(),
            value: "old".into(),
        }];
        let mutations = vec![ModuleKvMutation {
            key: "quotes".into(),
            value: Some("old-with-user-removed".into()),
        }];
        kv_set(&conn, "history", "quotes", "new-concurrent-value").unwrap();

        assert!(kv_apply_module_checked(&mut conn, "history", &expected, &mutations).is_err());
        assert_eq!(
            kv_get(&conn, "history", "quotes").unwrap().as_deref(),
            Some("new-concurrent-value")
        );
    }

    #[test]
    fn deletion_confirmation_is_requester_bound_and_completion_redacts_journal() {
        let mut conn = setup();
        let profile = profile_resolve(&conn, "net", "Alice", Some("alice-account"), 100).unwrap();
        set_scheduled_job(
            &conn,
            &ScheduledJob {
                module: "reminders".into(),
                id: "owned".into(),
                server: "net".into(),
                channel: "Alice".into(),
                owner_profile_id: Some(profile.id.clone()),
                due_at: 500,
                payload: "private".into(),
                created_at: 100,
            },
        )
        .unwrap();
        let job = DataDeletionJob {
            id: "job-1".into(),
            server: "net".into(),
            profile_id: profile.id.clone(),
            requester_profile_id: profile.id.clone(),
            status: "awaiting_confirmation".into(),
            confirmation_token: "token-1".into(),
            confirmation_expires_at: 200,
            created_at: 100,
            updated_at: 100,
            last_error: None,
        };
        deletion_create(&mut conn, &job, &["history".into(), "memos".into()]).unwrap();

        assert!(deletion_confirm(&conn, "token-1", "intruder", false, 150)
            .unwrap()
            .is_none());
        let confirmed = deletion_confirm(&conn, "token-1", &profile.id, false, 150)
            .unwrap()
            .unwrap();
        assert_eq!(confirmed.status, "pending");
        deletion_module_done(&conn, "job-1", "history").unwrap();
        assert_eq!(
            deletion_module_pending(&conn, "job-1").unwrap(),
            vec!["memos".to_string()]
        );
        deletion_module_done(&conn, "job-1", "memos").unwrap();
        deletion_finish(&mut conn, "job-1", "net", &profile.id, 160).unwrap();
        deletion_finish(&mut conn, "job-1", "net", &profile.id, 161).unwrap();

        assert!(profile_get(&conn, "net", "Alice").unwrap().is_none());
        assert!(load_scheduled_jobs(&conn).unwrap().is_empty());
        let completed = conn
            .query_row(
                &format!("{DELETION_SELECT} WHERE id='job-1'"),
                [],
                deletion_row,
            )
            .unwrap();
        assert_eq!(completed.status, "completed");
        assert!(completed.server.is_empty());
        assert!(completed.profile_id.is_empty());
        assert!(completed.requester_profile_id.is_empty());
        assert_eq!(completed.confirmation_token, "completed:job-1");
    }

    #[test]
    fn self_service_confirmation_cannot_confirm_another_profile() {
        let mut conn = setup();
        let target = profile_resolve(&conn, "net", "Target", None, 100).unwrap();
        let requester = profile_resolve(&conn, "net", "Admin", None, 100).unwrap();
        let job = DataDeletionJob {
            id: "job-other".into(),
            server: "net".into(),
            profile_id: target.id,
            requester_profile_id: requester.id.clone(),
            status: "awaiting_confirmation".into(),
            confirmation_token: "token-other".into(),
            confirmation_expires_at: 200,
            created_at: 100,
            updated_at: 100,
            last_error: None,
        };
        deletion_create(&mut conn, &job, &[]).unwrap();

        assert!(
            deletion_confirm(&conn, "token-other", &requester.id, false, 150)
                .unwrap()
                .is_none()
        );
        assert!(
            deletion_confirm(&conn, "token-other", &requester.id, true, 150)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn legacy_nick_keyed_profile_is_migrated_to_uuid_and_alias() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE profiles (
                server TEXT NOT NULL,
                nick TEXT NOT NULL COLLATE NOCASE,
                created INTEGER NOT NULL,
                last_seen INTEGER NOT NULL,
                title TEXT, birthday TEXT,
                pronoun_subject TEXT, pronoun_object TEXT, pronoun_possessive TEXT,
                location_display TEXT, location_label TEXT, lat REAL, lon REAL,
                PRIMARY KEY(server, nick)
             );
             INSERT INTO profiles(server, nick, created, last_seen, title)
             VALUES ('net', 'LegacyNick', 10, 20, 'Captain');",
        )
        .unwrap();
        migrate(&conn).unwrap();
        let profile = profile_get(&conn, "net", "legacynick").unwrap().unwrap();
        assert!(uuid::Uuid::parse_str(&profile.id).is_ok());
        assert_eq!(profile.nick, "LegacyNick");
        assert_eq!(profile.title.as_deref(), Some("Captain"));
    }

    #[test]
    fn unknown_nick_has_no_role() {
        let conn = setup();
        let r = resolve_role(&conn, "net", "nobody", "nobody!u@h", None).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn explicit_account_must_match() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role, account) VALUES (1, 'boss', 'superadmin', 'bossacct')",
            [],
        )
        .unwrap();
        // Wrong / missing account -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", None).unwrap(),
            None
        );
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("other")).unwrap(),
            None
        );
        // Correct account -> superadmin.
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("bossacct")).unwrap(),
            Some(Role::SuperAdmin)
        );
    }

    #[test]
    fn hostmask_tofu_binds_then_enforces() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role) VALUES (1, 'op', 'admin')",
            [],
        )
        .unwrap();
        // First contact (no account) binds the hostmask and grants.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-a", None).unwrap(),
            Some(Role::Admin)
        );
        // Same hostmask -> still granted.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-a", None).unwrap(),
            Some(Role::Admin)
        );
        // Different hostmask (spoof attempt) -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!user@host-b", None).unwrap(),
            None
        );
    }

    #[test]
    fn account_tofu_preferred_over_hostmask() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role) VALUES (1, 'op', 'admin')",
            [],
        )
        .unwrap();
        // First contact carries an account -> binds account (not hostmask).
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h1", Some("opacct")).unwrap(),
            Some(Role::Admin)
        );
        // Same account from a different host -> still granted (account beats host).
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h2", Some("opacct")).unwrap(),
            Some(Role::Admin)
        );
        // Different account -> denied.
        assert_eq!(
            resolve_role(&conn, "net", "op", "op!u@h1", Some("evil")).unwrap(),
            None
        );
    }

    #[test]
    fn profile_ensure_get_set_roundtrip() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // First contact creates a skeleton.
        profile_ensure(&conn, "net", "Alice", 1000).unwrap();
        let p = profile_get(&conn, "net", "alice").unwrap().unwrap(); // case-insensitive
        assert_eq!(p.nick, "Alice");
        assert_eq!(p.created, 1000);
        assert_eq!(p.title, None);

        // Partial update merges; untouched fields are preserved.
        let mut u = ProfileUpdate {
            server: "net".into(),
            nick: "alice".into(),
            ..Default::default()
        };
        u.title = Some("Queen".into());
        u.lat = Some(51.5);
        u.lon = Some(-0.05);
        u.location_display = Some("Hackney, England".into());
        u.timezone = Some("Europe/London".into());
        profile_set(&conn, &u).unwrap();

        let p = profile_get(&conn, "net", "Alice").unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Queen"));
        assert_eq!(p.location_display.as_deref(), Some("Hackney, England"));
        assert_eq!(p.lat, Some(51.5));
        assert_eq!(p.timezone.as_deref(), Some("Europe/London"));
        assert_eq!(p.created, 1000, "created preserved across update");

        // A second update touching only birthday leaves the title intact.
        let u2 = ProfileUpdate {
            server: "net".into(),
            nick: "alice".into(),
            birthday: Some("03-14".into()),
            ..Default::default()
        };
        profile_set(&conn, &u2).unwrap();
        let p = profile_get(&conn, "net", "alice").unwrap().unwrap();
        assert_eq!(p.title.as_deref(), Some("Queen"));
        assert_eq!(p.birthday.as_deref(), Some("03-14"));
    }

    #[test]
    fn negotiated_casemapping_controls_profile_identity() {
        let conn = setup();
        let profile =
            profile_resolve_mapped(&conn, "net", "Sailor[One]", None, 100, CaseMapping::Rfc1459)
                .unwrap();
        let equivalent = profile_get_mapped(&conn, "net", "sailor{one}", CaseMapping::Rfc1459)
            .unwrap()
            .unwrap();
        assert_eq!(equivalent.id, profile.id);

        assert!(
            profile_get_mapped(&conn, "net", "sailor{one}", CaseMapping::Ascii,)
                .unwrap()
                .is_none()
        );

        conn.execute(
            "INSERT INTO profile_aliases(server, nick, profile_id, last_seen)
             VALUES ('net', 'sailor{one}', 'different-profile', 100)",
            [],
        )
        .unwrap();
        assert!(profile_get_mapped(&conn, "net", "SAILOR[ONE]", CaseMapping::Rfc1459,).is_err());
    }

    #[test]
    fn negotiated_casemapping_applies_to_admin_nicks_and_bound_hostmasks() {
        let conn = setup();
        let sid: i64 = conn
            .query_row("SELECT id FROM servers WHERE label='net'", [], |row| {
                row.get(0)
            })
            .unwrap();
        upsert_admin(
            &conn,
            sid,
            &AdminEntry {
                nick: "Oper[One]".into(),
                role: Role::Admin,
                account: None,
                bound_hostmask: None,
                bound_account: None,
            },
        )
        .unwrap();

        assert_eq!(
            resolve_role_mapped(
                &conn,
                "net",
                "oper{one}",
                "oper{one}!user@host",
                None,
                CaseMapping::Rfc1459,
            )
            .unwrap(),
            Some(Role::Admin)
        );
        assert_eq!(
            resolve_role_mapped(
                &conn,
                "net",
                "OPER[ONE]",
                "OPER[ONE]!user@host",
                None,
                CaseMapping::Rfc1459,
            )
            .unwrap(),
            Some(Role::Admin)
        );
        assert_eq!(
            resolve_role_mapped(
                &conn,
                "net",
                "oper{one}",
                "oper{one}!user@host",
                None,
                CaseMapping::Ascii,
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn stable_profile_survives_nick_change_and_account_lookup() {
        let conn = setup();
        let first = profile_resolve(&conn, "net", "Alice", Some("alice_account"), 100).unwrap();
        profile_bind_nick(
            &conn,
            "net",
            "Alice",
            "AliceAway",
            Some("alice_account"),
            200,
        )
        .unwrap();
        let renamed = profile_get(&conn, "net", "AliceAway").unwrap().unwrap();
        let account = profile_resolve(
            &conn,
            "net",
            "CompletelyDifferentNick",
            Some("alice_account"),
            300,
        )
        .unwrap();
        assert_eq!(first.id, renamed.id);
        assert_eq!(first.id, account.id);
        assert_eq!(account.nick, "CompletelyDifferentNick");
    }

    #[test]
    fn different_accounts_reusing_a_nick_do_not_merge_profiles() {
        let conn = setup();
        let first = profile_resolve(&conn, "net", "SharedNick", Some("account_a"), 100).unwrap();
        let second = profile_resolve(&conn, "net", "SharedNick", Some("account_b"), 200).unwrap();
        assert_ne!(first.id, second.id);
        let first_again =
            profile_resolve(&conn, "net", "AnotherNick", Some("account_a"), 300).unwrap();
        assert_eq!(first.id, first_again.id);
    }

    #[test]
    fn profile_list_and_atomic_repair_roundtrip() {
        let conn = setup();
        let mut profile = profile_resolve(&conn, "net", "Alice", Some("alice"), 100).unwrap();
        profile.title = Some("Captain".into());
        profile.birthday = Some("03-14".into());
        profile.pronoun_subject = Some("they".into());
        profile.pronoun_object = Some("them".into());
        profile.pronoun_possessive = Some("their".into());
        profile_repair(&conn, &profile).unwrap();

        let listed = profile_list(&conn).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, profile.id);
        assert_eq!(listed[0].title.as_deref(), Some("Captain"));

        profile.title = None;
        profile_repair(&conn, &profile).unwrap();
        assert_eq!(
            profile_get(&conn, "net", "Alice").unwrap().unwrap().title,
            None
        );
    }

    #[test]
    fn profile_repair_rejects_invalid_or_partial_values_without_writing() {
        let conn = setup();
        let mut profile = profile_resolve(&conn, "net", "Alice", None, 100).unwrap();
        profile.title = Some("Original".into());
        profile_repair(&conn, &profile).unwrap();

        profile.birthday = Some("02-31".into());
        profile.pronoun_subject = Some("they".into());
        assert!(profile_repair(&conn, &profile).is_err());
        let stored = profile_get(&conn, "net", "Alice").unwrap().unwrap();
        assert_eq!(stored.title.as_deref(), Some("Original"));
        assert_eq!(stored.birthday, None);
    }

    #[test]
    fn checked_profile_repair_rejects_concurrent_field_change() {
        let conn = setup();
        let profile = profile_resolve(&conn, "net", "Alice", None, 100).unwrap();
        let expected = profile.clone();
        let mut concurrent = profile.clone();
        concurrent.title = Some("Changed in chat".into());
        profile_repair(&conn, &concurrent).unwrap();

        let mut operator_edit = expected.clone();
        operator_edit.birthday = Some("03-14".into());
        assert!(profile_repair_checked(&conn, &operator_edit, &expected).is_err());
        let stored = profile_get(&conn, "net", "Alice").unwrap().unwrap();
        assert_eq!(stored.title.as_deref(), Some("Changed in chat"));
        assert_eq!(stored.birthday, None);
    }

    #[test]
    fn nick_match_is_case_insensitive() {
        let conn = setup();
        conn.execute(
            "INSERT INTO admins (server_id, nick, role, account) VALUES (1, 'Boss', 'admin', 'a')",
            [],
        )
        .unwrap();
        assert_eq!(
            resolve_role(&conn, "net", "boss", "boss!u@h", Some("a")).unwrap(),
            Some(Role::Admin)
        );
    }
}
