//! Interactive TUI (ratatui + crossterm).
//!
//! Screens: Servers (list of network profiles), Edit server (per-profile fields), Admins (per
//! server access list), Edit admin, and Logs (filterable). The TUI reads and writes the database
//! directly through the DB actor's blocking API (it runs on a blocking thread), and asks the
//! runtime to (re)connect via an [`AppRequest`].

use crate::action::AppRequest;
use crate::config::{AdminEntry, ServerConfig};
use crate::db::DbHandle;
use crate::log_bus::LogEvent;
use anyhow::Result;
use jeeves_abi::{Category, Role};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::sync::mpsc;

pub fn run(db: DbHandle, logs_rx: Receiver<LogEvent>, app_tx: mpsc::Sender<AppRequest>) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(db);
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
        Field { label: label.into(), value, secret: false, cycle: None }
    }
    fn secret(label: &str, value: String) -> Self {
        Field { label: label.into(), value, secret: true, cycle: None }
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

// Admin-edit field indices.
const A_NICK: usize = 0;
const A_ROLE: usize = 1;
const A_ACCOUNT: usize = 2;

impl App {
    fn new(db: DbHandle) -> Self {
        let servers = db.load_servers_blocking().unwrap_or_default();
        App {
            db,
            screen: Screen::Servers,
            status: "F1 Servers · F2 Logs · Ctrl-R apply/connect · Ctrl-Q quit".into(),
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
                        _ => match self.screen {
                            Screen::Servers => self.servers_key(key.code),
                            Screen::EditServer => self.edit_server_key(key.code, ctrl),
                            Screen::Admins => self.admins_key(key.code),
                            Screen::EditAdmin => self.edit_admin_key(key.code, ctrl),
                            Screen::Logs => self.logs_key(key.code),
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
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1).min(self.fields.len() - 1),
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
                let key = parts.next().map(|k| k.trim().to_string()).filter(|k| !k.is_empty());
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
            KeyCode::Down | KeyCode::Tab => self.focus = (self.focus + 1).min(self.fields.len() - 1),
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
        let entry = AdminEntry { nick, role, account, bound_hostmask: None, bound_account: None };
        match self.db.upsert_admin_blocking(self.admin_server_id, entry) {
            Ok(()) => {
                self.status = "admin saved".into();
                self.refresh_admins();
                self.screen = Screen::Admins;
            }
            Err(e) => self.status = format!("save failed: {e}"),
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
        let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
            .split(f.area());

        let selected = matches!(self.screen, Screen::Logs) as usize;
        let tabs = Tabs::new(vec!["Servers (F1)", "Logs (F2)"])
            .select(selected)
            .block(Block::default().borders(Borders::ALL).title("rustjeeves"))
            .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        f.render_widget(tabs, chunks[0]);

        match self.screen {
            Screen::Servers => self.render_servers(f, chunks[1]),
            Screen::EditServer => self.render_form(f, chunks[1], "Edit server — ↑/↓ move · type · Space toggles · Ctrl-S save · Esc cancel"),
            Screen::Admins => self.render_admins(f, chunks[1]),
            Screen::EditAdmin => self.render_form(f, chunks[1], "Edit admin — ↑/↓ move · Space cycles role · Ctrl-S save · Esc cancel"),
            Screen::Logs => self.render_logs(f, chunks[1]),
        }

        let status = Paragraph::new(self.status.clone()).style(Style::default().fg(Color::DarkGray));
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
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    };
                    ListItem::new(Line::from(vec![Span::styled(
                        format!("{mark} {:<16} {}:{} (tls={})", s.label, s.host, s.port, s.tls),
                        style,
                    )]))
                })
                .collect()
        };
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Servers — ↑/↓ · Enter edit · a add · d delete · Space enable/disable · m admins"),
        );
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
                        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
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
                let cursor = if focused && field.cycle.is_none() { "_" } else { "" };
                let label_style = if focused {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<34}", field.label), label_style),
                    Span::raw(format!("{shown}{cursor}")),
                ]))
            })
            .collect();
        let list = List::new(items).block(Block::default().borders(Borders::ALL).title(title.to_string()));
        f.render_widget(list, area);
    }

    fn render_logs(&self, f: &mut Frame, area: Rect) {
        let filtered: Vec<&LogEvent> =
            self.logs.iter().filter(|e| self.filter.is_none_or(|c| e.category == c)).collect();
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
                    Span::styled(format!("{:<8}", cat_label(e.category)), Style::default().fg(color)),
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

fn cat_label(c: Category) -> &'static str {
    match c {
        Category::Error => "ERROR",
        Category::Debug => "DEBUG",
        Category::Message => "MSG",
        Category::Command => "CMD",
    }
}
