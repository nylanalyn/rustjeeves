//! Structured log bus. A single broadcast channel carries [`LogEvent`]s to any number of
//! subscribers — the TUI logs screen in interactive mode, and a stdout/DB sink always.

use jeeves_abi::{Category, Level};
use tokio::sync::broadcast;

/// One structured log line.
#[derive(Debug, Clone)]
pub struct LogEvent {
    /// Unix timestamp (seconds).
    pub ts: i64,
    pub level: Level,
    pub category: Category,
    /// Where it came from, e.g. "irc", "tui", a module name.
    pub source: String,
    pub message: String,
}

/// Cloneable handle used to publish logs and hand out subscriptions.
#[derive(Clone)]
pub struct LogBus {
    tx: broadcast::Sender<LogEvent>,
}

impl LogBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _rx) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Subscribe to all future log events.
    pub fn subscribe(&self) -> broadcast::Receiver<LogEvent> {
        self.tx.subscribe()
    }

    /// Publish a log event. A send error only means there are no subscribers yet — harmless.
    pub fn log(&self, level: Level, category: Category, source: impl Into<String>, message: impl Into<String>) {
        let ev = LogEvent {
            ts: now_secs(),
            level,
            category,
            source: source.into(),
            message: message.into(),
        };
        let _ = self.tx.send(ev);
    }

    pub fn info(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(Level::Info, Category::Debug, source, message);
    }

    pub fn debug(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(Level::Debug, Category::Debug, source, message);
    }

    pub fn error(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(Level::Error, Category::Error, source, message);
    }

    pub fn message(&self, source: impl Into<String>, message: impl Into<String>) {
        self.log(Level::Info, Category::Message, source, message);
    }
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
