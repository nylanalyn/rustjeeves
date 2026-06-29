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
use crate::scheduler::SchedulerHandle;
use crate::settings::{scope_name, RegisteredSetting, SettingOverride, SharedSettingRegistry};
use anyhow::Result;
use jeeves_abi::{Category, Level, Role, ScheduledJob, SettingKind, SettingScope};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
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

pub(crate) struct Services {
    pub commands: SharedCommandRegistry,
    pub settings: SharedSettingRegistry,
    pub scheduler: SchedulerHandle,
    pub backups: BackupHandle,
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

    scheduler_jobs: Vec<ScheduledJob>,
    scheduler_sel: usize,
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
    ) -> Self {
        let servers = db.load_servers_blocking().unwrap_or_default();
        App {
            db,
            log,
            command_registry,
            setting_registry,
            scheduler,
            backups,
            screen: Screen::Servers,
            status: "F1 Servers · F2 Logs · F3 Integrations · F4 Commands · F5 Modules · F6 Scheduler · F7 Backups · Ctrl-R apply/connect · Ctrl-Q quit".into(),
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
            scheduler_jobs: Vec::new(),
            scheduler_sel: 0,
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
        let scope = if setting.spec.scopes.contains(&SettingScope::Global) {
            SettingScope::Global
        } else {
            setting.spec.scopes[0]
        };
        let server = self
            .servers
            .get(self.server_sel)
            .or_else(|| self.servers.first())
            .map(|server| server.label.clone())
            .unwrap_or_default();
        let channel = self
            .servers
            .iter()
            .find(|candidate| candidate.label == server)
            .and_then(|server| server.channels.first())
            .map(|(channel, _)| channel.clone())
            .unwrap_or_default();
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
        match self.db.setting_override_set_blocking(
            &setting.module,
            &setting.spec.key,
            scope,
            server,
            channel,
            Some(value),
        ) {
            Ok(()) => {
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
        match self.db.setting_override_set_blocking(
            &setting.module,
            &setting.spec.key,
            scope,
            server,
            channel,
            None,
        ) {
            Ok(()) => {
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
    use super::truncate_with_ellipsis;

    #[test]
    fn truncates_unicode_at_character_boundaries() {
        assert_eq!(truncate_with_ellipsis("éééé", 3), "éé…");
        assert_eq!(truncate_with_ellipsis("short", 30), "short");
        assert_eq!(truncate_with_ellipsis("anything", 0), "");
    }
}
