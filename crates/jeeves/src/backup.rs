//! Host-owned SQLite backups, retention, verification, and optional encrypted B2 replication.

use crate::db::{verify_backup_file, DbHandle};
use crate::log_bus::LogBus;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use chrono::{Datelike, Timelike, Utc, Weekday};
use ring::{aead, digest, rand as ring_rand};
use ring_rand::SecureRandom;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

pub const KEY_ENABLED: &str = "backup_enabled";
pub const KEY_DIRECTORY: &str = "backup_directory";
pub const KEY_HOUR: &str = "backup_hour_utc";
pub const KEY_KEEP_DAILY: &str = "backup_keep_daily";
pub const KEY_KEEP_WEEKLY: &str = "backup_keep_weekly";
pub const KEY_KEEP_MONTHLY: &str = "backup_keep_monthly";
pub const KEY_B2_ENABLED: &str = "backup_b2_enabled";
pub const KEY_B2_AUTHORIZE_URL: &str = "backup_b2_authorize_url";
pub const KEY_B2_BUCKET: &str = "backup_b2_bucket";
pub const KEY_B2_PREFIX: &str = "backup_b2_prefix";
pub const KEY_B2_WEEKDAY: &str = "backup_b2_weekday";
pub const KEY_B2_KEY_ID: &str = "backup_b2_key_id";
pub const KEY_B2_APPLICATION_KEY: &str = "backup_b2_application_key";
pub const KEY_ENCRYPTION_KEY: &str = "backup_encryption_key";
const KEY_LAST_LOCAL_DAY: &str = "backup_last_local_day";
const KEY_LAST_REMOTE_WEEK: &str = "backup_last_remote_week";
const KEY_LAST_SUCCESS_AT: &str = "backup_last_success_at";
const KEY_LAST_ERROR: &str = "backup_last_error";
const KEY_LAST_LOCAL_PATH: &str = "backup_last_local_path";
const KEY_LAST_REMOTE_OBJECT: &str = "backup_last_remote_object";
const DEFAULT_AUTHORIZE_URL: &str = "https://api.backblazeb2.com/b2api/v4/b2_authorize_account";
const ENCRYPTED_MAGIC: &[u8] = b"RUSTJEEVES-BACKUP\x01";
const MAX_ENCRYPTED_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct BackupConfig {
    pub enabled: bool,
    pub directory: String,
    pub hour_utc: u32,
    pub keep_daily: usize,
    pub keep_weekly: usize,
    pub keep_monthly: usize,
    pub b2_enabled: bool,
    pub b2_authorize_url: String,
    pub b2_bucket: String,
    pub b2_prefix: String,
    pub b2_weekday: Weekday,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            directory: "backups".into(),
            hour_utc: 4,
            keep_daily: 3,
            keep_weekly: 4,
            keep_monthly: 3,
            b2_enabled: false,
            b2_authorize_url: DEFAULT_AUTHORIZE_URL.into(),
            b2_bucket: String::new(),
            b2_prefix: "rustjeeves".into(),
            b2_weekday: Weekday::Sun,
        }
    }
}

impl BackupConfig {
    pub fn load(db: &DbHandle) -> Result<Self> {
        let mut config = Self::default();
        config.enabled = get_bool(db, KEY_ENABLED, config.enabled)?;
        config.directory = get_string(db, KEY_DIRECTORY, &config.directory)?;
        config.hour_utc = get_number(db, KEY_HOUR, config.hour_utc)?;
        config.keep_daily = get_number(db, KEY_KEEP_DAILY, config.keep_daily)?;
        config.keep_weekly = get_number(db, KEY_KEEP_WEEKLY, config.keep_weekly)?;
        config.keep_monthly = get_number(db, KEY_KEEP_MONTHLY, config.keep_monthly)?;
        config.b2_enabled = get_bool(db, KEY_B2_ENABLED, config.b2_enabled)?;
        config.b2_authorize_url = get_string(db, KEY_B2_AUTHORIZE_URL, &config.b2_authorize_url)?;
        config.b2_bucket = get_string(db, KEY_B2_BUCKET, &config.b2_bucket)?;
        config.b2_prefix = get_string(db, KEY_B2_PREFIX, &config.b2_prefix)?;
        let weekday = get_string(db, KEY_B2_WEEKDAY, "sun")?;
        config.b2_weekday = parse_weekday(&weekday)?;
        if config.directory.trim().is_empty() {
            bail!("backup directory cannot be empty");
        }
        if config.hour_utc > 23 {
            bail!("backup hour must be from 0 to 23");
        }
        if config.keep_daily == 0 || config.keep_daily > 365 {
            bail!("daily backup retention must be from 1 to 365");
        }
        if config.keep_weekly > 260 {
            bail!("weekly backup retention must be from 0 to 260");
        }
        if config.keep_monthly > 120 {
            bail!("monthly backup retention must be from 0 to 120");
        }
        if config.b2_enabled && config.keep_weekly == 0 {
            bail!("weekly retention must be at least 1 when Backblaze backups are enabled");
        }
        Ok(config)
    }
}

#[derive(Clone, Debug, Default)]
pub struct BackupStatus {
    pub running: bool,
    pub last_started_at: Option<String>,
    pub last_success_at: Option<String>,
    pub last_error: Option<String>,
    pub last_local_path: Option<String>,
    pub last_remote_object: Option<String>,
}

#[derive(Clone)]
pub struct BackupHandle {
    tx: mpsc::SyncSender<BackupCommand>,
    status: Arc<Mutex<BackupStatus>>,
}

enum BackupCommand {
    RunNow,
    Shutdown,
}

impl BackupHandle {
    pub fn spawn(db: DbHandle, log: LogBus) -> Self {
        let (tx, rx) = mpsc::sync_channel(4);
        let status = Arc::new(Mutex::new(BackupStatus::default()));
        let worker_status = status.clone();
        std::thread::Builder::new()
            .name("jeeves-backups".into())
            .spawn(move || worker(db, log, rx, worker_status))
            .expect("failed to spawn backup worker");
        Self { tx, status }
    }

    pub fn run_now(&self) -> Result<()> {
        self.tx
            .try_send(BackupCommand::RunNow)
            .map_err(|e| anyhow!("could not queue backup: {e}"))
    }

    pub fn status(&self) -> BackupStatus {
        self.status.lock().unwrap().clone()
    }
}

impl Drop for BackupHandle {
    fn drop(&mut self) {
        if Arc::strong_count(&self.status) == 2 {
            let _ = self.tx.try_send(BackupCommand::Shutdown);
        }
    }
}

fn worker(
    db: DbHandle,
    log: LogBus,
    rx: mpsc::Receiver<BackupCommand>,
    status: Arc<Mutex<BackupStatus>>,
) {
    {
        let mut state = status.lock().unwrap();
        state.last_success_at = db.config_get_blocking(KEY_LAST_SUCCESS_AT).ok().flatten();
        state.last_error = db.config_get_blocking(KEY_LAST_ERROR).ok().flatten();
        state.last_local_path = db.config_get_blocking(KEY_LAST_LOCAL_PATH).ok().flatten();
        state.last_remote_object = db
            .config_get_blocking(KEY_LAST_REMOTE_OBJECT)
            .ok()
            .flatten();
    }
    loop {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(BackupCommand::RunNow) => run_and_record(&db, &log, &status, true),
            Ok(BackupCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => match BackupConfig::load(&db) {
                Ok(config) if config.enabled && local_due(&db, &config, Utc::now()) => {
                    run_and_record(&db, &log, &status, false);
                }
                Ok(_) => {}
                Err(error) => {
                    let message = format!("backup settings are invalid: {error:#}");
                    let changed =
                        status.lock().unwrap().last_error.as_deref() != Some(message.as_str());
                    if changed {
                        status.lock().unwrap().last_error = Some(message.clone());
                        let _ = db.config_set_blocking(KEY_LAST_ERROR, Some(&message));
                        log.error("backup", message);
                    }
                }
            },
        }
    }
}

fn run_and_record(db: &DbHandle, log: &LogBus, status: &Arc<Mutex<BackupStatus>>, force: bool) {
    {
        let mut state = status.lock().unwrap();
        if state.running {
            return;
        }
        state.running = true;
        state.last_started_at = Some(Utc::now().to_rfc3339());
        state.last_error = None;
    }
    let result = run_backup(db, force);
    let mut state = status.lock().unwrap();
    state.running = false;
    match result {
        Ok(outcome) => {
            let success_at = Utc::now().to_rfc3339();
            state.last_success_at = Some(success_at.clone());
            state.last_local_path = Some(outcome.local.display().to_string());
            state.last_remote_object = outcome.remote;
            state.last_error = outcome.remote_error.clone();
            let _ = db.config_set_blocking(KEY_LAST_SUCCESS_AT, Some(&success_at));
            let _ = db.config_set_blocking(KEY_LAST_LOCAL_PATH, state.last_local_path.as_deref());
            let _ =
                db.config_set_blocking(KEY_LAST_REMOTE_OBJECT, state.last_remote_object.as_deref());
            let _ = db.config_set_blocking(KEY_LAST_ERROR, state.last_error.as_deref());
            if let Some(error) = outcome.remote_error {
                log.error(
                    "backup",
                    format!("local backup completed; remote replication failed: {error}"),
                );
            } else {
                log.info("backup", "backup completed and verified");
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            state.last_error = Some(message.clone());
            let _ = db.config_set_blocking(KEY_LAST_ERROR, Some(&message));
            log.error("backup", message);
        }
    }
}

struct BackupOutcome {
    local: PathBuf,
    remote: Option<String>,
    remote_error: Option<String>,
}

fn run_backup(db: &DbHandle, force: bool) -> Result<BackupOutcome> {
    let config = BackupConfig::load(db)?;
    let now = Utc::now();
    let directory = PathBuf::from(&config.directory);
    create_private_dir(&directory)?;
    let stamp = now.format("%Y%m%d-%H%M%S");
    let daily = directory.join(format!("jeeves-daily-{stamp}.sqlite"));
    if let Err(error) = db
        .backup_to_blocking(&daily)
        .with_context(|| format!("snapshotting database to {}", daily.display()))
    {
        fs::remove_file(&daily).ok();
        return Err(error);
    }
    if let Err(error) = fs::set_permissions(&daily, fs::Permissions::from_mode(0o600)) {
        fs::remove_file(&daily).ok();
        return Err(error.into());
    }
    let verification = match verify_backup_file(&daily) {
        Ok(verification) => verification,
        Err(error) => {
            fs::remove_file(&daily).ok();
            return Err(error);
        }
    };
    write_manifest(&daily, "daily", true, &verification)?;
    promote_if_missing(
        &daily,
        &directory.join(format!(
            "jeeves-weekly-{}-W{:02}.sqlite",
            now.iso_week().year(),
            now.iso_week().week()
        )),
        "weekly",
        &verification,
    )?;
    promote_if_missing(
        &daily,
        &directory.join(format!("jeeves-monthly-{}.sqlite", now.format("%Y-%m"))),
        "monthly",
        &verification,
    )?;
    prune(&directory, "jeeves-daily-", config.keep_daily)?;
    prune(&directory, "jeeves-weekly-", config.keep_weekly)?;
    prune(&directory, "jeeves-monthly-", config.keep_monthly)?;
    db.config_set_blocking(
        KEY_LAST_LOCAL_DAY,
        Some(&now.format("%Y-%m-%d").to_string()),
    )?;

    let remote_due = config.b2_enabled && (force || weekly_due(db, &config, now));
    let (remote, remote_error) = if remote_due {
        match upload_remote(db, &config, &daily, &directory, now) {
            Ok(object) => {
                db.config_set_blocking(
                    KEY_LAST_REMOTE_WEEK,
                    Some(&format!(
                        "{}-W{:02}",
                        now.iso_week().year(),
                        now.iso_week().week()
                    )),
                )?;
                (Some(object), None)
            }
            Err(error) => (None, Some(format!("{error:#}"))),
        }
    } else {
        (None, None)
    };
    Ok(BackupOutcome {
        local: daily,
        remote,
        remote_error,
    })
}

fn local_due(db: &DbHandle, config: &BackupConfig, now: chrono::DateTime<Utc>) -> bool {
    if now.hour() < config.hour_utc {
        return false;
    }
    db.config_get_blocking(KEY_LAST_LOCAL_DAY)
        .ok()
        .flatten()
        .as_deref()
        != Some(&now.format("%Y-%m-%d").to_string())
}

fn weekly_due(db: &DbHandle, config: &BackupConfig, now: chrono::DateTime<Utc>) -> bool {
    if now.weekday().num_days_from_monday() < config.b2_weekday.num_days_from_monday() {
        return false;
    }
    let week = format!("{}-W{:02}", now.iso_week().year(), now.iso_week().week());
    db.config_get_blocking(KEY_LAST_REMOTE_WEEK)
        .ok()
        .flatten()
        .as_deref()
        != Some(week.as_str())
}

#[derive(Serialize, Deserialize)]
struct BackupManifest {
    format_version: u32,
    created_at: String,
    app_version: String,
    tier: String,
    database_file: String,
    database_bytes: u64,
    sha256: String,
    schema_version: i64,
    integrity_check: String,
    credentials_included: bool,
}

fn write_manifest(
    database: &Path,
    tier: &str,
    credentials_included: bool,
    verification: &crate::db::BackupVerification,
) -> Result<PathBuf> {
    let manifest = BackupManifest {
        format_version: 1,
        created_at: Utc::now().to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        tier: tier.into(),
        database_file: file_name(database)?,
        database_bytes: fs::metadata(database)?.len(),
        sha256: sha256_file(database)?,
        schema_version: verification.schema_version,
        integrity_check: verification.integrity_check.clone(),
        credentials_included,
    };
    let path = manifest_path(database);
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    write_private(&path, &bytes)?;
    Ok(path)
}

fn promote_if_missing(
    source: &Path,
    destination: &Path,
    tier: &str,
    verification: &crate::db::BackupVerification,
) -> Result<()> {
    if destination.exists() {
        if !manifest_path(destination).exists() {
            write_manifest(destination, tier, true, verification)?;
        }
        return Ok(());
    }
    if fs::hard_link(source, destination).is_err() {
        fs::copy(source, destination)?;
    }
    fs::set_permissions(destination, fs::Permissions::from_mode(0o600))?;
    write_manifest(destination, tier, true, verification)?;
    Ok(())
}

fn prune(directory: &Path, prefix: &str, keep: usize) -> Result<()> {
    let mut files = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if ty.is_file() && name.starts_with(prefix) && name.ends_with(".sqlite") {
            files.push(entry.path());
        }
    }
    files.sort();
    let remove = files.len().saturating_sub(keep);
    for path in files.into_iter().take(remove) {
        fs::remove_file(manifest_path(&path)).ok();
        fs::remove_file(path)?;
    }
    Ok(())
}

fn upload_remote(
    db: &DbHandle,
    config: &BackupConfig,
    local: &Path,
    directory: &Path,
    now: chrono::DateTime<Utc>,
) -> Result<String> {
    if config.b2_bucket.trim().is_empty() {
        bail!("B2 bucket is required when remote backups are enabled");
    }
    let key_id = secret(db, KEY_B2_KEY_ID, "RUSTJEEVES_B2_KEY_ID")?;
    let app_key = secret(db, KEY_B2_APPLICATION_KEY, "RUSTJEEVES_B2_APPLICATION_KEY")?;
    let encryption_key = secret(db, KEY_ENCRYPTION_KEY, "RUSTJEEVES_BACKUP_ENCRYPTION_KEY")?;
    let temp_id = uuid::Uuid::new_v4();
    let plain = directory.join(format!(".remote-{temp_id}.sqlite"));
    let encrypted = directory.join(format!(".remote-{temp_id}.rjb"));
    let cleanup = TempFiles(vec![plain.clone(), encrypted.clone()]);
    fs::copy(local, &plain)?;
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o600))?;
    sanitize_remote_copy(&plain)?;
    let verification = verify_backup_file(&plain)?;
    encrypt_file(&plain, &encrypted, &encryption_key)?;
    let prefix = config.b2_prefix.trim_matches('/');
    let basename = format!("jeeves-{}.sqlite.rjb", now.format("%Y%m%d-%H%M%S"));
    let object = if prefix.is_empty() {
        basename
    } else {
        format!("{prefix}/{basename}")
    };
    let manifest = BackupManifest {
        format_version: 1,
        created_at: now.to_rfc3339(),
        app_version: env!("CARGO_PKG_VERSION").into(),
        tier: "remote-weekly-encrypted".into(),
        database_file: object.clone(),
        database_bytes: fs::metadata(&encrypted)?.len(),
        sha256: sha256_file(&encrypted)?,
        schema_version: verification.schema_version,
        integrity_check: verification.integrity_check,
        credentials_included: false,
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let client = B2Client::authorize(&config.b2_authorize_url, &key_id, &app_key)?;
    let bucket_id = client.bucket_id(&config.b2_bucket)?;
    client.upload(&bucket_id, &object, &fs::read(&encrypted)?)?;
    client.upload(
        &bucket_id,
        &format!("{object}.manifest.json"),
        &manifest_bytes,
    )?;
    client.prune_backups(&bucket_id, prefix, config.keep_weekly)?;
    drop(cleanup);
    Ok(object)
}

struct TempFiles(Vec<PathBuf>);
impl Drop for TempFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            fs::remove_file(path).ok();
        }
    }
}

fn sanitize_remote_copy(path: &Path) -> Result<()> {
    let conn = Connection::open(path)?;
    conn.execute_batch(
        "PRAGMA secure_delete = ON;
         UPDATE sasl SET password = NULL, nick_password = NULL;
         UPDATE channels SET key = NULL;
         DELETE FROM config WHERE key IN (
           'tavily_api_key', 'deepl_api_key',
           'ai_api_key', 'youtube_api_key',
           'backup_b2_key_id', 'backup_b2_application_key', 'backup_encryption_key'
         );
         VACUUM;",
    )?;
    Ok(())
}

/// Create and verify a short-lived safety snapshot immediately before an operator repair.
/// These are separate from scheduled retention and capped at ten restore points.
pub(crate) fn create_pre_repair_snapshot(db: &DbHandle) -> Result<PathBuf> {
    let config = BackupConfig::load(db)?;
    let directory = PathBuf::from(config.directory);
    create_private_dir(&directory)?;
    let path = directory.join(format!(
        "jeeves-pre-repair-{}-{}.sqlite",
        Utc::now().format("%Y%m%d-%H%M%S"),
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = db
        .backup_to_blocking(&path)
        .with_context(|| format!("creating pre-repair snapshot {}", path.display()))
    {
        fs::remove_file(&path).ok();
        return Err(error);
    }
    if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
        fs::remove_file(&path).ok();
        return Err(error.into());
    }
    let verification = match verify_backup_file(&path) {
        Ok(verification) => verification,
        Err(error) => {
            fs::remove_file(&path).ok();
            return Err(error);
        }
    };
    if let Err(error) = write_manifest(&path, "pre-repair", true, &verification) {
        fs::remove_file(&path).ok();
        fs::remove_file(manifest_path(&path)).ok();
        return Err(error);
    }
    prune(&directory, "jeeves-pre-repair-", 10)?;
    Ok(path)
}

pub fn generate_encryption_key() -> Result<String> {
    let mut key = [0_u8; 32];
    ring_rand::SystemRandom::new()
        .fill(&mut key)
        .map_err(|_| anyhow!("system random generator failed"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(key))
}

pub fn encrypt_file(input: &Path, output: &Path, encoded_key: &str) -> Result<()> {
    let metadata = fs::metadata(input)?;
    if metadata.len() > MAX_ENCRYPTED_BYTES {
        bail!("backup exceeds the 128 MiB encryption safety limit");
    }
    let key = decode_key(encoded_key)?;
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key)
        .map_err(|_| anyhow!("invalid encryption key"))?;
    let key = aead::LessSafeKey::new(unbound);
    let mut nonce = [0_u8; 12];
    ring_rand::SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("system random generator failed"))?;
    let mut data = fs::read(input)?;
    key.seal_in_place_append_tag(
        aead::Nonce::assume_unique_for_key(nonce),
        aead::Aad::from(ENCRYPTED_MAGIC),
        &mut data,
    )
    .map_err(|_| anyhow!("backup encryption failed"))?;
    let mut bytes = Vec::with_capacity(ENCRYPTED_MAGIC.len() + nonce.len() + data.len());
    bytes.extend_from_slice(ENCRYPTED_MAGIC);
    bytes.extend_from_slice(&nonce);
    bytes.extend_from_slice(&data);
    write_private(output, &bytes)
}

pub fn decrypt_file(input: &Path, output: &Path, encoded_key: &str) -> Result<()> {
    let bytes = fs::read(input)?;
    let header = ENCRYPTED_MAGIC.len() + 12;
    if bytes.len() < header || &bytes[..ENCRYPTED_MAGIC.len()] != ENCRYPTED_MAGIC {
        bail!("not a rustjeeves encrypted backup");
    }
    let key_bytes = decode_key(encoded_key)?;
    let unbound = aead::UnboundKey::new(&aead::CHACHA20_POLY1305, &key_bytes)
        .map_err(|_| anyhow!("invalid encryption key"))?;
    let key = aead::LessSafeKey::new(unbound);
    let mut nonce = [0_u8; 12];
    nonce.copy_from_slice(&bytes[ENCRYPTED_MAGIC.len()..header]);
    let mut ciphertext = bytes[header..].to_vec();
    let plaintext = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(ENCRYPTED_MAGIC),
            &mut ciphertext,
        )
        .map_err(|_| anyhow!("backup authentication failed (wrong key or damaged file)"))?;
    write_private(output, plaintext)
}

fn decode_key(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    let decoded = if value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex::decode(value)?
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(value)
            .context("encryption key must be base64 or 64 hexadecimal characters")?
    };
    if decoded.len() != 32 {
        bail!("encryption key must decode to exactly 32 bytes");
    }
    Ok(decoded)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AuthorizeResponse {
    account_id: String,
    authorization_token: String,
    api_info: ApiInfo,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiInfo {
    storage_api: StorageApi,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StorageApi {
    api_url: String,
    allowed: Allowed,
}
#[derive(Deserialize)]
struct Allowed {
    buckets: Vec<AllowedBucket>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllowedBucket {
    id: String,
    name: String,
}
#[derive(Deserialize)]
struct BucketList {
    buckets: Vec<AllowedBucket>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadTarget {
    upload_url: String,
    authorization_token: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileVersions {
    files: Vec<RemoteFile>,
    next_file_name: Option<String>,
    next_file_id: Option<String>,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct RemoteFile {
    action: String,
    file_id: Option<String>,
    file_name: String,
    upload_timestamp: i64,
}

struct B2Client {
    agent: ureq::Agent,
    account_id: String,
    token: String,
    api_url: String,
    allowed: Vec<AllowedBucket>,
}

impl B2Client {
    fn authorize(url: &str, key_id: &str, app_key: &str) -> Result<Self> {
        let agent = ureq::Agent::new_with_defaults();
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{key_id}:{app_key}"));
        let mut response = agent
            .get(url)
            .header("Authorization", &format!("Basic {basic}"))
            .call()
            .context("authorizing with Backblaze B2")?;
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()?;
        let auth: AuthorizeResponse =
            serde_json::from_str(&body).context("reading B2 authorization response")?;
        Ok(Self {
            agent,
            account_id: auth.account_id,
            token: auth.authorization_token,
            api_url: auth.api_info.storage_api.api_url,
            allowed: auth.api_info.storage_api.allowed.buckets,
        })
    }

    fn bucket_id(&self, name: &str) -> Result<String> {
        if let Some(bucket) = self.allowed.iter().find(|bucket| bucket.name == name) {
            return Ok(bucket.id.clone());
        }
        let mut response = self
            .agent
            .post(format!("{}/b2api/v4/b2_list_buckets", self.api_url))
            .header("Authorization", &self.token)
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&serde_json::json!({
                "accountId": self.account_id,
                "bucketName": name,
            }))?)
            .context("listing B2 buckets")?;
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()?;
        let list: BucketList = serde_json::from_str(&body)?;
        list.buckets
            .into_iter()
            .find(|bucket| bucket.name == name)
            .map(|bucket| bucket.id)
            .ok_or_else(|| anyhow!("B2 bucket '{name}' was not found or is not allowed"))
    }

    fn upload(&self, bucket_id: &str, object: &str, bytes: &[u8]) -> Result<()> {
        let mut response = self
            .agent
            .get(format!(
                "{}/b2api/v4/b2_get_upload_url?bucketId={}",
                self.api_url,
                percent_encode(bucket_id)
            ))
            .header("Authorization", &self.token)
            .call()
            .context("requesting B2 upload URL")?;
        let body = response
            .body_mut()
            .with_config()
            .limit(1024 * 1024)
            .read_to_string()?;
        let target: UploadTarget = serde_json::from_str(&body)?;
        let sha1 = hex::encode(digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, bytes));
        self.agent
            .post(&target.upload_url)
            .header("Authorization", &target.authorization_token)
            .header("X-Bz-File-Name", &percent_encode(object))
            .header("Content-Type", "application/octet-stream")
            .header("X-Bz-Content-Sha1", &sha1)
            .header("X-Bz-Server-Side-Encryption", "AES256")
            .send(bytes)
            .context("uploading encrypted backup to B2")?;
        Ok(())
    }

    fn prune_backups(&self, bucket_id: &str, prefix: &str, keep: usize) -> Result<()> {
        let backup_prefix = if prefix.is_empty() {
            "jeeves-".to_string()
        } else {
            format!("{prefix}/jeeves-")
        };
        let mut files = Vec::new();
        let mut next: Option<(String, String)> = None;
        loop {
            let mut url = format!(
                "{}/b2api/v4/b2_list_file_versions?bucketId={}&prefix={}&maxFileCount=1000",
                self.api_url,
                percent_encode(bucket_id),
                percent_encode(&backup_prefix)
            );
            if let Some((name, id)) = &next {
                url.push_str("&startFileName=");
                url.push_str(&percent_encode(name));
                url.push_str("&startFileId=");
                url.push_str(&percent_encode(id));
            }
            let mut response = self
                .agent
                .get(&url)
                .header("Authorization", &self.token)
                .call()
                .context("listing retained B2 backup versions")?;
            let body = response
                .body_mut()
                .with_config()
                .limit(8 * 1024 * 1024)
                .read_to_string()?;
            let page: FileVersions = serde_json::from_str(&body)?;
            files.extend(page.files);
            next = page.next_file_name.zip(page.next_file_id);
            if next.is_none() {
                break;
            }
        }

        let mut bases: Vec<String> = files
            .iter()
            .filter(|file| file.action == "upload" && file.file_name.ends_with(".rjb"))
            .map(|file| file.file_name.clone())
            .collect();
        bases.sort();
        bases.dedup();
        let retained: std::collections::HashSet<String> =
            bases.into_iter().rev().take(keep).collect();
        let mut newest: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for file in &files {
            if file.action == "upload" {
                newest
                    .entry(file.file_name.clone())
                    .and_modify(|ts| *ts = (*ts).max(file.upload_timestamp))
                    .or_insert(file.upload_timestamp);
            }
        }
        for file in files {
            let Some(file_id) = file.file_id else {
                continue;
            };
            let base = file
                .file_name
                .strip_suffix(".manifest.json")
                .unwrap_or(&file.file_name);
            let keep_this = retained.contains(base)
                && file.action == "upload"
                && newest.get(&file.file_name) == Some(&file.upload_timestamp);
            if !keep_this {
                self.delete_version(&file.file_name, &file_id)?;
            }
        }
        Ok(())
    }

    fn delete_version(&self, file_name: &str, file_id: &str) -> Result<()> {
        self.agent
            .post(format!("{}/b2api/v4/b2_delete_file_version", self.api_url))
            .header("Authorization", &self.token)
            .header("Content-Type", "application/json")
            .send(serde_json::to_vec(&serde_json::json!({
                "fileName": file_name,
                "fileId": file_id,
            }))?)
            .context("pruning an expired B2 backup version")?;
        Ok(())
    }
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn get_string(db: &DbHandle, key: &str, default: &str) -> Result<String> {
    Ok(db
        .config_get_blocking(key)?
        .unwrap_or_else(|| default.to_string()))
}
fn get_bool(db: &DbHandle, key: &str, default: bool) -> Result<bool> {
    match db.config_get_blocking(key)?.as_deref() {
        None => Ok(default),
        Some("true" | "1" | "yes" | "on") => Ok(true),
        Some("false" | "0" | "no" | "off") => Ok(false),
        Some(value) => bail!("invalid boolean for {key}: {value}"),
    }
}
fn get_number<T>(db: &DbHandle, key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    db.config_get_blocking(key)?
        .map(|value| value.parse().map_err(|e| anyhow!("invalid {key}: {e}")))
        .unwrap_or(Ok(default))
}
fn secret(db: &DbHandle, key: &str, environment: &str) -> Result<String> {
    let value = std::env::var(environment)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .or(db.config_get_blocking(key)?)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| anyhow!("missing {key}; configure it in Integrations or {environment}"))?;
    Ok(value)
}
fn parse_weekday(value: &str) -> Result<Weekday> {
    match value.trim().to_ascii_lowercase().as_str() {
        "mon" | "monday" => Ok(Weekday::Mon),
        "tue" | "tuesday" => Ok(Weekday::Tue),
        "wed" | "wednesday" => Ok(Weekday::Wed),
        "thu" | "thursday" => Ok(Weekday::Thu),
        "fri" | "friday" => Ok(Weekday::Fri),
        "sat" | "saturday" => Ok(Weekday::Sat),
        "sun" | "sunday" => Ok(Weekday::Sun),
        _ => bail!("invalid backup weekday '{value}'"),
    }
}
fn create_private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    if !path.is_dir() {
        bail!("backup path is not a directory: {}", path.display());
    }
    Ok(())
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hex::encode(hash.finalize()))
}
fn manifest_path(database: &Path) -> PathBuf {
    PathBuf::from(format!("{}.manifest.json", database.display()))
}
fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("backup path has no valid UTF-8 file name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encryption_round_trip_and_authentication() {
        let dir = std::env::temp_dir().join(format!("jeeves-backup-test-{}", uuid::Uuid::new_v4()));
        create_private_dir(&dir).unwrap();
        let source = dir.join("source");
        let encrypted = dir.join("encrypted");
        let restored = dir.join("restored");
        write_private(&source, b"test database bytes").unwrap();
        let key = generate_encryption_key().unwrap();
        encrypt_file(&source, &encrypted, &key).unwrap();
        decrypt_file(&encrypted, &restored, &key).unwrap();
        assert_eq!(fs::read(restored).unwrap(), b"test database bytes");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn object_names_are_header_safe() {
        assert_eq!(percent_encode("folder/a b.sqlite"), "folder/a%20b.sqlite");
    }

    #[test]
    fn creates_verified_retained_local_snapshot() {
        let root =
            std::env::temp_dir().join(format!("jeeves-backup-test-{}", uuid::Uuid::new_v4()));
        create_private_dir(&root).unwrap();
        let source = root.join("bot.db");
        let destination = root.join("copies");
        let db = DbHandle::open(source.to_str().unwrap()).unwrap();
        db.config_set_blocking(KEY_DIRECTORY, Some(destination.to_str().unwrap()))
            .unwrap();
        let outcome = run_backup(&db, true).unwrap();
        assert!(outcome.local.exists());
        assert!(manifest_path(&outcome.local).exists());
        assert_eq!(
            verify_backup_file(&outcome.local).unwrap().integrity_check,
            "ok"
        );
        let names: Vec<String> = fs::read_dir(&destination)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().any(|name| name.starts_with("jeeves-weekly-")));
        assert!(names.iter().any(|name| name.starts_with("jeeves-monthly-")));
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn creates_verified_pre_repair_snapshot_when_scheduled_backups_are_disabled() {
        let root =
            std::env::temp_dir().join(format!("jeeves-repair-test-{}", uuid::Uuid::new_v4()));
        create_private_dir(&root).unwrap();
        let db_path = root.join("bot.db");
        let backup_dir = root.join("backups");
        let db = DbHandle::open(db_path.to_str().unwrap()).unwrap();
        db.config_set_blocking(KEY_ENABLED, Some("false")).unwrap();
        db.config_set_blocking(KEY_DIRECTORY, Some(backup_dir.to_str().unwrap()))
            .unwrap();
        let snapshot = create_pre_repair_snapshot(&db).unwrap();
        assert!(snapshot.exists());
        assert!(manifest_path(&snapshot).exists());
        assert_eq!(verify_backup_file(&snapshot).unwrap().integrity_check, "ok");
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn remote_copy_scrubs_known_credentials() {
        let root =
            std::env::temp_dir().join(format!("jeeves-backup-test-{}", uuid::Uuid::new_v4()));
        create_private_dir(&root).unwrap();
        let path = root.join("remote.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            crate::db::verify_backup_file(&path).unwrap();
            conn.execute("INSERT INTO servers (id, label) VALUES (1, 'net')", [])
                .unwrap();
            conn.execute(
                "INSERT INTO sasl (server_id, password, nick_password) VALUES (1, 'secret', 'identify')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO config (key, value) VALUES ('backup_encryption_key', 'secret-key')",
                [],
            )
            .unwrap();
        }
        sanitize_remote_copy(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let sasl: (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT password, nick_password FROM sasl WHERE server_id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sasl, (None, None));
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM config WHERE key = 'backup_encryption_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
        drop(conn);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn b2_native_api_flow_uses_discovered_storage_endpoint() {
        let server = match tiny_http::Server::http("127.0.0.1:0") {
            Ok(server) => server,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied) =>
            {
                return;
            }
            Err(error) => panic!("could not start mock B2 server: {error}"),
        };
        let address = server.server_addr().to_ip().unwrap();
        let base = format!("http://{address}");
        let server_base = base.clone();
        let thread = std::thread::spawn(move || {
            let mut paths = Vec::new();
            for _ in 0..6 {
                let request = server.recv().unwrap();
                let path = request.url().to_string();
                let body = if path.contains("b2_authorize_account") {
                    serde_json::json!({
                        "accountId": "account",
                        "authorizationToken": "account-token",
                        "apiInfo": {"storageApi": {
                            "apiUrl": server_base,
                            "allowed": {"buckets": [{"id": "bucket-id", "name": "bucket"}]}
                        }}
                    })
                    .to_string()
                } else if path.contains("b2_get_upload_url") {
                    serde_json::json!({
                        "uploadUrl": format!("{server_base}/upload"),
                        "authorizationToken": "upload-token"
                    })
                    .to_string()
                } else if path.contains("b2_list_file_versions") {
                    r#"{"files":[],"nextFileName":null,"nextFileId":null}"#.into()
                } else {
                    "{}".into()
                };
                paths.push(path);
                request
                    .respond(tiny_http::Response::from_string(body))
                    .unwrap();
            }
            paths
        });

        let client = B2Client::authorize(
            &format!("{base}/b2api/v4/b2_authorize_account"),
            "key-id",
            "application-key",
        )
        .unwrap();
        let bucket = client.bucket_id("bucket").unwrap();
        client
            .upload(&bucket, "prefix/one.rjb", b"encrypted")
            .unwrap();
        client
            .upload(&bucket, "prefix/one.rjb.manifest.json", b"{}")
            .unwrap();
        client.prune_backups(&bucket, "prefix", 4).unwrap();
        let paths = thread.join().unwrap();
        assert_eq!(paths.iter().filter(|p| p.as_str() == "/upload").count(), 2);
        assert!(paths.iter().any(|p| p.contains("b2_list_file_versions")));
    }
}
