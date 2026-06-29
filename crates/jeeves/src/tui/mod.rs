//! Interactive TUI (ratatui + crossterm).
//!
//! Screens: Servers (list of network profiles), Edit server (per-profile fields), Admins (per
//! server access list), Edit admin, Integrations (global API credentials), Commands/Aliases,
//! module settings, backup policy/status, and Logs (filterable).
//! The TUI reads and writes the database directly through the DB actor's blocking API (it runs on
//! a blocking thread), and asks the runtime to (re)connect via an [`AppRequest`].

use crate::action::AppRequest;
use crate::backup::{self, BackupHandle};
use crate::commands::{parse_alias_csv, RegisteredCommand, SharedCommandRegistry};
use crate::config::{AdminEntry, ServerConfig};
use crate::db::DbHandle;
use crate::log_bus::{LogBus, LogEvent};
use crate::modules::{ProfileAdminHandle, ProfileModuleData, ProfileModuleResetPlan};
use crate::scheduler::SchedulerHandle;
use crate::settings::{scope_name, RegisteredSetting, SettingOverride, SharedSettingRegistry};
use anyhow::Result;
use jeeves_abi::{
    Category, Level, Profile, ProfileAliasExport, Role, ScheduledJob, SettingKind, SettingScope,
};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::HashMap;
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::sync::mpsc;

fn weekday_name(day: chrono::Weekday) -> &'static str {
    match day {
        chrono::Weekday::Mon => "mon",
        chrono::Weekday::Tue => "tue",
        chrono::Weekday::Wed => "wed",
        chrono::Weekday::Thu => "thu",
        chrono::Weekday::Fri => "fri",
        chrono::Weekday::Sat => "sat",
        chrono::Weekday::Sun => "sun",
    }
}

fn parse_bounded(value: &str, name: &str, maximum: usize) -> std::result::Result<usize, String> {
    let parsed = value
        .trim()
        .parse::<usize>()
        .map_err(|_| format!("{name} must be a number from 0 to {maximum}"))?;
    if parsed > maximum {
        return Err(format!("{name} must be a number from 0 to {maximum}"));
    }
    Ok(parsed)
}

fn optional_text(value: &Option<String>) -> String {
    value.clone().unwrap_or_default()
}

fn optional_field(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn optional_number(value: &str, field: &str) -> std::result::Result<Option<f64>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    value
        .parse::<f64>()
        .map(Some)
        .map_err(|_| format!("{field} must be a number or blank"))
}

fn display_optional(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("—")
}

fn profile_changes(before: &Profile, after: &Profile) -> Vec<String> {
    let mut changes = Vec::new();
    {
        let mut text = |label: &str, old: &Option<String>, new: &Option<String>| {
            if old != new {
                changes.push(format!(
                    "{label}: {} → {}",
                    display_optional(old),
                    display_optional(new)
                ));
            }
        };
        text("title", &before.title, &after.title);
        text("birthday", &before.birthday, &after.birthday);
        text(
            "pronoun subject",
            &before.pronoun_subject,
            &after.pronoun_subject,
        );
        text(
            "pronoun object",
            &before.pronoun_object,
            &after.pronoun_object,
        );
        text(
            "pronoun possessive",
            &before.pronoun_possessive,
            &after.pronoun_possessive,
        );
        text(
            "location display",
            &before.location_display,
            &after.location_display,
        );
        text(
            "location label",
            &before.location_label,
            &after.location_label,
        );
        text("timezone", &before.timezone, &after.timezone);
    }
    if before.lat != after.lat {
        changes.push(format!("latitude: {:?} → {:?}", before.lat, after.lat));
    }
    if before.lon != after.lon {
        changes.push(format!("longitude: {:?} → {:?}", before.lon, after.lon));
    }
    changes
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn format_timestamp(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0)
        .map(|value| value.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| timestamp.to_string())
}

pub(crate) struct Services {
    pub commands: SharedCommandRegistry,
    pub settings: SharedSettingRegistry,
    pub scheduler: SchedulerHandle,
    pub backups: BackupHandle,
    pub profile_admin: ProfileAdminHandle,
}

pub fn run(
    db: DbHandle,
    log: LogBus,
    logs_rx: Receiver<LogEvent>,
    app_tx: mpsc::Sender<AppRequest>,
    services: Services,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(
        db,
        log,
        services.commands,
        services.settings,
        services.scheduler,
        services.backups,
        services.profile_admin,
    );
    let result = app.run(&mut terminal, logs_rx, &app_tx);
    ratatui::restore();
    let _ = app_tx.blocking_send(AppRequest::Shutdown);
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Servers,
    EditServer,
    Admins,
    EditAdmin,
    Logs,
    Integrations,
    Commands,
    EditAliases,
    ModuleSettings,
    EditModuleSetting,
    Scheduler,
    Backups,
    Profiles,
    ProfileDetail,
    EditProfile,
    ProfileModuleData,
    ConfirmProfileRepair,
}

/// One editable form field. A `cycle` field advances through fixed options on Space.
struct Field {
    label: String,
    value: String,
    secret: bool,
    cycle: Option<Vec<String>>,
}

impl Field {
    fn text(label: &str, value: String) -> Self {
        Field {
            label: label.into(),
            value,
            secret: false,
            cycle: None,
        }
    }
    fn secret(label: &str, value: String) -> Self {
        Field {
            label: label.into(),
            value,
            secret: true,
            cycle: None,
        }
    }
    fn boolean(label: &str, on: bool) -> Self {
        Field {
            label: label.into(),
            value: if on { "true".into() } else { "false".into() },
            secret: false,
            cycle: Some(vec!["false".into(), "true".into()]),
        }
    }
    fn choice(label: &str, options: &[&str], current: &str) -> Self {
        Field {
            label: label.into(),
            value: current.to_string(),
            secret: false,
            cycle: Some(options.iter().map(|s| s.to_string()).collect()),
        }
    }
    fn choices(label: &str, options: Vec<String>, current: String) -> Self {
        Field {
            label: label.into(),
            value: current,
            secret: false,
            cycle: Some(options),
        }
    }
    fn is_on(&self) -> bool {
        self.value == "true"
    }
    fn advance(&mut self) {
        if let Some(opts) = &self.cycle {
            let i = opts.iter().position(|o| o == &self.value).unwrap_or(0);
            self.value = opts[(i + 1) % opts.len()].clone();
        }
    }
}

struct App {
    db: DbHandle,
    log: LogBus,
    command_registry: SharedCommandRegistry,
    setting_registry: SharedSettingRegistry,
    scheduler: SchedulerHandle,
    backups: BackupHandle,
    profile_admin: ProfileAdminHandle,
    screen: Screen,
    status: String,

    servers: Vec<ServerConfig>,
    server_sel: usize,

    // Current edit form (server or admin), with the row id being edited.
    fields: Vec<Field>,
    focus: usize,
    edit_server_id: i64,

    admin_server_id: i64,
    admin_server_label: String,
    admins: Vec<AdminEntry>,
    admin_sel: usize,
    edit_admin_new: bool,

    logs: Vec<LogEvent>,
    filter: Option<Category>,
    scroll: usize,
    follow: bool,

    commands: Vec<RegisteredCommand>,
    command_sel: usize,
    edit_command: Option<(String, String)>,

    settings: Vec<RegisteredSetting>,
    setting_overrides: Vec<SettingOverride>,
    setting_sel: usize,
    edit_setting: Option<RegisteredSetting>,
    setting_locations: HashMap<(String, String), SettingLocation>,

    scheduler_jobs: Vec<ScheduledJob>,
    scheduler_sel: usize,

    profiles: Vec<Profile>,
    profile_sel: usize,
    profile_filter: String,
    profile_filter_editing: bool,
    selected_profile: Option<Profile>,
    profile_aliases: Vec<ProfileAliasExport>,
    profile_accounts: Vec<String>,
    profile_modules: Vec<ProfileModuleData>,
    profile_module_sel: usize,
    profile_module_scroll: u16,
    pending_profile_repair: Option<PendingProfileRepair>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SettingLocation {
    scope: SettingScope,
    server: String,
    channel: String,
}

enum PendingProfileRepair {
    Host {
        profile: Box<Profile>,
        expected: Box<Profile>,
        changes: Vec<String>,
    },
    ModuleReset {
        plan: ProfileModuleResetPlan,
    },
}

// Server-edit field indices.
const S_LABEL: usize = 0;
const S_ENABLED: usize = 1;
const S_HOST: usize = 2;
const S_PORT: usize = 3;
const S_TLS: usize = 4;
const S_ACCEPT: usize = 5;
const S_NICK: usize = 6;
const S_USER: usize = 7;
const S_REAL: usize = 8;
const S_SASL_ACCT: usize = 9;
const S_SASL_PASS: usize = 10;
const S_NICKPASS: usize = 11;
const S_CHANNELS: usize = 12;
const S_UMODES: usize = 13;

// Admin-edit field indices.
const A_NICK: usize = 0;
const A_ROLE: usize = 1;
const A_ACCOUNT: usize = 2;

// Integrations field indices.
const I_TAVILY_KEY: usize = 0;
const I_DEEPL_KEY: usize = 1;
const I_B2_KEY_ID: usize = 2;
const I_B2_APPLICATION_KEY: usize = 3;
const I_BACKUP_ENCRYPTION_KEY: usize = 4;
const I_AI_PROVIDER: usize = 5;
const I_AI_ENDPOINT: usize = 6;
const I_AI_MODEL: usize = 7;
const I_AI_SOUL_PATH: usize = 8;
const I_AI_API_KEY: usize = 9;
const I_YOUTUBE_API_KEY: usize = 10;

const B_ENABLED: usize = 0;
const B_DIRECTORY: usize = 1;
const B_HOUR: usize = 2;
const B_KEEP_DAILY: usize = 3;
const B_KEEP_WEEKLY: usize = 4;
const B_KEEP_MONTHLY: usize = 5;
const B_REMOTE_ENABLED: usize = 6;
const B_AUTHORIZE_URL: usize = 7;
const B_BUCKET: usize = 8;
const B_PREFIX: usize = 9;
const B_WEEKDAY: usize = 10;

const P_TITLE: usize = 0;
const P_BIRTHDAY: usize = 1;
const P_PRONOUN_SUBJECT: usize = 2;
const P_PRONOUN_OBJECT: usize = 3;
const P_PRONOUN_POSSESSIVE: usize = 4;
const P_LOCATION_DISPLAY: usize = 5;
const P_LOCATION_LABEL: usize = 6;
const P_LATITUDE: usize = 7;
const P_LONGITUDE: usize = 8;
const P_TIMEZONE: usize = 9;

const M_SCOPE: usize = 0;
const M_NETWORK: usize = 1;
const M_CHANNEL: usize = 2;
const M_VALUE: usize = 3;

impl App {
    fn new(
        db: DbHandle,
        log: LogBus,
        command_registry: SharedCommandRegistry,
        setting_registry: SharedSettingRegistry,
        scheduler: SchedulerHandle,
        backups: BackupHandle,
        profile_admin: ProfileAdminHandle,
    ) -> Self {
        let servers = db.load_servers_blocking().unwrap_or_default();
        App {
            db,
            log,
            command_registry,
            setting_registry,
            scheduler,
            backups,
            profile_admin,
            screen: Screen::Servers,
            status: "F1 Servers · F2 Logs · F3 Integrations · F4 Commands · F5 Modules · F6 Scheduler · F7 Backups · F8 Profiles · Ctrl-Q quit".into(),
            servers,
            server_sel: 0,
            fields: Vec::new(),
            focus: 0,
            edit_server_id: 0,
            admin_server_id: 0,
            admin_server_label: String::new(),
            admins: Vec::new(),
            admin_sel: 0,
            edit_admin_new: false,
            logs: Vec::new(),
            filter: None,
            scroll: 0,
            follow: true,
            commands: Vec::new(),
            command_sel: 0,
            edit_command: None,
            settings: Vec::new(),
            setting_overrides: Vec::new(),
            setting_sel: 0,
            edit_setting: None,
            setting_locations: HashMap::new(),
            scheduler_jobs: Vec::new(),
            scheduler_sel: 0,
            profiles: Vec::new(),
            profile_sel: 0,
            profile_filter: String::new(),
            profile_filter_editing: false,
            selected_profile: None,
            profile_aliases: Vec::new(),
            profile_accounts: Vec::new(),
            profile_modules: Vec::new(),
            profile_module_sel: 0,
            profile_module_scroll: 0,
            pending_profile_repair: None,
        }
    }

    fn run(
        &mut self,
        terminal: &mut DefaultTerminal,
        logs_rx: Receiver<LogEvent>,
        app_tx: &mpsc::Sender<AppRequest>,
    ) -> Result<()> {
        loop {
            while let Ok(ev) = logs_rx.try_recv() {
                self.logs.push(ev);
                if self.logs.len() > 5000 {
                    self.logs.drain(0..1000);
                }
            }

            terminal.draw(|f| self.render(f))?;

            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Char('q') if ctrl => return Ok(()),
                        KeyCode::Char('r') if ctrl => {
                            let _ = app_tx.blocking_send(AppRequest::Reconnect);
                            self.status = "applying — reconnecting all enabled networks…".into();
                        }
                        KeyCode::F(1) => self.screen = Screen::Servers,
                        KeyCode::F(2) => self.screen = Screen::Logs,
                        KeyCode::F(3) => self.open_integrations(),
                        KeyCode::F(4) => self.open_commands(),
                        KeyCode::F(5) => self.open_module_settings(),
                        KeyCode::F(6) => self.open_scheduler(),
                        KeyCode::F(7) => self.open_backups(),
                        KeyCode::F(8) => self.open_profiles(),
                        _ => match self.screen {
                            Screen::Servers => self.servers_key(key.code),
                            Screen::EditServer => self.edit_server_key(key.code, ctrl),
                            Screen::Admins => self.admins_key(key.code),
                            Screen::EditAdmin => self.edit_admin_key(key.code, ctrl),
                            Screen::Logs => self.logs_key(key.code),
                            Screen::Integrations => self.integrations_key(key.code, ctrl),
                            Screen::Commands => self.commands_key(key.code),
                            Screen::EditAliases => self.edit_aliases_key(key.code, ctrl),
                            Screen::ModuleSettings => self.module_settings_key(key.code),
                            Screen::EditModuleSetting => {
                                self.edit_module_setting_key(key.code, ctrl)
                            }
                            Screen::Scheduler => self.scheduler_key(key.code),
                            Screen::Backups => self.backups_key(key.code, ctrl),
                            Screen::Profiles => self.profiles_key(key.code),
                            Screen::ProfileDetail => self.profile_detail_key(key.code),
                            Screen::EditProfile => self.edit_profile_key(key.code, ctrl),
                            Screen::ProfileModuleData => self.profile_module_data_key(key.code),
                            Screen::ConfirmProfileRepair => {
                                self.confirm_profile_repair_key(key.code, ctrl)
                            }
                        },
                    }
                }
            }
        }
    }

    // ---- Servers list ----

    fn refresh_servers(&mut self) {
        match self.db.load_servers_blocking() {
            Ok(s) => {
                self.servers = s;
                if self.server_sel >= self.servers.len() {
                    self.server_sel = self.servers.len().saturating_sub(1);
                }
            }
            Err(e) => self.status = format!("load failed: {e}"),
        }
    }

    fn servers_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => self.server_sel = self.server_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.servers.is_empty() {
                    self.server_sel = (self.server_sel + 1).min(self.servers.len() - 1);
                }
            }
            KeyCode::Char('a') => self.open_server_edit(ServerConfig::placeholder()),
            KeyCode::Enter => {
                if let Some(cfg) = self.servers.get(self.server_sel).cloned() {
                    self.open_server_edit(cfg);
                }
            }
            KeyCode::Char('m') => {
                if let Some(cfg) = self.servers.get(self.server_sel) {
                    self.admin_server_id = cfg.id;
                    self.admin_server_label = cfg.label.clone();
                    self.admin_sel = 0;
                    self.refresh_admins();
                    self.screen = Screen::Admins;
                }
            }
            KeyCode::Char('d') => {
                if let Some(cfg) = self.servers.get(self.server_sel) {
                    if cfg.id != 0 {
                        match self.db.delete_server_blocking(cfg.id) {
                            Ok(()) => self.status = format!("deleted server '{}'", cfg.label),
                            Err(e) => self.status = format!("delete failed: {e}"),
                        }
                        self.refresh_servers();
                    }
                }
            }
            KeyCode::Char(' ') => {
                if let Some(cfg) = self.servers.get(self.server_sel).cloned() {
                    let mut cfg = cfg;
                    cfg.enabled = !cfg.enabled;
                    if let Err(e) = self.db.upsert_server_blocking(cfg) {
                        self.status = format!("toggle failed: {e}");
                    }
                    self.refresh_servers();
                }
            }
            _ => {}
        }
    }

    fn open_server_edit(&mut self, cfg: ServerConfig) {
        let channels = cfg
            .channels
            .iter()
            .map(|(n, k)| match k {
                Some(k) => format!("{n} {k}"),
                None => n.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.edit_server_id = cfg.id;
        self.fields = vec![
            Field::text("Label", cfg.label),
            Field::boolean("Enabled", cfg.enabled),
            Field::text("Server host", cfg.host),
            Field::text("Port", cfg.port.to_string()),
            Field::boolean("Use TLS", cfg.tls),
            Field::boolean("Accept invalid TLS cert", cfg.accept_invalid_certs),
            Field::text("Nick", cfg.nick),
            Field::text("Username", cfg.username),
            Field::text("Realname", cfg.realname),
            Field::text("SASL account", cfg.sasl_account.unwrap_or_default()),
            Field::secret("SASL password", cfg.sasl_password.unwrap_or_default()),
            Field::secret("NickServ password", cfg.nick_password.unwrap_or_default()),
            Field::text("Channels (comma-sep, '#chan key')", channels),
            Field::text("User modes (e.g. +B)", cfg.umodes.unwrap_or_default()),
        ];
        self.focus = 0;
        self.screen = Screen::EditServer;
    }

    fn edit_server_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_server();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::Char(' ') if self.fields[self.focus].cycle.is_some() => {
                self.fields[self.focus].advance()
            }
            KeyCode::Char(c) if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.push(c)
            }
            KeyCode::Backspace if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn save_server(&mut self) {
        let get = |i: usize| self.fields[i].value.trim().to_string();
        let opt = |i: usize| {
            let v = self.fields[i].value.trim();
            (!v.is_empty()).then(|| v.to_string())
        };
        let channels = self.fields[S_CHANNELS]
            .value
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|entry| {
                let mut parts = entry.splitn(2, ' ');
                let name = parts.next().unwrap_or("").to_string();
                let key = parts
                    .next()
                    .map(|k| k.trim().to_string())
                    .filter(|k| !k.is_empty());
                (name, key)
            })
            .collect();

        let cfg = ServerConfig {
            id: self.edit_server_id,
            label: get(S_LABEL),
            enabled: self.fields[S_ENABLED].is_on(),
            host: get(S_HOST),
            port: get(S_PORT).parse().unwrap_or(6697),
            tls: self.fields[S_TLS].is_on(),
            accept_invalid_certs: self.fields[S_ACCEPT].is_on(),
            nick: get(S_NICK),
            username: get(S_USER),
            realname: get(S_REAL),
            sasl_account: opt(S_SASL_ACCT),
            sasl_password: opt(S_SASL_PASS),
            nick_password: opt(S_NICKPASS),
            channels,
            umodes: opt(S_UMODES),
        };
        match self.db.upsert_server_blocking(cfg) {
            Ok(_) => {
                self.status = "server saved (Ctrl-R to connect)".into();
                self.refresh_servers();
                self.screen = Screen::Servers;
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    // ---- Admins ----

    fn refresh_admins(&mut self) {
        match self.db.load_admins_blocking(self.admin_server_id) {
            Ok(a) => {
                self.admins = a;
                if self.admin_sel >= self.admins.len() {
                    self.admin_sel = self.admins.len().saturating_sub(1);
                }
            }
            Err(e) => self.status = format!("load admins failed: {e}"),
        }
    }

    fn admins_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.admin_sel = self.admin_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.admins.is_empty() {
                    self.admin_sel = (self.admin_sel + 1).min(self.admins.len() - 1);
                }
            }
            KeyCode::Char('a') => self.open_admin_edit(None),
            KeyCode::Enter => {
                if let Some(a) = self.admins.get(self.admin_sel).cloned() {
                    self.open_admin_edit(Some(a));
                }
            }
            KeyCode::Char('d') => {
                if let Some(a) = self.admins.get(self.admin_sel) {
                    let nick = a.nick.clone();
                    if let Err(e) = self.db.delete_admin_blocking(self.admin_server_id, &nick) {
                        self.status = format!("delete failed: {e}");
                    }
                    self.refresh_admins();
                }
            }
            _ => {}
        }
    }

    fn open_admin_edit(&mut self, entry: Option<AdminEntry>) {
        self.edit_admin_new = entry.is_none();
        let (nick, role, account) = match entry {
            Some(e) => (
                e.nick,
                match e.role {
                    Role::Admin => "admin",
                    Role::SuperAdmin => "superadmin",
                },
                e.account.unwrap_or_default(),
            ),
            None => (String::new(), "admin", String::new()),
        };
        self.fields = vec![
            Field::text("Nick", nick),
            Field::choice("Role", &["admin", "superadmin"], role),
            Field::text("Account (optional; blank = hostmask TOFU)", account),
        ];
        self.focus = 0;
        self.screen = Screen::EditAdmin;
    }

    fn edit_admin_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_admin();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::Admins,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::Char(' ') if self.fields[self.focus].cycle.is_some() => {
                self.fields[self.focus].advance()
            }
            KeyCode::Char(c) if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.push(c)
            }
            KeyCode::Backspace if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn save_admin(&mut self) {
        let nick = self.fields[A_NICK].value.trim().to_string();
        if nick.is_empty() {
            self.status = "admin nick cannot be empty".into();
            return;
        }
        let role = match self.fields[A_ROLE].value.as_str() {
            "superadmin" => Role::SuperAdmin,
            _ => Role::Admin,
        };
        let account = {
            let v = self.fields[A_ACCOUNT].value.trim();
            (!v.is_empty()).then(|| v.to_string())
        };
        let entry = AdminEntry {
            nick,
            role,
            account,
            bound_hostmask: None,
            bound_account: None,
        };
        match self.db.upsert_admin_blocking(self.admin_server_id, entry) {
            Ok(()) => {
                self.status = "admin saved".into();
                self.refresh_admins();
                self.screen = Screen::Admins;
            }
            Err(e) => self.status = format!("save failed: {e}"),
        }
    }

    // ---- Integrations ----

    fn open_integrations(&mut self) {
        let tavily_key = self.load_integration(crate::search::API_KEY_CONFIG);
        let deepl_key = self.load_integration(crate::deepl::API_KEY_CONFIG);
        let b2_key_id = self.load_integration(backup::KEY_B2_KEY_ID);
        let b2_application_key = self.load_integration(backup::KEY_B2_APPLICATION_KEY);
        let encryption_key = self.load_integration(backup::KEY_ENCRYPTION_KEY);
        let ai_provider = self
            .load_integration(crate::ai::PROVIDER_CONFIG)
            .trim()
            .to_string();
        let ai_endpoint = self.load_integration(crate::ai::ENDPOINT_CONFIG);
        let ai_model = self.load_integration(crate::ai::MODEL_CONFIG);
        let ai_soul_path = self.load_integration(crate::ai::SOUL_PATH_CONFIG);
        let ai_api_key = self.load_integration(crate::ai::API_KEY_CONFIG);
        let youtube_api_key = self.load_integration(crate::youtube::API_KEY_CONFIG);
        self.fields = vec![
            Field::secret("Tavily API key", tavily_key),
            Field::secret("DeepL API key", deepl_key),
            Field::secret("B2 application key ID", b2_key_id),
            Field::secret("B2 application key", b2_application_key),
            Field::secret("Backup encryption key", encryption_key),
            Field::choice(
                "AI provider mode",
                &["ollama", "openai", "compatible"],
                if ai_provider.is_empty() {
                    crate::ai::DEFAULT_PROVIDER
                } else {
                    &ai_provider
                },
            ),
            Field::text(
                "AI chat-completions endpoint",
                if ai_endpoint.is_empty() {
                    crate::ai::DEFAULT_ENDPOINT.into()
                } else {
                    ai_endpoint
                },
            ),
            Field::text(
                "AI model",
                if ai_model.is_empty() {
                    crate::ai::DEFAULT_MODEL.into()
                } else {
                    ai_model
                },
            ),
            Field::text(
                "AI SOUL.md path",
                if ai_soul_path.is_empty() {
                    crate::ai::DEFAULT_SOUL_PATH.into()
                } else {
                    ai_soul_path
                },
            ),
            Field::secret("AI API key (optional)", ai_api_key),
            Field::secret("YouTube API key", youtube_api_key),
        ];
        self.focus = I_TAVILY_KEY;
        self.screen = Screen::Integrations;
    }

    fn load_integration(&mut self, key: &str) -> String {
        match self.db.config_get_blocking(key) {
            Ok(value) => value.unwrap_or_default(),
            Err(e) => {
                self.status = format!("integration settings load failed: {e}");
                String::new()
            }
        }
    }

    fn integrations_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_integrations();
            return;
        }
        if ctrl && code == KeyCode::Char('g') && self.focus == I_BACKUP_ENCRYPTION_KEY {
            match backup::generate_encryption_key() {
                Ok(key) => {
                    self.fields[I_BACKUP_ENCRYPTION_KEY].value = key;
                    self.status = "generated a new backup encryption key; Ctrl-S to save".into();
                }
                Err(e) => self.status = format!("key generation failed: {e}"),
            }
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::Char(' ') if self.fields[self.focus].cycle.is_some() => {
                self.fields[self.focus].advance()
            }
            KeyCode::Char('u') if ctrl => self.fields[self.focus].value.clear(),
            KeyCode::Char(c) if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.push(c)
            }
            KeyCode::Backspace if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn save_integrations(&mut self) {
        let tavily = self.fields[I_TAVILY_KEY].value.trim().to_string();
        let deepl = self.fields[I_DEEPL_KEY].value.trim().to_string();
        let values = [
            (crate::search::API_KEY_CONFIG, tavily),
            (crate::deepl::API_KEY_CONFIG, deepl),
            (
                backup::KEY_B2_KEY_ID,
                self.fields[I_B2_KEY_ID].value.trim().to_string(),
            ),
            (
                backup::KEY_B2_APPLICATION_KEY,
                self.fields[I_B2_APPLICATION_KEY].value.trim().to_string(),
            ),
            (
                backup::KEY_ENCRYPTION_KEY,
                self.fields[I_BACKUP_ENCRYPTION_KEY]
                    .value
                    .trim()
                    .to_string(),
            ),
            (
                crate::ai::PROVIDER_CONFIG,
                self.fields[I_AI_PROVIDER].value.trim().to_string(),
            ),
            (
                crate::ai::ENDPOINT_CONFIG,
                self.fields[I_AI_ENDPOINT].value.trim().to_string(),
            ),
            (
                crate::ai::MODEL_CONFIG,
                self.fields[I_AI_MODEL].value.trim().to_string(),
            ),
            (
                crate::ai::SOUL_PATH_CONFIG,
                self.fields[I_AI_SOUL_PATH].value.trim().to_string(),
            ),
            (
                crate::ai::API_KEY_CONFIG,
                self.fields[I_AI_API_KEY].value.trim().to_string(),
            ),
            (
                crate::youtube::API_KEY_CONFIG,
                self.fields[I_YOUTUBE_API_KEY].value.trim().to_string(),
            ),
        ];
        for (key, value) in values {
            if let Err(e) = self
                .db
                .config_set_blocking(key, (!value.is_empty()).then_some(value.as_str()))
            {
                self.status = format!("integration settings save failed: {e}");
                return;
            }
        }
        self.status = "integration keys saved; changes apply immediately".into();
    }

    // ---- Backups ----

    fn open_backups(&mut self) {
        match backup::BackupConfig::load(&self.db) {
            Ok(config) => {
                self.fields = vec![
                    Field::boolean("Local backups enabled", config.enabled),
                    Field::text("Local directory", config.directory),
                    Field::text("Daily hour (UTC, 0-23)", config.hour_utc.to_string()),
                    Field::text("Daily copies to keep", config.keep_daily.to_string()),
                    Field::text("Weekly copies to keep", config.keep_weekly.to_string()),
                    Field::text("Monthly copies to keep", config.keep_monthly.to_string()),
                    Field::boolean("Backblaze weekly enabled", config.b2_enabled),
                    Field::text("B2 authorization URL", config.b2_authorize_url),
                    Field::text("B2 bucket name", config.b2_bucket),
                    Field::text("B2 object prefix", config.b2_prefix),
                    Field::choice(
                        "B2 upload weekday (UTC)",
                        &["mon", "tue", "wed", "thu", "fri", "sat", "sun"],
                        weekday_name(config.b2_weekday),
                    ),
                ];
                self.focus = B_ENABLED;
                self.screen = Screen::Backups;
            }
            Err(e) => self.status = format!("backup settings load failed: {e}"),
        }
    }

    fn backups_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_backups();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::BackTab => self.focus = self.focus.saturating_sub(1),
            KeyCode::Char(' ') if self.fields[self.focus].cycle.is_some() => {
                self.fields[self.focus].advance()
            }
            KeyCode::Char('r') => match self.backups.run_now() {
                Ok(()) => self.status = "backup queued; status updates on this page".into(),
                Err(e) => self.status = format!("could not queue backup: {e}"),
            },
            KeyCode::Char('u') if ctrl => self.fields[self.focus].value.clear(),
            KeyCode::Char(c) if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.push(c)
            }
            KeyCode::Backspace if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn save_backups(&mut self) {
        let hour = match parse_bounded(&self.fields[B_HOUR].value, "hour", 23) {
            Ok(value) => value,
            Err(e) => {
                self.status = e;
                return;
            }
        };
        let mut values = vec![
            (backup::KEY_ENABLED, self.fields[B_ENABLED].value.clone()),
            (
                backup::KEY_DIRECTORY,
                self.fields[B_DIRECTORY].value.trim().to_string(),
            ),
            (backup::KEY_HOUR, hour.to_string()),
            (
                backup::KEY_B2_ENABLED,
                self.fields[B_REMOTE_ENABLED].value.clone(),
            ),
            (
                backup::KEY_B2_AUTHORIZE_URL,
                self.fields[B_AUTHORIZE_URL].value.trim().to_string(),
            ),
            (
                backup::KEY_B2_BUCKET,
                self.fields[B_BUCKET].value.trim().to_string(),
            ),
            (
                backup::KEY_B2_PREFIX,
                self.fields[B_PREFIX].value.trim().to_string(),
            ),
            (backup::KEY_B2_WEEKDAY, self.fields[B_WEEKDAY].value.clone()),
        ];
        for (index, key, maximum) in [
            (B_KEEP_DAILY, backup::KEY_KEEP_DAILY, 365),
            (B_KEEP_WEEKLY, backup::KEY_KEEP_WEEKLY, 260),
            (B_KEEP_MONTHLY, backup::KEY_KEEP_MONTHLY, 120),
        ] {
            match parse_bounded(&self.fields[index].value, key, maximum) {
                Ok(0) if index == B_KEEP_DAILY => {
                    self.status = "daily backup retention must be from 1 to 365".into();
                    return;
                }
                Ok(value) => values.push((key, value.to_string())),
                Err(e) => {
                    self.status = e;
                    return;
                }
            }
        }
        if values[1].1.is_empty() || values[4].1.is_empty() {
            self.status = "backup directory and B2 authorization URL cannot be empty".into();
            return;
        }
        if self.fields[B_REMOTE_ENABLED].is_on() && self.fields[B_KEEP_WEEKLY].value.trim() == "0" {
            self.status =
                "weekly retention must be at least 1 when Backblaze backups are enabled".into();
            return;
        }
        for (key, value) in values {
            if let Err(e) = self.db.config_set_blocking(key, Some(&value)) {
                self.status = format!("backup settings save failed: {e}");
                return;
            }
        }
        self.status = "backup settings saved; press r to run now".into();
    }

    // ---- Profile inspection and repair ----

    fn open_profiles(&mut self) {
        match self.db.profile_list_blocking() {
            Ok(profiles) => {
                self.profiles = profiles;
                self.profile_sel = 0;
                self.profile_filter_editing = false;
                self.screen = Screen::Profiles;
                self.status = format!("{} known profiles", self.profiles.len());
            }
            Err(error) => self.status = format!("profile load failed: {error}"),
        }
    }

    fn filtered_profiles(&self) -> Vec<&Profile> {
        let needle = self.profile_filter.trim().to_ascii_lowercase();
        self.profiles
            .iter()
            .filter(|profile| {
                needle.is_empty()
                    || profile.server.to_ascii_lowercase().contains(&needle)
                    || profile.nick.to_ascii_lowercase().contains(&needle)
                    || profile.id.to_ascii_lowercase().contains(&needle)
            })
            .collect()
    }

    fn profiles_key(&mut self, code: KeyCode) {
        if self.profile_filter_editing {
            match code {
                KeyCode::Enter => self.profile_filter_editing = false,
                KeyCode::Esc => self.profile_filter_editing = false,
                KeyCode::Backspace => {
                    self.profile_filter.pop();
                    self.profile_sel = 0;
                }
                KeyCode::Char(character) if !character.is_control() => {
                    self.profile_filter.push(character);
                    self.profile_sel = 0;
                }
                _ => {}
            }
            return;
        }
        match code {
            KeyCode::Char('/') => {
                self.profile_filter_editing = true;
                self.status = "profile filter: type, then Enter".into();
            }
            KeyCode::Char('c') => {
                self.profile_filter.clear();
                self.profile_sel = 0;
            }
            KeyCode::Char('r') => self.open_profiles(),
            KeyCode::Up => self.profile_sel = self.profile_sel.saturating_sub(1),
            KeyCode::Down => {
                let len = self.filtered_profiles().len();
                if len > 0 {
                    self.profile_sel = (self.profile_sel + 1).min(len - 1);
                }
            }
            KeyCode::Enter => {
                let selected = self
                    .filtered_profiles()
                    .get(self.profile_sel)
                    .map(|profile| (*profile).clone());
                if let Some(profile) = selected {
                    self.open_profile_detail(profile);
                }
            }
            KeyCode::Esc => self.screen = Screen::Servers,
            _ => {}
        }
    }

    fn open_profile_detail(&mut self, profile: Profile) {
        let links = self
            .db
            .profile_identity_links_blocking(&profile.server, &profile.id);
        let modules = self
            .profile_admin
            .inspect_blocking(&profile.server, &profile.id);
        match (links, modules) {
            (Ok((aliases, accounts)), Ok(modules)) => {
                self.selected_profile = Some(profile);
                self.profile_aliases = aliases;
                self.profile_accounts = accounts;
                self.profile_modules = modules;
                self.profile_module_sel = 0;
                self.profile_module_scroll = 0;
                self.screen = Screen::ProfileDetail;
                self.status =
                    "Enter opens host fields or module data; r previews a module reset".into();
            }
            (Err(error), _) => self.status = format!("identity links load failed: {error}"),
            (_, Err(error)) => self.status = format!("module profile inspection failed: {error}"),
        }
    }

    fn profile_detail_key(&mut self, code: KeyCode) {
        let count = self.profile_modules.len() + 1;
        match code {
            KeyCode::Up => self.profile_module_sel = self.profile_module_sel.saturating_sub(1),
            KeyCode::Down => {
                self.profile_module_sel = (self.profile_module_sel + 1).min(count.saturating_sub(1))
            }
            KeyCode::Enter if self.profile_module_sel == 0 => self.open_profile_editor(),
            KeyCode::Enter => {
                self.profile_module_scroll = 0;
                self.screen = Screen::ProfileModuleData;
            }
            KeyCode::Char('r') if self.profile_module_sel > 0 => {
                self.preview_profile_module_reset()
            }
            KeyCode::Esc => self.screen = Screen::Profiles,
            _ => {}
        }
    }

    fn open_profile_editor(&mut self) {
        let Some(profile) = self.selected_profile.as_ref() else {
            return;
        };
        self.fields = vec![
            Field::text("Title", optional_text(&profile.title)),
            Field::text("Birthday (MM-DD[-YYYY])", optional_text(&profile.birthday)),
            Field::text("Pronoun subject", optional_text(&profile.pronoun_subject)),
            Field::text("Pronoun object", optional_text(&profile.pronoun_object)),
            Field::text(
                "Pronoun possessive",
                optional_text(&profile.pronoun_possessive),
            ),
            Field::text("Location display", optional_text(&profile.location_display)),
            Field::text(
                "Location canonical label",
                optional_text(&profile.location_label),
            ),
            Field::text(
                "Latitude",
                profile
                    .lat
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            Field::text(
                "Longitude",
                profile
                    .lon
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            ),
            Field::text("Timezone", optional_text(&profile.timezone)),
        ];
        self.focus = 0;
        self.screen = Screen::EditProfile;
    }

    fn edit_profile_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.preview_host_profile_repair();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::ProfileDetail,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len().saturating_sub(1))
            }
            KeyCode::BackTab => self.focus = self.focus.saturating_sub(1),
            KeyCode::Char('u') if ctrl => self.fields[self.focus].value.clear(),
            KeyCode::Char(character) if !character.is_control() => {
                self.fields[self.focus].value.push(character)
            }
            KeyCode::Backspace => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn preview_host_profile_repair(&mut self) {
        let Some(original) = self.selected_profile.as_ref() else {
            return;
        };
        let mut profile = original.clone();
        profile.title = optional_field(&self.fields[P_TITLE].value);
        profile.birthday = optional_field(&self.fields[P_BIRTHDAY].value);
        profile.pronoun_subject = optional_field(&self.fields[P_PRONOUN_SUBJECT].value);
        profile.pronoun_object = optional_field(&self.fields[P_PRONOUN_OBJECT].value);
        profile.pronoun_possessive = optional_field(&self.fields[P_PRONOUN_POSSESSIVE].value);
        profile.location_display = optional_field(&self.fields[P_LOCATION_DISPLAY].value);
        profile.location_label = optional_field(&self.fields[P_LOCATION_LABEL].value);
        profile.lat = match optional_number(&self.fields[P_LATITUDE].value, "latitude") {
            Ok(value) => value,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        profile.lon = match optional_number(&self.fields[P_LONGITUDE].value, "longitude") {
            Ok(value) => value,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        profile.timezone = optional_field(&self.fields[P_TIMEZONE].value);
        if let Err(error) = crate::db::validate_profile_repair(&profile) {
            self.status = format!("invalid profile repair: {error}");
            return;
        }
        let changes = profile_changes(original, &profile);
        if changes.is_empty() {
            self.status = "no profile fields changed".into();
            self.screen = Screen::ProfileDetail;
            return;
        }
        self.pending_profile_repair = Some(PendingProfileRepair::Host {
            profile: Box::new(profile),
            expected: Box::new(original.clone()),
            changes,
        });
        self.screen = Screen::ConfirmProfileRepair;
    }

    fn selected_module(&self) -> Option<&ProfileModuleData> {
        self.profile_module_sel
            .checked_sub(1)
            .and_then(|index| self.profile_modules.get(index))
    }

    fn preview_profile_module_reset(&mut self) {
        let Some(profile) = self.selected_profile.as_ref() else {
            return;
        };
        let Some(module) = self.selected_module().map(|module| module.module.clone()) else {
            return;
        };
        match self
            .profile_admin
            .plan_reset_blocking(&profile.server, &profile.id, &module)
        {
            Ok(plan) if plan.mutation_count() == 0 => {
                self.status = format!("{module} reports no profile-owned data to reset")
            }
            Ok(plan) => {
                self.pending_profile_repair = Some(PendingProfileRepair::ModuleReset { plan });
                self.screen = Screen::ConfirmProfileRepair;
            }
            Err(error) => self.status = format!("module reset preview failed: {error}"),
        }
    }

    fn profile_module_data_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => {
                self.profile_module_scroll = self.profile_module_scroll.saturating_sub(1)
            }
            KeyCode::Down => {
                self.profile_module_scroll = self.profile_module_scroll.saturating_add(1)
            }
            KeyCode::PageUp => {
                self.profile_module_scroll = self.profile_module_scroll.saturating_sub(10)
            }
            KeyCode::PageDown => {
                self.profile_module_scroll = self.profile_module_scroll.saturating_add(10)
            }
            KeyCode::Char('r') => self.preview_profile_module_reset(),
            KeyCode::Esc => self.screen = Screen::ProfileDetail,
            _ => {}
        }
    }

    fn confirm_profile_repair_key(&mut self, code: KeyCode, ctrl: bool) {
        if code == KeyCode::Esc {
            self.pending_profile_repair = None;
            self.screen = Screen::ProfileDetail;
            return;
        }
        if !(ctrl && code == KeyCode::Char('s')) {
            return;
        }
        let Some(pending) = self.pending_profile_repair.take() else {
            self.screen = Screen::ProfileDetail;
            return;
        };
        let snapshot = match backup::create_pre_repair_snapshot(&self.db) {
            Ok(path) => path,
            Err(error) => {
                self.pending_profile_repair = Some(pending);
                self.status = format!("repair blocked: safety snapshot failed: {error}");
                return;
            }
        };
        let result = match pending {
            PendingProfileRepair::Host {
                profile,
                expected,
                changes,
            } => {
                let fields = changes
                    .iter()
                    .filter_map(|change| change.split_once(':').map(|(field, _)| field))
                    .collect::<Vec<_>>()
                    .join(",");
                let id = profile.id.clone();
                match self
                    .db
                    .profile_repair_blocking((*profile).clone(), *expected)
                {
                    Ok(()) => {
                        self.log.info(
                            "profile-repair",
                            format!("profile {} host fields changed: {fields}", short_id(&id)),
                        );
                        self.selected_profile = Some(*profile);
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
            PendingProfileRepair::ModuleReset { plan } => {
                let module = plan.module.clone();
                let profile_id = plan.profile_id.clone();
                self.profile_admin.apply_reset_blocking(plan).map(|()| {
                    self.log.info(
                        "profile-repair",
                        format!(
                            "profile {} module {module} data reset",
                            short_id(&profile_id)
                        ),
                    );
                })
            }
        };
        match result {
            Ok(()) => {
                self.status = format!("repair applied; safety snapshot: {}", snapshot.display());
                if let Some(profile) = self.selected_profile.clone() {
                    self.open_profile_detail(profile);
                } else {
                    self.open_profiles();
                }
            }
            Err(error) => {
                self.status = format!(
                    "repair failed after snapshot {}: {error}",
                    snapshot.display()
                );
                self.screen = Screen::ProfileDetail;
            }
        }
    }

    // ---- Commands and aliases ----

    fn open_commands(&mut self) {
        self.refresh_commands();
        self.screen = Screen::Commands;
    }

    fn refresh_commands(&mut self) {
        self.commands = self.command_registry.lock().unwrap().snapshot();
        if self.command_sel >= self.commands.len() {
            self.command_sel = self.commands.len().saturating_sub(1);
        }
    }

    fn commands_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.command_sel = self.command_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.commands.is_empty() {
                    self.command_sel = (self.command_sel + 1).min(self.commands.len() - 1);
                }
            }
            KeyCode::Enter => {
                if let Some(command) = self.commands.get(self.command_sel).cloned() {
                    self.edit_command = Some((command.module.clone(), command.name.clone()));
                    self.fields = vec![Field::text(
                        &format!("Aliases for !{} (without !)", command.name),
                        command.aliases.join(","),
                    )];
                    self.focus = 0;
                    self.screen = Screen::EditAliases;
                }
            }
            KeyCode::Char('r') => self.restore_alias_defaults(),
            _ => {}
        }
    }

    fn edit_aliases_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_aliases();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::Commands,
            KeyCode::Char('u') if ctrl => self.fields[0].value.clear(),
            KeyCode::Char(character) => self.fields[0].value.push(character),
            KeyCode::Backspace => {
                self.fields[0].value.pop();
            }
            _ => {}
        }
    }

    fn save_aliases(&mut self) {
        let Some((module, command)) = self.edit_command.clone() else {
            self.status = "no command selected".into();
            return;
        };
        let aliases = match parse_alias_csv(&self.fields[0].value) {
            Ok(aliases) => aliases,
            Err(error) => {
                self.status = format!("invalid aliases: {error}");
                return;
            }
        };
        if let Err(error) = self
            .command_registry
            .lock()
            .unwrap()
            .validate_override(&module, &command, &aliases)
        {
            self.status = format!("alias conflict: {error}");
            return;
        }
        if let Err(error) = self
            .db
            .set_alias_override_blocking(&module, &command, Some(&aliases))
        {
            self.status = format!("alias save failed: {error}");
            return;
        }
        let new_aliases = self.fields[0].value.clone();
        self.command_registry
            .lock()
            .unwrap()
            .set_override(&module, &command, Some(aliases));
        self.log.log(
            Level::Info,
            Category::Command,
            "tui",
            format!("{module}: aliases for !{command} overridden → {new_aliases}"),
        );
        self.status = format!("aliases for !{command} saved; changes apply immediately");
        self.refresh_commands();
        self.screen = Screen::Commands;
    }

    fn restore_alias_defaults(&mut self) {
        let Some(command) = self.commands.get(self.command_sel).cloned() else {
            return;
        };
        if let Err(error) =
            self.db
                .set_alias_override_blocking(&command.module, &command.name, None)
        {
            self.status = format!("alias reset failed: {error}");
            return;
        }
        self.command_registry
            .lock()
            .unwrap()
            .set_override(&command.module, &command.name, None);
        self.log.log(
            Level::Info,
            Category::Command,
            "tui",
            format!(
                "{}: aliases for !{} reset to module defaults",
                command.module, command.name
            ),
        );
        self.status = format!("aliases for !{} restored to module defaults", command.name);
        self.refresh_commands();
    }

    // ---- Module settings ----

    fn open_module_settings(&mut self) {
        self.refresh_module_settings();
        self.screen = Screen::ModuleSettings;
    }

    fn refresh_module_settings(&mut self) {
        self.settings = self.setting_registry.lock().unwrap().snapshot();
        self.setting_overrides = self
            .db
            .load_setting_overrides_blocking()
            .unwrap_or_default();
        if self.setting_sel >= self.settings.len() {
            self.setting_sel = self.settings.len().saturating_sub(1);
        }
    }

    fn module_settings_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.setting_sel = self.setting_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.settings.is_empty() {
                    self.setting_sel = (self.setting_sel + 1).min(self.settings.len() - 1);
                }
            }
            KeyCode::Enter => self.begin_setting_edit(),
            _ => {}
        }
    }

    fn begin_setting_edit(&mut self) {
        let Some(setting) = self.settings.get(self.setting_sel).cloned() else {
            return;
        };
        let default_server = self
            .servers
            .get(self.server_sel)
            .or_else(|| self.servers.first())
            .map(|server| server.label.clone())
            .unwrap_or_default();
        let default_channel = self
            .servers
            .iter()
            .find(|candidate| candidate.label == default_server)
            .and_then(|server| server.channels.first())
            .map(|(channel, _)| channel.clone())
            .unwrap_or_default();
        let setting_id = (setting.module.clone(), setting.spec.key.clone());
        let remembered = self.setting_locations.get(&setting_id);
        let location = initial_setting_location(
            &setting,
            &self.setting_overrides,
            remembered,
            &default_server,
            &default_channel,
        );
        let scope = location.scope;
        let server = location.server;
        let channel = location.channel;
        let (scope_server, scope_channel) = normalized_scope(scope, &server, &channel);
        let value = self
            .db
            .setting_override_get_blocking(
                &setting.module,
                &setting.spec.key,
                scope,
                scope_server,
                scope_channel,
            )
            .ok()
            .flatten()
            .unwrap_or_else(|| setting.spec.default.clone());
        let scope_options = setting
            .spec
            .scopes
            .iter()
            .map(|scope| scope_name(*scope).to_string())
            .collect();
        let value_field = match &setting.spec.kind {
            SettingKind::Boolean => Field::boolean("Value", value == "true"),
            SettingKind::Choice { options } => {
                Field::choices("Value", options.clone(), value.clone())
            }
            _ => Field::text("Value", value),
        };
        self.fields = vec![
            Field::choices("Scope", scope_options, scope_name(scope).into()),
            Field::text("Network label", server),
            Field::text("Channel", channel),
            value_field,
        ];
        self.focus = 0;
        self.edit_setting = Some(setting);
        self.screen = Screen::EditModuleSetting;
    }

    fn edit_module_setting_key(&mut self, code: KeyCode, ctrl: bool) {
        if ctrl && code == KeyCode::Char('s') {
            self.save_module_setting();
            return;
        }
        if ctrl && code == KeyCode::Char('d') {
            self.reset_module_setting();
            return;
        }
        match code {
            KeyCode::Esc => self.screen = Screen::ModuleSettings,
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::Char(' ') if self.fields[self.focus].cycle.is_some() => {
                self.fields[self.focus].advance()
            }
            KeyCode::Char(character) if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.push(character)
            }
            KeyCode::Backspace if self.fields[self.focus].cycle.is_none() => {
                self.fields[self.focus].value.pop();
            }
            _ => {}
        }
    }

    fn save_module_setting(&mut self) {
        let Some(setting) = self.edit_setting.clone() else {
            return;
        };
        let Some(scope) = parse_scope(&self.fields[M_SCOPE].value) else {
            self.status = "invalid setting scope".into();
            return;
        };
        let server = self.fields[M_NETWORK].value.trim();
        let channel = self.fields[M_CHANNEL].value.trim();
        let value = self.fields[M_VALUE].value.trim();
        if let Err(error) = self.setting_registry.lock().unwrap().validate_override(
            &setting.module,
            &setting.spec.key,
            scope,
            server,
            channel,
            value,
        ) {
            self.status = format!("invalid setting: {error}");
            return;
        }
        let (server, channel) = normalized_scope(scope, server, channel);
        let location = SettingLocation {
            scope,
            server: server.to_string(),
            channel: channel.to_string(),
        };
        match self.db.setting_override_set_blocking(
            &setting.module,
            &setting.spec.key,
            scope,
            server,
            channel,
            Some(value),
        ) {
            Ok(()) => {
                self.setting_locations
                    .insert((setting.module.clone(), setting.spec.key.clone()), location);
                self.setting_registry.lock().unwrap().set_override(
                    &setting.module,
                    &setting.spec.key,
                    scope,
                    server,
                    channel,
                    Some(value.to_string()),
                );
                self.log.log(
                    Level::Info,
                    Category::Command,
                    "tui",
                    format!(
                        "{}.{} [{scope}] server={server:?} channel={channel:?} = {value}",
                        setting.module,
                        setting.spec.key,
                        scope = scope_name(scope),
                    ),
                );
                self.status = format!(
                    "{}.{} override saved; applies immediately",
                    setting.module, setting.spec.key
                );
                self.open_module_settings();
            }
            Err(error) => self.status = format!("setting save failed: {error}"),
        }
    }

    fn reset_module_setting(&mut self) {
        let Some(setting) = self.edit_setting.clone() else {
            return;
        };
        let Some(scope) = parse_scope(&self.fields[M_SCOPE].value) else {
            return;
        };
        let (server, channel) = normalized_scope(
            scope,
            self.fields[M_NETWORK].value.trim(),
            self.fields[M_CHANNEL].value.trim(),
        );
        let location = SettingLocation {
            scope,
            server: server.to_string(),
            channel: channel.to_string(),
        };
        match self.db.setting_override_set_blocking(
            &setting.module,
            &setting.spec.key,
            scope,
            server,
            channel,
            None,
        ) {
            Ok(()) => {
                self.setting_locations
                    .insert((setting.module.clone(), setting.spec.key.clone()), location);
                self.setting_registry.lock().unwrap().set_override(
                    &setting.module,
                    &setting.spec.key,
                    scope,
                    server,
                    channel,
                    None,
                );
                self.log.log(
                    Level::Info,
                    Category::Command,
                    "tui",
                    format!(
                        "{}.{} [{scope}] server={server:?} channel={channel:?} override removed",
                        setting.module,
                        setting.spec.key,
                        scope = scope_name(scope),
                    ),
                );
                self.status = format!(
                    "{}.{} override removed; module default/fallback now applies",
                    setting.module, setting.spec.key
                );
                self.open_module_settings();
            }
            Err(error) => self.status = format!("setting reset failed: {error}"),
        }
    }

    // ---- Scheduler ----

    fn open_scheduler(&mut self) {
        self.refresh_scheduler();
        self.screen = Screen::Scheduler;
    }

    fn refresh_scheduler(&mut self) {
        match self.scheduler.list_all_blocking() {
            Ok(jobs) => {
                self.scheduler_jobs = jobs;
                if self.scheduler_sel >= self.scheduler_jobs.len() {
                    self.scheduler_sel = self.scheduler_jobs.len().saturating_sub(1);
                }
            }
            Err(e) => self.status = format!("scheduler load failed: {e}"),
        }
    }

    fn scheduler_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Esc => self.screen = Screen::Servers,
            KeyCode::Up => self.scheduler_sel = self.scheduler_sel.saturating_sub(1),
            KeyCode::Down => {
                if !self.scheduler_jobs.is_empty() {
                    self.scheduler_sel =
                        (self.scheduler_sel + 1).min(self.scheduler_jobs.len() - 1);
                }
            }
            KeyCode::Char('r') => self.refresh_scheduler(),
            KeyCode::Char('d') | KeyCode::Delete => {
                if let Some(job) = self.scheduler_jobs.get(self.scheduler_sel).cloned() {
                    match self.scheduler.cancel_blocking(&job.module, &job.id) {
                        Ok(true) => {
                            self.status =
                                format!("cancelled '{}' (module: {})", job.id, job.module);
                            self.refresh_scheduler();
                        }
                        Ok(false) => {
                            self.status = format!("job '{}' was already gone", job.id);
                            self.refresh_scheduler();
                        }
                        Err(e) => self.status = format!("cancel failed: {e}"),
                    }
                }
            }
            _ => {}
        }
    }

    // ---- Logs ----

    fn logs_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('0') => self.filter = None,
            KeyCode::Char('1') => self.filter = Some(Category::Error),
            KeyCode::Char('2') => self.filter = Some(Category::Debug),
            KeyCode::Char('3') => self.filter = Some(Category::Message),
            KeyCode::Char('4') => self.filter = Some(Category::Command),
            KeyCode::Up => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down => self.scroll += 1,
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => self.scroll += 10,
            KeyCode::End => self.follow = true,
            _ => {}
        }
    }

    // ---- Rendering ----

    fn render(&self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

        let selected = match self.screen {
            Screen::Logs => 1,
            Screen::Integrations => 2,
            Screen::Commands | Screen::EditAliases => 3,
            Screen::ModuleSettings | Screen::EditModuleSetting => 4,
            Screen::Scheduler => 5,
            Screen::Backups => 6,
            Screen::Profiles
            | Screen::ProfileDetail
            | Screen::EditProfile
            | Screen::ProfileModuleData
            | Screen::ConfirmProfileRepair => 7,
            _ => 0,
        };
        let tabs = Tabs::new(vec![
            "Servers (F1)",
            "Logs (F2)",
            "Integrations (F3)",
            "Commands (F4)",
            "Modules (F5)",
            "Scheduler (F6)",
            "Backups (F7)",
            "Profiles (F8)",
        ])
        .select(selected)
        .block(Block::default().borders(Borders::ALL).title("rustjeeves"))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        f.render_widget(tabs, chunks[0]);

        match self.screen {
            Screen::Servers => self.render_servers(f, chunks[1]),
            Screen::EditServer => self.render_form(
                f,
                chunks[1],
                "Edit server — ↑/↓ move · type · Space toggles · Ctrl-S save · Esc cancel",
            ),
            Screen::Admins => self.render_admins(f, chunks[1]),
            Screen::EditAdmin => self.render_form(
                f,
                chunks[1],
                "Edit admin — ↑/↓ move · Space cycles role · Ctrl-S save · Esc cancel",
            ),
            Screen::Logs => self.render_logs(f, chunks[1]),
            Screen::Integrations => self.render_form(
                f,
                chunks[1],
                "Integrations — ↑/↓ move · keys masked · Ctrl-G generate backup key · Ctrl-S save · Ctrl-U clear · Esc back",
            ),
            Screen::Commands => self.render_commands(f, chunks[1]),
            Screen::EditAliases => self.render_form(
                f,
                chunks[1],
                "Edit aliases — comma-separated without ! · Ctrl-S save · Ctrl-U clear · Esc cancel",
            ),
            Screen::ModuleSettings => self.render_module_settings(f, chunks[1]),
            Screen::EditModuleSetting => self.render_form(
                f,
                chunks[1],
                "Edit module setting — Space cycles · Ctrl-S save · Ctrl-D reset override · Esc cancel",
            ),
            Screen::Scheduler => self.render_scheduler(f, chunks[1]),
            Screen::Backups => self.render_backups(f, chunks[1]),
            Screen::Profiles => self.render_profiles(f, chunks[1]),
            Screen::ProfileDetail => self.render_profile_detail(f, chunks[1]),
            Screen::EditProfile => self.render_form(
                f,
                chunks[1],
                "Edit host profile — blanks clear fields · Ctrl-S preview · Ctrl-U clear · Esc cancel",
            ),
            Screen::ProfileModuleData => self.render_profile_module_data(f, chunks[1]),
            Screen::ConfirmProfileRepair => self.render_profile_repair_confirmation(f, chunks[1]),
        }

        let status =
            Paragraph::new(self.status.clone()).style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, chunks[2]);
    }

    fn render_servers(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.servers.is_empty() {
            vec![ListItem::new("(no servers — press 'a' to add one)")]
        } else {
            self.servers
                .iter()
                .enumerate()
                .map(|(i, s)| {
                    let focused = i == self.server_sel;
                    let mark = if s.enabled { "●" } else { "○" };
                    let style = if focused {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(Line::from(vec![Span::styled(
                        format!(
                            "{mark} {:<16} {}:{} (tls={})",
                            s.label, s.host, s.port, s.tls
                        ),
                        style,
                    )]))
                })
                .collect()
        };
        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(
            "Servers — ↑/↓ · Enter edit · a add · d delete · Space enable/disable · m admins",
        ));
        f.render_widget(list, area);
    }

    fn render_admins(&self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = if self.admins.is_empty() {
            vec![ListItem::new("(no admins — press 'a' to add one)")]
        } else {
            self.admins
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let focused = i == self.admin_sel;
                    let role = match a.role {
                        Role::Admin => "admin",
                        Role::SuperAdmin => "superadmin",
                    };
                    let bound = match (&a.bound_account, &a.bound_hostmask) {
                        (Some(acc), _) => format!("account:{acc}"),
                        (None, Some(h)) => format!("host:{h}"),
                        _ => "unbound".into(),
                    };
                    let acct = a.account.as_deref().unwrap_or("-");
                    let style = if focused {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(Line::from(vec![Span::styled(
                        format!("{:<16} {:<11} acct={:<10} [{bound}]", a.nick, role, acct),
                        style,
                    )]))
                })
                .collect()
        };
        let title = format!(
            "Admins on '{}' — ↑/↓ · Enter edit · a add · d delete · Esc back",
            self.admin_server_label
        );
        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title));
        f.render_widget(list, area);
    }

    fn render_commands(&self, f: &mut Frame, area: Rect) {
        let items = if self.commands.is_empty() {
            vec![ListItem::new("(no loaded modules advertise commands)")]
        } else {
            self.commands
                .iter()
                .enumerate()
                .map(|(index, command)| {
                    let style = if index == self.command_sel {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let aliases = command
                        .aliases
                        .iter()
                        .map(|alias| format!("!{alias}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let aliases = if aliases.is_empty() {
                        "-".into()
                    } else {
                        aliases
                    };
                    let source = if command.has_override {
                        "custom"
                    } else {
                        "default"
                    };
                    ListItem::new(Line::from(vec![Span::styled(
                        format!(
                            "{:<18} {:<12} aliases: {:<28} [{}]  {}",
                            format!("!{}", command.name),
                            command.module,
                            aliases,
                            source,
                            command.description
                        ),
                        style,
                    )]))
                })
                .collect()
        };
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Commands — ↑/↓ · Enter edit aliases · r restore defaults · Esc back"),
        );
        f.render_widget(list, area);
    }

    fn render_module_settings(&self, f: &mut Frame, area: Rect) {
        let items = if self.settings.is_empty() {
            vec![ListItem::new("(no loaded modules advertise settings)")]
        } else {
            self.settings
                .iter()
                .enumerate()
                .map(|(index, setting)| {
                    let style = if index == self.setting_sel {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let scopes = setting
                        .spec
                        .scopes
                        .iter()
                        .map(|scope| scope_name(*scope))
                        .collect::<Vec<_>>()
                        .join("/");
                    let override_count = self
                        .setting_overrides
                        .iter()
                        .filter(|entry| {
                            entry.module == setting.module && entry.key == setting.spec.key
                        })
                        .count();
                    ListItem::new(Line::from(vec![Span::styled(
                        format!(
                            "{:<14} {:<22} default={:<10} scopes={:<22} overrides={}  {}",
                            setting.module,
                            setting.spec.key,
                            setting.spec.default,
                            scopes,
                            override_count,
                            setting.spec.description
                        ),
                        style,
                    )]))
                })
                .collect()
        };
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Module settings — ↑/↓ · Enter edit scoped override · Esc back"),
        );
        f.render_widget(list, area);
    }

    fn render_profiles(&self, f: &mut Frame, area: Rect) {
        let profiles = self.filtered_profiles();
        let items = if profiles.is_empty() {
            vec![ListItem::new("(no profiles match the filter)")]
        } else {
            profiles
                .iter()
                .enumerate()
                .map(|(index, profile)| {
                    let style = if index == self.profile_sel {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(Line::from(Span::styled(
                        format!(
                            "{:<16} {:<24} last seen {}  [{}]",
                            profile.server,
                            profile.nick,
                            format_timestamp(profile.last_seen),
                            short_id(&profile.id)
                        ),
                        style,
                    )))
                })
                .collect()
        };
        let filter_cursor = if self.profile_filter_editing { "_" } else { "" };
        let title = format!(
            "Profiles — ↑/↓ · Enter inspect · / filter · c clear · r refresh · filter: {}{}",
            self.profile_filter, filter_cursor
        );
        f.render_widget(
            List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
    }

    fn render_profile_detail(&self, f: &mut Frame, area: Rect) {
        let sections = Layout::vertical([Constraint::Length(7), Constraint::Min(4)]).split(area);
        let Some(profile) = self.selected_profile.as_ref() else {
            f.render_widget(Paragraph::new("No profile selected."), area);
            return;
        };
        let aliases = self
            .profile_aliases
            .iter()
            .map(|alias| alias.nick.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let accounts = self.profile_accounts.join(", ");
        let summary = vec![
            Line::from(format!(
                "Network: {}    Nick: {}",
                profile.server, profile.nick
            )),
            Line::from(format!("UUID: {}", profile.id)),
            Line::from(format!(
                "Created: {}    Last seen: {}",
                format_timestamp(profile.created),
                format_timestamp(profile.last_seen)
            )),
            Line::from(format!(
                "Aliases: {}",
                if aliases.is_empty() { "—" } else { &aliases }
            )),
            Line::from(format!(
                "Accounts: {}",
                if accounts.is_empty() {
                    "—"
                } else {
                    &accounts
                }
            )),
        ];
        f.render_widget(
            Paragraph::new(summary).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Stable identity (read-only)"),
            ),
            sections[0],
        );

        let mut rows = vec![("Host profile fields".to_string(), None)];
        rows.extend(self.profile_modules.iter().map(|module| {
            let state = if let Some(error) = &module.error {
                format!("unavailable: {error}")
            } else if module.data.is_some() {
                "profile data present".into()
            } else {
                "no profile data".into()
            };
            (module.module.clone(), Some(state))
        }));
        let items = rows
            .into_iter()
            .enumerate()
            .map(|(index, (name, state))| {
                let style = if index == self.profile_module_sel {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(Span::styled(
                    match state {
                        Some(state) => format!("{name:<18} {state}"),
                        None => name,
                    },
                    style,
                )))
            })
            .collect::<Vec<_>>();
        f.render_widget(
            List::new(items).block(
                Block::default().borders(Borders::ALL).title(
                    "Profile data — Enter inspect/edit · r reset selected module · Esc back",
                ),
            ),
            sections[1],
        );
    }

    fn render_profile_module_data(&self, f: &mut Frame, area: Rect) {
        let Some(module) = self.selected_module() else {
            f.render_widget(Paragraph::new("No module selected."), area);
            return;
        };
        let content = if let Some(error) = &module.error {
            format!("Unavailable: {error}")
        } else if let Some(data) = &module.data {
            serde_json::to_string_pretty(data).unwrap_or_else(|_| "(invalid module data)".into())
        } else {
            "(This module reports no data owned by the selected profile.)".into()
        };
        f.render_widget(
            Paragraph::new(content)
                .wrap(Wrap { trim: false })
                .scroll((self.profile_module_scroll, 0))
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "{} — read-only lifecycle view · ↑/↓ scroll · r reset owned data · Esc back",
                    module.module
                ))),
            area,
        );
    }

    fn render_profile_repair_confirmation(&self, f: &mut Frame, area: Rect) {
        let lines = match self.pending_profile_repair.as_ref() {
            Some(PendingProfileRepair::Host { changes, .. }) => {
                let mut lines = vec![Line::from("Host profile dry-run:")];
                lines.extend(changes.iter().map(|change| Line::from(format!("  {change}"))));
                lines
            }
            Some(PendingProfileRepair::ModuleReset { plan }) => vec![
                Line::from(format!("Module reset dry-run: {}", plan.module)),
                Line::from(format!(
                    "  {} validated mutation(s) will remove or rewrite this profile's owned data.",
                    plan.mutation_count()
                )),
                Line::from("  Other profiles and unrelated aggregate data are preserved by the module hook."),
            ],
            None => vec![Line::from("No repair is pending.")],
        };
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Confirm repair — Ctrl-S snapshots and applies · Esc cancels"),
            ),
            area,
        );
    }

    fn render_form(&self, f: &mut Frame, area: Rect, title: &str) {
        let items: Vec<ListItem> = self
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let shown = if field.secret && !field.value.is_empty() {
                    "•".repeat(field.value.len())
                } else {
                    field.value.clone()
                };
                let focused = i == self.focus;
                let cursor = if focused && field.cycle.is_none() {
                    "_"
                } else {
                    ""
                };
                let label_style = if focused {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<34}", field.label), label_style),
                    Span::raw(format!("{shown}{cursor}")),
                ]))
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title.to_string()),
        );
        f.render_widget(list, area);
    }

    fn render_backups(&self, f: &mut Frame, area: Rect) {
        let sections = Layout::vertical([Constraint::Min(12), Constraint::Length(7)]).split(area);
        self.render_form(
            f,
            sections[0],
            "Backups — ↑/↓ · Space toggles · Ctrl-S save · r run now · Esc back",
        );
        let status = self.backups.status();
        let lines = vec![
            Line::from(format!(
                "State: {}",
                if status.running { "running" } else { "idle" }
            )),
            Line::from(format!(
                "Last success: {}",
                status.last_success_at.as_deref().unwrap_or("never")
            )),
            Line::from(format!(
                "Local: {}",
                status.last_local_path.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "Remote: {}",
                status.last_remote_object.as_deref().unwrap_or("-")
            )),
            Line::from(format!(
                "Last error: {}",
                status.last_error.as_deref().unwrap_or("-")
            )),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: true }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Backup status"),
            ),
            sections[1],
        );
    }

    fn render_scheduler(&self, f: &mut Frame, area: Rect) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let items = if self.scheduler_jobs.is_empty() {
            vec![ListItem::new("(no pending scheduled jobs)")]
        } else {
            self.scheduler_jobs
                .iter()
                .enumerate()
                .map(|(index, job)| {
                    let style = if index == self.scheduler_sel {
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    let diff = job.due_at - now;
                    let when = if diff > 0 {
                        format!("in {}", fmt_duration(diff))
                    } else {
                        format!("{} overdue", fmt_duration(-diff))
                    };
                    let id_display = truncate_with_ellipsis(&job.id, 30);
                    ListItem::new(Line::from(vec![Span::styled(
                        format!(
                            "{:<14} {:<14} {:<14} {:<31} {}",
                            job.module, job.server, job.channel, id_display, when
                        ),
                        style,
                    )]))
                })
                .collect()
        };
        let list =
            List::new(items).block(Block::default().borders(Borders::ALL).title(
                "Scheduled jobs — ↑/↓ · d/Del cancel · r refresh · Esc back  [payload hidden]",
            ));
        f.render_widget(list, area);
    }

    fn render_logs(&self, f: &mut Frame, area: Rect) {
        let filtered: Vec<&LogEvent> = self
            .logs
            .iter()
            .filter(|e| self.filter.is_none_or(|c| e.category == c))
            .collect();
        let height = area.height.saturating_sub(2) as usize;
        let start = if self.follow {
            filtered.len().saturating_sub(height)
        } else {
            self.scroll.min(filtered.len().saturating_sub(1))
        };
        let lines: Vec<Line> = filtered
            .iter()
            .skip(start)
            .take(height)
            .map(|e| {
                let color = match e.category {
                    Category::Error => Color::Red,
                    Category::Debug => Color::DarkGray,
                    Category::Message => Color::Green,
                    Category::Command => Color::Cyan,
                };
                Line::from(vec![
                    Span::styled(
                        format!("{:<8}", cat_label(e.category)),
                        Style::default().fg(color),
                    ),
                    Span::raw(format!("{}: {}", e.source, e.message)),
                ])
            })
            .collect();
        let filter_label = match self.filter {
            None => "ALL",
            Some(Category::Error) => "ERROR",
            Some(Category::Debug) => "DEBUG",
            Some(Category::Message) => "MESSAGE",
            Some(Category::Command) => "COMMAND",
        };
        let title = format!("Logs [{filter_label}] — 0 all · 1 err · 2 dbg · 3 msg · 4 cmd · ↑/↓ scroll · End follow");
        let para = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    }
}

fn parse_scope(value: &str) -> Option<SettingScope> {
    match value {
        "global" => Some(SettingScope::Global),
        "network" => Some(SettingScope::Network),
        "channel" => Some(SettingScope::Channel),
        _ => None,
    }
}

fn initial_setting_location(
    setting: &RegisteredSetting,
    overrides: &[SettingOverride],
    remembered: Option<&SettingLocation>,
    default_server: &str,
    default_channel: &str,
) -> SettingLocation {
    if let Some(location) = remembered.filter(|location| {
        setting.spec.scopes.contains(&location.scope)
            && (location.scope == SettingScope::Global || !location.server.is_empty())
            && (location.scope != SettingScope::Channel || !location.channel.is_empty())
    }) {
        return location.clone();
    }

    let saved = overrides
        .iter()
        .filter(|entry| {
            entry.module == setting.module
                && entry.key == setting.spec.key
                && setting.spec.scopes.contains(&entry.scope)
        })
        .max_by_key(|entry| match entry.scope {
            SettingScope::Global => 0,
            SettingScope::Network => 1,
            SettingScope::Channel => 2,
        });
    if let Some(saved) = saved {
        return SettingLocation {
            scope: saved.scope,
            server: saved.server.clone(),
            channel: saved.channel.clone(),
        };
    }

    let scope = if setting.spec.scopes.contains(&SettingScope::Global) {
        SettingScope::Global
    } else {
        setting.spec.scopes[0]
    };
    SettingLocation {
        scope,
        server: default_server.into(),
        channel: default_channel.into(),
    }
}

fn normalized_scope<'a>(
    scope: SettingScope,
    server: &'a str,
    channel: &'a str,
) -> (&'a str, &'a str) {
    match scope {
        SettingScope::Global => ("", ""),
        SettingScope::Network => (server, ""),
        SettingScope::Channel => (server, channel),
    }
}

fn cat_label(c: Category) -> &'static str {
    match c {
        Category::Error => "ERROR",
        Category::Debug => "DEBUG",
        Category::Message => "MSG",
        Category::Command => "CMD",
    }
}

fn fmt_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn truncate_with_ellipsis(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut truncated = value.chars().take(max_chars - 1).collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod rendering_tests {
    use super::{
        initial_setting_location, optional_number, profile_changes, truncate_with_ellipsis,
        RegisteredSetting, SettingLocation, SettingOverride,
    };
    use jeeves_abi::{Profile, SettingKind, SettingScope};

    #[test]
    fn truncates_unicode_at_character_boundaries() {
        assert_eq!(truncate_with_ellipsis("éééé", 3), "éé…");
        assert_eq!(truncate_with_ellipsis("short", 30), "short");
        assert_eq!(truncate_with_ellipsis("anything", 0), "");
    }

    #[test]
    fn profile_dry_run_reports_only_changed_fields() {
        let before = Profile {
            title: Some("Captain".into()),
            lat: Some(10.0),
            lon: Some(20.0),
            ..Default::default()
        };
        let mut after = before.clone();
        after.title = None;
        after.lat = Some(11.0);
        let changes = profile_changes(&before, &after);
        assert_eq!(changes.len(), 2);
        assert!(changes[0].starts_with("title:"));
        assert!(changes[1].starts_with("latitude:"));
    }

    #[test]
    fn optional_coordinates_accept_blank_and_reject_text() {
        assert_eq!(optional_number("", "latitude").unwrap(), None);
        assert_eq!(optional_number("12.5", "latitude").unwrap(), Some(12.5));
        assert!(optional_number("north", "latitude").is_err());
    }

    #[test]
    fn setting_editor_reopens_an_existing_channel_override() {
        let setting = RegisteredSetting {
            module: "ai".into(),
            spec: jeeves_abi::SettingSpec {
                key: "enabled".into(),
                description: String::new(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![
                    SettingScope::Global,
                    SettingScope::Network,
                    SettingScope::Channel,
                ],
                applies_immediately: true,
            },
        };
        let overrides = vec![SettingOverride {
            module: "ai".into(),
            key: "enabled".into(),
            scope: SettingScope::Channel,
            server: "network".into(),
            channel: "#transience".into(),
            value: "true".into(),
        }];

        let location = initial_setting_location(&setting, &overrides, None, "network", "#bots");
        assert_eq!(location.scope, SettingScope::Channel);
        assert_eq!(location.server, "network");
        assert_eq!(location.channel, "#transience");
    }

    #[test]
    fn setting_editor_prefers_the_location_just_saved() {
        let setting = RegisteredSetting {
            module: "ai".into(),
            spec: jeeves_abi::SettingSpec {
                key: "enabled".into(),
                description: String::new(),
                default: "false".into(),
                kind: SettingKind::Boolean,
                scopes: vec![SettingScope::Channel],
                applies_immediately: true,
            },
        };
        let remembered = SettingLocation {
            scope: SettingScope::Channel,
            server: "network".into(),
            channel: "#transience".into(),
        };

        let location =
            initial_setting_location(&setting, &[], Some(&remembered), "network", "#bots");
        assert_eq!(location, remembered);
    }
}
