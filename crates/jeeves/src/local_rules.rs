//! Operator-local message-dispatch rules.
//!
//! This file intentionally contains no game-specific behavior. The repository can carry the
//! parser and dispatch hook while an operator keeps the actual identities and rooms in a file
//! outside the checkout.

use crate::log_bus::LogBus;
use anyhow::{anyhow, Context, Result};
use jeeves_abi::{Event, EventEnvelope};
use std::path::Path;

#[derive(Clone, Default)]
pub(crate) struct LocalRules {
    rules: Vec<LocalRule>,
}

#[derive(Clone)]
struct LocalRule {
    server: String,
    channel: String,
    profile_id: String,
    modules: Vec<String>,
    drop_percent: u8,
}

impl LocalRules {
    pub(crate) fn load(path: Option<&Path>, log: &LogBus) -> Self {
        let Some(path) = path else {
            return Self::default();
        };
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(error) => {
                log.error(
                    "local-rules",
                    format!(
                        "cannot read {} ({error}); local rules disabled",
                        path.display()
                    ),
                );
                return Self::default();
            }
        };
        match Self::from_text(&text) {
            Ok(rules) => rules,
            Err(error) => {
                log.error(
                    "local-rules",
                    format!("invalid {} ({error}); local rules disabled", path.display()),
                );
                Self::default()
            }
        }
    }

    pub(crate) fn from_text(text: &str) -> Result<Self> {
        let document = text
            .parse::<toml_edit::DocumentMut>()
            .context("parsing TOML")?;
        let Some(tables) = document
            .get("rules")
            .and_then(toml_edit::Item::as_array_of_tables)
        else {
            return Ok(Self::default());
        };

        let mut rules = Vec::new();
        for (index, table) in tables.iter().enumerate() {
            let field = |name: &str| {
                table
                    .get(name)
                    .and_then(toml_edit::Item::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
                    .ok_or_else(|| anyhow!("rules[{index}].{name} must be a non-empty string"))
            };
            let modules = table
                .get("modules")
                .and_then(toml_edit::Item::as_array)
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|value| value.as_str().map(str::trim))
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .filter(|modules| !modules.is_empty())
                .ok_or_else(|| anyhow!("rules[{index}].modules must contain a module name"))?;
            let drop_percent = table
                .get("drop_percent")
                .and_then(toml_edit::Item::as_integer)
                .ok_or_else(|| anyhow!("rules[{index}].drop_percent must be an integer"))?;
            let drop_percent = u8::try_from(drop_percent)
                .ok()
                .filter(|value| *value <= 100)
                .ok_or_else(|| anyhow!("rules[{index}].drop_percent must be 0..=100"))?;

            rules.push(LocalRule {
                server: field("server")?,
                channel: field("channel")?,
                profile_id: field("profile_id")?,
                modules,
                drop_percent,
            });
        }
        Ok(Self { rules })
    }

    pub(crate) fn should_drop(&self, env: &EventEnvelope, module: &str) -> bool {
        let Event::Message(message) = &env.event else {
            return false;
        };
        if message.is_private || message.user_id.is_empty() {
            return false;
        }
        self.rules.iter().any(|rule| {
            rule.server == env.server
                && rule.channel.eq_ignore_ascii_case(&message.target)
                && rule.profile_id == message.user_id
                && rule.modules.iter().any(|name| name == module)
                && random_percent() < rule.drop_percent
        })
    }
}

fn random_percent() -> u8 {
    let mut byte = [0u8; 1];
    if ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut byte).is_err() {
        return 0;
    }
    byte[0] % 100
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::MessagePayload;

    fn envelope(server: &str, channel: &str, profile_id: &str) -> EventEnvelope {
        EventEnvelope {
            server: server.into(),
            event: Event::Message(MessagePayload {
                user_id: profile_id.into(),
                nick: "tester".into(),
                display: "tester".into(),
                target: channel.into(),
                text: "!wordle".into(),
                is_private: false,
                user: "user".into(),
                host: "host".into(),
                tags: Vec::new(),
                role: None,
            }),
        }
    }

    #[test]
    fn parses_rules_without_embedding_target_data_in_code() {
        let rules = LocalRules::from_text(
            r##"
                [[rules]]
                server = "vesper"
                channel = "#main"
                profile_id = "profile-a"
                modules = ["wordle", "darts"]
                drop_percent = 100
            "##,
        )
        .unwrap();
        assert_eq!(rules.rules.len(), 1);
        assert!(rules.should_drop(&envelope("vesper", "#MAIN", "profile-a"), "wordle"));
        assert!(rules.should_drop(&envelope("vesper", "#main", "profile-a"), "darts"));
        assert!(!rules.should_drop(&envelope("vesper", "#games", "profile-a"), "wordle"));
        assert!(!rules.should_drop(&envelope("vesper", "#main", "profile-b"), "wordle"));
    }

    #[test]
    fn zero_percent_never_drops() {
        let rules = LocalRules::from_text(
            r##"
                [[rules]]
                server = "vesper"
                channel = "#main"
                profile_id = "profile-a"
                modules = ["wordle"]
                drop_percent = 0
            "##,
        )
        .unwrap();
        assert!(!rules.should_drop(&envelope("vesper", "#main", "profile-a"), "wordle"));
    }
}
