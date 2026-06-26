//! Shared ABI types crossing the host <-> guest (WASM module) boundary.
//!
//! Everything is exchanged as JSON. The host serializes [`Event`] and passes it to a module's
//! `on_message` / `on_event` export; modules call host functions with the request structs below.
//! This crate is the single source of truth for that contract — both `jeeves` (host) and every
//! module depend on it.

use serde::{Deserialize, Serialize};

/// An event delivered from the host to a module.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    /// Successfully connected and registered with the server.
    Connected,
    /// Disconnected from the server.
    Disconnected,
    /// The bot joined `channel`.
    Joined { channel: String },
    /// The bot parted `channel`.
    Parted { channel: String },
    /// A PRIVMSG addressed to a channel or directly to the bot.
    Message(MessagePayload),
    /// Any other raw IRC command the host chose to forward.
    Raw { command: String, args: Vec<String> },
}

/// A channel or private message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagePayload {
    /// Nick of the sender (best-effort; empty if unknown).
    pub nick: String,
    /// Where the message was sent — a channel (`#foo`) or the bot's nick for a PM.
    pub target: String,
    /// The message text.
    pub text: String,
    /// True if this was a private message to the bot rather than a channel message.
    pub is_private: bool,
    /// IRCv3 message tags, if any.
    #[serde(default)]
    pub tags: Vec<(String, Option<String>)>,
}

// ---- Host function request payloads (guest -> host) ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessage {
    pub target: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendNotice {
    pub target: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub channel: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvGet {
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KvSet {
    pub key: String,
    pub value: String,
}

/// Log severity. Maps to the TUI/stdout log levels.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Level {
    Error,
    Info,
    Debug,
}

/// Log category used for filtering in the TUI logs screen.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Category {
    Error,
    Debug,
    Message,
    Command,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogReq {
    pub level: Level,
    pub category: Category,
    pub message: String,
}
