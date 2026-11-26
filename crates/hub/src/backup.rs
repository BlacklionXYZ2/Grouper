//! Consistent snapshots of the hub database and thread workspaces.
//!
//! SQLite's online backup API copies `grouper.db` while `grouper-hub serve` may have
//! it open, including pages that are still only in the WAL file. A plain file copy can
//! miss those pages. The master key is never written into the snapshot: provider keys
//! stay ciphertext, and restore checks that the key this process can see decrypts them
//! before it replaces anything.

use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use core_protocol::PROTOCOL_VERSION;
use rusqlite::backup::{Backup as SqliteBackup, StepResult};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use fs2::FileExt;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

use hub::secrets::Secrets;

const FORMAT_VERSION: u32 = 1;
const MANIFEST_NAME: &str = "manifest.json";
const DB_NAME: &str = "grouper.db";
const WORKSPACES: &str = "workspaces";
const MASTER_KEY_NAME: &str = "master.key";
/// Pages copied per step so a hub that is writing can run between steps.
const BACKUP_PAGES_PER_STEP: i32 = 32;
const BACKUP_STEP_PAUSE: Duration = Duration::from_millis(5);
const BACKUP_BUSY_PAUSE: Duration = Duration::from_millis(20);
const BACKUP_MAX_BUSY_ROUNDS: u32 = 1_500;
const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
const LOCK_NAME: &str = "grouper.lock";
const MAX_ARCHIVE_ENTRIES: u64 = 100_000;
const MAX_EXTRACTED_BYTES: u64 = 20 * 1024 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    format: u32,
    created: String,
    hub_id: Option<String>,
    hub_version: String,
    protocol_version: u32,
    schema: SchemaInfo,
    include_workspaces: bool,
    files: Vec<FileEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SchemaInfo {
    user_version: i64,
    schema_version: i64,
    tables: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileEntry {
    path: String,
    #[serde(default, skip_serializing_if = "is_false")]
    directory: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

struct SnapshotMeta {
    hub_id: Option<String>,
    schema: SchemaInfo,
}

struct Filled {
    meta: SnapshotMeta,
    files: usize,
    bytes: u64,
}

struct Node {
    rel: String,
    abs: PathBuf,
    directory: bool,
}

struct OpenedBackup {
    root: PathBuf,
    cleanup: bool,
}

pub(crate) struct DataLock {
    file: File,
    path: PathBuf,
    remove_on_drop: bool,
}

impl Drop for DataLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn lock_data_dir(data_dir: &Path) -> Result<DataLock> {
    create_private_dir_all(data_dir)?;
    let path = data_dir.join(LOCK_NAME);
    let remove_on_drop = !path.exists();
    let file = open_private(&path, true, false).with_context(|| format!("opening {}", path.display()))?;
    FileExt::try_lock_exclusive(&file).with_context(|| {
        format!(
            "{} is locked by a running hub; stop grouper-hub serve before restoring",
            path.display()
        )
    })?;
    Ok(DataLock { file, path, remove_on_drop })
}

fn create_private_dir_all(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        let existed = path.exists();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;
        if !existed {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        }
    }
    #[cfg(not(unix))]
    fs::create_dir_all(path)?;
    Ok(())
}

fn open_private(path: &Path, create_new: bool, truncate: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).truncate(truncate);
    if create_new {
        options.create(true);
    }
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn write_private(path: PathBuf, bytes: &[u8]) -> io::Result<()> {
    let mut file = open_private(&path, true, true)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn create_new_private(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(path)
}

fn sibling_stage(target: &Path, label: &str) -> Result<PathBuf> {
    let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    create_private_dir_all(parent)?;
    let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("grouper");
    let dir = parent.join(format!(".{name}.{label}-{}-{}", std::process::id(), hub::db::random_hex(4)));
    create_private_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir)
}

impl Drop for OpenedBackup {
    fn drop(&mut self) {
        if self.cleanup {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

/// Write a snapshot of `data_dir` to `output` (a directory, or a `.tar.gz` / `.tgz` file).
pub(crate) fn backup(data_dir: &Path, output: &Path, include_workspaces: bool) -> Result<()> {
    let db_path = data_dir.join(DB_NAME);
    if !db_path.is_file() {
        bail!(
            "no hub database at {}; nothing to back up",
            db_path.display()
        );
    }
    if output.exists() {
        bail!("backup destination already exists: {}", output.display());
    }
    let staging = sibling_stage(output, "backup")?;
    let filled = match fill_staging(&staging, data_dir, include_workspaces) {
        Ok(filled) => filled,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    let published = if is_archive(output) {
        let result = write_tar_gz(&staging, output);
        let _ = fs::remove_dir_all(&staging);
        result
    } else {
        place_tree(&staging, output)
    };
    published?;
    print_backup_done(output, &filled);
    Ok(())
}

/// Restore `backup` (a snapshot directory or `.tar.gz`) into `data_dir`.
pub(crate) fn restore(data_dir: &Path, backup_path: &Path, force: bool) -> Result<()> {
    let _lock = lock_data_dir(data_dir).context("refusing restore while grouper-hub serve is using this data directory")?;
    if data_dir.exists() && !data_dir.is_dir() {
        bail!("{} is not a directory", data_dir.display());
    }
    let opened = open_backup(backup_path, data_dir)?;
    if paths_equal(data_dir, &opened.root) {
        bail!("refusing to restore {} onto itself", backup_path.display());
    }
    let manifest = verify_backup(&opened.root)?;
    // Refuse a non-empty directory before the master-key check so a wrong key is not
    // the error you see when the directory itself cannot be restored into.
    let existing = hub_entries(data_dir)?;
    if !existing.is_empty() && !force {
        bail!(
            "data directory {} is not empty. Restore into an empty directory, or pass --force to move \
             the current files aside (they are not deleted). {MASTER_KEY_NAME} is left in place.",
            data_dir.display()
        );
    }
    let sealed = decrypt_provider_keys(&opened.root, data_dir)?;
    let aside = install_with_rollback(data_dir, &opened.root, force)?;
    print_restore_done(data_dir, &manifest.hub_id, sealed, aside.as_deref());
    Ok(())
}

fn fill_staging(staging: &Path, data_dir: &Path, include_workspaces: bool) -> Result<Filled> {
    let meta = snapshot_database(&data_dir.join(DB_NAME), &staging.join(DB_NAME))?;
    if include_workspaces {
        let workspaces = data_dir.join(WORKSPACES);
        if workspaces.exists() && !workspaces.is_dir() {
            bail!("{} is not a directory", workspaces.display());
        }
        if workspaces.is_dir() {
            copy_dir(&workspaces, &staging.join(WORKSPACES))?;
        }
    }
    if staging.join(MASTER_KEY_NAME).exists() {
        bail!("internal error: backup staging contains {MASTER_KEY_NAME}");
    }
    let nodes = walk(staging)?;
    let mut files = Vec::with_capacity(nodes.len());
    let mut file_count = 0usize;
    let mut bytes = 0u64;
    for node in nodes {
        if !allowed_backup_path(&node.rel) {
            bail!("refusing to put {} in a backup", node.rel);
        }
        if node.directory {
            files.push(FileEntry {
                path: node.rel,
                directory: true,
                size: None,
                sha256: None,
            });
            continue;
        }
        let (size, sha256) = hash_file(&node.abs)?;
        file_count += 1;
        bytes += size;
        files.push(FileEntry {
            path: node.rel,
            directory: false,
            size: Some(size),
            sha256: Some(sha256),
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    let manifest = Manifest {
        format: FORMAT_VERSION,
        created: rfc3339(now_secs()),
        hub_id: meta.hub_id.clone(),
        hub_version: HUB_VERSION.to_string(),
        protocol_version: PROTOCOL_VERSION,
        schema: SchemaInfo {
            user_version: meta.schema.user_version,
            schema_version: meta.schema.schema_version,
            tables: meta.schema.tables.clone(),
        },
        include_workspaces,
        files,
    };
    let json = serde_json::to_vec_pretty(&manifest).context("writing the backup manifest")?;
    write_private(staging.join(MANIFEST_NAME), &json).context("writing manifest.json")?;
    Ok(Filled {
        meta,
        files: file_count,
        bytes,
    })
}

fn snapshot_database(src_path: &Path, dst_path: &Path) -> Result<SnapshotMeta> {
    let src =
        Connection::open(src_path).with_context(|| format!("opening {}", src_path.display()))?;
    src.busy_timeout(Duration::from_secs(30))
        .context("setting a busy timeout on the hub database")?;
    let mut dst =
        Connection::open(dst_path).with_context(|| format!("creating {}", dst_path.display()))?;
    copy_online(&src, &mut dst)?;
    integrity_ok(&dst)?;
    let meta = read_meta(&dst)?;
    drop(dst);
    drop(src);
    clear_sidecars(dst_path);
    let file = OpenOptions::new().read(true).write(true).open(dst_path)?;
    file.sync_all()
        .with_context(|| format!("syncing {}", dst_path.display()))?;
    #[cfg(unix)]
    fs::set_permissions(dst_path, fs::Permissions::from_mode(0o600))?;
    Ok(meta)
}

fn copy_online(src: &Connection, dst: &mut Connection) -> Result<()> {
    let backup = SqliteBackup::new(src, dst).context("starting a SQLite online backup")?;
    let mut busy_rounds = 0u32;
    let mut stalls = 0u32;
    let mut last_remaining = i32::MAX;
    loop {
        match backup.step(BACKUP_PAGES_PER_STEP)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {
                busy_rounds = 0;
                let remaining = backup.progress().remaining;
                // A write between steps restarts the copy. If that keeps happening,
                // finish under one lock so the snapshot still completes.
                if remaining >= last_remaining {
                    stalls += 1;
                    if stalls >= 3 {
                        return finish_backup(&backup);
                    }
                } else {
                    stalls = 0;
                }
                last_remaining = remaining;
                std::thread::sleep(BACKUP_STEP_PAUSE);
            }
            StepResult::Busy | StepResult::Locked => {
                busy_rounds = wait_for_backup(busy_rounds)?;
            }
            _ => bail!("unexpected SQLite backup step result"),
        }
    }
}

/// Copy every remaining page in one step. Writers block until it finishes.
fn finish_backup(backup: &SqliteBackup) -> Result<()> {
    let mut busy_rounds = 0u32;
    loop {
        match backup.step(-1)? {
            StepResult::Done => return Ok(()),
            StepResult::More => {}
            StepResult::Busy | StepResult::Locked => {
                busy_rounds = wait_for_backup(busy_rounds)?;
            }
            _ => bail!("unexpected SQLite backup step result"),
        }
    }
}

fn wait_for_backup(busy_rounds: u32) -> Result<u32> {
    if busy_rounds >= BACKUP_MAX_BUSY_ROUNDS {
        bail!("database stayed busy while taking a backup; retry when writers finish");
    }
    std::thread::sleep(BACKUP_BUSY_PAUSE);
    Ok(busy_rounds + 1)
}

fn read_meta(conn: &Connection) -> Result<SnapshotMeta> {
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .context("reading PRAGMA user_version")?;
    let schema_version: i64 = conn
        .query_row("PRAGMA schema_version", [], |row| row.get(0))
        .context("reading PRAGMA schema_version")?;
    let mut statement = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")?;
    let tables = statement
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let hub_id = read_hub_id(conn)?;
    Ok(SnapshotMeta {
        hub_id,
        schema: SchemaInfo {
            user_version,
            schema_version,
            tables,
        },
    })
}

fn read_hub_id(conn: &Connection) -> Result<Option<String>> {
    let has_meta = conn
        .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'hub_meta'")?
        .exists([])?;
    if !has_meta {
        return Ok(None);
    }
    conn.query_row(
        "SELECT value FROM hub_meta WHERE key = 'hub_id'",
        [],
        |row| row.get(0),
    )
    .optional()
    .context("reading hub_id")
}

fn integrity_ok(conn: &Connection) -> Result<()> {
    let status: String = conn
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .context("running integrity_check")?;
    if status == "ok" {
        Ok(())
    } else {
        bail!("database failed integrity_check: {status}")
    }
}

fn open_backup(path: &Path, data_dir: &Path) -> Result<OpenedBackup> {
    if path.exists() && fs::symlink_metadata(path)?.file_type().is_symlink() {
        bail!("refusing to restore from symlink {}", path.display());
    }
    if path.is_dir() {
        if is_within(path, data_dir) {
            let root = sibling_stage(data_dir, "restore")?;
            copy_dir(path, &root).context("copying the backup out of the data directory")?;
            return Ok(OpenedBackup {
                root,
                cleanup: true,
            });
        }
        return Ok(OpenedBackup {
            root: path.to_path_buf(),
            cleanup: false,
        });
    }
    if path.is_file() && is_archive(path) {
        let root = sibling_stage(data_dir, "restore")?;
        if let Err(error) = extract_tar_gz(path, &root) {
            let _ = fs::remove_dir_all(&root);
            return Err(error);
        }
        return Ok(OpenedBackup {
            root,
            cleanup: true,
        });
    }
    if path.exists() {
        bail!(
            "backup {} is neither a directory nor a .tar.gz file",
            path.display()
        );
    }
    bail!("backup not found: {}", path.display())
}

fn verify_backup(root: &Path) -> Result<Manifest> {
    // Walk the complete directory first. In particular, reject symlinks before
    // opening manifest.json or any file named by it.
    let mut on_disk = list_rel_paths(root)?;
    let manifest_path = root.join(MANIFEST_NAME);
    let file = File::open(&manifest_path).with_context(|| {
        format!(
            "backup has no {MANIFEST_NAME} ({})",
            manifest_path.display()
        )
    })?;
    let manifest: Manifest = serde_json::from_reader(file).context("reading manifest.json")?;
    if manifest.format != FORMAT_VERSION {
        bail!(
            "unsupported backup format {}; this hub restores format {FORMAT_VERSION}",
            manifest.format
        );
    }
    if !manifest
        .files
        .iter()
        .any(|entry| entry.path == DB_NAME && !entry.directory)
    {
        bail!("backup manifest has no {DB_NAME}");
    }
    let mut listed = BTreeSet::new();
    for entry in &manifest.files {
        if entry.path == MASTER_KEY_NAME {
            bail!(
                "backup lists {MASTER_KEY_NAME}. Hub backups never include the master key; \
                 remove that file from the backup and keep the key separately. Nothing was restored."
            );
        }
        if !allowed_backup_path(&entry.path) {
            bail!(
                "backup lists unexpected path {}. Nothing was restored.",
                entry.path
            );
        }
        let rel = safe_rel(&entry.path)?;
        let path = root.join(&rel);
        if entry.directory {
            if !path.is_dir() {
                bail!("backup is missing directory {}", entry.path);
            }
        } else {
            let expected_size = entry
                .size
                .with_context(|| format!("manifest entry {} has no size", entry.path))?;
            let expected_sha = entry
                .sha256
                .as_deref()
                .with_context(|| format!("manifest entry {} has no sha256", entry.path))?;
            let (size, sha256) = hash_file(&path).with_context(|| {
                format!("checksum mismatch for {}: file is missing", entry.path)
            })?;
            if size != expected_size || sha256 != expected_sha {
                bail!(
                    "checksum mismatch for {}: manifest sha256 {expected_sha} size {expected_size}, file sha256 {sha256} size {size}",
                    entry.path
                );
            }
        }
        ensure_inside(root, &path)?;
        if !listed.insert(entry.path.clone()) {
            bail!("duplicate path in manifest: {}", entry.path);
        }
    }
    on_disk.remove(MANIFEST_NAME);
    if listed != on_disk {
        bail!("{}", contents_differ(&listed, &on_disk));
    }
    let db_path = root.join(DB_NAME);
    let hub_id = with_snapshot(&db_path, |conn| {
        integrity_ok(conn)?;
        read_hub_id(conn)
    })?;
    if hub_id != manifest.hub_id {
        bail!(
            "backup manifest hub_id {:?} does not match the database hub_id {:?}. Nothing was restored.",
            manifest.hub_id,
            hub_id
        );
    }
    Ok(manifest)
}

fn decrypt_provider_keys(root: &Path, data_dir: &Path) -> Result<usize> {
    let db_path = root.join(DB_NAME);
    let sealed = with_snapshot(&db_path, |conn| {
        let has_providers = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'providers'")?
            .exists([])?;
        if !has_providers {
            bail!("backup database has no providers table");
        }
        let mut statement = conn
            .prepare("SELECT id, api_key FROM providers WHERE api_key IS NOT NULL AND api_key != ''")?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows.into_iter().filter(|(_, key)| Secrets::is_sealed(key)).collect::<Vec<_>>())
    })?;
    if sealed.is_empty() {
        return Ok(0);
    }
    let secrets = load_existing_secrets(data_dir)?;
    for (id, key) in &sealed {
        secrets.open(key).with_context(|| {
            format!(
                "cannot decrypt the provider key for {id}. The master key available to this process \
                 does not match the key that encrypted this backup (GROUPER_MASTER_KEY or \
                 GROUPER_MASTER_KEY_FILE; Docker uses /secrets/master.key). The backup does not \
                 contain the master key. Nothing was restored"
            )
        })?;
    }
    Ok(sealed.len())
}

/// Open the snapshot read-only, then delete WAL sidecars SQLite may create beside it.
/// A backup directory has to stay byte-for-byte what `backup` wrote.
fn with_snapshot<T>(db_path: &Path, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
    let result = (|| {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("opening {}", db_path.display()))?;
        f(&conn)
    })();
    clear_sidecars(db_path);
    result
}

fn clear_sidecars(db_path: &Path) {
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = fs::remove_file(db_path.with_file_name(format!("{DB_NAME}{suffix}")));
    }
}

fn load_existing_secrets(data_dir: &Path) -> Result<Secrets> {
    if std::env::var_os("GROUPER_MASTER_KEY").is_some() {
        return Secrets::load(data_dir).context("reading GROUPER_MASTER_KEY");
    }
    let path = std::env::var_os("GROUPER_MASTER_KEY_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join(MASTER_KEY_NAME));
    if !path.is_file() {
        bail!(
            "master key missing ({}). This backup does not contain the master key. Restore the same \
             key with GROUPER_MASTER_KEY or GROUPER_MASTER_KEY_FILE (Docker: /secrets/master.key) \
             before restoring, or provider keys cannot be decrypted. Nothing was restored",
            path.display()
        );
    }
    Secrets::load(data_dir).with_context(|| format!("reading master key {}", path.display()))
}

fn install_with_rollback(data_dir: &Path, root: &Path, force: bool) -> Result<Option<PathBuf>> {
    let existing = hub_entries(data_dir)?;
    if !existing.is_empty() && !force {
        bail!(
            "data directory {} is not empty. Restore into an empty directory, or pass --force to move \
             the current files aside (they are not deleted). {MASTER_KEY_NAME} is left in place.",
            data_dir.display()
        );
    }
    let aside = if existing.is_empty() {
        None
    } else {
        Some(move_aside(data_dir, &existing)?)
    };
    if let Err(error) = install_snapshot(data_dir, root) {
        let undo = undo_install(data_dir, aside.as_deref());
        return Err(match undo {
            Ok(()) => error.context("restore failed; the previous data directory was put back"),
            Err(undo_error) => error.context(format!(
                "restore failed and putting the previous data back also failed ({undo_error}). \
                 Previous files are in {}.",
                aside
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| data_dir.display().to_string())
            )),
        });
    }
    Ok(aside)
}

fn install_snapshot(data_dir: &Path, root: &Path) -> Result<()> {
    create_private_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    #[cfg(unix)]
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))?;
    let workspaces = root.join(WORKSPACES);
    let workspace_stage = if workspaces.is_dir() {
        let stage = sibling_stage(data_dir, "workspaces")?;
        if let Err(error) = copy_dir(&workspaces, &stage) {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        Some(stage)
    } else {
        None
    };
    let partial = data_dir.join(format!("{DB_NAME}.partial"));
    let installed = (|| -> Result<()> {
        copy_private_file(&root.join(DB_NAME), &partial)
            .with_context(|| format!("copying {DB_NAME} into {}", data_dir.display()))?;
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(&partial)?
            .sync_all()
            .context("syncing the restored database")?;
        fs::rename(&partial, data_dir.join(DB_NAME)).context("placing the restored database")?;
        if let Some(stage) = workspace_stage.as_ref() {
            fs::rename(stage, data_dir.join(WORKSPACES)).context("placing restored workspaces")?;
        }
        sync_dir(data_dir)
    })();
    if let Err(error) = installed {
        let _ = fs::remove_file(&partial);
        if let Some(stage) = workspace_stage {
            let _ = fs::remove_dir_all(stage);
        }
        return Err(error);
    }
    Ok(())
}

fn undo_install(data_dir: &Path, aside: Option<&Path>) -> Result<()> {
    for name in [
        DB_NAME,
        &format!("{DB_NAME}.partial"),
        &format!("{DB_NAME}-wal"),
        &format!("{DB_NAME}-shm"),
        &format!("{DB_NAME}-journal"),
    ] {
        let _ = fs::remove_file(data_dir.join(name));
    }
    let workspaces = data_dir.join(WORKSPACES);
    if workspaces.exists() {
        fs::remove_dir_all(&workspaces)
            .with_context(|| format!("removing {}", workspaces.display()))?;
    }
    if let Some(aside) = aside {
        move_back(aside, data_dir)?;
    }
    Ok(())
}

fn hub_entries(data_dir: &Path) -> Result<Vec<PathBuf>> {
    if !data_dir.exists() {
        return Ok(Vec::new());
    }
    if !data_dir.is_dir() {
        bail!("{} is not a directory", data_dir.display());
    }
    let mut paths = Vec::new();
    for entry in
        fs::read_dir(data_dir).with_context(|| format!("reading {}", data_dir.display()))?
    {
        let entry = entry?;
        if entry.file_name() == MASTER_KEY_NAME || entry.file_name() == LOCK_NAME {
            continue;
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

fn move_aside(data_dir: &Path, entries: &[PathBuf]) -> Result<PathBuf> {
    let mut dest = data_dir.join(format!("pre-restore-{}", stamp(now_secs())));
    let mut n = 0u32;
    while dest.exists() {
        n += 1;
        dest = data_dir.join(format!("pre-restore-{}-{n}", stamp(now_secs())));
    }
    create_private_dir_all(&dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut moved = Vec::new();
    for entry in entries {
        let name = entry.file_name().unwrap_or_default();
        let target = dest.join(name);
        if let Err(error) = fs::rename(entry, &target) {
            for (from, back) in moved.iter().rev() {
                let _ = fs::rename(from, back);
            }
            let _ = fs::remove_dir(&dest);
            return Err(error).with_context(|| {
                format!(
                    "moving {} aside. If the hub is still running, stop it and retry. Nothing was restored.",
                    entry.display()
                )
            });
        }
        moved.push((target, entry.clone()));
    }
    Ok(dest)
}

fn move_back(aside: &Path, data_dir: &Path) -> Result<()> {
    let entries = read_dir_sorted(aside)?;
    for entry in entries {
        let dest = data_dir.join(entry.file_name());
        if dest.exists() {
            bail!(
                "cannot move {} back; {} already exists",
                entry.path().display(),
                dest.display()
            );
        }
        fs::rename(entry.path(), &dest)
            .with_context(|| format!("moving {} back", entry.path().display()))?;
    }
    fs::remove_dir(aside).with_context(|| format!("removing {}", aside.display()))?;
    Ok(())
}

fn allowed_backup_path(path: &str) -> bool {
    path == DB_NAME || path == WORKSPACES || path.starts_with(&format!("{WORKSPACES}/"))
}

fn allowed_archive_path(path: &str) -> bool {
    path == MANIFEST_NAME || allowed_backup_path(path)
}

fn contents_differ(listed: &BTreeSet<String>, on_disk: &BTreeSet<String>) -> String {
    let extra: Vec<&str> = on_disk
        .difference(listed)
        .take(5)
        .map(String::as_str)
        .collect();
    let missing: Vec<&str> = listed
        .difference(on_disk)
        .take(5)
        .map(String::as_str)
        .collect();
    format!(
        "backup contents do not match the manifest; extra {extra:?}, missing {missing:?}. Nothing was restored"
    )
}

fn print_backup_done(output: &Path, filled: &Filled) {
    let hub = filled.meta.hub_id.as_deref().unwrap_or("(no hub_id yet)");
    println!(
        "Backed up hub {hub} to {} ({} files, {} bytes).",
        output.display(),
        filled.files,
        filled.bytes
    );
    println!();
    println!(
        "The master key was NOT included. Back it up separately and keep it with this snapshot. \
         It is the GROUPER_MASTER_KEY value, or the file GROUPER_MASTER_KEY_FILE names \
         (in Docker, /secrets/master.key on the grouper-secrets volume). \
         Restoring provider keys needs that same key."
    );
}

fn print_restore_done(
    data_dir: &Path,
    hub_id: &Option<String>,
    sealed: usize,
    aside: Option<&Path>,
) {
    let hub = hub_id.as_deref().unwrap_or("(no hub_id)");
    println!("Restored hub {hub} into {}.", data_dir.display());
    if let Some(path) = aside {
        println!(
            "Moved the previous contents of {} to {} (not deleted).",
            data_dir.display(),
            path.display()
        );
    }
    if sealed == 0 {
        println!("This backup has no encrypted provider keys.");
    } else {
        println!(
            "Decrypted {sealed} encrypted provider keys with the master key available to this process."
        );
    }
}

fn is_archive(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tar.gz") || lower.ends_with(".tgz")
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn is_within(path: &Path, dir: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(dir) = dir.canonicalize() else {
        return false;
    };
    path.starts_with(dir)
}

fn place_tree(staging: &Path, output: &Path) -> Result<()> {
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        create_private_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    match fs::rename(staging, output) {
        Ok(()) => Ok(()),
        Err(rename_error) => match copy_dir(staging, output) {
            Ok(()) => {
                fs::remove_dir_all(staging)?;
                Ok(())
            }
            Err(copy_error) => {
                let _ = fs::remove_dir_all(staging);
                let _ = fs::remove_dir_all(output);
                Err(copy_error).context(format!(
                    "could not move the backup into place ({rename_error})"
                ))
            }
        },
    }
}

fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    create_private_dir_all(dst).with_context(|| format!("creating {}", dst.display()))?;
    for entry in read_dir_sorted(src)? {
        let target = dst.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "refusing to copy symlink {} (it could point outside the backup)",
                entry.path().display()
            );
        } else if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_file() {
            copy_private_file(&entry.path(), &target)
                .with_context(|| format!("copying {}", entry.path().display()))?;
        } else {
            bail!(
                "refusing to copy non-regular file {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn copy_private_file(src: &Path, dst: &Path) -> Result<u64> {
    let mut input = File::open(src)?;
    let mut output = open_private(dst, true, true)?;
    let copied = io::copy(&mut input, &mut output)?;
    output.sync_all()?;
    Ok(copied)
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> Result<()> {
    Ok(())
}

fn walk(root: &Path) -> Result<Vec<Node>> {
    let mut nodes = Vec::new();
    walk_into(root, root, &mut nodes)?;
    nodes.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(nodes)
}

fn walk_into(root: &Path, dir: &Path, nodes: &mut Vec<Node>) -> Result<()> {
    for entry in read_dir_sorted(dir)? {
        let rel = rel_string(root, &entry.path())?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            bail!(
                "refusing to back up symlink {} (it could point outside the data directory)",
                entry.path().display()
            );
        } else if kind.is_dir() {
            nodes.push(Node {
                rel,
                abs: entry.path(),
                directory: true,
            });
            walk_into(root, &entry.path(), nodes)?;
        } else if kind.is_file() {
            nodes.push(Node {
                rel,
                abs: entry.path(),
                directory: false,
            });
        } else {
            bail!(
                "refusing to back up non-regular file {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

fn list_rel_paths(root: &Path) -> Result<BTreeSet<String>> {
    Ok(walk(root)?.into_iter().map(|node| node.rel).collect())
}

fn read_dir_sorted(dir: &Path) -> Result<Vec<fs::DirEntry>> {
    let mut entries = fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    Ok(entries)
}

fn rel_string(root: &Path, path: &Path) -> Result<String> {
    let rel = path
        .strip_prefix(root)
        .with_context(|| format!("{} is outside {}", path.display(), root.display()))?;
    let mut parts = Vec::new();
    for component in rel.components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
                if part == ".." || part.contains(':') || part.contains('/') || part.contains('\\') {
                    bail!("unsafe path component in {}", path.display());
                }
                parts.push(part);
            }
            Component::CurDir => {}
            _ => bail!("unsafe path {}", path.display()),
        }
    }
    if parts.is_empty() {
        bail!("empty relative path for {}", path.display());
    }
    Ok(parts.join("/"))
}

fn safe_rel(path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        bail!("unsafe backup path {path}");
    }
    let mut out = PathBuf::new();
    for part in path.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            bail!("unsafe backup path {path}");
        }
        out.push(part);
    }
    if out.as_os_str().is_empty() {
        bail!("unsafe backup path {path}");
    }
    Ok(out)
}

fn ensure_inside(root: &Path, path: &Path) -> Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    let path = path
        .canonicalize()
        .with_context(|| format!("resolving {}", path.display()))?;
    if path.starts_with(&root) {
        Ok(())
    } else {
        bail!("backup path {} escapes {}", path.display(), root.display())
    }
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, hex::encode(hasher.finalize())))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn ymd_hms(secs: u64) -> (i32, u32, u32, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    (year, month, day, tod / 3600, (tod % 3600) / 60, tod % 60)
}

fn rfc3339(secs: u64) -> String {
    let (year, month, day, hour, minute, second) = ymd_hms(secs);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn stamp(secs: u64) -> String {
    let (year, month, day, hour, minute, second) = ymd_hms(secs);
    format!("{year:04}{month:02}{day:02}T{hour:02}{minute:02}{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i32, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    if month <= 2 {
        year += 1;
    }
    (year as i32, month as u32, day as u32)
}

fn mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

// --- gzip (stored deflate blocks) and ustar --------------------------------

const CRC_TABLE: [u32; 256] = crc_table();

const fn crc_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut crc = i as u32;
        let mut bit = 0;
        while bit < 8 {
            if crc & 1 == 1 {
                crc = 0xedb8_8320 ^ (crc >> 1);
            } else {
                crc >>= 1;
            }
            bit += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

fn crc_update(mut crc: u32, data: &[u8]) -> u32 {
    for byte in data {
        let index = ((crc ^ u32::from(*byte)) & 0xff) as usize;
        crc = CRC_TABLE[index] ^ (crc >> 8);
    }
    crc
}

struct GzipEncoder<W> {
    inner: W,
    crc: u32,
    size: u32,
    started: bool,
}

impl<W: Write> GzipEncoder<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            crc: 0xffff_ffff,
            size: 0,
            started: false,
        }
    }

    fn write_header(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        self.inner
            .write_all(&[0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff])?;
        self.started = true;
        Ok(())
    }

    /// Stored deflate blocks. They are valid gzip, and the reader does not need a compressor.
    fn push(&mut self, mut data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        self.write_header()?;
        while !data.is_empty() {
            let n = data.len().min(65_535);
            self.write_block(&data[..n], false)?;
            data = &data[n..];
        }
        Ok(())
    }

    fn write_block(&mut self, data: &[u8], final_block: bool) -> Result<()> {
        let len = u16::try_from(data.len()).context("gzip block longer than 65535 bytes")?;
        let header = if final_block { 0x01 } else { 0x00 };
        self.inner.write_all(&[header])?;
        self.inner.write_all(&len.to_le_bytes())?;
        self.inner.write_all(&(!len).to_le_bytes())?;
        self.inner.write_all(data)?;
        self.crc = crc_update(self.crc, data);
        self.size = self.size.wrapping_add(u32::from(len));
        Ok(())
    }

    fn finish(mut self) -> Result<W> {
        self.write_header()?;
        self.write_block(&[], true)?;
        let crc = !self.crc;
        self.inner.write_all(&crc.to_le_bytes())?;
        self.inner.write_all(&self.size.to_le_bytes())?;
        self.inner.flush()?;
        Ok(self.inner)
    }
}

impl<W: Write> Write for GzipEncoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.push(buf).map_err(io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

struct GzipDecoder<R> {
    inner: R,
    block: Vec<u8>,
    pos: usize,
    crc: u32,
    size: u32,
    finished: bool,
}

impl<R: Read> GzipDecoder<R> {
    fn new(mut inner: R) -> Result<Self> {
        read_gzip_header(&mut inner)?;
        Ok(Self {
            inner,
            block: Vec::new(),
            pos: 0,
            crc: 0xffff_ffff,
            size: 0,
            finished: false,
        })
    }

    fn fill(&mut self) -> Result<bool> {
        loop {
            if self.pos < self.block.len() {
                return Ok(true);
            }
            if self.finished {
                return Ok(false);
            }
            self.block.clear();
            self.pos = 0;
            let header = read_u8(&mut self.inner)?;
            let bfinal = header & 1;
            let btype = (header >> 1) & 0b11;
            if btype != 0 {
                bail!(
                    "gzip data uses compressed deflate blocks (type {btype}); grouper-hub backups are stored blocks"
                );
            }
            let len = read_u16(&mut self.inner)?;
            let nlen = read_u16(&mut self.inner)?;
            if nlen != !len {
                bail!("corrupt gzip block length");
            }
            self.block.resize(usize::from(len), 0);
            self.inner.read_exact(&mut self.block)?;
            self.crc = crc_update(self.crc, &self.block);
            self.size = self.size.wrapping_add(u32::from(len));
            if bfinal == 1 {
                let mut trailer = [0u8; 8];
                self.inner.read_exact(&mut trailer)?;
                let expect_crc =
                    u32::from_le_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]);
                let expect_size =
                    u32::from_le_bytes([trailer[4], trailer[5], trailer[6], trailer[7]]);
                if expect_crc != !self.crc {
                    bail!("gzip checksum mismatch");
                }
                if expect_size != self.size {
                    bail!("gzip size mismatch");
                }
                self.finished = true;
            }
            if !self.block.is_empty() {
                return Ok(true);
            }
        }
    }
}

impl<R: Read> Read for GzipDecoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.pos < self.block.len() {
                let n = buf.len().min(self.block.len() - self.pos);
                buf[..n].copy_from_slice(&self.block[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            match self.fill() {
                Ok(true) => continue,
                Ok(false) => return Ok(0),
                Err(error) => return Err(io::Error::other(error)),
            }
        }
    }
}

fn read_gzip_header<R: Read>(reader: &mut R) -> Result<()> {
    let mut magic = [0u8; 10];
    reader.read_exact(&mut magic)?;
    if magic[0] != 0x1f || magic[1] != 0x8b {
        bail!("not a gzip file");
    }
    if magic[2] != 8 {
        bail!("unsupported gzip compression method {}", magic[2]);
    }
    let flags = magic[3];
    if flags & 0xe0 != 0 {
        bail!("corrupt gzip header flags");
    }
    if flags & 0x04 != 0 {
        let extra = read_u16(reader)?;
        skip_n(reader, u64::from(extra))?;
    }
    if flags & 0x08 != 0 {
        skip_cstr(reader)?;
    }
    if flags & 0x10 != 0 {
        skip_cstr(reader)?;
    }
    if flags & 0x02 != 0 {
        skip_n(reader, 2)?;
    }
    Ok(())
}

fn read_u8<R: Read>(reader: &mut R) -> Result<u8> {
    let mut byte = [0u8; 1];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}

fn read_u16<R: Read>(reader: &mut R) -> Result<u16> {
    let mut bytes = [0u8; 2];
    reader.read_exact(&mut bytes)?;
    Ok(u16::from_le_bytes(bytes))
}

fn skip_n<R: Read>(reader: &mut R, mut n: u64) -> Result<()> {
    let mut buf = [0u8; 4096];
    while n > 0 {
        let chunk = n.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..chunk])?;
        n -= chunk as u64;
    }
    Ok(())
}

fn skip_cstr<R: Read>(reader: &mut R) -> Result<()> {
    for _ in 0..65_536 {
        if read_u8(reader)? == 0 {
            return Ok(());
        }
    }
    bail!("gzip header string is too long")
}

struct TarWriter<W: Write> {
    gzip: GzipEncoder<W>,
}

impl<W: Write> TarWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            gzip: GzipEncoder::new(inner),
        }
    }

    fn write_dir(&mut self, rel: &str, mtime: u64) -> Result<()> {
        self.write_entry(rel, 0, mtime, b'5')
    }

    fn write_file(&mut self, rel: &str, path: &Path) -> Result<()> {
        let size = fs::metadata(path)
            .with_context(|| format!("reading {}", path.display()))?
            .len();
        self.write_entry(rel, size, mtime(path), b'0')?;
        let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
        let mut left = size;
        let mut buf = [0u8; 64 * 1024];
        while left > 0 {
            let n = file.read(&mut buf)?;
            if n == 0 {
                bail!("unexpected end of {}", path.display());
            }
            let n = n.min(left as usize);
            self.gzip.write_all(&buf[..n])?;
            left -= n as u64;
        }
        self.write_padding(size)
    }

    fn write_entry(&mut self, rel: &str, size: u64, mtime: u64, typeflag: u8) -> Result<()> {
        if split_ustar(rel).is_none() {
            self.write_long_name(rel)?;
        }
        let (name, prefix) = split_ustar(rel).unwrap_or_else(|| {
            let short = rel.rsplit('/').next().unwrap_or(rel);
            (truncate_bytes(short, 99).to_string(), String::new())
        });
        let mut header = [0u8; 512];
        put_bytes(&mut header[0..100], name.as_bytes())?;
        put_octal(&mut header[100..108], 0o700)?;
        if typeflag != b'5' {
            put_octal(&mut header[100..108], 0o600)?;
        }
        put_octal(&mut header[108..116], 0)?;
        put_octal(&mut header[116..124], 0)?;
        put_octal(&mut header[124..136], size)?;
        put_octal(&mut header[136..148], mtime)?;
        header[156] = typeflag;
        put_bytes(&mut header[257..263], b"ustar")?;
        header[263] = b'0';
        header[264] = b'0';
        put_bytes(&mut header[345..500], prefix.as_bytes())?;
        put_checksum(&mut header);
        self.gzip.write_all(&header)?;
        Ok(())
    }

    fn write_long_name(&mut self, rel: &str) -> Result<()> {
        let mut body = rel.as_bytes().to_vec();
        body.push(0);
        let mut header = [0u8; 512];
        put_bytes(&mut header[0..100], b"././@LongLink")?;
        put_octal(&mut header[100..108], 0o600)?;
        put_octal(&mut header[124..136], body.len() as u64)?;
        header[156] = b'L';
        put_bytes(&mut header[257..263], b"ustar")?;
        header[263] = b'0';
        header[264] = b'0';
        put_checksum(&mut header);
        self.gzip.write_all(&header)?;
        self.gzip.write_all(&body)?;
        self.write_padding(body.len() as u64)
    }

    fn write_padding(&mut self, size: u64) -> Result<()> {
        let extra = (512 - (size % 512)) % 512;
        if extra > 0 {
            let zeros = vec![0u8; extra as usize];
            self.gzip.write_all(&zeros)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<W> {
        let zeros = [0u8; 1024];
        self.gzip.write_all(&zeros)?;
        self.gzip.finish()
    }
}

fn put_bytes(field: &mut [u8], bytes: &[u8]) -> Result<()> {
    if bytes.len() >= field.len() {
        bail!(
            "tar header field is too small ({} bytes into {})",
            bytes.len(),
            field.len()
        );
    }
    field[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

fn put_octal(field: &mut [u8], value: u64) -> Result<()> {
    let text = format!("{value:o}");
    if text.len() >= field.len() {
        bail!("value {value} does not fit in a tar header field");
    }
    field[..text.len()].copy_from_slice(text.as_bytes());
    Ok(())
}

fn put_checksum(header: &mut [u8; 512]) {
    let sum: u32 = header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u32::from(b' ')
            } else {
                u32::from(*byte)
            }
        })
        .sum();
    let text = format!("{sum:06o}");
    header[148..154].copy_from_slice(text.as_bytes());
    header[154] = 0;
    header[155] = b' ';
}

/// USTAR name/prefix split. `None` means the path needs a GNU long-name header.
fn split_ustar(path: &str) -> Option<(String, String)> {
    if path.len() < 100 && !path.is_empty() {
        return Some((path.to_string(), String::new()));
    }
    let bytes = path.as_bytes();
    for index in (1..path.len()).rev() {
        if bytes[index] != b'/' {
            continue;
        }
        let prefix = &path[..index];
        let name = &path[index + 1..];
        if !prefix.is_empty() && prefix.len() < 155 && !name.is_empty() && name.len() < 100 {
            return Some((name.to_string(), prefix.to_string()));
        }
    }
    None
}

fn truncate_bytes(text: &str, max_bytes: usize) -> &str {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > max_bytes {
            break;
        }
        end = index + ch.len_utf8();
    }
    &text[..end]
}

fn write_tar_gz(root: &Path, output: &Path) -> Result<()> {
    if output.exists() {
        bail!("backup destination already exists: {}", output.display());
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        create_private_dir_all(parent)?;
    }
    let partial = output.with_file_name({
        let mut name = output
            .file_name()
            .with_context(|| format!("{} has no file name", output.display()))?
            .to_os_string();
        name.push(".partial");
        name
    });
    if let Err(error) = write_tar_file(root, &partial) {
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    fs::rename(&partial, output)
        .with_context(|| format!("renaming {} to {}", partial.display(), output.display()))?;
    Ok(())
}

fn write_tar_file(root: &Path, output: &Path) -> Result<()> {
    let file = open_private(output, true, true).with_context(|| format!("creating {}", output.display()))?;
    let mut tar = TarWriter::new(file);
    for node in walk(root)? {
        if node.directory {
            tar.write_dir(&node.rel, mtime(&node.abs))?;
        } else {
            tar.write_file(&node.rel, &node.abs)?;
        }
    }
    let file = tar.finish()?;
    file.sync_all()?;
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<()> {
    create_private_dir_all(dest).with_context(|| format!("creating {}", dest.display()))?;
    let file = File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let mut decoder = GzipDecoder::new(file)?;
    let mut long_name: Option<String> = None;
    let mut entries = 0u64;
    let mut extracted = 0u64;
    loop {
        let Some(header) = read_tar_header(&mut decoder)? else {
            break;
        };
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            bail!("backup archive has more than {MAX_ARCHIVE_ENTRIES} entries");
        }
        verify_tar_checksum(&header)?;
        let typeflag = header[156];
        let size = parse_octal(&header[124..136])?;
        extracted = add_extracted_bytes(extracted, size)?;
        let header_name = tar_name(&header)?;
        let name = long_name.take().unwrap_or(header_name);
        if typeflag == b'L' {
            if size > 1024 * 1024 {
                bail!("tar long name is unreasonably large");
            }
            let len = usize::try_from(size).context("tar long name length does not fit")?;
            let mut data = vec![0u8; len];
            decoder.read_exact(&mut data)?;
            skip_padding(&mut decoder, size)?;
            let end = data
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(data.len());
            long_name = Some(
                String::from_utf8(data[..end].to_vec()).context("tar long name is not UTF-8")?,
            );
            continue;
        }
        if typeflag == b'5' || name.ends_with('/') {
            if !allowed_archive_path(name.trim_end_matches('/')) {
                bail!("backup archive contains unexpected path {name}");
            }
            let rel = safe_rel(name.trim_end_matches('/'))?;
            let path = dest.join(rel);
            create_private_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
            ensure_inside(dest, &path)?;
            if size > 0 {
                skip_n(&mut decoder, size)?;
            }
            skip_padding(&mut decoder, size)?;
            continue;
        }
        if typeflag != b'0' && typeflag != 0 {
            bail!("unsupported tar entry type {} for {name}", typeflag as char);
        }
        if !allowed_archive_path(&name) {
            bail!("backup archive contains unexpected path {name}");
        }
        let rel = safe_rel(&name)?;
        let path = dest.join(&rel);
        if let Some(parent) = path.parent() {
            create_private_dir_all(parent)?;
        }
        let mut out = create_new_private(&path)
            .with_context(|| format!("creating {}", path.display()))?;
        copy_n(&mut decoder, &mut out, size)?;
        out.sync_all()?;
        ensure_inside(dest, &path)?;
        skip_padding(&mut decoder, size)?;
    }
    let mut rest = [0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut rest)?;
        if n == 0 {
            break;
        }
        if rest[..n].iter().any(|byte| *byte != 0) {
            bail!("data after the end of the tar archive");
        }
    }
    Ok(())
}

fn add_extracted_bytes(total: u64, entry: u64) -> Result<u64> {
    let total = total.checked_add(entry).context("backup archive size overflow")?;
    if total > MAX_EXTRACTED_BYTES {
        bail!("backup archive expands beyond the {} byte limit", MAX_EXTRACTED_BYTES);
    }
    Ok(total)
}

fn read_tar_header<R: Read>(reader: &mut R) -> Result<Option<[u8; 512]>> {
    let mut header = [0u8; 512];
    let mut read = 0;
    while read < 512 {
        match reader.read(&mut header[read..])? {
            0 if read == 0 => return Ok(None),
            0 => bail!("truncated tar header"),
            n => read += n,
        }
    }
    if header.iter().all(|byte| *byte == 0) {
        return Ok(None);
    }
    Ok(Some(header))
}

fn verify_tar_checksum(header: &[u8; 512]) -> Result<()> {
    let stored = parse_octal(&header[148..156])?;
    let sum = header_checksum(header);
    if stored == sum {
        Ok(())
    } else {
        bail!("tar header checksum mismatch")
    }
}

fn header_checksum(header: &[u8; 512]) -> u64 {
    header
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(*byte)
            }
        })
        .sum()
}

fn parse_octal(bytes: &[u8]) -> Result<u64> {
    if matches!(bytes.first(), Some(0x80 | 0xff)) {
        bail!("unsupported base-256 tar number");
    }
    let text: Vec<u8> = bytes
        .iter()
        .copied()
        .filter(|byte| *byte != 0 && *byte != b' ')
        .collect();
    if text.is_empty() {
        return Ok(0);
    }
    let text = std::str::from_utf8(&text).context("tar number is not UTF-8")?;
    u64::from_str_radix(text, 8).with_context(|| format!("bad tar octal {text}"))
}

fn tar_name(header: &[u8]) -> Result<String> {
    let name = cstr(&header[0..100]);
    let prefix = cstr(&header[345..500]);
    if prefix.is_empty() {
        Ok(name)
    } else {
        Ok(format!("{prefix}/{name}"))
    }
}

fn cstr(bytes: &[u8]) -> String {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn copy_n<R: Read, W: Write>(reader: &mut R, writer: &mut W, mut n: u64) -> Result<()> {
    let mut buf = [0u8; 64 * 1024];
    while n > 0 {
        let chunk = n.min(buf.len() as u64) as usize;
        reader.read_exact(&mut buf[..chunk])?;
        writer.write_all(&buf[..chunk])?;
        n -= chunk as u64;
    }
    Ok(())
}

fn skip_padding<R: Read>(reader: &mut R, size: u64) -> Result<()> {
    let extra = (512 - (size % 512)) % 512;
    if extra > 0 {
        skip_n(reader, extra)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
    use std::sync::{Arc, Mutex, MutexGuard};

    use base64::Engine as _;
    use core_protocol::{PROTOCOL_VERSION, ProviderConfig};
    use rusqlite::params;
    use serde_json::Value;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: Option<std::ffi::OsString>,
        file: Option<std::ffi::OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    fn isolate_master_key() -> EnvGuard {
        let lock = ENV_LOCK.lock().unwrap_or_else(|err| err.into_inner());
        let key = std::env::var_os("GROUPER_MASTER_KEY");
        let file = std::env::var_os("GROUPER_MASTER_KEY_FILE");
        // SAFETY: ENV_LOCK is held until this guard drops, and these tests are the only
        // writers of the master-key variables in the grouper-hub test process.
        unsafe {
            std::env::remove_var("GROUPER_MASTER_KEY");
            std::env::remove_var("GROUPER_MASTER_KEY_FILE");
        }
        EnvGuard {
            key,
            file,
            _lock: lock,
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            restore_var("GROUPER_MASTER_KEY", &self.key);
            restore_var("GROUPER_MASTER_KEY_FILE", &self.file);
        }
    }

    fn restore_var(name: &str, value: &Option<std::ffi::OsString>) {
        // SAFETY: caller holds `ENV_LOCK`, so no other backup test reads these variables.
        unsafe {
            match value {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "grouper-backup-{label}-{}-{}",
                std::process::id(),
                hub::db::random_hex(4)
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn setup_hub(dir: &Path) -> String {
        let db = hub::db::Db::open(dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "openai".into(),
            kind: "openai".into(),
            base_url: None,
            api_key: Some("sk-test-secret".into()),
            options: Value::Null,
        })
        .unwrap();
        let id = db.hub_id();
        let thread = dir.join("workspaces").join("thread1");
        fs::create_dir_all(thread.join("nested")).unwrap();
        fs::write(thread.join("nested").join("note.txt"), b"snapshot-me").unwrap();
        fs::write(thread.join("n".repeat(100)), b"long\0name").unwrap();
        drop(db);
        id
    }

    fn copy_key(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        fs::copy(from.join(MASTER_KEY_NAME), to.join(MASTER_KEY_NAME)).unwrap();
    }

    fn manifest_of(dir: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(dir.join(MANIFEST_NAME)).unwrap()).unwrap()
    }

    #[test]
    fn rfc3339_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn gzip_round_trip_covers_block_boundaries() {
        for size in [0usize, 1, 65_535, 65_536, 80_000] {
            let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
            let mut encoder = GzipEncoder::new(Vec::new());
            encoder.write_all(&data).unwrap();
            let bytes = encoder.finish().unwrap();
            let mut decoder = GzipDecoder::new(io::Cursor::new(bytes)).unwrap();
            let mut out = Vec::new();
            decoder.read_to_end(&mut out).unwrap();
            assert_eq!(out, data, "size {size}");
        }
    }

    #[test]
    fn tar_gz_round_trip_long_names_and_empty_files() {
        let tmp = TempDir::new("tar");
        let src = tmp.0.join("src");
        fs::create_dir_all(src.join(WORKSPACES).join("sub")).unwrap();
        fs::write(src.join(WORKSPACES).join("sub").join("a.txt"), b"hello").unwrap();
        fs::write(src.join(WORKSPACES).join("empty"), b"").unwrap();
        let long = "p".repeat(140);
        fs::write(src.join(WORKSPACES).join(&long), b"xyz").unwrap();
        let archive = tmp.0.join("b.tar.gz");
        write_tar_gz(&src, &archive).unwrap();
        let out = tmp.0.join("out");
        extract_tar_gz(&archive, &out).unwrap();
        assert_eq!(fs::read(out.join(WORKSPACES).join("sub").join("a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(out.join(WORKSPACES).join("empty")).unwrap(), b"");
        assert_eq!(fs::read(out.join(WORKSPACES).join(&long)).unwrap(), b"xyz");
    }

    #[test]
    fn backup_during_writes_is_consistent_and_keeps_hub_id() {
        let _env = isolate_master_key();
        let root = TempDir::new("live");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let db_path = data.join(DB_NAME);
        {
            let mut conn = Connection::open(&db_path).unwrap();
            conn.busy_timeout(Duration::from_secs(30)).unwrap();
            conn.execute_batch(
                "CREATE TABLE backup_probe (
                    n INTEGER PRIMARY KEY,
                    left INTEGER NOT NULL,
                    right INTEGER NOT NULL,
                    payload BLOB NOT NULL
                 );
                 CREATE TABLE backup_probe_meta (id INTEGER PRIMARY KEY, writes INTEGER NOT NULL);
                 INSERT INTO backup_probe_meta (id, writes) VALUES (1, 0);",
            )
            .unwrap();
            let payload = vec![0xabu8; 8 * 1024];
            for n in 1..=100 {
                insert_probe(&mut conn, n, &payload);
            }
        }
        // An open hub connection, as when `serve` is running, plus a writer committing rows.
        let live = hub::db::Db::open(&data).unwrap();
        assert_eq!(live.hub_id(), hub_id);
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicI64::new(100));
        let writer = std::thread::spawn({
            let db_path = db_path.clone();
            let stop = Arc::clone(&stop);
            let progress = Arc::clone(&progress);
            move || {
                let mut conn = Connection::open(&db_path).unwrap();
                conn.busy_timeout(Duration::from_secs(30)).unwrap();
                let payload = vec![0xabu8; 8 * 1024];
                let mut n = progress.load(Ordering::SeqCst);
                while !stop.load(Ordering::SeqCst) {
                    n += 1;
                    insert_probe(&mut conn, n, &payload);
                    progress.store(n, Ordering::SeqCst);
                }
                n
            }
        });
        let started = std::time::Instant::now();
        while progress.load(Ordering::SeqCst) < 105 {
            if started.elapsed() > Duration::from_secs(15) {
                panic!("writer made no progress (last {})", progress.load(Ordering::SeqCst));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let before = progress.load(Ordering::SeqCst);
        let snap = root.0.join("snap");
        // The writer keeps committing while this copies pages.
        backup(&data, &snap, true).unwrap();
        stop.store(true, Ordering::SeqCst);
        let after = writer.join().unwrap();
        drop(live);
        assert!(after > before, "writer made no progress around the backup");

        let conn = Connection::open(snap.join(DB_NAME)).unwrap();
        let status: String = conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(status, "ok");
        let id: String = conn
            .query_row(
                "SELECT value FROM hub_meta WHERE key = 'hub_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(id, hub_id);
        let rows = probe_rows(&conn);
        let meta: i64 = conn
            .query_row(
                "SELECT writes FROM backup_probe_meta WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(meta, rows.len() as i64);
        assert!(
            meta >= before,
            "snapshot dropped commits from before the backup"
        );
        assert!(meta <= after);
        for (index, (n, left, right)) in rows.iter().enumerate() {
            let expect = index as i64 + 1;
            assert_eq!(*n, expect);
            assert_eq!(*left, expect);
            assert_eq!(*right, expect);
        }
        drop(conn);

        let restored = root.0.join("restored");
        copy_key(&data, &restored);
        restore(&restored, &snap, false).unwrap();
        let db = hub::db::Db::open(&restored).unwrap();
        assert_eq!(db.hub_id(), hub_id);
        assert_eq!(
            db.provider("openai").unwrap().unwrap().api_key.as_deref(),
            Some("sk-test-secret")
        );
        assert_eq!(
            fs::read(restored.join("workspaces/thread1/nested/note.txt")).unwrap(),
            b"snapshot-me"
        );
    }

    fn insert_probe(conn: &mut Connection, n: i64, payload: &[u8]) {
        let tx = conn.transaction().unwrap();
        tx.execute(
            "INSERT INTO backup_probe (n, left, right, payload) VALUES (?1, ?1, ?1, ?2)",
            params![n, payload],
        )
        .unwrap();
        tx.execute(
            "UPDATE backup_probe_meta SET writes = writes + 1 WHERE id = 1",
            [],
        )
        .unwrap();
        tx.commit().unwrap();
    }

    fn probe_rows(conn: &Connection) -> Vec<(i64, i64, i64)> {
        let mut statement = conn
            .prepare("SELECT n, left, right FROM backup_probe ORDER BY n")
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, rusqlite::Error>>()
            .unwrap()
    }

    #[test]
    fn restore_round_trip_directory_and_archive_keeps_hub_id() {
        let _env = isolate_master_key();
        let root = TempDir::new("round");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();
        assert!(!snap.join(MASTER_KEY_NAME).exists());
        let manifest = manifest_of(&snap);
        assert_eq!(manifest["hub_id"], hub_id);
        assert_eq!(manifest["hub_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(manifest["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(manifest["format"], 1);
        assert!(
            manifest["schema"]["tables"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t == "hub_meta")
        );
        let paths: Vec<&str> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"grouper.db"));
        assert!(paths.iter().all(|path| *path != MASTER_KEY_NAME));
        let long = format!("workspaces/thread1/{}", "n".repeat(100));
        assert!(paths.iter().any(|path| *path == long));

        let restored = root.0.join("restored");
        copy_key(&data, &restored);
        let key_bytes = fs::read(restored.join(MASTER_KEY_NAME)).unwrap();
        restore(&restored, &snap, false).unwrap();
        assert_eq!(fs::read(restored.join(MASTER_KEY_NAME)).unwrap(), key_bytes);
        assert!(!restored.join(MANIFEST_NAME).exists());
        let db = hub::db::Db::open(&restored).unwrap();
        assert_eq!(db.hub_id(), hub_id);
        assert_eq!(
            db.provider("openai").unwrap().unwrap().api_key.as_deref(),
            Some("sk-test-secret")
        );
        drop(db);
        assert_eq!(
            fs::read(restored.join("workspaces/thread1/nested/note.txt")).unwrap(),
            b"snapshot-me"
        );
        assert_eq!(
            fs::read(restored.join("workspaces/thread1").join("n".repeat(100))).unwrap(),
            b"long\0name"
        );

        let archive = root.0.join("snap.tar.gz");
        backup(&data, &archive, true).unwrap();
        let from_archive = root.0.join("from-archive");
        copy_key(&data, &from_archive);
        restore(&from_archive, &archive, false).unwrap();
        let db = hub::db::Db::open(&from_archive).unwrap();
        assert_eq!(db.hub_id(), hub_id);
        assert_eq!(
            db.provider("openai").unwrap().unwrap().api_key.as_deref(),
            Some("sk-test-secret")
        );
        assert_eq!(
            fs::read(from_archive.join("workspaces/thread1/nested/note.txt")).unwrap(),
            b"snapshot-me"
        );
    }

    #[test]
    fn restore_refuses_non_empty_data_dir() {
        let _env = isolate_master_key();
        let root = TempDir::new("nonempty");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();

        let dest = root.0.join("dest");
        let old_id = {
            let db = hub::db::Db::open(&dest).unwrap();
            db.hub_id()
        };
        assert_ne!(old_id, hub_id);
        let error = restore(&dest, &snap, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("not empty"), "{message}");
        assert!(message.contains("--force"), "{message}");
        let db = hub::db::Db::open(&dest).unwrap();
        assert_eq!(db.hub_id(), old_id);
        assert!(db.provider("openai").unwrap().is_none());
    }

    #[test]
    fn force_moves_existing_data_aside() {
        let _env = isolate_master_key();
        let root = TempDir::new("force");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();

        let dest = root.0.join("dest");
        let old_id = {
            let db = hub::db::Db::open(&dest).unwrap();
            db.hub_id()
        };
        fs::copy(data.join(MASTER_KEY_NAME), dest.join(MASTER_KEY_NAME)).unwrap();
        let key = fs::read(dest.join(MASTER_KEY_NAME)).unwrap();
        restore(&dest, &snap, true).unwrap();
        assert_eq!(fs::read(dest.join(MASTER_KEY_NAME)).unwrap(), key);
        let db = hub::db::Db::open(&dest).unwrap();
        assert_eq!(db.hub_id(), hub_id);
        drop(db);

        let aside = fs::read_dir(&dest)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pre-restore-")
            })
            .expect("previous data was not moved aside");
        let old_db = Connection::open(aside.join(DB_NAME)).unwrap();
        let aside_id: String = old_db
            .query_row(
                "SELECT value FROM hub_meta WHERE key = 'hub_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(aside_id, old_id);
        assert!(!aside.join(MASTER_KEY_NAME).exists());
    }

    #[test]
    fn checksum_mismatch_is_refused() {
        let _env = isolate_master_key();
        let root = TempDir::new("checksum");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();
        let note = snap.join("workspaces/thread1/nested/note.txt");
        let mut bytes = fs::read(&note).unwrap();
        bytes[0] ^= 0xff;
        fs::write(&note, bytes).unwrap();

        let dest = root.0.join("dest");
        copy_key(&data, &dest);
        let error = restore(&dest, &snap, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("checksum mismatch"), "{message}");
        assert!(message.contains("note.txt"), "{message}");
        assert!(!dest.join(DB_NAME).exists());
        assert!(!dest.join("workspaces").exists());
        assert!(dest.join(MASTER_KEY_NAME).is_file());
    }

    #[test]
    fn wrong_master_key_is_refused_and_writes_nothing() {
        let _env = isolate_master_key();
        let root = TempDir::new("wrong-key");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();

        let dest = root.0.join("dest");
        fs::create_dir_all(&dest).unwrap();
        let wrong = base64::engine::general_purpose::STANDARD.encode([0x11u8; 32]);
        fs::write(dest.join(MASTER_KEY_NAME), format!("{wrong}\n")).unwrap();
        let error = restore(&dest, &snap, false).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("decrypt") || message.contains("master key"),
            "{message}"
        );
        assert!(
            !dest.join(DB_NAME).exists(),
            "wrong key must not leave a database"
        );
        assert!(!dest.join("workspaces").exists());
        assert!(dest.join(MASTER_KEY_NAME).is_file());
        let entries: Vec<_> = fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1);

        // The original hub id is unchanged, and a later restore with the right key still works.
        let again = root.0.join("again");
        copy_key(&data, &again);
        restore(&again, &snap, false).unwrap();
        assert_eq!(hub::db::Db::open(&again).unwrap().hub_id(), hub_id);
    }

    #[test]
    fn include_workspaces_false_omits_them() {
        let _env = isolate_master_key();
        let root = TempDir::new("nows");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, false).unwrap();
        assert!(!snap.join("workspaces").exists());
        assert!(data.join("workspaces/thread1/nested/note.txt").is_file());
        let manifest = manifest_of(&snap);
        assert_eq!(manifest["include_workspaces"], false);
        let paths: Vec<&str> = manifest["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["grouper.db"]);

        let dest = root.0.join("dest");
        copy_key(&data, &dest);
        restore(&dest, &snap, false).unwrap();
        assert!(!dest.join("workspaces").exists());
        assert!(dest.join(DB_NAME).is_file());
    }

    #[test]
    fn restore_archive_stored_inside_the_data_dir() {
        let _env = isolate_master_key();
        let root = TempDir::new("inside");
        let data = root.0.join("data");
        fs::create_dir_all(&data).unwrap();
        let hub_id = setup_hub(&data);
        let archive = data.join("backups").join("snap.tar.gz");
        backup(&data, &archive, true).unwrap();
        let old_marker = data.join("old-marker.txt");
        fs::write(&old_marker, b"previous").unwrap();
        restore(&data, &archive, true).unwrap();
        assert_eq!(hub::db::Db::open(&data).unwrap().hub_id(), hub_id);
        assert_eq!(
            fs::read(data.join("workspaces/thread1/nested/note.txt")).unwrap(),
            b"snapshot-me"
        );
        assert!(!old_marker.exists());
        let aside = fs::read_dir(&data)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pre-restore-")
            })
            .expect("old data dir contents were not moved aside");
        assert_eq!(fs::read(aside.join("old-marker.txt")).unwrap(), b"previous");
        assert!(aside.join("backups").join("snap.tar.gz").is_file());
    }

    #[test]
    fn restore_refuses_a_held_serve_lock() {
        let _env = isolate_master_key();
        let root = TempDir::new("locked");
        let data = root.0.join("data");
        setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, false).unwrap();
        let dest = root.0.join("dest");
        copy_key(&data, &dest);
        let _serve_lock = lock_data_dir(&dest).unwrap();
        let error = restore(&dest, &snap, false).unwrap_err();
        assert!(error.to_string().contains("serve") || error.to_string().contains("locked"), "{error:#}");
    }

    #[test]
    fn archive_rejects_unexpected_paths_and_total_size_bombs_before_creation() {
        let root = TempDir::new("archive-limits");
        let src = root.0.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("unexpected"), b"no").unwrap();
        let unexpected = root.0.join("unexpected.tar.gz");
        write_tar_gz(&src, &unexpected).unwrap();
        let out = root.0.join("out");
        let error = extract_tar_gz(&unexpected, &out).unwrap_err();
        assert!(error.to_string().contains("unexpected path"), "{error:#}");
        assert!(!out.join("unexpected").exists());

        let error = add_extracted_bytes(MAX_EXTRACTED_BYTES - 1, 2).unwrap_err();
        assert!(error.to_string().contains("expands beyond"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn directory_restore_rejects_symlinks_before_opening_manifest() {
        use std::os::unix::fs::symlink;

        let _env = isolate_master_key();
        let root = TempDir::new("symlink");
        let data = root.0.join("data");
        setup_hub(&data);
        let snap = root.0.join("snap");
        backup(&data, &snap, false).unwrap();
        let outside = root.0.join("outside.json");
        fs::write(&outside, fs::read(snap.join(MANIFEST_NAME)).unwrap()).unwrap();
        fs::remove_file(snap.join(MANIFEST_NAME)).unwrap();
        symlink(&outside, snap.join(MANIFEST_NAME)).unwrap();
        let error = verify_backup(&snap).unwrap_err();
        assert!(error.to_string().contains("symlink"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn backup_and_restore_use_private_unix_modes() {
        let _env = isolate_master_key();
        let root = TempDir::new("modes");
        let data = root.0.join("data");
        setup_hub(&data);
        fs::create_dir_all(data.join(WORKSPACES).join("t")).unwrap();
        fs::write(data.join(WORKSPACES).join("t/file"), b"private").unwrap();
        let snap = root.0.join("snap");
        backup(&data, &snap, true).unwrap();
        assert_eq!(fs::metadata(&snap).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(snap.join(DB_NAME)).unwrap().permissions().mode() & 0o777, 0o600);
        let restored = root.0.join("restored");
        copy_key(&data, &restored);
        restore(&restored, &snap, false).unwrap();
        assert_eq!(fs::metadata(&restored).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(restored.join(DB_NAME)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::metadata(restored.join(WORKSPACES).join("t")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(restored.join(WORKSPACES).join("t/file")).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
