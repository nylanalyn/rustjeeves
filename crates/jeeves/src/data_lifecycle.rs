//! Operator-facing profile export support.

use crate::db::DbHandle;
use anyhow::{anyhow, Context, Result};
use jeeves_abi::{DataSubject, ProfileDataExport, DATA_EXPORT_VERSION};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

const MAX_EXPORT_FILES: usize = 100;
const MAX_EXPORT_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

/// Export host-owned data for one profile. Opaque module KV remains excluded until modules expose
/// explicit lifecycle hooks; guessing at module-private formats would make exports unreliable.
pub async fn export_profile(
    db: &DbHandle,
    server: &str,
    nick: &str,
    output_dir: &Path,
) -> Result<PathBuf> {
    let profile = db
        .profile_get(server, nick)
        .await?
        .ok_or_else(|| anyhow!("no profile found for {nick} on {server}"))?;
    let (aliases, accounts) = db
        .profile_identity_links(&profile.server, &profile.id)
        .await?;

    let mut scheduled_jobs = db
        .scheduled_jobs_load()
        .await?
        .into_iter()
        .filter(|job| {
            job.server == profile.server
                && job.owner_profile_id.as_deref() == Some(profile.id.as_str())
        })
        .collect::<Vec<_>>();
    scheduled_jobs.sort_by_key(|job| (job.due_at, job.module.clone(), job.id.clone()));

    let export = assemble_export(profile, aliases, accounts, scheduled_jobs)?;

    write_private_json(output_dir, &export)
}

pub fn collect_profile_blocking(
    db: &DbHandle,
    server: &str,
    nick: &str,
) -> Result<ProfileDataExport> {
    let profile = db
        .profile_get_blocking(server, nick)?
        .ok_or_else(|| anyhow!("no profile found for {nick} on {server}"))?;
    let (aliases, accounts) = db.profile_identity_links_blocking(&profile.server, &profile.id)?;
    let mut scheduled_jobs = db
        .scheduled_jobs_load_blocking()?
        .into_iter()
        .filter(|job| {
            job.server == profile.server
                && job.owner_profile_id.as_deref() == Some(profile.id.as_str())
        })
        .collect::<Vec<_>>();
    scheduled_jobs.sort_by_key(|job| (job.due_at, job.module.clone(), job.id.clone()));
    assemble_export(profile, aliases, accounts, scheduled_jobs)
}

fn assemble_export(
    profile: jeeves_abi::Profile,
    aliases: Vec<jeeves_abi::ProfileAliasExport>,
    accounts: Vec<String>,
    scheduled_jobs: Vec<jeeves_abi::ScheduledJob>,
) -> Result<ProfileDataExport> {
    let exported_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time is before the Unix epoch")?
        .as_secs() as i64;
    Ok(ProfileDataExport {
        version: DATA_EXPORT_VERSION,
        exported_at,
        subject: DataSubject {
            server: profile.server.clone(),
            profile_id: profile.id.clone(),
        },
        profile,
        aliases,
        accounts,
        scheduled_jobs,
        modules: Vec::new(),
    })
}

pub fn write_private_json(output_dir: &Path, export: &ProfileDataExport) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(output_dir)
            .with_context(|| format!("create export directory {}", output_dir.display()))?;
    }
    #[cfg(not(unix))]
    fs::create_dir_all(output_dir)
        .with_context(|| format!("create export directory {}", output_dir.display()))?;

    prune_exports(output_dir)?;

    let path = output_dir.join(format!("profile-export-{}.json", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    let write_result = (|| -> Result<()> {
        let mut file = options
            .open(&path)
            .with_context(|| format!("create export file {}", path.display()))?;
        serde_json::to_writer_pretty(&mut file, export).context("serialize profile export")?;
        file.write_all(b"\n").context("finish profile export")?;
        file.sync_all().context("flush profile export")?;
        Ok(())
    })();

    if let Err(error) = write_result {
        let _ = fs::remove_file(&path);
        return Err(error);
    }
    Ok(path)
}

fn prune_exports(output_dir: &Path) -> Result<()> {
    let now = SystemTime::now();
    let mut files = fs::read_dir(output_dir)
        .with_context(|| format!("read export directory {}", output_dir.display()))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with("profile-export-") || !name.ends_with(".json") {
                return None;
            }
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((entry.path(), modified))
        })
        .collect::<Vec<_>>();

    for (path, modified) in &files {
        if now
            .duration_since(*modified)
            .is_ok_and(|age| age > MAX_EXPORT_AGE)
        {
            fs::remove_file(path)
                .with_context(|| format!("remove expired export {}", path.display()))?;
        }
    }
    files.retain(|(path, modified)| {
        path.exists()
            && now
                .duration_since(*modified)
                .is_ok_and(|age| age <= MAX_EXPORT_AGE)
    });
    files.sort_by_key(|(_, modified)| *modified);
    for (path, _) in files
        .iter()
        .take(files.len().saturating_sub(MAX_EXPORT_FILES - 1))
    {
        fs::remove_file(path)
            .with_context(|| format!("remove surplus export {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use jeeves_abi::ScheduledJob;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn export_pruning_caps_generated_files_and_preserves_other_files() {
        let dir = std::env::temp_dir().join(format!("jeeves-export-prune-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        for index in 0..=MAX_EXPORT_FILES {
            fs::write(dir.join(format!("profile-export-{index}.json")), b"{}").unwrap();
        }
        fs::write(dir.join("operator-note.txt"), b"keep").unwrap();

        prune_exports(&dir).unwrap();

        let exports = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("profile-export-")
            })
            .count();
        assert_eq!(exports, MAX_EXPORT_FILES - 1);
        assert!(dir.join("operator-note.txt").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn exports_only_jobs_owned_by_the_profile() {
        let db = DbHandle::open(":memory:").unwrap();
        let profile = db
            .profile_resolve("testnet", "Alice", Some("alice-account".into()), 100)
            .await
            .unwrap();
        db.scheduled_job_set(ScheduledJob {
            module: "reminders".into(),
            id: "owned".into(),
            server: "testnet".into(),
            channel: "Alice".into(),
            owner_profile_id: Some(profile.id.clone()),
            due_at: 200,
            payload: "private payload".into(),
            created_at: 100,
        })
        .await
        .unwrap();
        db.scheduled_job_set(ScheduledJob {
            module: "reminders".into(),
            id: "someone-elses".into(),
            server: "testnet".into(),
            channel: "Bob".into(),
            owner_profile_id: Some(Uuid::new_v4().to_string()),
            due_at: 200,
            payload: "must not leak".into(),
            created_at: 100,
        })
        .await
        .unwrap();

        let dir = std::env::temp_dir().join(format!("jeeves-export-test-{}", Uuid::new_v4()));
        let path = export_profile(&db, "testnet", "alice", &dir).await.unwrap();
        let export: ProfileDataExport = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();

        assert_eq!(export.version, DATA_EXPORT_VERSION);
        assert_eq!(export.subject.profile_id, profile.id);
        assert_eq!(export.profile.nick, "Alice");
        assert_eq!(export.aliases.len(), 1);
        assert_eq!(export.aliases[0].nick, "Alice");
        assert_eq!(export.accounts, ["alice-account"]);
        assert_eq!(export.scheduled_jobs.len(), 1);
        assert_eq!(export.scheduled_jobs[0].id, "owned");
        assert!(export.modules.is_empty());

        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn does_not_change_an_existing_export_directory_mode() {
        let db = DbHandle::open(":memory:").unwrap();
        db.profile_resolve("testnet", "Alice", None, 100)
            .await
            .unwrap();
        let dir = std::env::temp_dir().join(format!("jeeves-export-test-{}", Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o750)).unwrap();

        let path = export_profile(&db, "testnet", "Alice", &dir).await.unwrap();

        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o750
        );
        fs::remove_file(path).unwrap();
        fs::remove_dir(dir).unwrap();
    }

    #[tokio::test]
    async fn rejects_unknown_profiles_without_writing_a_file() {
        let db = DbHandle::open(":memory:").unwrap();
        let dir = std::env::temp_dir().join(format!("jeeves-export-test-{}", Uuid::new_v4()));

        let error = export_profile(&db, "testnet", "missing", &dir)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("no profile found"));
        assert!(!dir.exists());
    }
}
