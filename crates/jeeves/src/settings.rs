//! Loaded module-setting metadata, validation, and scoped default resolution.

use anyhow::{bail, Result};
use jeeves_abi::{SettingKind, SettingScope, SettingSpec};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub type SharedSettingRegistry = Arc<Mutex<SettingRegistry>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredSetting {
    pub module: String,
    pub spec: SettingSpec,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingOverride {
    pub module: String,
    pub key: String,
    pub scope: SettingScope,
    pub server: String,
    pub channel: String,
    pub value: String,
}

#[derive(Default)]
pub struct SettingRegistry {
    settings: Vec<RegisteredSetting>,
    lookup: HashMap<(String, String), SettingSpec>,
    overrides: HashMap<(String, String, SettingScope, String, String), String>,
}

impl SettingRegistry {
    pub fn shared() -> SharedSettingRegistry {
        Arc::new(Mutex::new(Self::default()))
    }

    pub fn replace_specs(&mut self, specs: Vec<(String, SettingSpec)>) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut settings = Vec::new();
        let mut lookup = HashMap::new();
        for (module, mut spec) in specs {
            spec.key = spec.key.trim().to_ascii_lowercase();
            if let Err(error) = validate_key(&spec.key) {
                warnings.push(format!("{module}: invalid setting '{}': {error}", spec.key));
                continue;
            }
            if spec.scopes.is_empty() {
                warnings.push(format!("{module}.{}: no supported scopes", spec.key));
                continue;
            }
            if spec.key == "enabled" && !matches!(spec.kind, SettingKind::Boolean) {
                warnings.push(format!(
                    "{module}.enabled: reserved setting must be boolean"
                ));
                continue;
            }
            let mut seen = HashSet::new();
            spec.scopes.retain(|scope| seen.insert(*scope));
            if let Err(error) = validate_value(&spec.kind, &spec.default) {
                warnings.push(format!(
                    "{module}.{}: invalid default '{}': {error}",
                    spec.key, spec.default
                ));
                continue;
            }
            let id = (module.clone(), spec.key.clone());
            if lookup.contains_key(&id) {
                warnings.push(format!("{module}: duplicate setting '{}'", spec.key));
                continue;
            }
            lookup.insert(id, spec.clone());
            settings.push(RegisteredSetting { module, spec });
        }
        settings.sort_by(|left, right| {
            left.module
                .cmp(&right.module)
                .then_with(|| left.spec.key.cmp(&right.spec.key))
        });
        self.settings = settings;
        self.lookup = lookup;
        warnings
    }

    pub fn snapshot(&self) -> Vec<RegisteredSetting> {
        self.settings.clone()
    }

    pub fn get(&self, module: &str, key: &str) -> Option<SettingSpec> {
        self.lookup
            .get(&(module.to_string(), key.trim().to_ascii_lowercase()))
            .cloned()
    }

    pub fn replace_overrides(&mut self, overrides: Vec<SettingOverride>) {
        self.overrides = overrides
            .into_iter()
            .map(|entry| {
                (
                    (
                        entry.module,
                        entry.key,
                        entry.scope,
                        entry.server,
                        entry.channel,
                    ),
                    entry.value,
                )
            })
            .collect();
    }

    pub fn set_override(
        &mut self,
        module: &str,
        key: &str,
        scope: SettingScope,
        server: &str,
        channel: &str,
        value: Option<String>,
    ) {
        let id = (
            module.to_string(),
            key.to_ascii_lowercase(),
            scope,
            server.to_string(),
            channel.to_string(),
        );
        match value {
            Some(value) => {
                self.overrides.insert(id, value);
            }
            None => {
                self.overrides.remove(&id);
            }
        }
    }

    pub fn effective(
        &self,
        module: &str,
        key: &str,
        server: Option<&str>,
        channel: Option<&str>,
    ) -> Option<String> {
        let spec = self.get(module, key)?;
        let server = server.unwrap_or("");
        let channel = channel.unwrap_or("");
        let candidates = [
            (SettingScope::Channel, server, channel),
            (SettingScope::Network, server, ""),
            (SettingScope::Global, "", ""),
        ];
        for (scope, scope_server, scope_channel) in candidates {
            if !spec.scopes.contains(&scope)
                || (scope != SettingScope::Global && scope_server.is_empty())
                || (scope == SettingScope::Channel && scope_channel.is_empty())
            {
                continue;
            }
            let id = (
                module.to_string(),
                spec.key.clone(),
                scope,
                scope_server.to_string(),
                scope_channel.to_string(),
            );
            if let Some(value) = self.overrides.get(&id) {
                if validate_value(&spec.kind, value).is_ok() {
                    return Some(value.clone());
                }
            }
        }
        Some(spec.default)
    }

    pub fn validate_override(
        &self,
        module: &str,
        key: &str,
        scope: SettingScope,
        server: &str,
        channel: &str,
        value: &str,
    ) -> Result<()> {
        let spec = self
            .get(module, key)
            .ok_or_else(|| anyhow::anyhow!("setting '{module}.{key}' is not loaded"))?;
        if !spec.scopes.contains(&scope) {
            bail!("{} scope is not supported", scope_name(scope));
        }
        validate_scope(scope, server, channel)?;
        validate_value(&spec.kind, value)
    }
}

pub fn validate_scope(scope: SettingScope, server: &str, channel: &str) -> Result<()> {
    match scope {
        SettingScope::Global => Ok(()),
        SettingScope::Network if server.trim().is_empty() => bail!("network label is required"),
        SettingScope::Channel if server.trim().is_empty() => bail!("network label is required"),
        SettingScope::Channel if channel.trim().is_empty() => bail!("channel is required"),
        _ => Ok(()),
    }
}

pub fn validate_value(kind: &SettingKind, value: &str) -> Result<()> {
    match kind {
        SettingKind::Boolean => match value {
            "true" | "false" => Ok(()),
            _ => bail!("expected true or false"),
        },
        SettingKind::Integer { min, max } | SettingKind::DurationSeconds { min, max } => {
            let parsed = value
                .parse::<i64>()
                .map_err(|_| anyhow::anyhow!("expected an integer"))?;
            if parsed < *min || parsed > *max {
                bail!("must be between {min} and {max}");
            }
            Ok(())
        }
        SettingKind::String { max_len } => {
            if value.chars().count() > *max_len {
                bail!("must be at most {max_len} characters");
            }
            Ok(())
        }
        SettingKind::Choice { options } => {
            if options.iter().any(|option| option == value) {
                Ok(())
            } else {
                bail!("expected one of: {}", options.join(", "));
            }
        }
    }
}

pub fn scope_name(scope: SettingScope) -> &'static str {
    match scope {
        SettingScope::Global => "global",
        SettingScope::Network => "network",
        SettingScope::Channel => "channel",
    }
}

fn validate_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > 64 {
        bail!("key must contain 1–64 characters");
    }
    if !key
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        bail!("use only ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> SettingSpec {
        SettingSpec {
            key: "retention_days".into(),
            description: String::new(),
            default: "30".into(),
            kind: SettingKind::Integer { min: 1, max: 365 },
            scopes: vec![
                SettingScope::Global,
                SettingScope::Network,
                SettingScope::Channel,
            ],
            applies_immediately: true,
        }
    }

    #[test]
    fn validates_specs_scopes_and_values() {
        let mut registry = SettingRegistry::default();
        assert!(registry
            .replace_specs(vec![("memos".into(), spec())])
            .is_empty());
        assert!(registry
            .validate_override(
                "memos",
                "retention_days",
                SettingScope::Channel,
                "net",
                "#room",
                "60"
            )
            .is_ok());
        assert!(registry
            .validate_override(
                "memos",
                "retention_days",
                SettingScope::Channel,
                "net",
                "",
                "60"
            )
            .is_err());

        registry.replace_overrides(vec![SettingOverride {
            module: "memos".into(),
            key: "retention_days".into(),
            scope: SettingScope::Global,
            server: String::new(),
            channel: String::new(),
            value: "45".into(),
        }]);
        registry.set_override(
            "memos",
            "retention_days",
            SettingScope::Network,
            "net",
            "",
            Some("10".into()),
        );
        registry.set_override(
            "memos",
            "retention_days",
            SettingScope::Channel,
            "net",
            "#room",
            Some("3".into()),
        );
        assert_eq!(
            registry.effective("memos", "retention_days", Some("net"), Some("#room")),
            Some("3".into())
        );
        assert_eq!(
            registry.effective("memos", "retention_days", Some("net"), Some("#other")),
            Some("10".into())
        );
        assert_eq!(
            registry.effective("memos", "retention_days", Some("other"), None),
            Some("45".into())
        );
        assert!(registry
            .validate_override("memos", "retention_days", SettingScope::Global, "", "", "0")
            .is_err());
    }
}
