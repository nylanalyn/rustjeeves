//! Actions that flow *into* the IRC actor, and control actions handled by the runtime.

/// Things the IRC actor can do to the connection. Submitted over an mpsc channel; the actor is the
/// only owner of the `irc` client.
#[derive(Debug, Clone)]
pub enum IrcAction {
    Privmsg { target: String, text: String },
    Notice { target: String, text: String },
    Join(String),
    Part(String),
    /// Graceful QUIT. Not yet emitted by any caller; reserved for clean-shutdown wiring.
    #[allow(dead_code)]
    Quit(Option<String>),
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
