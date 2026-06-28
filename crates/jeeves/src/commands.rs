//! Loaded-command registry and operator-defined aliases.

use anyhow::{anyhow, bail, Result};
use jeeves_abi::{CommandSpec, Event, EventEnvelope};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub type CommandId = (String, String);
pub type AliasOverrides = HashMap<CommandId, Vec<String>>;
pub type SharedCommandRegistry = Arc<Mutex<CommandRegistry>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredCommand {
    pub module: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    pub default_aliases: Vec<String>,
    pub aliases: Vec<String>,
    pub has_override: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandTarget {
    pub module: String,
    pub canonical: String,
}

#[derive(Default)]
pub struct CommandRegistry {
    specs: Vec<(String, CommandSpec)>,
    overrides: AliasOverrides,
    commands: Vec<RegisteredCommand>,
    lookup: HashMap<String, CommandTarget>,
}

impl CommandRegistry {
    pub fn shared() -> SharedCommandRegistry {
        Arc::new(Mutex::new(Self::default()))
    }

    /// Replace metadata from the currently loaded modules. Invalid/conflicting entries are
    /// omitted and returned as warnings for the module log.
    pub fn replace_specs(
        &mut self,
        specs: Vec<(String, CommandSpec)>,
        overrides: AliasOverrides,
    ) -> Vec<String> {
        self.specs = specs;
        self.overrides = overrides;
        self.rebuild()
    }

    pub fn snapshot(&self) -> Vec<RegisteredCommand> {
        self.commands.clone()
    }

    pub fn resolve(&self, token: &str) -> Option<CommandTarget> {
        let name = token.strip_prefix('!')?;
        self.lookup.get(&name.to_ascii_lowercase()).cloned()
    }

    pub fn validate_override(&self, module: &str, name: &str, aliases: &[String]) -> Result<()> {
        let canonical = name.to_ascii_lowercase();
        if !self
            .commands
            .iter()
            .any(|command| command.module == module && command.name == canonical)
        {
            bail!("command !{name} from module '{module}' is not loaded");
        }
        validate_alias_list(aliases)?;

        let mut occupied = HashMap::<String, String>::new();
        for command in &self.commands {
            occupied.insert(command.name.clone(), format!("!{}", command.name));
            if (command.module.as_str(), command.name.as_str()) != (module, canonical.as_str()) {
                for alias in &command.aliases {
                    occupied.insert(alias.clone(), format!("!{}", command.name));
                }
            }
        }
        for alias in aliases {
            let alias = normalize_name(alias)?;
            if alias == canonical {
                bail!("!{alias} is already the canonical command");
            }
            if let Some(owner) = occupied.get(&alias) {
                bail!("!{alias} is already used by {owner}");
            }
        }
        Ok(())
    }

    pub fn set_override(&mut self, module: &str, name: &str, aliases: Option<Vec<String>>) {
        let id = (module.to_string(), name.to_ascii_lowercase());
        match aliases {
            Some(aliases) => {
                self.overrides.insert(id, aliases);
            }
            None => {
                self.overrides.remove(&id);
            }
        }
        let _ = self.rebuild();
    }

    fn rebuild(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut commands = Vec::new();
        let mut canonical_owners = HashMap::<String, String>::new();

        for (module, spec) in &self.specs {
            let name = match normalize_name(&spec.name) {
                Ok(name) => name,
                Err(error) => {
                    warnings.push(format!(
                        "{module}: invalid command '{}': {error}",
                        spec.name
                    ));
                    continue;
                }
            };
            if let Some(owner) = canonical_owners.get(&name) {
                warnings.push(format!(
                    "{module}: command !{name} conflicts with module '{owner}'"
                ));
                continue;
            }
            canonical_owners.insert(name.clone(), module.clone());
            let id = (module.clone(), name.clone());
            let default_aliases = normalize_defaults(module, &name, &spec.aliases, &mut warnings);
            let (aliases, has_override) = match self.overrides.get(&id) {
                Some(aliases) => (aliases.clone(), true),
                None => (default_aliases.clone(), false),
            };
            commands.push(RegisteredCommand {
                module: module.clone(),
                name,
                description: spec.description.clone(),
                usage: spec.usage.clone(),
                default_aliases,
                aliases,
                has_override,
            });
        }

        commands.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.module.cmp(&right.module))
        });
        let mut lookup = HashMap::new();
        for command in &commands {
            lookup.insert(
                command.name.clone(),
                CommandTarget {
                    module: command.module.clone(),
                    canonical: command.name.clone(),
                },
            );
        }
        for command in &mut commands {
            let mut accepted = Vec::new();
            let mut seen = HashSet::new();
            for raw_alias in &command.aliases {
                let alias = match normalize_name(raw_alias) {
                    Ok(alias) => alias,
                    Err(error) => {
                        warnings.push(format!(
                            "{}: invalid alias '{}': {error}",
                            command.module, raw_alias
                        ));
                        continue;
                    }
                };
                if alias == command.name || !seen.insert(alias.clone()) {
                    continue;
                }
                if let Some(owner) = lookup.get(&alias) {
                    warnings.push(format!(
                        "{}: alias !{} conflicts with !{}",
                        command.module, alias, owner.canonical
                    ));
                    continue;
                }
                lookup.insert(
                    alias.clone(),
                    CommandTarget {
                        module: command.module.clone(),
                        canonical: command.name.clone(),
                    },
                );
                accepted.push(alias);
            }
            command.aliases = accepted;
        }
        self.commands = commands;
        self.lookup = lookup;
        warnings
    }
}

pub fn parse_alias_csv(value: &str) -> Result<Vec<String>> {
    if value.trim().is_empty() {
        return Ok(Vec::new());
    }
    let aliases = value
        .split(',')
        .map(str::trim)
        .map(normalize_name)
        .collect::<Result<Vec<_>>>()?;
    validate_alias_list(&aliases)?;
    Ok(aliases)
}

pub fn canonicalized_event(env: &EventEnvelope, canonical: &str) -> EventEnvelope {
    let mut rewritten = env.clone();
    let Event::Message(message) = &mut rewritten.event else {
        return rewritten;
    };
    let Some(start) = message
        .text
        .find(|character: char| !character.is_whitespace())
    else {
        return rewritten;
    };
    let end = message.text[start..]
        .find(char::is_whitespace)
        .map_or(message.text.len(), |offset| start + offset);
    message
        .text
        .replace_range(start..end, &format!("!{canonical}"));
    rewritten
}

fn normalize_defaults(
    module: &str,
    command: &str,
    aliases: &[String],
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for alias in aliases {
        match normalize_name(alias) {
            Ok(alias) if alias != command && seen.insert(alias.clone()) => out.push(alias),
            Ok(_) => {}
            Err(error) => warnings.push(format!("{module}: invalid alias '{alias}': {error}")),
        }
    }
    out
}

fn validate_alias_list(aliases: &[String]) -> Result<()> {
    let mut seen = HashSet::new();
    for alias in aliases {
        let normalized = normalize_name(alias)?;
        if !seen.insert(normalized.clone()) {
            bail!("duplicate alias !{normalized}");
        }
    }
    Ok(())
}

fn normalize_name(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("name cannot be empty");
    }
    if value.starts_with('!') {
        bail!("omit the leading !");
    }
    if value.len() > 32 {
        bail!("name is longer than 32 characters");
    }
    if !value
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(anyhow!("use only ASCII letters, digits, '-' or '_'"));
    }
    Ok(value.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(name: &str, aliases: &[&str]) -> CommandSpec {
        CommandSpec {
            name: name.into(),
            aliases: aliases.iter().map(|alias| (*alias).into()).collect(),
            description: String::new(),
            usage: String::new(),
        }
    }

    #[test]
    fn resolves_defaults_and_operator_overrides() {
        let mut registry = CommandRegistry::default();
        let mut overrides = AliasOverrides::new();
        overrides.insert(("weather".into(), "weather".into()), vec!["w".into()]);
        assert!(registry
            .replace_specs(
                vec![("weather".into(), spec("weather", &["weath"]))],
                overrides
            )
            .is_empty());
        assert_eq!(
            registry.resolve("!W"),
            Some(CommandTarget {
                module: "weather".into(),
                canonical: "weather".into()
            })
        );
        assert_eq!(registry.resolve("!weath"), None);
    }

    #[test]
    fn rejects_collisions_before_saving() {
        let mut registry = CommandRegistry::default();
        registry.replace_specs(
            vec![
                ("weather".into(), spec("weather", &[])),
                ("search".into(), spec("search", &["g"])),
            ],
            AliasOverrides::new(),
        );
        assert!(registry
            .validate_override("weather", "weather", &["g".into()])
            .unwrap_err()
            .to_string()
            .contains("already used"));
        assert!(registry
            .validate_override("weather", "weather", &["search".into()])
            .is_err());
    }

    #[test]
    fn parses_csv_and_rejects_prefixes() {
        assert_eq!(parse_alias_csv(" W, weath ").unwrap(), vec!["w", "weath"]);
        assert!(parse_alias_csv("!w").is_err());
        assert!(parse_alias_csv("w,w").is_err());
    }

    #[test]
    fn unloaded_module_override_returns_when_reinstalled() {
        let mut registry = CommandRegistry::default();
        let mut overrides = AliasOverrides::new();
        overrides.insert(
            ("weather".into(), "weather".into()),
            vec!["forecast".into()],
        );
        registry.replace_specs(
            vec![("weather".into(), spec("weather", &["w"]))],
            overrides.clone(),
        );
        assert!(registry.resolve("!forecast").is_some());
        registry.replace_specs(Vec::new(), overrides.clone());
        assert!(registry.snapshot().is_empty());
        registry.replace_specs(vec![("weather".into(), spec("weather", &["w"]))], overrides);
        assert!(registry.resolve("!forecast").is_some());
    }
}
