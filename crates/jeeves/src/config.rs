//! In-memory representation of the bot's configuration, as loaded from / saved to SQLite.

use jeeves_abi::Role;

/// A configured admin/super-admin for one network. Identity is verified by the services account
/// when present, otherwise by a hostmask bound on first use ("introduction" / trust-on-first-use).
#[derive(Debug, Clone)]
pub struct AdminEntry {
    pub nick: String,
    pub role: Role,
    /// Explicitly required services account, if the operator pinned one.
    pub account: Option<String>,
    /// Hostmask bound on first contact (when no account was available). Surfaced in the TUI.
    pub bound_hostmask: Option<String>,
    /// Services account bound on first contact (preferred over hostmask). Surfaced in the TUI.
    pub bound_account: Option<String>,
}

impl AdminEntry {
    /// Why this entry's identity check is weaker than it should be, if it is. An entry with no
    /// pinned account binds to whoever first speaks as that nick (trust on first use), and a
    /// hostmask binding breaks or can be spoofed where an account cannot.
    pub fn identity_warning(&self) -> Option<&'static str> {
        let pinned = self
            .account
            .as_deref()
            .is_some_and(|a| !a.trim().is_empty());
        if pinned || self.bound_account.is_some() {
            None
        } else if self.bound_hostmask.is_some() {
            Some("bound to a hostmask only; pin a services account")
        } else {
            Some("unclaimed: the next person to speak as this nick becomes admin; pin an account")
        }
    }
}

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
    /// User modes to set on ourselves after connecting, e.g. `+B` (bot flag). Applied
    /// automatically on end-of-MOTD.
    pub umodes: Option<String>,
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
            umodes: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admin(account: Option<&str>, host: Option<&str>, bound: Option<&str>) -> AdminEntry {
        AdminEntry {
            nick: "paul".into(),
            role: Role::Admin,
            account: account.map(str::to_string),
            bound_hostmask: host.map(str::to_string),
            bound_account: bound.map(str::to_string),
        }
    }

    #[test]
    fn only_account_backed_admins_are_free_of_warnings() {
        assert!(admin(Some("paul"), None, None).identity_warning().is_none());
        assert!(admin(None, None, Some("paul")).identity_warning().is_none());
        assert!(admin(None, Some("paul!u@h"), None)
            .identity_warning()
            .is_some_and(|w| w.contains("hostmask")));
        assert!(admin(None, None, None)
            .identity_warning()
            .is_some_and(|w| w.contains("unclaimed")));
        assert!(admin(Some("  "), None, None).identity_warning().is_some());
    }
}
