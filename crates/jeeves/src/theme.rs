//! Configurable personality ("themes").
//!
//! All user-facing text the bot posts comes from a human-editable `theme.toml`, one `[section]`
//! per module. Modules ask the host for a string via the `theme` host function, passing a default;
//! the default is written to the file on first use (lazy registration). Values may be a single
//! string or a list (one is chosen at random), and `{var}` placeholders are substituted.
//!
//! Edits to `theme.toml` apply live: the parsed document is cached and reloaded when the file's
//! mtime changes. `toml_edit` is used so writing new defaults preserves the user's edits/comments.
//!
//! Default upgrades: a bot-owned sidecar (`theme.seeded.toml` next to the theme file) records the
//! exact value the bot last wrote for each key. When a module ships improved default copy, keys
//! whose value still equals that record — i.e. the operator never edited them — are upgraded in
//! place. Edited keys are never touched. Keys seeded before the sidecar existed are adopted the
//! first time their value matches the module's current default.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use toml_edit::DocumentMut;

/// Shared, mutable handle to the theme store (one file shared across all modules).
pub type ThemeHandle = Arc<Mutex<ThemeStore>>;

pub struct ThemeStore {
    path: PathBuf,
    doc: DocumentMut,
    /// Bot-owned record of the defaults it wrote (see the module docs).
    seeded_path: PathBuf,
    seeded: DocumentMut,
    /// Keys upgraded during this run. A module that passes different defaults for one key
    /// would otherwise rewrite the file on every call, so each key upgrades at most once.
    upgraded: HashSet<(String, String)>,
    mtime: Option<SystemTime>,
    /// False when the on-disk file could not be parsed. We keep serving defaults but never
    /// overwrite that file; a later valid edit is picked up by the mtime reload.
    writable: bool,
}

impl ThemeStore {
    /// Open the theme file (empty document if it doesn't exist yet), returning a shared handle.
    pub fn open(path: impl Into<PathBuf>) -> ThemeHandle {
        let path = path.into();
        let (doc, mtime, writable) = read_doc(&path);
        let seeded_path = seeded_path_for(&path);
        let seeded = std::fs::read_to_string(&seeded_path)
            .ok()
            .and_then(|text| text.parse::<DocumentMut>().ok())
            .unwrap_or_default();
        Arc::new(Mutex::new(ThemeStore {
            path,
            doc,
            seeded_path,
            seeded,
            upgraded: HashSet::new(),
            mtime,
            writable,
        }))
    }

    /// Re-read the file if it changed on disk since we last loaded it.
    fn reload_if_changed(&mut self) {
        let disk = file_mtime(&self.path);
        if disk != self.mtime {
            let (doc, mtime, writable) = read_doc(&self.path);
            self.doc = doc;
            self.mtime = mtime;
            self.writable = writable;
        }
    }

    fn save(&mut self) {
        if !self.writable {
            return;
        }
        if let Err(e) = std::fs::write(&self.path, self.doc.to_string()) {
            eprintln!("theme: failed to write {}: {e}", self.path.display());
        }
        self.mtime = file_mtime(&self.path);
    }

    fn save_seeded(&self) {
        if let Err(e) = std::fs::write(&self.seeded_path, self.seeded.to_string()) {
            eprintln!("theme: failed to write {}: {e}", self.seeded_path.display());
        }
    }

    /// Remember `default` as the bot-written value for `[section].key`.
    fn record_seeded(&mut self, section: &str, key: &str, default: &[String]) {
        let table = self
            .seeded
            .as_table_mut()
            .entry(section)
            .or_insert(toml_edit::table());
        if let Some(table) = table.as_table_mut() {
            table.insert(key, theme_item(default));
        }
    }

    /// Upgrade an untouched key to a changed default, or adopt a legacy key that matches the
    /// current default. Returns true if the theme file changed.
    fn reconcile_default(&mut self, section: &str, key: &str, default: &[String]) -> bool {
        let Some(current) = read_values(&self.doc, section, key) else {
            return false;
        };
        match read_values(&self.seeded, section, key) {
            Some(seeded) if seeded == default => false,
            Some(seeded) if seeded == current => {
                if !self.upgraded.insert((section.to_string(), key.to_string())) {
                    eprintln!(
                        "theme: [{section}].{key} is requested with conflicting defaults; not upgrading it again this run"
                    );
                    return false;
                }
                // Never edited by the operator: the new default replaces the old one.
                if let Some(table) = self
                    .doc
                    .as_table_mut()
                    .get_mut(section)
                    .and_then(|item| item.as_table_mut())
                {
                    table.insert(key, theme_item(default));
                }
                self.record_seeded(section, key, default);
                self.save_seeded();
                true
            }
            // Edited by the operator: theirs wins, permanently.
            Some(_) => false,
            None => {
                if current == default {
                    self.record_seeded(section, key, default);
                    self.save_seeded();
                    false
                } else if is_catch_all(&current) && !is_catch_all(default) {
                    // A legacy pass-through like `"{text}"`, seeded before defaults were recorded,
                    // from a module that now supplies a real sentence: nobody wrote that.
                    if let Some(table) = self
                        .doc
                        .as_table_mut()
                        .get_mut(section)
                        .and_then(|item| item.as_table_mut())
                    {
                        table.insert(key, theme_item(default));
                    }
                    self.record_seeded(section, key, default);
                    self.save_seeded();
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Resolve `[section].key`, seeding `default` if absent, picking a random list entry, and
    /// substituting `{var}` placeholders.
    pub fn resolve(
        &mut self,
        section: &str,
        key: &str,
        default: &[String],
        vars: &[(String, String)],
    ) -> String {
        self.reload_if_changed();

        // A syntactically valid file can still use a scalar/array where a module table is
        // expected. Treat that as a local configuration error and serve the supplied default;
        // never panic the shared module host or rewrite the user's structure.
        if self
            .doc
            .as_table()
            .get(section)
            .is_some_and(|item| !item.is_table())
        {
            let chosen = choose(default);
            return render(&chosen, vars);
        }

        // Seed the default the first time this key is used.
        let table = self.doc.as_table_mut();
        let sect = table
            .entry(section)
            .or_insert(toml_edit::table())
            .as_table_mut()
            .expect("section is a table");
        if !sect.contains_key(key) {
            sect.insert(key, theme_item(default));
            self.save();
            if self.writable {
                self.record_seeded(section, key, default);
                self.save_seeded();
            }
        } else if self.writable
            && !default.is_empty()
            && self.reconcile_default(section, key, default)
        {
            self.save();
        }

        // Read the (possibly user-edited) current value.
        let values = read_values(&self.doc, section, key).unwrap_or_else(|| default.to_vec());
        let chosen = choose(&values);
        render(&chosen, vars)
    }
}

/// `theme.toml` → `theme.seeded.toml`, beside it.
fn seeded_path_for(path: &Path) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "theme".into());
    path.with_file_name(format!("{stem}.seeded.toml"))
}

/// A single default is stored as a string, several as an array (one is chosen at random).
fn theme_item(default: &[String]) -> toml_edit::Item {
    if default.len() <= 1 {
        toml_edit::value(default.first().cloned().unwrap_or_default())
    } else {
        let mut arr = toml_edit::Array::new();
        for d in default {
            arr.push(d.as_str());
        }
        toml_edit::value(arr)
    }
}

fn read_values(doc: &DocumentMut, section: &str, key: &str) -> Option<Vec<String>> {
    let item = doc.as_table().get(section)?.as_table()?.get(key)?;
    if let Some(arr) = item.as_array() {
        Some(
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
        )
    } else {
        item.as_str().map(|s| vec![s.to_string()])
    }
}

fn read_doc(path: &Path) -> (DocumentMut, Option<SystemTime>, bool) {
    match std::fs::read_to_string(path) {
        Ok(s) => match s.parse::<DocumentMut>() {
            Ok(doc) => (doc, file_mtime(path), true),
            Err(e) => {
                eprintln!(
                    "theme: {} is not valid TOML ({e}); serving defaults without overwriting it",
                    path.display()
                );
                (DocumentMut::new(), file_mtime(path), false)
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (DocumentMut::new(), None, true),
        Err(e) => {
            eprintln!("theme: failed to read {} ({e})", path.display());
            (DocumentMut::new(), file_mtime(path), false)
        }
    }
}

/// A single value that is nothing but one placeholder, e.g. `"{text}"`.
fn is_catch_all(values: &[String]) -> bool {
    let [value] = values else {
        return false;
    };
    let inner = value
        .trim()
        .strip_prefix('{')
        .and_then(|v| v.strip_suffix('}'));
    inner.is_some_and(|name| {
        !name.is_empty()
            && name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    })
}

fn choose(values: &[String]) -> String {
    match values.len() {
        0 => String::new(),
        1 => values[0].clone(),
        n => values[fastrand::usize(..n)].clone(),
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Replace each `{key}` placeholder in `template` with its value.
/// Substitute `{name}` placeholders in one left-to-right pass over the *template*. Substituted
/// values are never rescanned, so user-supplied text (a nick like `{loot}`, a memo full of
/// placeholders) cannot pull in other variables or multiply the output. Unknown placeholders and
/// stray braces are kept verbatim.
fn render(template: &str, vars: &[(String, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            vars.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| (value, close))
        });
        match value {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn render_substitutes_placeholders() {
        assert_eq!(
            render(
                "Welcome, {user}, to {chan}.",
                &vars(&[("user", "bob"), ("chan", "#x")])
            ),
            "Welcome, bob, to #x."
        );
        // Unknown placeholders are left intact.
        assert_eq!(render("hi {nope}", &vars(&[("user", "bob")])), "hi {nope}");
    }

    #[test]
    fn resolve_seeds_default_and_persists() {
        let dir = std::env::temp_dir().join(format!("jeeves-theme-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.toml");
        let _ = std::fs::remove_file(&path);

        let store = ThemeStore::open(&path);
        let out = store.lock().unwrap().resolve(
            "admin",
            "denied",
            &["No, {user}.".to_string()],
            &vars(&[("user", "eve")]),
        );
        assert_eq!(out, "No, eve.");

        // The default was written to disk under [admin].
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("[admin]"), "file: {written}");
        assert!(written.contains("denied"), "file: {written}");

        // A subsequent user edit is picked up live (new store reads same file).
        std::fs::write(&path, "[admin]\ndenied = \"Denied, {user}!\"\n").unwrap();
        let out2 = store.lock().unwrap().resolve(
            "admin",
            "denied",
            &["No, {user}.".to_string()],
            &vars(&[("user", "eve")]),
        );
        assert_eq!(out2, "Denied, eve!");

        let _ = std::fs::remove_file(&path);
    }

    fn temp_theme(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jeeves-theme-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.toml");
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(seeded_path_for(&path));
        path
    }

    #[test]
    fn untouched_defaults_upgrade_and_edits_are_kept() {
        let path = temp_theme("upgrade");
        let store = ThemeStore::open(&path);
        let old = ["Hello, sir.".to_string()];
        let new = ["Hello, {honorific}.".to_string()];
        store.lock().unwrap().resolve("m", "greet", &old, &[]);
        store.lock().unwrap().resolve("m", "edited", &old, &[]);
        // The operator edits one key.
        let text = std::fs::read_to_string(&path)
            .unwrap()
            .replace("edited = \"Hello, sir.\"", "edited = \"Ahoy.\"");
        std::fs::write(&path, text).unwrap();

        // The module ships new copy.
        let vars = vars(&[("honorific", "madam")]);
        let greet = store.lock().unwrap().resolve("m", "greet", &new, &vars);
        let edited = store.lock().unwrap().resolve("m", "edited", &new, &vars);
        assert_eq!(greet, "Hello, madam.");
        assert_eq!(edited, "Ahoy.");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("{honorific}"), "file: {written}");
        assert!(written.contains("Ahoy."), "file: {written}");
    }

    #[test]
    fn conflicting_defaults_upgrade_at_most_once() {
        let path = temp_theme("flap");
        let store = ThemeStore::open(&path);
        store.lock().unwrap().resolve("m", "k", &["A".into()], &[]);
        assert_eq!(
            store.lock().unwrap().resolve("m", "k", &["B".into()], &[]),
            "B"
        );
        // A second, different default for the same key no longer rewrites it.
        assert_eq!(
            store.lock().unwrap().resolve("m", "k", &["A".into()], &[]),
            "B"
        );
    }

    #[test]
    fn legacy_keys_are_adopted_only_when_they_match() {
        let path = temp_theme("legacy");
        // Seeded by an older host, so there is no sidecar record.
        std::fs::write(&path, "[m]\nsame = \"Hi.\"\nold = \"Old copy.\"\n").unwrap();
        let store = ThemeStore::open(&path);
        store
            .lock()
            .unwrap()
            .resolve("m", "same", &["Hi.".into()], &[]);
        // Unknown origin: never overwritten.
        let old = store
            .lock()
            .unwrap()
            .resolve("m", "old", &["New copy.".into()], &[]);
        assert_eq!(old, "Old copy.");
        // `same` was adopted, so a later default change upgrades it.
        let same = store
            .lock()
            .unwrap()
            .resolve("m", "same", &["Hello.".into()], &[]);
        assert_eq!(same, "Hello.");
    }

    #[test]
    fn legacy_catch_all_keys_take_a_real_sentence() {
        let path = temp_theme("catchall");
        std::fs::write(
            &path,
            "[m]\nplain = \"{text}\"\ndressed = \"🎣 {text}\"\nstill = \"{text}\"\n",
        )
        .unwrap();
        let store = ThemeStore::open(&path);
        let mut store = store.lock().unwrap();
        let vars = [
            ("user".to_string(), "ann".to_string()),
            ("text".to_string(), "ann casts.".to_string()),
        ];
        assert_eq!(
            store.resolve("m", "plain", &["{user} casts.".into()], &vars),
            "ann casts."
        );
        assert_eq!(
            store.resolve("m", "dressed", &["{user} casts.".into()], &vars),
            "🎣 ann casts.",
            "an operator's wrapper is theirs"
        );
        assert_eq!(
            store.resolve("m", "still", &["{text}".into()], &vars),
            "ann casts.",
            "a catch-all that is still wanted stays"
        );
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("plain = \"{user} casts.\""), "{written}");
    }

    #[test]
    fn resolve_handles_list_values() {
        let dir = std::env::temp_dir().join(format!("jeeves-theme-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.toml");
        std::fs::write(&path, "[m]\npong = [\"a\", \"b\", \"c\"]\n").unwrap();

        let store = ThemeStore::open(&path);
        for _ in 0..20 {
            let v = store
                .lock()
                .unwrap()
                .resolve("m", "pong", &["x".to_string()], &[]);
            assert!(["a", "b", "c"].contains(&v.as_str()), "got {v}");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn invalid_file_is_never_overwritten() {
        let path =
            std::env::temp_dir().join(format!("jeeves-theme-invalid-{}.toml", std::process::id()));
        std::fs::write(&path, "[broken\n").unwrap();
        let store = ThemeStore::open(&path);
        let out = store
            .lock()
            .unwrap()
            .resolve("m", "key", &["fallback".into()], &[]);
        assert_eq!(out, "fallback");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[broken\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn non_table_module_section_falls_back_without_panicking() {
        let path =
            std::env::temp_dir().join(format!("jeeves-theme-scalar-{}.toml", std::process::id()));
        std::fs::write(&path, "m = \"not a table\"\n").unwrap();
        let store = ThemeStore::open(&path);
        let out = store
            .lock()
            .unwrap()
            .resolve("m", "key", &["fallback".into()], &[]);
        assert_eq!(out, "fallback");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "m = \"not a table\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn substituted_values_are_never_rescanned_for_placeholders() {
        let out = render(
            "{user} plunders {loot}g",
            &vars(&[("user", "{loot}"), ("loot", "999")]),
        );
        assert_eq!(
            out, "{loot} plunders 999g",
            "a nick like {{loot}} stays literal"
        );

        // A value stuffed with placeholders cannot multiply another value into the output.
        let big = "x".repeat(400);
        let out = render(
            "{memo} from {sender}",
            &vars(&[("memo", &"{sender}".repeat(50)), ("sender", &big)]),
        );
        assert!(
            out.len() < 1_000,
            "output was amplified to {} bytes",
            out.len()
        );
    }

    #[test]
    fn unknown_placeholders_and_stray_braces_are_kept() {
        assert_eq!(
            render("{a} {missing} { } {", &vars(&[("a", "1")])),
            "1 {missing} { } {"
        );
    }
}
