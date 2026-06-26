//! Interactive TUI (ratatui + crossterm). Two screens: Settings (edit + save config to SQLite)
//! and Logs (scrollable, filterable by category).
//!
//! The TUI runs on a blocking thread. It drains [`LogEvent`]s from a std channel (fed by a task
//! subscribed to the log bus) and sends [`AppRequest`]s back to the async runtime supervisor.

use crate::action::AppRequest;
use crate::config::ServerConfig;
use crate::log_bus::LogEvent;
use anyhow::Result;
use jeeves_abi::Category;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::sync::mpsc;

/// Entry point: set up the terminal, run the app loop, restore on exit.
pub fn run(
    initial: ServerConfig,
    logs_rx: Receiver<LogEvent>,
    app_tx: mpsc::Sender<AppRequest>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new(initial);
    let result = app.run(&mut terminal, logs_rx, &app_tx);
    ratatui::restore();
    // Best-effort shutdown signal to the runtime.
    let _ = app_tx.blocking_send(AppRequest::Shutdown);
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Screen {
    Settings,
    Logs,
}

/// One editable settings field.
struct Field {
    label: &'static str,
    value: String,
    secret: bool,
    boolean: bool,
}

impl Field {
    fn text(label: &'static str, value: String) -> Self {
        Field { label, value, secret: false, boolean: false }
    }
    fn secret(label: &'static str, value: String) -> Self {
        Field { label, value, secret: true, boolean: false }
    }
    fn boolean(label: &'static str, on: bool) -> Self {
        Field { label, value: if on { "true".into() } else { "false".into() }, secret: false, boolean: true }
    }
    fn is_on(&self) -> bool {
        self.value == "true"
    }
}

struct App {
    screen: Screen,
    fields: Vec<Field>,
    focus: usize,
    status: String,
    logs: Vec<LogEvent>,
    filter: Option<Category>,
    scroll: usize,
    follow: bool,
    // Identity of the server profile being edited (not yet editable in the single-server form;
    // carried through so saves update the right row).
    srv_id: i64,
    srv_label: String,
    srv_enabled: bool,
}

const F_HOST: usize = 0;
const F_PORT: usize = 1;
const F_TLS: usize = 2;
const F_ACCEPT: usize = 3;
const F_NICK: usize = 4;
const F_USER: usize = 5;
const F_REAL: usize = 6;
const F_SASL_ACCT: usize = 7;
const F_SASL_PASS: usize = 8;
const F_NICKPASS: usize = 9;
const F_CHANNELS: usize = 10;

impl App {
    fn new(cfg: ServerConfig) -> Self {
        let channels = cfg
            .channels
            .iter()
            .map(|(n, k)| match k {
                Some(k) => format!("{n} {k}"),
                None => n.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");

        let fields = vec![
            Field::text("Server host", cfg.host),
            Field::text("Port", cfg.port.to_string()),
            Field::boolean("Use TLS", cfg.tls),
            Field::boolean("Accept invalid TLS cert (testing)", cfg.accept_invalid_certs),
            Field::text("Nick", cfg.nick),
            Field::text("Username", cfg.username),
            Field::text("Realname", cfg.realname),
            Field::text("SASL account", cfg.sasl_account.unwrap_or_default()),
            Field::secret("SASL password", cfg.sasl_password.unwrap_or_default()),
            Field::secret("NickServ password (fallback)", cfg.nick_password.unwrap_or_default()),
            Field::text("Channels (comma-sep, '#chan key')", channels),
        ];

        App {
            screen: Screen::Settings,
            fields,
            focus: 0,
            status: "F1 Settings · F2 Logs · Ctrl-S save · Ctrl-R save+connect · Ctrl-Q quit".into(),
            logs: Vec::new(),
            filter: None,
            scroll: 0,
            follow: true,
            srv_id: cfg.id,
            srv_label: cfg.label,
            srv_enabled: cfg.enabled,
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
                        KeyCode::F(1) => self.screen = Screen::Settings,
                        KeyCode::F(2) => self.screen = Screen::Logs,
                        KeyCode::Char('s') if ctrl => self.save(app_tx, false),
                        KeyCode::Char('r') if ctrl => self.save(app_tx, true),
                        _ => match self.screen {
                            Screen::Settings => self.settings_key(key.code),
                            Screen::Logs => self.logs_key(key.code),
                        },
                    }
                }
            }
        }
    }

    fn settings_key(&mut self, code: KeyCode) {
        let field = &mut self.fields[self.focus];
        match code {
            KeyCode::Up => self.focus = self.focus.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                self.focus = (self.focus + 1).min(self.fields.len() - 1)
            }
            KeyCode::Char(' ') if field.boolean => {
                field.value = if field.is_on() { "false".into() } else { "true".into() };
            }
            KeyCode::Char(c) if !field.boolean => field.value.push(c),
            KeyCode::Backspace if !field.boolean => {
                field.value.pop();
            }
            _ => {}
        }
    }

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
            KeyCode::Down => {
                self.scroll += 1;
            }
            KeyCode::PageUp => {
                self.follow = false;
                self.scroll = self.scroll.saturating_sub(10);
            }
            KeyCode::PageDown => self.scroll += 10,
            KeyCode::End => self.follow = true,
            _ => {}
        }
    }

    fn save(&mut self, app_tx: &mpsc::Sender<AppRequest>, reconnect: bool) {
        let cfg = self.collect_config();
        let _ = app_tx.blocking_send(AppRequest::SaveConfig(Box::new(cfg)));
        if reconnect {
            let _ = app_tx.blocking_send(AppRequest::Reconnect);
            self.status = "saved + reconnecting…".into();
        } else {
            self.status = "saved to database".into();
        }
    }

    fn collect_config(&self) -> ServerConfig {
        let get = |i: usize| self.fields[i].value.trim().to_string();
        let opt = |i: usize| {
            let v = self.fields[i].value.trim();
            if v.is_empty() { None } else { Some(v.to_string()) }
        };
        let channels = self.fields[F_CHANNELS]
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

        ServerConfig {
            id: self.srv_id,
            label: self.srv_label.clone(),
            enabled: self.srv_enabled,
            host: get(F_HOST),
            port: get(F_PORT).parse().unwrap_or(6697),
            tls: self.fields[F_TLS].is_on(),
            nick: get(F_NICK),
            username: get(F_USER),
            realname: get(F_REAL),
            accept_invalid_certs: self.fields[F_ACCEPT].is_on(),
            sasl_account: opt(F_SASL_ACCT),
            sasl_password: opt(F_SASL_PASS),
            nick_password: opt(F_NICKPASS),
            channels,
        }
    }

    fn render(&self, f: &mut Frame) {
        let chunks = Layout::vertical([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

        let titles = vec!["Settings (F1)", "Logs (F2)"];
        let selected = match self.screen {
            Screen::Settings => 0,
            Screen::Logs => 1,
        };
        let tabs = Tabs::new(titles)
            .select(selected)
            .block(Block::default().borders(Borders::ALL).title("rustjeeves"))
            .highlight_style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD));
        f.render_widget(tabs, chunks[0]);

        match self.screen {
            Screen::Settings => self.render_settings(f, chunks[1]),
            Screen::Logs => self.render_logs(f, chunks[1]),
        }

        let status = Paragraph::new(self.status.clone()).style(Style::default().fg(Color::DarkGray));
        f.render_widget(status, chunks[2]);
    }

    fn render_settings(&self, f: &mut Frame, area: Rect) {
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
                let cursor = if focused && !field.boolean { "_" } else { "" };
                let label_style = if focused {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let line = Line::from(vec![
                    Span::styled(format!("{:<34}", field.label), label_style),
                    Span::raw(format!("{shown}{cursor}")),
                ]);
                ListItem::new(line)
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("Settings — ↑/↓ move · type to edit · Space toggles TLS"),
        );
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
        let title = format!(
            "Logs [{filter_label}] — 0 all · 1 err · 2 dbg · 3 msg · 4 cmd · ↑/↓ scroll · End follow"
        );
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
