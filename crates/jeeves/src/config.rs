//! In-memory representation of the bot's configuration, as loaded from / saved to SQLite.

/// Everything needed to connect to one IRC network.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Database row id (0 = not yet persisted / new).
    pub id: i64,
    /// Unique human-friendly network label (e.g. "libera"). Used to tag events and target sends.
    pub label: String,
    /// Whether this profile should be connected at startup.
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub nick: String,
    pub username: String,
    pub realname: String,
    /// SASL PLAIN account name. If `Some` together with `sasl_password`, SASL is attempted.
    pub sasl_account: Option<String>,
    pub sasl_password: Option<String>,
    /// NickServ password for the message-based fallback (`/msg NickServ IDENTIFY`).
    /// Used when SASL is not configured.
    pub nick_password: Option<String>,
    /// Channels to join: (name, optional key).
    pub channels: Vec<(String, Option<String>)>,
    /// Accept invalid/self-signed TLS certificates. For local testing only — leave off in
    /// production.
    pub accept_invalid_certs: bool,
}

impl ServerConfig {
    /// True when SASL credentials are present.
    pub fn sasl_enabled(&self) -> bool {
        self.sasl_account.is_some() && self.sasl_password.is_some()
    }

    /// A blank default used on first run, before the user configures anything in the TUI.
    pub fn placeholder() -> Self {
        ServerConfig {
            id: 0,
            label: "default".into(),
            enabled: true,
            host: String::new(),
            port: 6697,
            tls: true,
            nick: "jeeves".into(),
            username: "jeeves".into(),
            realname: "rustjeeves".into(),
            sasl_account: None,
            sasl_password: None,
            nick_password: None,
            channels: Vec::new(),
            accept_invalid_certs: false,
        }
    }
}
