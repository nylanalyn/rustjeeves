//! Configurable personality ("themes").
//!
//! All user-facing text the bot posts comes from a human-editable `theme.toml`, one `[section]`
//! per module. Modules ask the host for a string via the `theme` host function, passing a default;
//! the default is written to the file on first use (lazy registration). Values may be a single
//! string or a list (one is chosen at random), and `{var}` placeholders are substituted.
//!
//! Edits to `theme.toml` apply live: the parsed document is cached and reloaded when the file's
//! mtime changes. `toml_edit` is used so writing new defaults preserves the user's edits/comments.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use toml_edit::DocumentMut;

/// Shared, mutable handle to the theme store (one file shared across all modules).
pub type ThemeHandle = Arc<Mutex<ThemeStore>>;

pub struct ThemeStore {
    path: PathBuf,
    doc: DocumentMut,
    mtime: Option<SystemTime>,
}

impl ThemeStore {
    /// Open the theme file (empty document if it doesn't exist yet), returning a shared handle.
    pub fn open(path: impl Into<PathBuf>) -> ThemeHandle {
        let path = path.into();
        let (doc, mtime) = read_doc(&path);
        Arc::new(Mutex::new(ThemeStore { path, doc, mtime }))
    }

    /// Re-read the file if it changed on disk since we last loaded it.
    fn reload_if_changed(&mut self) {
        let disk = file_mtime(&self.path);
        if disk != self.mtime {
            let (doc, mtime) = read_doc(&self.path);
            self.doc = doc;
            self.mtime = mtime;
        }
    }

    fn save(&mut self) {
        if let Err(e) = std::fs::write(&self.path, self.doc.to_string()) {
            eprintln!("theme: failed to write {}: {e}", self.path.display());
        }
        self.mtime = file_mtime(&self.path);
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

        // Seed the default the first time this key is used.
        let table = self.doc.as_table_mut();
        let sect = table
            .entry(section)
            .or_insert(toml_edit::table())
            .as_table_mut()
            .expect("section is a table");
        if !sect.contains_key(key) {
            let item = if default.len() <= 1 {
                toml_edit::value(default.first().cloned().unwrap_or_default())
            } else {
                let mut arr = toml_edit::Array::new();
                for d in default {
                    arr.push(d.as_str());
                }
                toml_edit::value(arr)
            };
            sect.insert(key, item);
            self.save();
        }

        // Read the (possibly user-edited) current value.
        let values = read_values(&self.doc, section, key).unwrap_or_else(|| default.to_vec());
        let chosen = match values.len() {
            0 => String::new(),
            1 => values[0].clone(),
            n => values[fastrand::usize(..n)].clone(),
        };
        render(&chosen, vars)
    }
}

fn read_values(doc: &DocumentMut, section: &str, key: &str) -> Option<Vec<String>> {
    let item = doc.as_table().get(section)?.as_table()?.get(key)?;
    if let Some(arr) = item.as_array() {
        Some(arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
    } else {
        item.as_str().map(|s| vec![s.to_string()])
    }
}

fn read_doc(path: &Path) -> (DocumentMut, Option<SystemTime>) {
    let doc = match std::fs::read_to_string(path) {
        Ok(s) => s.parse::<DocumentMut>().unwrap_or_else(|e| {
            eprintln!("theme: {} is not valid TOML ({e}); ignoring", path.display());
            DocumentMut::new()
        }),
        Err(_) => DocumentMut::new(),
    };
    (doc, file_mtime(path))
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Replace each `{key}` placeholder in `template` with its value.
fn render(template: &str, vars: &[(String, String)]) -> String {
    let mut out = template.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn render_substitutes_placeholders() {
        assert_eq!(
            render("Welcome, {user}, to {chan}.", &vars(&[("user", "bob"), ("chan", "#x")])),
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

    #[test]
    fn resolve_handles_list_values() {
        let dir = std::env::temp_dir().join(format!("jeeves-theme-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("theme.toml");
        std::fs::write(&path, "[m]\npong = [\"a\", \"b\", \"c\"]\n").unwrap();

        let store = ThemeStore::open(&path);
        for _ in 0..20 {
            let v = store.lock().unwrap().resolve("m", "pong", &["x".to_string()], &[]);
            assert!(["a", "b", "c"].contains(&v.as_str()), "got {v}");
        }
        let _ = std::fs::remove_file(&path);
    }
}
