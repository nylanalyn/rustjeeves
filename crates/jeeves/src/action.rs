//! Actions that flow *into* a connection actor (IRC or Fluxer), plus control actions handled by
//! the runtime.

/// Things a connection actor can do to its connection. Submitted over an mpsc channel; the actor
/// is the only owner of its client. Transport-specific variants (IRC moderation) are ignored with
/// a log line by transports that have no equivalent.
#[derive(Debug, Clone)]
pub enum TransportAction {
    Privmsg {
        target: String,
        text: String,
    },
    Notice {
        target: String,
        text: String,
    },
    Join(String),
    Part(String),
    ChannelMode {
        channel: String,
        mode: ChannelMode,
        adding: bool,
        target: String,
    },
    Kick {
        channel: String,
        nick: String,
        reason: String,
    },
    Topic {
        channel: String,
        topic: String,
    },
    /// Graceful QUIT used by refresh and shutdown.
    Quit(Option<String>),
}

/// The limited set of channel mode changes the operator host function can issue.
#[derive(Debug, Clone, Copy)]
pub enum ChannelMode {
    Ban,
    Op,
    Halfop,
    Voice,
}

/// Runtime-level control requests (from modules via host functions, or the TUI).
#[derive(Debug, Clone)]
pub enum Control {
    /// Re-scan the modules directory and reload plugins.
    Reload,
    /// Re-read configuration / re-apply state.
    Refresh,
    /// Cleanly shut the bot down.
    Shutdown,
}

/// Requests the TUI sends to the runtime supervisor. (The TUI persists config to SQLite directly
/// via the DB actor's blocking API; these are control signals.)
#[derive(Debug, Clone)]
pub enum AppRequest {
    /// Apply the saved config: (re)connect all enabled networks.
    Reconnect,
    /// Quit the application.
    Shutdown,
}
