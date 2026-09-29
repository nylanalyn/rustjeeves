//! Loaded-command registry and operator-defined aliases.

use anyhow::{anyhow, bail, Result};
use jeeves_abi::{CommandShortcut, CommandSpec, Event, EventEnvelope};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

pub type CommandId = (String, String);
pub type AliasOverrides = HashMap<CommandId, Vec<String>>;
pub type SharedCommandRegistry = Arc<Mutex<CommandRegistry>>;

/// Global SQLite config key for the accepted command-prefix characters.
pub const PREFIXES_CONFIG: &str = "command_prefixes";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegisteredCommand {
    pub module: String,
    pub name: String,
    pub description: String,
    pub usage: String,
    /// Built-in names, including shortcut names; the alias editor edits this list.
    pub default_aliases: Vec<String>,
    /// Effective names after operator overrides, including shortcut names.
    pub aliases: Vec<String>,
    /// Built-in shortcut expansions keyed by name. A name in `aliases` that appears here is a
    /// shortcut into a subcommand rather than a plain alias.
    pub shortcut_expansions: BTreeMap<String, CommandShortcut>,
    pub has_override: bool,
}

impl RegisteredCommand {
    /// Effective plain aliases (names that stand for the command itself).
    pub fn plain_aliases(&self) -> Vec<String> {
        self.aliases
            .iter()
            .filter(|alias| !self.shortcut_expansions.contains_key(*alias))
            .cloned()
            .collect()
    }

    /// Effective shortcuts (names that expand to a subcommand).
    pub fn shortcuts(&self) -> Vec<CommandShortcut> {
        self.aliases
            .iter()
            .filter_map(|alias| self.shortcut_expansions.get(alias).cloned())
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandTarget {
    pub module: String,
    pub canonical: String,
    /// Subcommand words a shortcut expands to (`!yes` → `!fish yes` has `Some("yes")`).
    pub expansion: Option<String>,
}

pub struct CommandRegistry {
    specs: Vec<(String, CommandSpec)>,
    overrides: AliasOverrides,
    commands: Vec<RegisteredCommand>,
    lookup: HashMap<String, CommandTarget>,
    prefixes: Vec<char>,
}

impl Default for CommandRegistry {
    fn default() -> Self {
        Self {
            specs: Vec::new(),
            overrides: AliasOverrides::new(),
            commands: Vec::new(),
            lookup: HashMap::new(),
            prefixes: vec!['!'],
        }
    }
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

    /// Render the live command catalog for trusted AI command-help context. Metadata comes from
    /// loaded module manifests and includes effective operator-overridden aliases.
    pub fn ai_reference(&self) -> String {
        self.commands
            .iter()
            .map(|command| {
                let plain = command.plain_aliases();
                let mut aliases = if plain.is_empty() {
                    String::new()
                } else {
                    format!(
                        " (aliases: {})",
                        plain
                            .iter()
                            .map(|alias| format!("!{alias}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                let shortcuts = command.shortcuts();
                if !shortcuts.is_empty() {
                    aliases.push_str(&format!(
                        " (shortcuts: {})",
                        shortcuts
                            .iter()
                            .map(|shortcut| format!(
                                "!{} = !{} {}",
                                shortcut.name, command.name, shortcut.expands_to
                            ))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                format!(
                    "{}: {}{} — {}",
                    command.module, command.usage, aliases, command.description
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn prefixes(&self) -> String {
        self.prefixes.iter().collect()
    }

    pub fn set_prefixes(&mut self, value: &str) -> Result<()> {
        self.prefixes = parse_prefixes(value)?;
        Ok(())
    }

    /// One line per named command, for the AI's list of read-only tools: "!weather <usage> — desc".
    pub fn tool_reference(&self, names: &[String]) -> String {
        names
            .iter()
            .take(16)
            .filter_map(|name| {
                let name = name.to_ascii_lowercase();
                self.commands
                    .iter()
                    .find(|command| command.name == name)
                    .map(|command| {
                        let line = format!("{} — {}", command.usage, command.description);
                        line.chars().take(300).collect::<String>()
                    })
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn resolve(&self, token: &str) -> Option<CommandTarget> {
        let prefix = token.chars().next()?;
        if !self.prefixes.contains(&prefix) {
            return None;
        }
        let name = &token[prefix.len_utf8()..];
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
            let shortcut_expansions =
                normalize_shortcuts(module, &name, &spec.shortcuts, &mut warnings);
            let default_names = spec
                .aliases
                .iter()
                .cloned()
                .chain(shortcut_expansions.keys().cloned())
                .collect::<Vec<_>>();
            let default_aliases = normalize_defaults(module, &name, &default_names, &mut warnings);
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
                shortcut_expansions,
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
                    expansion: None,
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
                        expansion: command
                            .shortcut_expansions
                            .get(&alias)
                            .map(|shortcut| shortcut.expands_to.clone()),
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

/// Parse the compact prefix editor value. Each character is an accepted prefix, so `!.,`
/// permits all three common IRC command styles.
pub fn parse_prefixes(value: &str) -> Result<Vec<char>> {
    let mut prefixes = Vec::new();
    for prefix in value.trim().chars() {
        if !prefix.is_ascii() || !prefix.is_ascii_graphic() || prefix.is_ascii_alphanumeric() {
            bail!("use one or more ASCII punctuation characters (for example !.,)");
        }
        if !prefixes.contains(&prefix) {
            prefixes.push(prefix);
        }
    }
    if prefixes.is_empty() {
        bail!("at least one command prefix is required");
    }
    Ok(prefixes)
}

/// Rewrite the leading command token to `!{canonical}` (plus a shortcut's subcommand words) for
/// the owning module, so modules only ever match their canonical names.
pub fn canonicalized_event(env: &EventEnvelope, target: &CommandTarget) -> EventEnvelope {
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
    let replacement = match target.expansion.as_deref() {
        Some(expansion) => format!("!{} {expansion}", target.canonical),
        None => format!("!{}", target.canonical),
    };
    message.text.replace_range(start..end, &replacement);
    rewritten
}

fn normalize_shortcuts(
    module: &str,
    command: &str,
    shortcuts: &[CommandShortcut],
    warnings: &mut Vec<String>,
) -> BTreeMap<String, CommandShortcut> {
    let mut out = BTreeMap::new();
    for shortcut in shortcuts {
        let name = match normalize_name(&shortcut.name) {
            Ok(name) if name != command => name,
            Ok(_) => continue,
            Err(error) => {
                warnings.push(format!(
                    "{module}: invalid shortcut '{}': {error}",
                    shortcut.name
                ));
                continue;
            }
        };
        let expansion = shortcut
            .expands_to
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if expansion.is_empty() || expansion.len() > 64 || expansion.chars().any(char::is_control) {
            warnings.push(format!(
                "{module}: shortcut !{name} needs a short, single-line expansion"
            ));
            continue;
        }
        let bounded = |text: &str, max: usize| {
            text.chars()
                .filter(|character| !character.is_control())
                .take(max)
                .collect::<String>()
        };
        out.entry(name.clone()).or_insert(CommandShortcut {
            name,
            expands_to: expansion,
            description: bounded(&shortcut.description, 300),
            usage: bounded(&shortcut.usage, 200),
        });
    }
    out
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
            ..Default::default()
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
                canonical: "weather".into(),
                expansion: None,
            })
        );
        assert_eq!(registry.resolve("!weath"), None);
        let reference = registry.ai_reference();
        assert!(reference.contains("aliases: !w"));
        assert!(!reference.contains("!weath"));
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

    fn shortcut_spec() -> CommandSpec {
        CommandSpec {
            name: "fish".into(),
            aliases: vec!["fishing".into()],
            shortcuts: vec![
                CommandShortcut::new("yes", "yes"),
                CommandShortcut::new("heal", "heal"),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn shortcuts_expand_into_subcommands() {
        let mut registry = CommandRegistry::default();
        assert!(registry
            .replace_specs(
                vec![("fishing".into(), shortcut_spec())],
                AliasOverrides::new()
            )
            .is_empty());
        let target = registry.resolve("!YES").unwrap();
        assert_eq!(target.canonical, "fish");
        assert_eq!(target.expansion.as_deref(), Some("yes"));
        assert_eq!(registry.resolve("!fishing").unwrap().expansion, None);

        let env = EventEnvelope {
            server: "net".into(),
            event: Event::Message(jeeves_abi::MessagePayload {
                user_id: String::new(),
                nick: "alice".into(),
                display: String::new(),
                user: String::new(),
                host: String::new(),
                target: "#c".into(),
                text: "  !Yes please".into(),
                is_private: false,
                tags: Vec::new(),
                role: None,
                honorific: String::new(),
                is_action: false,
            }),
        };
        let Event::Message(message) = canonicalized_event(&env, &target).event else {
            unreachable!()
        };
        assert_eq!(message.text, "  !fish yes please");

        let command = &registry.snapshot()[0];
        assert_eq!(command.plain_aliases(), vec!["fishing".to_string()]);
        assert_eq!(command.shortcuts().len(), 2);
        assert!(registry.ai_reference().contains("!yes = !fish yes"));
    }

    #[test]
    fn removing_a_shortcut_frees_the_name() {
        let mut registry = CommandRegistry::default();
        let mut overrides = AliasOverrides::new();
        overrides.insert(("fishing".into(), "fish".into()), vec!["heal".into()]);
        registry.replace_specs(
            vec![
                ("fishing".into(), shortcut_spec()),
                (
                    "other".into(),
                    CommandSpec {
                        name: "vote".into(),
                        aliases: vec!["yes".into()],
                        ..Default::default()
                    },
                ),
            ],
            overrides,
        );
        assert_eq!(registry.resolve("!yes").unwrap().module, "other");
        assert_eq!(
            registry.resolve("!heal").unwrap().expansion.as_deref(),
            Some("heal")
        );
    }

    #[test]
    fn resolves_each_configured_prefix() {
        let mut registry = CommandRegistry::default();
        registry.replace_specs(
            vec![("weather".into(), spec("weather", &[]))],
            AliasOverrides::new(),
        );
        registry.set_prefixes("!.,").unwrap();
        assert!(registry.resolve("!weather").is_some());
        assert!(registry.resolve(".weather").is_some());
        assert!(registry.resolve(",weather").is_some());
        assert!(registry.resolve("?weather").is_none());
    }

    #[test]
    fn rejects_invalid_prefixes() {
        assert!(parse_prefixes("").is_err());
        assert!(parse_prefixes("a").is_err());
        assert!(parse_prefixes("! a").is_err());
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
