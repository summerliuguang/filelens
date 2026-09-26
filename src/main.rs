use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};

/// Latest schema version this build understands. Older databases are
/// upgraded stepwise through MIGRATIONS on open; newer ones are rejected so a
/// downgrade can never misread an unknown schema.
const SCHEMA_VERSION: i64 = 8;
const COPY_BUFFER_SIZE: usize = 8 * 1024 * 1024;
const QUICK_HASH_CHUNK: u64 = 64 * 1024;
/// Files above this size get the two-stage hash: quick fingerprint first,
/// full BLAKE3 only if a same-(size, quick) sibling shows up.
const LARGE_FILE_QUICK_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Stepwise schema upgrades: entry N upgrades a version-N database to N+1.
/// Each runs inside a transaction with its schema_version bump.
const MIGRATIONS: &[&str] = &[
    // v1 -> v2: quick fingerprint column for the two-stage large-file hash.
    "ALTER TABLE files ADD COLUMN quick_hash TEXT;
     CREATE INDEX IF NOT EXISTS files_size_quick_hash ON files(size, quick_hash);",
    // v2 -> v3: device/inode pair for hardlink detection (0/0 on platforms
    // without POSIX metadata, where the feature degrades to "unknown").
    "ALTER TABLE files ADD COLUMN dev INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE files ADD COLUMN inode INTEGER NOT NULL DEFAULT 0;",
    // v3 -> v4: batch id grouping the operations of one user action so the
    // recycle bin can be presented (and reasoned about) per cleanup.
    "ALTER TABLE operations ADD COLUMN batch_id INTEGER;
     CREATE INDEX IF NOT EXISTS operations_batch ON operations(batch_id);",
    // v4 -> v5: pHash column for photo fingerprints (old rows are dropped, so
    // the missing-fingerprint rule in is_unchanged re-fingerprints every
    // image on the next scan), plus the NTFS file-reference column used by
    // the optional USN fast scan on Windows. The fingerprint table is
    // rebuilt rather than altered so databases predating the table upgrade
    // cleanly too.
    "CREATE TABLE photo_fingerprints_new (
       file_id INTEGER PRIMARY KEY,
       dhash INTEGER NOT NULL,
       phash INTEGER NOT NULL DEFAULT 0,
       part_a INTEGER NOT NULL,
       part_b INTEGER NOT NULL,
       part_c INTEGER NOT NULL,
       part_d INTEGER NOT NULL,
       part_e INTEGER NOT NULL,
       FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
     );
     DROP TABLE IF EXISTS photo_fingerprints;
     ALTER TABLE photo_fingerprints_new RENAME TO photo_fingerprints;
     CREATE INDEX IF NOT EXISTS photo_fingerprints_parts ON photo_fingerprints(part_a, part_b, part_c, part_d, part_e);
     ALTER TABLE files ADD COLUMN frn INTEGER NOT NULL DEFAULT 0;",
    // v5 -> v6: cache pixel-verification verdicts for duplicate-photo pairs.
    // Rows are keyed by the two paths and the index sizes/mtimes at check
    // time; any rescan that changes either file invalidates the row through
    // the join used on lookup, so nothing needs active eviction.
    "CREATE TABLE IF NOT EXISTS photo_pair_verdicts (
       path_a TEXT NOT NULL,
       size_a INTEGER NOT NULL,
       mtime_a INTEGER NOT NULL,
       path_b TEXT NOT NULL,
       size_b INTEGER NOT NULL,
       mtime_b INTEGER NOT NULL,
       identical INTEGER NOT NULL,
       checked_at INTEGER NOT NULL,
       PRIMARY KEY (path_a, path_b)
     );",
    // v6 -> v7: identity lookups for rename/move detection. A file that
    // changed path since the last scan is matched back to its old row by
    // NTFS file reference (Windows) or device/inode (POSIX), so moves do
    // not pay a full re-hash.
    "CREATE INDEX IF NOT EXISTS files_frn ON files(frn);
     CREATE INDEX IF NOT EXISTS files_dev_ino ON files(dev, inode);",
    // v7 -> v8: EXIF capture time for images (unix seconds, 0 = unknown),
    // parsed during the scan for containers that carry EXIF. A failed
    // re-parse keeps the previous value while the content stays identical.
    "ALTER TABLE files ADD COLUMN exif_taken INTEGER NOT NULL DEFAULT 0;",
];

fn create_schema_sql() -> &'static str {
    "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
     CREATE TABLE IF NOT EXISTS files (
       id INTEGER PRIMARY KEY,
       path TEXT NOT NULL UNIQUE,
       size INTEGER NOT NULL,
       modified INTEGER NOT NULL,
       hash TEXT NOT NULL,
       protected INTEGER NOT NULL DEFAULT 0,
       approved INTEGER NOT NULL DEFAULT 0,
       present INTEGER NOT NULL DEFAULT 1,
       scanned_at INTEGER NOT NULL,
       quick_hash TEXT,
       dev INTEGER NOT NULL DEFAULT 0,
       inode INTEGER NOT NULL DEFAULT 0,
       frn INTEGER NOT NULL DEFAULT 0,
       exif_taken INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX IF NOT EXISTS files_hash_size_present ON files(hash, size, present);
     CREATE INDEX IF NOT EXISTS files_size_quick_hash ON files(size, quick_hash);
     CREATE TABLE IF NOT EXISTS photo_fingerprints (
       file_id INTEGER PRIMARY KEY,
       dhash INTEGER NOT NULL,
       phash INTEGER NOT NULL DEFAULT 0,
       part_a INTEGER NOT NULL,
       part_b INTEGER NOT NULL,
       part_c INTEGER NOT NULL,
       part_d INTEGER NOT NULL,
       part_e INTEGER NOT NULL,
       FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
     );
     CREATE INDEX IF NOT EXISTS photo_fingerprints_parts ON photo_fingerprints(part_a, part_b, part_c, part_d, part_e);
     CREATE TABLE IF NOT EXISTS document_fingerprints (
       file_id INTEGER PRIMARY KEY,
       simhash INTEGER NOT NULL,
       token_count INTEGER NOT NULL,
       FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
     );
     CREATE INDEX IF NOT EXISTS document_fingerprints_hash ON document_fingerprints(simhash);
     CREATE TABLE IF NOT EXISTS operations (
       id INTEGER PRIMARY KEY,
       file_id INTEGER NOT NULL,
       source_path TEXT NOT NULL,
       trash_path TEXT NOT NULL,
       hash TEXT NOT NULL,
       state TEXT NOT NULL,
       created_at INTEGER NOT NULL,
       restored_at INTEGER,
       batch_id INTEGER,
       FOREIGN KEY(file_id) REFERENCES files(id)
     );
     CREATE INDEX IF NOT EXISTS operations_batch ON operations(batch_id);
     CREATE TABLE IF NOT EXISTS photo_pair_verdicts (
       path_a TEXT NOT NULL,
       size_a INTEGER NOT NULL,
       mtime_a INTEGER NOT NULL,
       path_b TEXT NOT NULL,
       size_b INTEGER NOT NULL,
       mtime_b INTEGER NOT NULL,
       identical INTEGER NOT NULL,
       checked_at INTEGER NOT NULL,
       PRIMARY KEY (path_a, path_b)
     );
     CREATE INDEX IF NOT EXISTS files_frn ON files(frn);
     CREATE INDEX IF NOT EXISTS files_dev_ino ON files(dev, inode);"
}

#[derive(Parser)]
#[command(
    name = "filelens",
    version,
    about = "Local-first exact duplicate finder"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create or open a project database and configure its recycle bin.
    Init { database: PathBuf, trash: PathBuf },
    /// Index roots and calculate full BLAKE3 hashes for changed files.
    Scan {
        database: PathBuf,
        #[arg(long = "root", required = true)]
        roots: Vec<PathBuf>,
        /// Mark matching source paths protected from recycle operations.
        #[arg(long)]
        protect: Vec<String>,
        /// Skip directories whose normalized path contains one of these rules.
        #[arg(long)]
        exclude: Vec<String>,
        /// Ignore files smaller than this many bytes.
        #[arg(long, default_value_t = 0)]
        min_size: u64,
    },
    /// Print exact duplicate groups and their review state.
    Groups { database: PathBuf },
    /// Explicitly approve a duplicate item for recycle-bin movement.
    Approve {
        database: PathBuf,
        #[arg(long)]
        file_id: i64,
    },
    /// Remove approval without changing the source file.
    Unapprove {
        database: PathBuf,
        #[arg(long)]
        file_id: i64,
    },
    /// Move one approved exact duplicate to the configured recycle bin.
    Trash {
        database: PathBuf,
        #[arg(long)]
        file_id: i64,
    },
    /// Replace an approved duplicate with a hard link to its kept copy.
    Hardlink {
        database: PathBuf,
        #[arg(long)]
        file_id: i64,
    },
    /// Enable or disable the strict pre-delete byte comparison.
    StrictVerify {
        database: PathBuf,
        #[arg(long)]
        enabled: bool,
    },
    /// Restore a recycled file to its recorded original path.
    Restore {
        database: PathBuf,
        #[arg(long)]
        operation_id: i64,
    },
    /// Show indexed-file, duplicate, and recycle-bin counts.
    Status { database: PathBuf },
    /// Print files currently available in the application recycle bin.
    TrashList { database: PathBuf },
    /// Export the current duplicate groups as a CSV report.
    Export {
        database: PathBuf,
        output: PathBuf,
    },
}

#[derive(Debug)]
struct IndexedFile {
    id: i64,
    path: String,
    size: i64,
    hash: String,
    protected: bool,
    approved: bool,
    /// POSIX device/inode identity; (0, 0) where the platform has none.
    dev: i64,
    inode: i64,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    match Cli::parse().command {
        Command::Init { database, trash } => init(&database, &trash),
        Command::Scan {
            database,
            roots,
            protect,
            exclude,
            min_size,
        } => {
            scan_with_options(&database, &roots, &protect, &exclude, min_size, false)?;
            Ok(())
        }
        Command::Groups { database } => groups(&database),
        Command::Approve { database, file_id } => set_approval(&database, file_id, true),
        Command::Unapprove { database, file_id } => set_approval(&database, file_id, false),
        Command::Trash { database, file_id } => trash(&database, file_id),
        Command::Hardlink { database, file_id } => hardlink(&database, file_id),
        Command::StrictVerify {
            database,
            enabled,
        } => set_strict_verify(&database, enabled),
        Command::Restore {
            database,
            operation_id,
        } => restore(&database, operation_id),
        Command::Status { database } => status(&database),
        Command::TrashList { database } => trash_list(&database),
        Command::Export { database, output } => {
            let rows = export_report(&database, &output)?;
            println!("Exported {rows} rows to {}", output.display());
            Ok(())
        }
    }
}

fn open_database(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create database directory: {e}"))?;
    }
    let connection = Connection::open(path).map_err(|e| format!("open database: {e}"))?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )
        .map_err(|e| format!("configure database: {e}"))?;
    Ok(connection)
}

pub fn init(database: &Path, trash: &Path) -> Result<(), String> {
    fs::create_dir_all(trash).map_err(|e| format!("create recycle bin: {e}"))?;
    let connection = open_database(database)?;
    let initialized = connection
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='settings'",
            [],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if initialized.is_some() {
        // Existing project: migrate stepwise. Re-running the current-schema
        // DDL would fail on older databases whose tables lack the columns
        // the new indexes reference.
        ensure_initialized(&connection)?;
    } else {
        connection
            .execute_batch(create_schema_sql())
            .map_err(|e| format!("create schema: {e}"))?;
        set_setting(&connection, "schema_version", &SCHEMA_VERSION.to_string())?;
    }
    set_setting(
        &connection,
        "trash_path",
        &absolute_path(trash)?.to_string_lossy(),
    )?;
    println!("Project initialized: {}", database.display());
    println!("Recycle bin: {}", absolute_path(trash)?.display());
    Ok(())
}

pub fn scan(database: &Path, roots: &[PathBuf], protect: &[String]) -> Result<(), String> {
    scan_with_options(database, roots, protect, &[], 0, false).map(|_| ())
}

/// Scan with user-configured directory exclusions and a minimum file size.
/// Exclusions reuse the protection-rule matching semantics (substring on the
/// normalized path, case-insensitive on Windows): over-excluding only hides
/// files, while a false-negative would put noise back into the report.
pub fn scan_with_options(
    database: &Path,
    roots: &[PathBuf],
    protect: &[String],
    exclude: &[String],
    min_file_size: u64,
    silent: bool,
) -> Result<ScanSummary, String> {
    scan_with_control(
        database,
        roots,
        protect,
        exclude,
        min_file_size,
        silent,
        &|| false,
        &|_, _, _| {},
        None,
        None,
    )
}

/// Aggregated outcome of one scan run: counters for the CLI report plus the
/// expired-recycle cleanup count and skipped roots for the GUI completion
/// message.
pub struct ScanSummary {
    pub new: u64,
    pub unchanged: u64,
    pub updated: u64,
    pub skipped: u64,
    pub errors: u64,
    pub missing: u64,
    pub pruned: u64,
    /// Long-absent index rows (90 days, never touched by an operation)
    /// removed as housekeeping after this scan.
    pub index_pruned: u64,
    /// Present images whose photo fingerprint had to be recomputed this scan
    /// (e.g. after a fingerprint-algorithm migration), so the UI can explain
    /// why a routine incremental scan takes longer than usual.
    pub fingerprint_rebuild: u64,
    pub failed_roots: Vec<String>,
}

/// How many per-file failure samples the scan keeps for the GUI; the full
/// count keeps accumulating so the UI can say "N failed" after wrap-around.
const SCAN_ERROR_SAMPLES: usize = 20;

/// Ring buffer of recent per-file scan failures, shared between the scan
/// pipeline and the GUI task slot. Pushed from the hashing workers' results,
/// read from the `scan_state` poller; a poisoned lock degrades to best-effort
/// access instead of failing the scan.
#[derive(Default)]
pub struct ScanErrorLog {
    inner: Mutex<ScanErrorLogInner>,
}

#[derive(Default)]
struct ScanErrorLogInner {
    total: u64,
    recent: std::collections::VecDeque<(String, String)>,
}

impl ScanErrorLog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, path: &str, error: &str) {
        let mut inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        inner.total += 1;
        if inner.recent.len() >= SCAN_ERROR_SAMPLES {
            inner.recent.pop_front();
        }
        inner
            .recent
            .push_back((path.to_string(), error.to_string()));
    }

    /// (total failures, most recent samples in recording order)
    pub fn snapshot(&self) -> (u64, Vec<(String, String)>) {
        let inner = match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        (inner.total, inner.recent.iter().cloned().collect())
    }
}

/// Live byte counters for the full-hash stage of a running scan, shared
/// between hashing workers and the GUI task slot. Workers call `begin`/`add`;
/// the `scan_state` poller reads `snapshot`. A poisoned lock degrades to
/// best-effort access exactly like `ScanErrorLog`.
#[derive(Default)]
pub struct HashProgress {
    current_path: Mutex<Option<String>>,
    bytes_done: AtomicU64,
}

impl HashProgress {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&self, path: &Path) {
        if let Ok(mut current) = self.current_path.lock() {
            *current = Some(path.to_string_lossy().into_owned());
        }
    }

    fn add(&self, bytes: u64) {
        self.bytes_done.fetch_add(bytes, Ordering::Relaxed);
    }

    /// (most recently started file, cumulative bytes hashed this scan)
    pub fn snapshot(&self) -> (Option<String>, u64) {
        let current = match self.current_path.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        (current, self.bytes_done.load(Ordering::Relaxed))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn scan_with_control(
    database: &Path,
    roots: &[PathBuf],
    protect: &[String],
    exclude: &[String],
    min_file_size: u64,
    silent: bool,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
    progress: &(dyn Fn(u64, u64, Option<&str>) + Send + Sync),
    errors: Option<&ScanErrorLog>,
    hash_progress: Option<&HashProgress>,
) -> Result<ScanSummary, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let trash = PathBuf::from(required_setting(&connection, "trash_path")?);
    let scan_started = now_seconds()?;
    let mut counters = ScanCounters::default();
    // Fingerprints wiped by an algorithm upgrade come back through the
    // regular scan; counting them up front lets the UI set expectations
    // ("this incremental scan re-fingerprints N images").
    let fingerprint_rebuild = count_fingerprint_rebuild(&connection)?;
    if fingerprint_rebuild > 0 && !silent {
        println!(
            "Re-fingerprinting {fingerprint_rebuild} image(s) this scan (fingerprint format changed or rows missing)."
        );
    }
    let absolute_roots = roots
        .iter()
        .map(|root| absolute_path(root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut scanned_roots = Vec::new();
    let mut failed_roots = Vec::new();
    let total = count_entries(&absolute_roots, &trash, exclude, cancelled)?;
    progress(0, total, None);
    for root in &absolute_roots {
        if cancelled() {
            return Err("scan cancelled".to_string());
        }
        if !root.is_dir() {
            failed_roots.push(root.to_string_lossy().into_owned());
            eprintln!(
                "warning: skipping scan root that is not a directory: {}",
                root.display()
            );
            continue;
        }
        run_parallel_scan(
            &connection,
            database,
            root,
            &trash,
            protect,
            exclude,
            min_file_size,
            &mut counters,
            cancelled,
            progress,
            errors,
            hash_progress,
            total,
        )?;
        scanned_roots.push(root.clone());
    }
    let removed = mark_missing_absent(&connection, &scanned_roots, scan_started)?;
    let pruned = prune_expired_trash_with(&connection, now_seconds()?)?;
    let index_pruned = prune_absent_rows(&connection)?;
    set_setting(&connection, "last_scan_at", &now_seconds()?.to_string())?;
    if !silent {
        println!(
            "Scanned: {} new, {} unchanged, {} updated, {} skipped, {} errors, {} missing, {} expired recycled, {} stale index rows.",
            counters.new,
            counters.unchanged,
            counters.updated,
            counters.skipped,
            counters.errors,
            removed,
            pruned,
            index_pruned
        );
        if !failed_roots.is_empty() {
            println!("Skipped missing roots: {}", failed_roots.join(", "));
        }
    }
    Ok(ScanSummary {
        new: counters.new,
        unchanged: counters.unchanged,
        updated: counters.updated,
        skipped: counters.skipped,
        errors: counters.errors,
        missing: removed,
        pruned,
        index_pruned,
        fingerprint_rebuild,
        failed_roots,
    })
}

/// Pre-walk the roots counting the entries the scan will report, so progress
/// can be shown as a fraction. Counts mirror scan_directory: every entry in a
/// walked directory counts once, except non-excluded subdirectories which are
/// walked instead of counted.
fn count_entries(
    roots: &[PathBuf],
    trash: &Path,
    exclude: &[String],
    cancelled: &dyn Fn() -> bool,
) -> Result<u64, String> {
    let mut total = 0_u64;
    let mut directories: Vec<PathBuf> = roots
        .iter()
        .filter(|root| root.is_dir())
        .cloned()
        .collect();
    while let Some(directory) = directories.pop() {
        if cancelled() {
            return Err("scan cancelled".to_string());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => {
                total += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            if cancelled() {
                return Err("scan cancelled".to_string());
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() && !file_type.is_symlink() => {
                    if is_excluded(&path, trash, exclude) {
                        total += 1;
                    } else {
                        directories.push(path);
                    }
                }
                Ok(_) => total += 1,
                Err(_) => total += 1,
            }
        }
    }
    Ok(total)
}

/// Mark files under the successfully scanned roots that were not seen this
/// round (scanned_at older than the scan start) as absent, so files deleted
/// outside the application stop showing up in duplicate groups.
fn mark_missing_absent(
    connection: &Connection,
    roots: &[PathBuf],
    scan_started: i64,
) -> Result<u64, String> {
    let mut removed = 0_u64;
    for root in roots {
        let root_text = root.to_string_lossy();
        let root_str: &str = &root_text;
        let escaped = like_escape(root_str);
        let posix_pattern = format!("{escaped}/%");
        let windows_pattern = format!("{escaped}\\\\%");
        let changed = connection
            .execute(
                "UPDATE files SET present=0, approved=0
                 WHERE present=1 AND scanned_at < ?1
                   AND (path = ?2 OR path LIKE ?3 ESCAPE '\\' OR path LIKE ?4 ESCAPE '\\')",
                params![scan_started, root_str, posix_pattern, windows_pattern],
            )
            .map_err(|e| e.to_string())?;
        removed += changed as u64;
    }
    Ok(removed)
}

/// Index housekeeping: absent rows (present = 0) that have stayed stale for
/// 90 days are dropped — but only when no operation ever referenced them, so
/// the recycle-history anchors that restore depends on are never touched.
/// Rows merely disappeared externally (deleted outside the app) re-hash for
/// free if they ever come back after this horizon.
fn prune_absent_rows(connection: &Connection) -> Result<u64, String> {
    let cutoff = now_seconds()? - 90 * 86_400;
    connection.execute(
        "DELETE FROM photo_fingerprints WHERE file_id IN \
           (SELECT id FROM files WHERE present = 0 AND scanned_at < ?1 \
             AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.file_id = files.id))",
        params![cutoff],
    )
    .map_err(|e| e.to_string())?;
    connection.execute(
        "DELETE FROM document_fingerprints WHERE file_id IN \
           (SELECT id FROM files WHERE present = 0 AND scanned_at < ?1 \
             AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.file_id = files.id))",
        params![cutoff],
    )
    .map_err(|e| e.to_string())?;
    let changed = connection
        .execute(
            "DELETE FROM files WHERE present = 0 AND scanned_at < ?1 \
             AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.file_id = files.id)",
            params![cutoff],
        )
        .map_err(|e| e.to_string())?;
    Ok(changed as u64)
}

fn like_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[derive(Default)]
struct ScanCounters {
    new: u64,
    unchanged: u64,
    updated: u64,
    skipped: u64,
    errors: u64,
}

struct ProcessedEntry {
    path_text: String,
    size: i64,
    modified: i64,
    hash: String,
    /// BLAKE3 over the first QUICK_HASH_CHUNK bytes, present only for files
    /// above the quick threshold whose full hash was deferred; `hash` is then
    /// empty until the writer promotes the entry to a full hash.
    quick_hash: Option<String>,
    /// POSIX device/inode pair identifying the physical file; (0, 0) where
    /// the platform has no such metadata.
    dev: i64,
    inode: i64,
    /// NTFS file reference number, populated by the scan workers on
    /// Windows; 0 elsewhere.
    frn: i64,
    /// EXIF capture time (unix seconds; 0 = unknown or non-image).
    exif_taken: i64,
    photo: Option<(u64, u64)>,
    document: Option<(u64, i64)>,
}

/// POSIX identity of a file for hardlink detection; (0, 0) on platforms
/// without `MetadataExt`, where hardlink marking stays off.
#[cfg(unix)]
fn device_inode(metadata: &fs::Metadata) -> (i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (
        i64::try_from(metadata.dev()).unwrap_or(0),
        i64::try_from(metadata.ino()).unwrap_or(0),
    )
}

#[cfg(not(unix))]
fn device_inode(_metadata: &fs::Metadata) -> (i64, i64) {
    (0, 0)
}

enum WorkResult {
    Unchanged(String),
    Skipped,
    WalkError(String),
    Processed(Result<ProcessedEntry, (String, String)>),
    /// A file that changed path since the last scan, already matched back to
    /// its previous index row: the writer re-points the row in place instead
    /// of re-hashing the content.
    Moved { path: String, file_id: i64 },
}

/// Hash and fingerprint one file on a worker thread. Pure file I/O: the
/// database is only touched by the writer thread.
fn process_file(path: PathBuf, hash_progress: Option<&HashProgress>) -> WorkResult {
    let path_text = match absolute_path(&path) {
        Ok(absolute) => absolute.to_string_lossy().into_owned(),
        Err(error) => {
            return WorkResult::Processed(Err((path.to_string_lossy().into_owned(), error)))
        }
    };
    let work = || -> Result<ProcessedEntry, String> {
        let metadata = fs::metadata(&path).map_err(|e| e.to_string())?;
        let size =
            i64::try_from(metadata.len()).map_err(|_| "file is too large".to_string())?;
        let modified = unix_seconds(metadata.modified().map_err(|e| e.to_string())?)?;
        // Large files only pay for the quick fingerprint here; the full hash
        // is deferred until the writer finds a same-(size, quick) sibling,
        // so same-size archives/videos with different content never get read
        // in full.
        let (hash, quick_hash) = if size as u64 > LARGE_FILE_QUICK_THRESHOLD {
            (String::new(), Some(quick_hash_file(&path)?))
        } else {
            (hash_file_progress(&path, hash_progress)?, None)
        };
        let after = fs::metadata(&path).map_err(|e| e.to_string())?;
        if after.len() != metadata.len()
            || unix_seconds(after.modified().map_err(|e| e.to_string())?)? != modified
        {
            return Err("file changed while hashing; retry on next scan".to_string());
        }
        let photo = perceptual_hash(&path);
        let document = document_simhash(&path);
        let (dev, inode) = device_inode(&metadata);
        #[cfg(windows)]
        let frn = i64::try_from(usn::file_reference_of(&path).unwrap_or(0)).unwrap_or(0);
        #[cfg(not(windows))]
        let frn = 0;
        let exif_taken = if is_image_path(&path) {
            exif_taken_seconds(&path)
        } else {
            0
        };
        Ok(ProcessedEntry {
            path_text: path_text.clone(),
            size,
            modified,
            hash,
            quick_hash,
            dev,
            inode,
            frn,
            exif_taken,
            photo,
            document,
        })
    };
    match work() {
        Ok(entry) => WorkResult::Processed(Ok(entry)),
        Err(error) => WorkResult::Processed(Err((path_text, error))),
    }
}

/// Walk one root, sending unchanged files straight to the writer and the rest
/// to the hashing workers. Uses its own read-only view of the database. When
/// the optional USN fast scan is enabled (and permitted), the file list comes
/// from the MFT instead of directory recursion.
#[allow(clippy::too_many_arguments)]
fn walk_root(
    database: &Path,
    root: &Path,
    trash: &Path,
    exclude: &[String],
    min_file_size: u64,
    work_tx: &std::sync::mpsc::Sender<PathBuf>,
    result_tx: &std::sync::mpsc::Sender<WorkResult>,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), String> {
    let connection = open_database(database)?;
    if usn_scan_enabled(&connection) {
        match usn_root_paths(root) {
            Some(Ok(paths)) => {
                for path in paths {
                    if cancelled() {
                        return Err("scan cancelled".to_string());
                    }
                    dispatch_path(
                        &connection,
                        &path,
                        trash,
                        exclude,
                        min_file_size,
                        work_tx,
                        result_tx,
                    );
                }
                return Ok(());
            }
            Some(Err(error)) => {
                // Permission or volume problems degrade to the plain walk;
                // the scan must never fail because the accelerator is off.
                eprintln!("warning: USN fast scan unavailable, walking normally: {error}");
            }
            None => {}
        }
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        if cancelled() {
            return Err("scan cancelled".to_string());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("warning: cannot read {}: {error}", directory.display());
                let _ = result_tx.send(WorkResult::WalkError(
                    directory.to_string_lossy().into_owned(),
                ));
                continue;
            }
        };
        for entry in entries.flatten() {
            if cancelled() {
                return Err("scan cancelled".to_string());
            }
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                let _ = result_tx.send(WorkResult::Skipped);
                continue;
            };
            if file_type.is_symlink() {
                let _ = result_tx.send(WorkResult::Skipped);
                continue;
            }
            if file_type.is_dir() {
                if is_excluded(&path, trash, exclude) {
                    let _ = result_tx.send(WorkResult::Skipped);
                } else {
                    directories.push(path);
                }
                continue;
            }
            if !file_type.is_file() {
                let _ = result_tx.send(WorkResult::Skipped);
                continue;
            }
            dispatch_path(
                &connection,
                &path,
                trash,
                exclude,
                min_file_size,
                work_tx,
                result_tx,
            );
        }
    }
    Ok(())
}

/// Shared per-file decision for both walkers: exclusion, minimum size,
/// unchanged short-circuit, otherwise hand the path to the hashing workers.
fn dispatch_path(
    connection: &Connection,
    path: &Path,
    trash: &Path,
    exclude: &[String],
    min_file_size: u64,
    work_tx: &std::sync::mpsc::Sender<PathBuf>,
    result_tx: &std::sync::mpsc::Sender<WorkResult>,
) {
    if is_excluded(path, trash, exclude) {
        let _ = result_tx.send(WorkResult::Skipped);
        return;
    }
    // Below-threshold files are dropped from the report entirely: they are
    // skipped here (not marked seen), so a previous index row is marked
    // absent by mark_missing_absent after the scan.
    if min_file_size > 0 {
        let too_small = fs::metadata(path)
            .map(|metadata| metadata.len() < min_file_size)
            .unwrap_or(false);
        if too_small {
            let _ = result_tx.send(WorkResult::Skipped);
            return;
        }
    }
    if is_unchanged(connection, path) {
        let _ = result_tx.send(WorkResult::Unchanged(
            path.to_string_lossy().into_owned(),
        ));
    } else if let Some(file_id) = find_moved_row(connection, path) {
        // The content was not re-hashed: the writer only re-points the old
        // row (path and flags), keeping hash and fingerprints via file_id.
        let _ = result_tx.send(WorkResult::Moved {
            path: path.to_string_lossy().into_owned(),
            file_id,
        });
    } else if work_tx.send(path.to_path_buf()).is_err() {
        // Workers only exit early on cancellation; a send error surfaces as
        // a cancelled scan on the walker's own next check.
    }
}

/// The previous index row for a file whose path changed since the last scan:
/// matched by NTFS file reference on Windows and by device/inode on POSIX,
/// with size and mtime still equal (a pure rename preserves both). None when
/// the platform cannot identify files this way or nothing matches.
#[cfg(windows)]
fn find_moved_row(connection: &Connection, path: &Path) -> Option<i64> {
    let metadata = fs::metadata(path).ok()?;
    let size = i64::try_from(metadata.len()).ok()?;
    let modified = unix_seconds(metadata.modified().ok()?).ok()?;
    let frn = i64::try_from(usn::file_reference_of(path).ok()?).ok()?;
    if frn == 0 {
        return None;
    }
    connection
        .query_row(
            "SELECT id FROM files WHERE present = 1 AND frn = ?1 AND size = ?2 \
             AND modified = ?3 AND path <> ?4 LIMIT 1",
            params![frn, size, modified, path.to_string_lossy()],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

#[cfg(not(windows))]
fn find_moved_row(connection: &Connection, path: &Path) -> Option<i64> {
    let metadata = fs::metadata(path).ok()?;
    let size = i64::try_from(metadata.len()).ok()?;
    let modified = unix_seconds(metadata.modified().ok()?).ok()?;
    let (dev, inode) = device_inode(&metadata);
    if dev == 0 || inode == 0 {
        return None;
    }
    connection
        .query_row(
            "SELECT id FROM files WHERE present = 1 AND dev = ?1 AND inode = ?2 \
             AND size = ?3 AND modified = ?4 AND path <> ?5 LIMIT 1",
            params![dev, inode, size, modified, path.to_string_lossy()],
            |row| row.get(0),
        )
        .optional()
        .ok()
        .flatten()
}

// --- Optional USN fast scan (Windows) ----------------------------------------

/// Setting read for the MFT-order file listing. Off by default: the fast
/// path only engages for elevated processes (NTFS refuses generic volume
/// reads otherwise) and silently degrades to the directory walk everywhere
/// else.
fn usn_scan_enabled(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key='usn_scan'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|value| value == "1")
        .unwrap_or(false)
}

pub fn set_usn_scan(database: &Path, enabled: bool) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    set_setting(&connection, "usn_scan", if enabled { "1" } else { "0" })
}

/// Similar-photo decision threshold: the maximum pHash distance for the
/// "similar" view. Stored clamped so a stray value can neither flood the
/// review queue nor silently hide everything; default 10.
pub fn set_similar_threshold(database: &Path, max_distance: i64) -> Result<i64, String> {
    let clamped = max_distance.clamp(4, 20);
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    set_setting(&connection, "similar_phash_max", &clamped.to_string())?;
    Ok(clamped)
}

/// Persist the real-time watch preference; the desktop layer owns the actual
/// watcher lifecycle.
pub fn set_watch_scan(database: &Path, enabled: bool) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    set_setting(&connection, "watch_scan", if enabled { "1" } else { "0" })
}

/// File list for one root from the NTFS change journal. `None` off Windows;
/// `Some(Err(..))` whenever the journal cannot be read (permissions, non-NTFS
/// volume) so the caller can walk the directory tree instead.
#[cfg(windows)]
fn usn_root_paths(root: &Path) -> Option<Result<Vec<PathBuf>, String>> {
    Some(usn::root_paths(root))
}

#[cfg(not(windows))]
fn usn_root_paths(root: &Path) -> Option<Result<Vec<PathBuf>, String>> {
    let _ = root;
    None
}

#[cfg(windows)]
mod usn {
    use std::collections::HashMap;
    use std::os::windows::ffi::OsStrExt;
    use std::path::{Path, PathBuf};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        FILE_ATTRIBUTE_DIRECTORY, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    const FSCTL_ENUM_USN_DATA: u32 = 0x000900B3;
    const FSCTL_GET_NTFS_USN_JOURNAL: u32 = 0x000900E4;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    const ENUM_BUFFER: usize = 16 * 1024 * 1024;

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.0) };
        }
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    fn ctl_code(device: u32, function: u32, method: u32, access: u32) -> u32 {
        (device << 16) | (access << 14) | (function << 2) | method
    }

    fn open_volume(drive: &str) -> Result<Handle, String> {
        let wide: Vec<u16> = drive.encode_utf16().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("open volume {drive}: {}", last_error()));
        }
        Ok(Handle(handle))
    }

    fn device_io(control: u32, handle: HANDLE, input: &[u8], output: &mut [u8]) -> Result<usize, String> {
        let mut returned = 0_u32;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                control,
                input.as_ptr() as *const _,
                input.len() as u32,
                output.as_mut_ptr() as *mut _,
                output.len() as u32,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(last_error());
        }
        Ok(returned as usize)
    }

    // #pragma pack(4) in the SDK; repr(C) on x64 gives the same 48-byte size.
    #[repr(C)]
    struct MftEnumDataV0 {
        start_usn: u64,
        reason_mask: u64,
        return_only_on_close: u64,
        timeout: u64,
        bytes_to_wait_for: u64,
        usn_journal_id: u32,
    }

    #[repr(C)]
    struct UsnJournalData {
        usn_journal_id: u64,
        first_usn: i64,
        next_usn: i64,
        lowest_valid_usn: i64,
        max_usn: i64,
        maximum_size: u64,
        allocation_delta: u64,
    }

    /// Every file (and directory) below `root`, in MFT order. Directory
    /// recursion is replaced by a parent-reference walk over the journal's
    /// records; reparse points are pruned exactly like the plain walker.
    pub fn root_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
        let root_text = root.to_string_lossy().into_owned();
        let bytes = root_text.as_bytes();
        if bytes.len() < 2 || bytes[1] != b':' {
            return Err("USN fast scan supports drive-letter roots only".into());
        }
        let drive_letter = root_text[..1].to_ascii_uppercase();
        let volume_path = format!(r"\\.\{drive_letter}:");
        let volume = open_volume(&volume_path)?;

        let mut journal = [0_u8; std::mem::size_of::<UsnJournalData>()];
        let read = device_io(FSCTL_GET_NTFS_USN_JOURNAL, volume.0, &[], &mut journal)?;
        if read < std::mem::size_of::<UsnJournalData>() {
            return Err("short USN journal reply".into());
        }
        let journal: UsnJournalData = unsafe { std::ptr::read(journal.as_ptr() as *const _) };

        let mut input = MftEnumDataV0 {
            start_usn: 0,
            reason_mask: 0,
            return_only_on_close: 0,
            timeout: 0,
            bytes_to_wait_for: 0,
            usn_journal_id: journal.usn_journal_id as u32,
        };
        // frn -> (parent frn, file name, attributes)
        let mut entries: HashMap<u64, (u64, String, u32)> = HashMap::new();
        let mut output = vec![0_u8; ENUM_BUFFER];
        loop {
            let read = device_io(
                FSCTL_ENUM_USN_DATA,
                volume.0,
                unsafe { std::slice::from_raw_parts((&input as *const MftEnumDataV0).cast::<u8>(), std::mem::size_of::<MftEnumDataV0>()) },
                &mut output,
            )?;
            if read <= std::mem::size_of::<u64>() {
                break;
            }
            let mut offset = std::mem::size_of::<u64>();
            while offset + 4 <= read {
                let record_len =
                    u32::from_le_bytes(output[offset..offset + 4].try_into().unwrap()) as usize;
                if record_len == 0 || offset + record_len > read {
                    break;
                }
                let record = &output[offset..offset + record_len];
                let major = u16::from_le_bytes(record[4..6].try_into().unwrap());
                if major == 2 {
                    let frn = u64::from_le_bytes(record[8..16].try_into().unwrap());
                    let parent = u64::from_le_bytes(record[16..24].try_into().unwrap());
                    let attributes = u32::from_le_bytes(record[52..56].try_into().unwrap());
                    let name_len =
                        u16::from_le_bytes(record[56..58].try_into().unwrap()) as usize;
                    let name_offset =
                        u16::from_le_bytes(record[58..60].try_into().unwrap()) as usize;
                    if name_offset + name_len <= record_len {
                        let name: String = String::from_utf16_lossy(
                            record[name_offset..name_offset + name_len]
                                .chunks_exact(2)
                                .map(|pair| u16::from_le_bytes(pair.try_into().unwrap()))
                                .collect::<Vec<_>>()
                                .as_slice(),
                        );
                        entries.insert(frn, (parent, name, attributes));
                    }
                }
                offset += record_len;
            }
            input.start_usn = u64::from_le_bytes(output[..8].try_into().unwrap());
        }
        if entries.is_empty() {
            return Err("USN enumeration returned nothing".into());
        }

        // Anchor: the scan root's own file reference number.
        let root_frn = file_reference_of(root)?;
        let mut children: HashMap<u64, Vec<(u64, String, u32)>> = HashMap::new();
        for (frn, (parent, name, attributes)) in &entries {
            children.entry(*parent).or_default().push((*frn, name.clone(), *attributes));
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| a.1.cmp(&b.1));
        }

        let mut files = Vec::new();
        let mut stack = vec![(root_frn, PathBuf::from(&root_text))];
        while let Some((frn, base)) = stack.pop() {
            for (child_frn, name, attributes) in children.get(&frn).into_iter().flatten() {
                let path = base.join(name);
                if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    continue; // junctions/symlinks: the plain walker skips them too
                }
                if attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                    stack.push((*child_frn, path));
                } else {
                    files.push(path);
                }
            }
        }
        Ok(files)
    }

    /// NTFS file reference number of an existing directory (no access needed,
    /// backup-semantics open).
    pub(super) fn file_reference_of(path: &Path) -> Result<u64, String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(format!("open {}: {}", path.display(), last_error()));
        }
        let owned = Handle(handle);
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(owned.0, &mut info) } == 0 {
            return Err(last_error());
        }
        Ok(((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64)
    }
}

fn is_unchanged(connection: &Connection, path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    let Ok(size) = i64::try_from(metadata.len()) else {
        return false;
    };
    let Ok(modified_time) = metadata.modified() else {
        return false;
    };
    let Ok(modified) = unix_seconds(modified_time) else {
        return false;
    };
    // Non-image files need no fingerprint; images are reprocessed until their
    // photo fingerprint exists.
    let fingerprint_required = is_image_path(path);
    let current: Option<(i64, i64, bool)> = connection
        .query_row(
            "SELECT f.size, f.modified, EXISTS(SELECT 1 FROM photo_fingerprints p WHERE p.file_id = f.id) \
             FROM files f WHERE f.path = ?1",
            params![path.to_string_lossy()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0)),
        )
        .optional()
        .ok()
        .flatten();
    matches!(&current, Some((size_, modified_, has_fingerprint))
        if *size_ == size && *modified_ == modified && (!fingerprint_required || *has_fingerprint))
}

fn is_image_path(path: &Path) -> bool {
    let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
        return false;
    };
    matches!(
        extension.to_ascii_lowercase().as_str(),
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff"
    )
}

/// Present images without a stored fingerprint: the rebuild set produced by
/// a fingerprint-format migration. SQL-side extension check mirrors
/// `is_image_path` so the count matches what the scan will reprocess.
fn count_fingerprint_rebuild(connection: &Connection) -> Result<u64, String> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM files f WHERE f.present=1 \
             AND NOT EXISTS(SELECT 1 FROM photo_fingerprints p WHERE p.file_id=f.id) \
             AND (lower(f.path) LIKE '%.jpg' OR lower(f.path) LIKE '%.jpeg' \
               OR lower(f.path) LIKE '%.png' OR lower(f.path) LIKE '%.gif' \
               OR lower(f.path) LIKE '%.webp' OR lower(f.path) LIKE '%.bmp' \
               OR lower(f.path) LIKE '%.tif' OR lower(f.path) LIKE '%.tiff')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(count as u64)
}

fn write_entry(
    connection: &Connection,
    entry: &ProcessedEntry,
    protect: &[String],
) -> Result<IndexOutcome, String> {
    let is_protected = is_protected_path(&entry.path_text, protect);
    let now = now_seconds()?;
    let outcome = if connection
        .query_row(
            "SELECT 1 FROM files WHERE path=?1",
            params![entry.path_text],
            |_| Ok(()),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .is_some()
    {
        IndexOutcome::Updated
    } else {
        IndexOutcome::New
    };
    connection.execute(
        "INSERT INTO files(path,size,modified,hash,protected,approved,present,scanned_at,quick_hash,dev,inode,frn,exif_taken) VALUES(?1,?2,?3,?4,?5,0,1,?6,?7,?8,?9,?10,?11)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size,modified=excluded.modified,hash=excluded.hash,protected=excluded.protected,approved=0,present=1,scanned_at=excluded.scanned_at,quick_hash=excluded.quick_hash,dev=excluded.dev,inode=excluded.inode,frn=CASE WHEN excluded.frn<>0 THEN excluded.frn ELSE files.frn END,exif_taken=CASE WHEN excluded.exif_taken<>0 THEN excluded.exif_taken WHEN excluded.hash<>files.hash THEN 0 ELSE files.exif_taken END",
        params![entry.path_text, entry.size, entry.modified, entry.hash, is_protected as i64, now, entry.quick_hash, entry.dev, entry.inode, entry.frn, entry.exif_taken],
    )
    .map_err(|e| e.to_string())?;
    connection.execute(
        "DELETE FROM photo_fingerprints WHERE file_id=(SELECT id FROM files WHERE path=?1)",
        params![entry.path_text],
    )
    .map_err(|e| e.to_string())?;
    let file_id: i64 = connection
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![entry.path_text],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    if let Some((dhash, phash)) = entry.photo {
        let parts = fingerprint_parts(dhash);
        connection.execute(
            "INSERT INTO photo_fingerprints(file_id,dhash,phash,part_a,part_b,part_c,part_d,part_e) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![file_id, dhash as i64, phash as i64, parts[0], parts[1], parts[2], parts[3], parts[4]],
        )
        .map_err(|e| e.to_string())?;
    }
    if let Some((simhash, token_count)) = entry.document {
        connection.execute(
            "INSERT INTO document_fingerprints(file_id,simhash,token_count) VALUES(?1,?2,?3) ON CONFLICT(file_id) DO UPDATE SET simhash=excluded.simhash,token_count=excluded.token_count",
            params![file_id, simhash as i64, token_count],
        )
        .map_err(|e| e.to_string())?;
    } else {
        // The file no longer parses as a document (non-UTF-8 content, changed
        // extension); drop the stale fingerprint so it stops pairing.
        connection
            .execute(
                "DELETE FROM document_fingerprints WHERE file_id=?1",
                params![file_id],
            )
            .map_err(|e| e.to_string())?;
    }
    Ok(outcome)
}

/// Parallel scan pipeline for one root: a walker thread feeds hashing workers
/// through a channel; the caller thread is the single database writer.
#[allow(clippy::too_many_arguments)]
fn run_parallel_scan(
    connection: &Connection,
    database: &Path,
    root: &Path,
    trash: &Path,
    protect: &[String],
    exclude: &[String],
    min_file_size: u64,
    counters: &mut ScanCounters,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
    progress: &(dyn Fn(u64, u64, Option<&str>) + Send + Sync),
    errors: Option<&ScanErrorLog>,
    hash_progress: Option<&HashProgress>,
    total: u64,
) -> Result<(), String> {
    use std::sync::mpsc::{self, TryRecvError};

    let worker_count = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(2)
        .min(4);
    let (work_tx, work_rx) = mpsc::channel::<PathBuf>();
    let work_rx = std::sync::Arc::new(std::sync::Mutex::new(work_rx));
    let (result_tx, result_rx) = mpsc::channel::<WorkResult>();

    std::thread::scope(|scope| -> Result<(), String> {
        let walker = {
            let work_tx = work_tx.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move || {
                walk_root(
                    database,
                    root,
                    trash,
                    exclude,
                    min_file_size,
                    &work_tx,
                    &result_tx,
                    cancelled,
                )
            })
        };
        drop(work_tx);
        // Large-file entries waiting for a same-(size, quick) sibling before
        // paying for a full hash. Every bucket here holds at most one entry:
        // a second arrival promotes and removes the whole bucket.
        let mut pending: std::collections::HashMap<(i64, String), ProcessedEntry> =
            std::collections::HashMap::new();
        for _ in 0..worker_count {
            let work_rx = work_rx.clone();
            let result_tx = result_tx.clone();
            scope.spawn(move || loop {
                if cancelled() {
                    break;
                }
                let item = match work_rx.lock() {
                    Ok(guard) => guard.try_recv(),
                    Err(_) => break,
                };
                match item {
                    Ok(path) => {
                        let _ = result_tx.send(process_file(path, hash_progress));
                    }
                    Err(TryRecvError::Empty) => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                    }
                    Err(TryRecvError::Disconnected) => break,
                }
            });
        }
        drop(result_tx);
        for result in result_rx {
            let current: Option<String> = match &result {
                WorkResult::Unchanged(path_text) => Some(path_text.clone()),
                WorkResult::WalkError(path) => Some(path.clone()),
                WorkResult::Processed(Ok(entry)) => Some(entry.path_text.clone()),
                WorkResult::Processed(Err((path, _))) => Some(path.clone()),
                WorkResult::Moved { path, .. } => Some(path.clone()),
                WorkResult::Skipped => None,
            };
            match result {
                WorkResult::Unchanged(path_text) => {
                    connection.execute(
                        "UPDATE files SET present=1, scanned_at=?2 WHERE path=?1",
                        params![path_text, now_seconds()?],
                    )
                    .map_err(|e| e.to_string())?;
                    counters.unchanged += 1;
                }
                WorkResult::Moved { path, file_id } => {
                    // Renames and moves keep hash and fingerprints: only the
                    // row's location and flags are refreshed. Protection is
                    // re-evaluated for the new path so a file moved into a
                    // protected subtree is covered immediately.
                    // The target path can still be held by a stale row (its
                    // file was deleted externally before this scan could
                    // mark it absent; the UNIQUE index covers absent rows
                    // too). Rename it aside and retire it first or the
                    // re-point below hits the path UNIQUE constraint and
                    // aborts every future scan at the same file.
                    connection.execute(
                        "UPDATE files SET path = path || '#replaced#' || id, present=0, approved=0 WHERE path=?1 AND id<>?2",
                        params![path, file_id],
                    )
                    .map_err(|e| e.to_string())?;
                    connection.execute(
                        "UPDATE files SET path=?1, present=1, approved=0, protected=?2, scanned_at=?3 WHERE id=?4",
                        params![
                            path,
                            is_protected_path(&path, protect) as i64,
                            now_seconds()?,
                            file_id
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                    counters.updated += 1;
                }
                WorkResult::Skipped => counters.skipped += 1,
                WorkResult::WalkError(path) => {
                    eprintln!("warning: unreadable directory skipped: {path}");
                    counters.errors += 1;
                    if let Some(log) = errors {
                        log.record(&path, "directory unreadable");
                    }
                }
                WorkResult::Processed(Ok(entry)) => {
                    if entry.quick_hash.is_none() {
                        match write_entry(connection, &entry, protect)? {
                            IndexOutcome::New => counters.new += 1,
                            IndexOutcome::Updated => counters.updated += 1,
                            IndexOutcome::Unchanged => counters.unchanged += 1,
                        }
                    } else {
                        let quick = entry.quick_hash.clone().expect("checked above");
                        let key = (entry.size, quick.clone());
                        // A pending sibling or an indexed row sharing the
                        // (size, quick) bucket means a real duplicate is
                        // plausible: promote everything to full hashes.
                        let has_pending = pending.contains_key(&key);
                        let has_indexed = !has_pending
                            && connection
                                .query_row(
                                    "SELECT 1 FROM files WHERE size=?1 AND quick_hash=?2 AND path<>?3 LIMIT 1",
                                    params![entry.size, quick, entry.path_text],
                                    |_| Ok(()),
                                )
                                .optional()
                                .map_err(|e| e.to_string())?
                                .is_some();
                        if has_pending || has_indexed {
                            let mut bucket = vec![entry];
                            if let Some(sibling) = pending.remove(&key) {
                                bucket.push(sibling);
                            }
                            for mut promoted in bucket {
                                // The file can vanish between its quick hash
                                // and this full hash; record the failure and
                                // keep scanning instead of aborting the pass.
                                match hash_file_progress(
                                    Path::new(&promoted.path_text),
                                    hash_progress,
                                ) {
                                    Ok(hash) => {
                                        promoted.hash = hash;
                                        match write_entry(connection, &promoted, protect)? {
                                            IndexOutcome::New => counters.new += 1,
                                            IndexOutcome::Updated => counters.updated += 1,
                                            IndexOutcome::Unchanged => counters.unchanged += 1,
                                        }
                                    }
                                    Err(error) => {
                                        eprintln!("warning: {}: {error}", promoted.path_text);
                                        counters.errors += 1;
                                        if let Some(log) = errors {
                                            log.record(&promoted.path_text, &error);
                                        }
                                    }
                                }
                            }
                            // Indexed rows promoted in an earlier scan still
                            // carry their quick marker; refresh them so a
                            // newly found twin groups under the real hash.
                            let refresh_errors = std::cell::Cell::new(0u64);
                            refresh_quick_hash_rows(
                                connection,
                                key.0,
                                &quick,
                                hash_progress,
                                &|path, error| {
                                    eprintln!("warning: {path}: {error}");
                                    refresh_errors.set(refresh_errors.get() + 1);
                                    if let Some(log) = errors {
                                        log.record(path, error);
                                    }
                                },
                            )?;
                            counters.errors += refresh_errors.get();
                        } else {
                            pending.insert(key, entry);
                        }
                    }
                }
                WorkResult::Processed(Err((path, error))) => {
                    eprintln!("warning: {path}: {error}");
                    counters.errors += 1;
                    if let Some(log) = errors {
                        log.record(&path, &error);
                    }
                }
            }
            progress(
                counters.new
                    + counters.unchanged
                    + counters.updated
                    + counters.skipped
                    + counters.errors,
                total,
                current.as_deref(),
            );
        }
        // Remaining pending entries never found a sibling: unique for now.
        // Store the quick fingerprint as a marked hash so they cannot be
        // mistaken for BLAKE3 duplicates; if a twin appears in a later scan
        // the promotion path refreshes both rows with real hashes.
        for (_, entry) in pending {
            let mut unique = entry;
            unique.hash = format!(
                "q:{}",
                unique.quick_hash.clone().expect("pending entries are quick-hashed")
            );
            match write_entry(connection, &unique, protect)? {
                IndexOutcome::New => counters.new += 1,
                IndexOutcome::Updated => counters.updated += 1,
                IndexOutcome::Unchanged => counters.unchanged += 1,
            }
        }
        walker.join().map_err(|_| "scan worker panicked".to_string())?
    })?;
    Ok(())
}

// Rows promoted in an earlier scan still carry the "q:<quick>" marker until a
// same-bucket twin shows up; re-hash them with real BLAKE3 so the marker
// never splits what is actually one duplicate group. A row whose file has
// vanished since is reported through `on_error` and skipped — one stale row
// must not abort the whole scan.
fn refresh_quick_hash_rows(
    connection: &Connection,
    size: i64,
    quick: &str,
    hash_progress: Option<&HashProgress>,
    on_error: &dyn Fn(&str, &str),
) -> Result<(), String> {
    let mut statement = connection
        .prepare(
            "SELECT path FROM files WHERE size=?1 AND quick_hash=?2 AND hash LIKE 'q:%'",
        )
        .map_err(|e| e.to_string())?;
    let paths = statement
        .query_map(params![size, quick], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    for path in paths {
        match hash_file_progress(Path::new(&path), hash_progress) {
            Ok(hash) => {
                connection
                    .execute(
                        "UPDATE files SET hash=?1 WHERE path=?2",
                        params![hash, path],
                    )
                    .map_err(|e| e.to_string())?;
            }
            Err(error) => on_error(&path, &error),
        }
    }
    Ok(())
}

fn is_excluded(path: &Path, trash: &Path, exclude: &[String]) -> bool {
    let value = path.to_string_lossy();
    path.starts_with(trash)
        || value.contains("/.git/")
        || value.contains("\\.git\\")
        || value.contains("/node_modules/")
        || value.contains("\\node_modules\\")
        || value.contains("/$RECYCLE.BIN/")
        || value.contains("\\$RECYCLE.BIN\\")
        || exclude
            .iter()
            .any(|rule| !rule.trim().is_empty() && contains_normalized(&value, rule))
}

fn contains_normalized(path_text: &str, rule: &str) -> bool {
    normalize_path_text(path_text).contains(&normalize_path_text(rule))
}

enum IndexOutcome {
    New,
    Unchanged,
    Updated,
}

fn hash_file(path: &Path) -> Result<String, String> {
    hash_file_progress(path, None)
}

/// Full BLAKE3 hash with optional live byte reporting: `progress` sees the
/// most recently started file plus cumulative bytes across all concurrent
/// hashing workers.
fn hash_file_progress(path: &Path, progress: Option<&HashProgress>) -> Result<String, String> {
    if let Some(progress) = progress {
        progress.begin(path);
    }
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        if let Some(progress) = progress {
            progress.add(count as u64);
        }
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Quick fingerprint over the first QUICK_HASH_CHUNK bytes: cheap identity
/// for same-size bucketing. Two files with equal (size, quick hash) still get
/// full hashes before they can be reported as duplicates.
fn quick_hash_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 4096];
    let mut remaining = QUICK_HASH_CHUNK;
    while remaining > 0 {
        let want = remaining.min(buffer.len() as u64) as usize;
        let count = file.read(&mut buffer[..want]).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        remaining -= count as u64;
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// EXIF capture time (DateTimeOriginal) as unix seconds, 0 when the file
/// carries no readable EXIF. Only JPEG/TIFF-style containers are tried; the
/// timestamp is recorded without timezone conversion, which keeps comparisons
/// between copies of the same photo stable.
fn exif_taken_seconds(path: &Path) -> i64 {
    let try_read = || -> Option<i64> {
        let file = File::open(path).ok()?;
        let exif = exif::Reader::new()
            .read_from_container(&mut std::io::BufReader::new(file))
            .ok()?;
        let field = exif.get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(parts) => {
                let raw = parts.first()?;
                Some(parse_exif_datetime(&String::from_utf8_lossy(raw)))
            }
            _ => None,
        }
    };
    try_read().unwrap_or(0)
}

/// "YYYY:MM:DD HH:MM:SS" (EXIF ASCII date) to unix seconds; 0 on anything
/// unexpected.
fn parse_exif_datetime(text: &str) -> i64 {
    fn digits(text: &str, range: std::ops::Range<usize>) -> Option<i64> {
        text.get(range)?.parse::<i64>().ok()
    }
    let text = text.trim_end_matches('\0').trim();
    if text.len() < 19 {
        return 0;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits(text, 0..4),
        digits(text, 5..7),
        digits(text, 8..10),
        digits(text, 11..13),
        digits(text, 14..16),
        digits(text, 17..19),
    ) else {
        return 0;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return 0;
    }
    days_from_civil(year, month, day) * 86_400
        + hour * 3_600
        + minute * 60
        + second
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's
/// days_from_civil), so date math needs no timezone or calendar library.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn perceptual_hash(path: &Path) -> Option<(u64, u64)> {
    let image = image::ImageReader::open(path).ok()?.decode().ok()?;
    // dHash: gradient over a 9x8 luma grid.
    let small = image
        .resize_exact(9, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut dhash = 0_u64;
    for y in 0..8 {
        for x in 0..8 {
            dhash =
                (dhash << 1) | u64::from(small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0]);
        }
    }
    // pHash: 32x32 DCT-II, sign of the 8x8 low-frequency block against its
    // median. Robust to resizing and recompression where the gradient hash
    // alone is not.
    let big = image
        .resize_exact(32, 32, image::imageops::FilterType::Triangle)
        .to_luma8();
    let phash = dct_phash(&big);
    Some((dhash, phash))
}

/// Sign bits of the 8x8 top-left DCT block versus its median (DC excluded).
fn dct_phash(gray: &image::GrayImage) -> u64 {
    const N: usize = 32;
    // Separable DCT-II: rows first, then columns of the result.
    let mut rows = [[0_f32; N]; N];
    for (y, row) in rows.iter_mut().enumerate() {
        for (u, cell) in row.iter_mut().enumerate() {
            let sum: f32 = (0..N)
                .map(|x| (f32::from(gray.get_pixel(x as u32, y as u32)[0]) - 128.0) * dct_cos(u, x))
                .sum();
            *cell = sum;
        }
    }
    let mut block = [[0_f32; 8]; 8];
    for v in 0..8 {
        for u in 0..8 {
            let sum: f32 = (0..N)
                .map(|y| rows[y][v] * dct_cos(u, y))
                .sum();
            block[v][u] = sum;
        }
    }
    block[0][0] = f32::NAN; // DC carries brightness, not structure.
    let mut sorted: Vec<f32> = block
        .iter()
        .flat_map(|row| row.iter().copied())
        .filter(|c| !c.is_nan())
        .collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median = sorted[sorted.len() / 2];
    let mut hash = 0_u64;
    for v in 0..8 {
        for u in 0..8 {
            hash = (hash << 1) | u64::from(block[v][u] > median);
        }
    }
    hash
}

fn dct_cos(k: usize, n: usize) -> f32 {
    const PI: f32 = std::f32::consts::PI;
    let scale = if k == 0 { (0.5_f32).sqrt() } else { 1.0 };
    scale * ((2 * n + 1) as f32 * k as f32 * PI / (2.0 * 32.0)).cos()
}

/// Definitive check for the "duplicate photos" view: decode both files and
/// compare every pixel. Fingerprint equality (dHash 0 + pHash 0) is only
/// a candidate signal — burst shots and recompressed copies can share a
/// fingerprint while being different pictures, so the UI offers this verdict
/// before the user deletes either side.
pub fn photos_pixel_identical(first: &str, second: &str) -> Result<bool, String> {
    let decode = |path: &str| -> Result<image::DynamicImage, String> {
        image::ImageReader::open(path)
            .map_err(|e| format!("cannot open {path}: {e}"))?
            .decode()
            .map_err(|e| format!("cannot decode {path}: {e}"))
    };
    let left = decode(first)?;
    let right = decode(second)?;
    if (left.width(), left.height()) != (right.width(), right.height()) {
        return Ok(false);
    }
    // Same dimensions: compare one canonical RGBA buffer per side.
    let left_rgba = left.to_rgba8();
    let right_rgba = right.to_rgba8();
    Ok(rgba_buffers_identical(&left_rgba, &right_rgba))
}

/// Row-by-row RGBA comparison with early exit: near-duplicates usually
/// differ within the first rows, so bailing keeps rejected pairs cheap.
fn rgba_buffers_identical(left: &image::RgbaImage, right: &image::RgbaImage) -> bool {
    if left.dimensions() != right.dimensions() {
        return false;
    }
    let stride = left.width() as usize * 4;
    left.as_raw()
        .chunks_exact(stride)
        .zip(right.as_raw().chunks_exact(stride))
        .all(|(l, r)| l == r)
}

/// One verification outcome for a candidate pair.
#[derive(Debug, Clone)]
pub struct PairVerdict {
    pub first: String,
    pub second: String,
    pub identical: bool,
}

/// Header-only dimension read: pixel-identical images always share
/// dimensions, so a mismatch settles the pair without any decode.
fn photo_dimensions(path: &str) -> Option<(u32, u32)> {
    image::ImageReader::open(path)
        .ok()?
        .into_dimensions()
        .ok()
}

fn verify_pair_pixels(first: &str, second: &str) -> bool {
    if let (Some(a), Some(b)) = (photo_dimensions(first), photo_dimensions(second)) {
        if a != b {
            return false;
        }
    }
    // Undecodable files can never be confirmed identical, so they count as
    // "different" and drop out of the duplicate view.
    photos_pixel_identical(first, second).unwrap_or(false)
}

/// Verify candidate photo pairs in parallel, publishing every verdict as
/// soon as it is known so the UI can stream results in. `should_stop` is
/// cooperative cancellation, checked between pairs. Returns
/// (identical, different) totals for the pairs actually verified.
pub fn verify_photo_pairs(
    pairs: &[(String, String)],
    on_verdict: &(dyn Fn(&PairVerdict) + Sync),
    should_stop: &(dyn Fn() -> bool + Sync),
) -> (usize, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let same = AtomicUsize::new(0);
    let different = AtomicUsize::new(0);
    let workers = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(4)
        .clamp(1, 6)
        .min(pairs.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if should_stop() {
                    return;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some((first, second)) = pairs.get(index) else {
                    return;
                };
                let identical = verify_pair_pixels(first, second);
                let verdict = PairVerdict {
                    first: first.clone(),
                    second: second.clone(),
                    identical,
                };
                if identical {
                    same.fetch_add(1, Ordering::Relaxed);
                } else {
                    different.fetch_add(1, Ordering::Relaxed);
                }
                on_verdict(&verdict);
            });
        }
    });
    (same.into_inner(), different.into_inner())
}

fn fingerprint_parts(hash: u64) -> [i64; 5] {
    [
        ((hash >> 51) & 0x1fff) as i64,
        ((hash >> 38) & 0x1fff) as i64,
        ((hash >> 25) & 0x1fff) as i64,
        ((hash >> 12) & 0x1fff) as i64,
        (hash & 0x0fff) as i64,
    ]
}

fn document_simhash(path: &Path) -> Option<(u64, i64)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    if !["txt", "md", "csv", "json", "xml", "html", "htm"].contains(&extension.as_str()) {
        return None;
    }
    let text = fs::read_to_string(path).ok()?;
    let tokens: Vec<_> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_ascii_lowercase)
        .collect();
    if tokens.len() < 20 {
        return None;
    }
    let mut bits = [0_i32; 64];
    for token in &tokens {
        let hash = blake3::hash(token.as_bytes());
        for (index, byte) in hash.as_bytes()[..8].iter().enumerate() {
            for bit in 0..8 {
                bits[index * 8 + bit] += if byte & (1 << bit) != 0 { 1 } else { -1 };
            }
        }
    }
    let mut hash = 0_u64;
    for (bit, weight) in bits.iter().enumerate() {
        if *weight > 0 {
            hash |= 1 << bit;
        }
    }
    Some((hash, tokens.len() as i64))
}

fn groups(database: &Path) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection.prepare("SELECT hash, size, COUNT(*) AS n FROM files WHERE present=1 GROUP BY hash,size HAVING n > 1 ORDER BY size DESC").map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut group_count = 0;
    for row in rows {
        let (hash, size, count) = row.map_err(|e| e.to_string())?;
        group_count += 1;
        println!(
            "\nGroup {group_count}: {count} identical files, {} bytes each, hash {}",
            size,
            &hash[..12]
        );
        let mut members = connection.prepare("SELECT id,path,protected,approved FROM files WHERE present=1 AND hash=?1 AND size=?2 ORDER BY path").map_err(|e| e.to_string())?;
        let files = members
            .query_map(params![hash, size], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        for file in files {
            let (id, path, protected, approved) = file.map_err(|e| e.to_string())?;
            println!(
                "  [{id}] {}{}{}",
                path,
                if protected != 0 { " [protected]" } else { "" },
                if approved != 0 { " [approved]" } else { "" }
            );
        }
    }
    if group_count == 0 {
        println!("No exact duplicate groups found.");
    }
    Ok(())
}

pub fn set_approval(database: &Path, file_id: i64, approved: bool) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    set_approval_with(&connection, file_id, approved)
}

fn set_approval_with(connection: &Connection, file_id: i64, approved: bool) -> Result<(), String> {
    let file =
        file_by_id(connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if approved && file.protected {
        return Err("protected files cannot be approved".to_string());
    }
    if approved && !is_exact_duplicate(connection, &file)? {
        return Err("only members of an exact duplicate group can be approved".to_string());
    }
    connection
        .execute(
            "UPDATE files SET approved=?1 WHERE id=?2",
            params![approved as i64, file_id],
        )
        .map_err(|e| e.to_string())?;
    println!(
        "File {file_id} {}.",
        if approved { "approved" } else { "unapproved" }
    );
    Ok(())
}

/// Which copy a group keeps when bulk-marking; mirrors the review page's
/// single-group smart-mark strategies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkStrategy {
    Newest,
    Oldest,
    Shortest,
    /// Keeper by the heuristics of `keeper_score` (original naming, no
    /// copy/social traces, earliest modification).
    Scored,
}

/// Heuristic "which copy deserves to survive" score, mirroring the DESIGN §7
/// direction: original camera naming helps (+15), copy/social traces hurt
/// (-20), the earliest modification wins up to +10 (originals usually age).
/// Scores are advisory only — they pick the suggested keeper, nothing moves
/// without a human mark.
pub fn keeper_score(file: &GroupFile, earliest: i64, latest: i64) -> i64 {
    let path = file.path.to_ascii_lowercase();
    let name = path.rsplit(['/', '\\']).next().unwrap_or("");
    let stem = name.split_once('.').map(|(stem, _)| stem).unwrap_or(name);
    let mut score: i64 = 0;
    let camera_naming = stem.starts_with("img_")
        || stem.starts_with("dsc")
        || stem.starts_with("pxl")
        || stem.starts_with("mvi")
        || (stem.len() >= 8 && stem.chars().take(8).all(|c| c.is_ascii_digit()));
    if camera_naming {
        score += 15;
    }
    let copy_marked = stem.contains("(1)")
        || stem.contains("copy")
        || stem.contains("副本")
        || stem.contains("mmexport")
        || stem.contains("wx_camera")
        || stem.contains("screenshot");
    let social_dir = ["wechat", "weixin", "微信", "qq", "download", "下载"]
        .iter()
        .any(|mark| path.contains(mark));
    if copy_marked || social_dir {
        score -= 20;
    }
    let span = (latest - earliest).max(1);
    score += 10 * (latest - file.modified) / span;
    score
}

/// Keep-one-mark-the-rest applied to every duplicate group matching the
/// review filters, not just the currently loaded page. Keeper choice mirrors
/// the frontend smart mark (strict comparison, first occurrence wins ties),
/// so both paths select the same copy. Per-copy failures are collected;
/// the sweep never aborts midway.
#[allow(clippy::too_many_arguments)]
pub fn approve_groups_except_keeper(
    database: &Path,
    strategy: MarkStrategy,
    min_size: i64,
    path_contains: &str,
    kind: Option<&str>,
    dir_contains: &str,
) -> Result<BatchOutcome, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let pattern = path_contains.trim();
    let mut outcome = BatchOutcome::default();
    let mut query = GroupQuery {
        min_size,
        path_contains: (!pattern.is_empty()).then_some(pattern),
        sort: GroupSort::Recoverable,
        offset: 0,
        limit: 200,
        kind,
        dir_contains: (!dir_contains.is_empty()).then_some(dir_contains),
    };
    loop {
        let page = query_groups_with(&connection, &query)?;
        if page.groups.is_empty() {
            break;
        }
        let page_len = page.groups.len() as i64;
        for group in page.groups {
            let pool: Vec<&GroupFile> = group.files.iter().filter(|f| !f.protected).collect();
            if pool.len() < 2 {
                continue;
            }
            let mut keeper = pool[0];
            let earliest = pool.iter().map(|f| f.modified).min().unwrap_or(0);
            let latest = pool.iter().map(|f| f.modified).max().unwrap_or(0);
            for candidate in pool.clone() {
                match strategy {
                    MarkStrategy::Newest => {
                        if candidate.modified > keeper.modified {
                            keeper = candidate;
                        }
                    }
                    MarkStrategy::Oldest => {
                        if candidate.modified < keeper.modified {
                            keeper = candidate;
                        }
                    }
                    MarkStrategy::Shortest => {
                        if candidate.path.len() < keeper.path.len() {
                            keeper = candidate;
                        }
                    }
                    MarkStrategy::Scored => {
                        if keeper_score(candidate, earliest, latest)
                            > keeper_score(keeper, earliest, latest)
                        {
                            keeper = candidate;
                        }
                    }
                }
            }
            for file in pool {
                if file.id == keeper.id || file.approved {
                    continue;
                }
                match set_approval_with(&connection, file.id, true) {
                    Ok(()) => outcome.succeeded += 1,
                    Err(error) => outcome.failures.push(format!("{}：{error}", file.path)),
                }
            }
        }
        if page_len < query.limit {
            break;
        }
        query.offset += query.limit;
    }
    Ok(outcome)
}

/// What the settings-page rule preview shows: how many indexed files the
/// given rules would shield, with a few example paths. Rules are passed in
/// because the user previews before saving them.
#[derive(serde::Serialize)]
pub struct ProtectPreview {
    pub matched: i64,
    pub examples: Vec<String>,
}

pub fn protect_preview(database: &Path, rules: &[String]) -> Result<ProtectPreview, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection
        .prepare("SELECT path FROM files WHERE present=1")
        .map_err(|e| e.to_string())?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    let mut preview = ProtectPreview {
        matched: 0,
        examples: Vec::new(),
    };
    for path in paths {
        if is_protected_path(&path, rules) {
            preview.matched += 1;
            if preview.examples.len() < PROTECT_PREVIEW_EXAMPLES {
                preview.examples.push(path);
            }
        }
    }
    Ok(preview)
}

const PROTECT_PREVIEW_EXAMPLES: usize = 20;

pub fn trash(database: &Path, file_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    trash_one(&connection, file_id, None)
}

/// Move several approved duplicates in one user action: one connection, and
/// every resulting operations row shares a single batch id so the recycle
/// bin can present them as one cleanup.
pub fn trash_batch(database: &Path, file_ids: &[i64]) -> BatchOutcome {
    trash_batch_with_progress(database, file_ids, None)
}

/// Same as `trash_batch`, reporting (done, total) after every file so a UI
/// can show live progress for large sweeps.
pub fn trash_batch_with_progress(
    database: &Path,
    file_ids: &[i64],
    progress: Option<&dyn Fn(usize, usize)>,
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    if file_ids.is_empty() {
        return outcome;
    }
    let Ok(connection) = open_database(database).map_err(|e| outcome.failures.push(e)) else {
        return outcome;
    };
    if let Err(error) = ensure_initialized(&connection) {
        outcome.failures.push(error);
        return outcome;
    }
    let batch_id = new_batch_id();
    for (done, file_id) in file_ids.iter().enumerate() {
        match trash_one(&connection, *file_id, Some(batch_id)) {
            Ok(()) => outcome.succeeded += 1,
            Err(error) => outcome.failures.push(format!("文件 #{file_id}：{error}")),
        }
        if let Some(progress) = progress {
            progress(done + 1, file_ids.len());
        }
    }
    outcome
}

fn trash_one(connection: &Connection, file_id: i64, batch_id: Option<i64>) -> Result<(), String> {
    let file =
        file_by_id(connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if file.protected || !file.approved || !is_exact_duplicate(connection, &file)? {
        return Err(
            "file must be an approved, unprotected member of an exact duplicate group".to_string(),
        );
    }
    let source = PathBuf::from(&file.path);
    if hash_file(&source)? != file.hash {
        return Err("source no longer matches indexed hash; scan again".to_string());
    }
    if strict_verify_enabled(connection) {
        strict_verify_against_keeper(connection, &file)?;
    }
    let destination = recycle_destination(connection, file.id, &source)?;
    move_verified(&source, &destination, &file.hash)?;
    connection.execute("INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,?3,?4,'trashed',?5,?6)", params![file.id, file.path, destination.to_string_lossy(), file.hash, now_seconds()?, batch_id]).map_err(|e| e.to_string())?;
    connection
        .execute(
            "UPDATE files SET present=0, approved=0 WHERE id=?1",
            params![file.id],
        )
        .map_err(|e| e.to_string())?;
    println!("Moved to recycle bin: {}", destination.display());
    Ok(())
}

pub fn restore(database: &Path, operation_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    restore_with(&connection, operation_id, None)
}

/// Restore into a chosen directory instead of the recorded original path
/// (which may have been deleted or reoccupied). The file keeps its name; a
/// name collision in the target directory refuses the restore.
pub fn restore_to(
    database: &Path,
    operation_id: i64,
    target_dir: &Path,
) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    restore_with(&connection, operation_id, Some(target_dir))
}

/// Restore every still-recycled file from one bulk user action, sharing a
/// single connection. Per-file failures (destination occupied, integrity
/// check failed) are collected; the batch never aborts midway.
pub fn restore_batch(database: &Path, batch_id: i64) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    let Ok(connection) = open_database(database).map_err(|e| outcome.failures.push(e)) else {
        return outcome;
    };
    if let Err(error) = ensure_initialized(&connection) {
        outcome.failures.push(error);
        return outcome;
    }
    let ids = match connection
        .prepare("SELECT id FROM operations WHERE batch_id=?1 AND state='trashed' ORDER BY id")
    {
        Ok(mut statement) => statement
            .query_map(params![batch_id], |row| row.get::<_, i64>(0))
            .map_err(|e| e.to_string())
            .and_then(|rows| rows.collect::<Result<Vec<_>, _>>().map_err(|e| e.to_string())),
        Err(e) => Err(e.to_string()),
    };
    match ids {
        Ok(ids) => {
            for id in ids {
                match restore_with(&connection, id, None) {
                    Ok(()) => outcome.succeeded += 1,
                    Err(error) => outcome.failures.push(format!("记录 #{id}：{error}")),
                }
            }
        }
        Err(error) => outcome.failures.push(error),
    }
    outcome
}

fn restore_with(
    connection: &Connection,
    operation_id: i64,
    target_dir: Option<&Path>,
) -> Result<(), String> {
    let record: Option<(i64, String, String, String, String)> = connection
        .query_row(
            "SELECT file_id,source_path,trash_path,hash,state FROM operations WHERE id=?1",
            params![operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let (file_id, source, trashed, hash, state) =
        record.ok_or_else(|| format!("unknown operation id {operation_id}"))?;
    if state != "trashed" {
        return Err("operation is not available for restore".to_string());
    }
    let source = PathBuf::from(source);
    let trashed = PathBuf::from(trashed);
    let destination = match target_dir {
        Some(dir) => {
            if !dir.is_dir() {
                return Err(format!("target directory not found: {}", dir.display()));
            }
            let name = trashed
                .file_name()
                .ok_or_else(|| "recycle-bin file has no name".to_string())?;
            let target = dir.join(name);
            if target.exists() {
                return Err(format!(
                    "restore refused: destination already exists: {}",
                    target.display()
                ));
            }
            target
        }
        None => {
            if source.exists() {
                return Err(format!(
                    "restore refused: destination already exists: {}",
                    source.display()
                ));
            }
            source.clone()
        }
    };
    if hash_file(&trashed)? != hash {
        return Err("recycle-bin file failed integrity check".to_string());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create restore directory: {e}"))?;
    }
    move_verified(&trashed, &destination, &hash)?;
    connection
        .execute(
            "UPDATE operations SET state='restored',restored_at=?1 WHERE id=?2",
            params![now_seconds()?, operation_id],
        )
        .map_err(|e| e.to_string())?;
    connection
        .execute(
            "UPDATE files SET present=1,approved=0 WHERE id=?1",
            params![file_id],
        )
        .map_err(|e| e.to_string())?;
    println!("Restored: {}", destination.display());
    Ok(())
}

/// Permanently delete one approved duplicate instead of moving it to the
/// recycle bin. Safety checks mirror `trash`.
pub fn delete_direct(database: &Path, file_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    delete_one(&connection, file_id, None)
}

/// Permanently delete several approved duplicates from one user action,
/// sharing a single batch id (see `trash_batch`).
pub fn delete_approved_batch(database: &Path, file_ids: &[i64]) -> BatchOutcome {
    delete_approved_batch_with_progress(database, file_ids, None)
}

/// Same as `delete_approved_batch`, reporting (done, total) per file.
pub fn delete_approved_batch_with_progress(
    database: &Path,
    file_ids: &[i64],
    progress: Option<&dyn Fn(usize, usize)>,
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    if file_ids.is_empty() {
        return outcome;
    }
    let Ok(connection) = open_database(database).map_err(|e| outcome.failures.push(e)) else {
        return outcome;
    };
    if let Err(error) = ensure_initialized(&connection) {
        outcome.failures.push(error);
        return outcome;
    }
    let batch_id = new_batch_id();
    for (done, file_id) in file_ids.iter().enumerate() {
        match delete_one(&connection, *file_id, Some(batch_id)) {
            Ok(()) => outcome.succeeded += 1,
            Err(error) => outcome.failures.push(format!("文件 #{file_id}：{error}")),
        }
        if let Some(progress) = progress {
            progress(done + 1, file_ids.len());
        }
    }
    outcome
}

fn delete_one(
    connection: &Connection,
    file_id: i64,
    batch_id: Option<i64>,
) -> Result<(), String> {
    let file =
        file_by_id(connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if file.protected || !file.approved || !is_exact_duplicate(connection, &file)? {
        return Err(
            "file must be an approved, unprotected member of an exact duplicate group".to_string(),
        );
    }
    let source = PathBuf::from(&file.path);
    if hash_file(&source)? != file.hash {
        return Err("source no longer matches indexed hash; scan again".to_string());
    }
    if strict_verify_enabled(connection) {
        strict_verify_against_keeper(connection, &file)?;
    }
    fs::remove_file(&source).map_err(|e| format!("delete file: {e}"))?;
    connection
        .execute(
            "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,'',?3,'deleted',?4,?5)",
            params![file.id, file.path, file.hash, now_seconds()?, batch_id],
        )
        .map_err(|e| e.to_string())?;
    connection
        .execute(
            "UPDATE files SET present=0, approved=0 WHERE id=?1",
            params![file.id],
        )
        .map_err(|e| e.to_string())?;
    println!("Deleted permanently: {}", source.display());
    Ok(())
}

/// Outcome of removing user-picked paths: partial success is allowed and the
/// failures carry per-path reasons for the UI to display verbatim.
#[derive(Default)]
pub struct BatchOutcome {
    pub succeeded: usize,
    pub failures: Vec<String>,
}

// --- Strict mode (optional byte-for-byte recheck) ---------------------------

/// Setting read/write for the strict pre-delete byte comparison. Off by
/// default; the hash-based safety chain already runs without it.
pub fn set_strict_verify(database: &Path, enabled: bool) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    set_setting(
        &connection,
        "strict_verify",
        if enabled { "1" } else { "0" },
    )
}

fn strict_verify_enabled(connection: &Connection) -> bool {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key='strict_verify'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(|value| value == "1")
        .unwrap_or(false)
}

/// Stream-compare the doomed copy against another present copy of the same
/// (hash, size). Reads 2x the file's bytes; enabled per setting.
fn strict_verify_against_keeper(
    connection: &Connection,
    file: &IndexedFile,
) -> Result<(), String> {
    let keeper: Option<String> = connection
        .query_row(
            "SELECT path FROM files WHERE present=1 AND hash=?1 AND size=?2 AND id<>?3 \
             ORDER BY id LIMIT 1",
            params![file.hash, file.size, file.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some(keeper) = keeper else {
        return Ok(()); // no sibling survives: nothing to compare with
    };
    if !byte_identical(Path::new(&file.path), Path::new(&keeper))? {
        return Err(
            "strict check failed: content differs from the kept copy; operation aborted"
                .to_string(),
        );
    }
    Ok(())
}

/// Byte-for-byte comparison in 1 MiB chunks. CPU cost is a memcmp; the I/O is
/// two full reads, which is why it stays behind a setting.
fn byte_identical(a: &Path, b: &Path) -> Result<bool, String> {
    let mut left = File::open(a).map_err(|e| format!("open {}: {e}", a.display()))?;
    let mut right = File::open(b).map_err(|e| format!("open {}: {e}", b.display()))?;
    let (mut buf_a, mut buf_b) = (vec![0_u8; 1 << 20], vec![0_u8; 1 << 20]);
    loop {
        let na = read_fill(&mut left, &mut buf_a)?;
        let nb = read_fill(&mut right, &mut buf_b)?;
        if na != nb {
            return Ok(false);
        }
        if na == 0 {
            return Ok(true);
        }
        if buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}

/// Read until the buffer is full or EOF, so chunk boundaries never fake a
/// mismatch.
fn read_fill(file: &mut File, buffer: &mut [u8]) -> Result<usize, String> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(filled)
}

// --- Hardlink dedup ----------------------------------------------------------

/// Replace each approved duplicate with a hard link to its kept copy: every
/// path survives, the duplicated physical file is freed. Not routed through
/// the recycle bin; the keeper copy is the safety net (hash-verified before
/// the swap, same content by construction).
pub fn hardlink(database: &Path, file_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    hardlink_one(&connection, file_id, None)
}

/// Batch variant sharing one connection and one operations batch id (see
/// `trash_batch`).
pub fn hardlink_batch(database: &Path, file_ids: &[i64]) -> BatchOutcome {
    hardlink_batch_with_progress(database, file_ids, None)
}

/// Same as `hardlink_batch`, reporting (done, total) per file.
pub fn hardlink_batch_with_progress(
    database: &Path,
    file_ids: &[i64],
    progress: Option<&dyn Fn(usize, usize)>,
) -> BatchOutcome {
    let mut outcome = BatchOutcome::default();
    if file_ids.is_empty() {
        return outcome;
    }
    let Ok(connection) = open_database(database).map_err(|e| outcome.failures.push(e)) else {
        return outcome;
    };
    if let Err(error) = ensure_initialized(&connection) {
        outcome.failures.push(error);
        return outcome;
    }
    let batch_id = new_batch_id();
    for (done, file_id) in file_ids.iter().enumerate() {
        match hardlink_one(&connection, *file_id, Some(batch_id)) {
            Ok(()) => outcome.succeeded += 1,
            Err(error) => outcome.failures.push(format!("文件 #{file_id}：{error}")),
        }
        if let Some(progress) = progress {
            progress(done + 1, file_ids.len());
        }
    }
    outcome
}

fn hardlink_one(
    connection: &Connection,
    file_id: i64,
    batch_id: Option<i64>,
) -> Result<(), String> {
    let file =
        file_by_id(connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if file.protected || !file.approved || !is_exact_duplicate(connection, &file)? {
        return Err(
            "file must be an approved, unprotected member of an exact duplicate group".to_string(),
        );
    }
    let source = PathBuf::from(&file.path);
    if hash_file(&source)? != file.hash {
        return Err("source no longer matches indexed hash; scan again".to_string());
    }
    // Keeper preference: unprotected first, then the shortest path (mirrors
    // the shortest-path keep strategy).
    let keeper: String = connection
        .query_row(
            "SELECT path FROM files WHERE present=1 AND hash=?1 AND size=?2 AND id<>?3 \
             ORDER BY protected ASC, LENGTH(path) ASC, id ASC LIMIT 1",
            params![file.hash, file.size, file.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "no other present copy to link to".to_string())?;
    let keeper = PathBuf::from(keeper);
    if already_same_physical_file(connection, &file)? {
        return Err("source is already a hard link of the kept copy".to_string());
    }
    if !same_volume(&source, &keeper)? {
        return Err(
            "source and kept copy are on different volumes; hard links cannot cross volumes"
                .to_string(),
        );
    }
    // Swap the name over via a temporary link: if anything fails before the
    // final rename, the original name and file are untouched.
    let file_name = source
        .file_name()
        .ok_or_else(|| "source path has no file name".to_string())?;
    let mut temp_name = file_name.to_os_string();
    temp_name.push(".flink-tmp");
    let temp = source
        .parent()
        .ok_or_else(|| "source path has no parent".to_string())?
        .join(temp_name);
    let _ = fs::remove_file(&temp);
    fs::hard_link(&keeper, &temp).map_err(|e| format!("create hard link: {e}"))?;
    if let Err(error) = fs::remove_file(&source) {
        let _ = fs::remove_file(&temp);
        return Err(format!("remove original copy: {error}"));
    }
    if let Err(error) = fs::rename(&temp, &source) {
        // The original name is gone but its content lives on at the keeper;
        // try to restore the name as a plain hard link before giving up.
        if fs::hard_link(&keeper, &source).is_err() {
            return Err(format!(
                "restoring the path failed ({error}); content remains at {}",
                keeper.display()
            ));
        }
    }
    let (dev, inode) = fs::metadata(&source)
        .map(|metadata| device_inode(&metadata))
        .unwrap_or((0, 0));
    // On Windows the linked name now resolves to the keeper's file
    // reference; keeping the old one would make rename detection re-point
    // the keeper's own row when this path is renamed later.
    #[cfg(windows)]
    let frn = i64::try_from(usn::file_reference_of(&source).unwrap_or(0)).unwrap_or(0);
    #[cfg(not(windows))]
    let frn = 0;
    connection
        .execute(
            "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,?3,?4,'hardlinked',?5,?6)",
            params![file.id, file.path, keeper.to_string_lossy(), file.hash, now_seconds()?, batch_id],
        )
        .map_err(|e| e.to_string())?;
    connection
        .execute(
            "UPDATE files SET approved=0, dev=?2, inode=?3, frn=?4 WHERE id=?1",
            params![file.id, dev, inode, frn],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// True when the index already records both names as one physical file
/// (POSIX dev/inode; always false where that metadata is unavailable).
fn already_same_physical_file(
    connection: &Connection,
    file: &IndexedFile,
) -> Result<bool, String> {
    if file.dev == 0 && file.inode == 0 {
        return Ok(false);
    }
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM files WHERE present=1 AND hash=?1 AND size=?2 AND id<>?3 \
             AND dev=?4 AND inode=?5",
            params![file.hash, file.size, file.id, file.dev, file.inode],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(count > 0)
}

/// Same-volume test guarding the hardlink swap. POSIX compares device ids;
/// Windows compares the drive/UNC share prefix, which is exact unless volume
/// mount points hide behind one letter (rare on personal machines).
fn same_volume(a: &Path, b: &Path) -> Result<bool, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let da = fs::metadata(a).map_err(|e| e.to_string())?.dev();
        let db = fs::metadata(b).map_err(|e| e.to_string())?.dev();
        Ok(da == db)
    }
    #[cfg(not(unix))]
    {
        let _ = fs::metadata(a).map_err(|e| e.to_string())?;
        let _ = fs::metadata(b).map_err(|e| e.to_string())?;
        Ok(volume_prefix(a).eq_ignore_ascii_case(&volume_prefix(b)))
    }
}

#[cfg(not(unix))]
fn volume_prefix(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        let share: Vec<&str> = rest.splitn(3, ['\\', '/']).take(2).collect();
        return format!(r"\\?\UNC\{}", share.join(r"\"));
    }
    if let Some(rest) = text.strip_prefix(r"\\") {
        let share: Vec<&str> = rest.splitn(3, ['\\', '/']).take(2).collect();
        return format!(r"\\{}", share.join(r"\"));
    }
    text.chars().take(2).collect()
}

/// Nanosecond timestamp as a batch identity: rows written by one user action
/// share it, rows from different actions never collide in practice.
fn new_batch_id() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as i64)
        .unwrap_or(0)
}

fn remove_paths(database: &Path, paths: &[String], to_trash: bool) -> Result<BatchOutcome, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let batch_id = new_batch_id();
    let mut outcome = BatchOutcome {
        succeeded: 0,
        failures: Vec::new(),
    };
    for path in paths {
        match remove_indexed_path(&connection, path, to_trash, Some(batch_id)) {
            Ok(()) => outcome.succeeded += 1,
            Err(error) => outcome.failures.push(format!("{path}：{error}")),
        }
    }
    Ok(outcome)
}

/// Move user-picked paths (e.g. similar-photo candidates) to the recycle bin.
/// There is no approval step like the exact-duplicate flow, but every path
/// must be indexed, unprotected, and still hash-match the index before it is
/// touched; every move is recorded in operations.
#[derive(serde::Serialize, Default)]
pub struct DirKeepOutcome {
    /// Copies recycled (their content survives elsewhere or via the keeper).
    pub recycled: usize,
    /// Copies moved into the keep directory to preserve a last-of-content.
    pub moved: usize,
    pub failures: Vec<String>,
}

/// Recycle every duplicate candidate under `dir`. Contents whose removable
/// copies ALL live inside `dir` would lose their last copy in a plain sweep,
/// so one unprotected copy is first moved into `keep_dir` (the index records
/// the new path, operations records the audit row) and only the remaining
/// copies are recycled. Contents with a survivor elsewhere (or a protected
/// copy anywhere) are recycled straight away; protected files are never
/// touched.
pub fn recycle_dir_keep_one(
    database: &Path,
    dir: &str,
    keep_dir: &Path,
) -> Result<DirKeepOutcome, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let canonical_dir = absolute_path(Path::new(dir.trim()))?;
    let canonical_keep = absolute_path(keep_dir)?;
    if canonical_keep.starts_with(&canonical_dir) {
        return Err("保留目录不能位于被清理目录之内".to_string());
    }
    let dir_text = canonical_dir.to_string_lossy().into_owned();
    let posix_pattern = format!("%{}%", like_escape(&format!("{dir_text}/")));
    let windows_pattern = format!("%{}%", like_escape(&format!("{dir_text}\\")));
    let mut statement = connection
        .prepare(
            "SELECT f.id,f.path,f.hash,f.protected FROM files f \
             WHERE f.present=1 AND (f.path LIKE ?1 ESCAPE '\\' OR f.path LIKE ?2 ESCAPE '\\') \
             AND EXISTS (SELECT 1 FROM files g WHERE g.present=1 \
               AND g.hash=f.hash AND g.size=f.size AND g.id<>f.id)",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map(params![posix_pattern, windows_pattern], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);

    // Group the in-directory candidates by content hash.
    let mut groups: HashMap<String, Vec<(i64, String, bool)>> = HashMap::new();
    for (id, path, hash, protected) in rows {
        groups.entry(hash).or_default().push((id, path, protected));
    }

    let mut outcome = DirKeepOutcome::default();
    let batch_id = new_batch_id();
    let mut recycle_list: Vec<String> = Vec::new();
    for (hash, members) in groups {
        // Live copies everywhere and how many of them are protected.
        let total: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE present=1 AND hash=?1",
                params![hash],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let in_dir = members.len() as i64;
        let outside = total - in_dir;
        let protected_in_dir = members.iter().filter(|(_, _, protected)| *protected).count() as i64;
        // The content survives the sweep when copies exist outside the dir or
        // a protected copy (untouchable by design) stays behind.
        let survivor = outside > 0 || protected_in_dir > 0;
        let mut unprotected: Vec<(i64, String)> = members
            .iter()
            .filter(|(_, _, protected)| !protected)
            .map(|(id, path, _)| (*id, path.clone()))
            .collect();
        if !survivor {
            if unprotected.is_empty() {
                outcome
                    .failures
                    .push("目录内的候选均为受保护文件，无法处理".to_string());
                continue;
            }
            // Keeper: the shortest path moves into the keep directory.
            unprotected.sort_by_key(|(_, path)| path.len());
            let (keeper_id, keeper_path) = unprotected.remove(0);
            let source = PathBuf::from(&keeper_path);
            if hash_file(&source)? != hash {
                outcome
                    .failures
                    .push(format!("{keeper_path}: content no longer matches the index"));
                continue;
            }
            if let Err(error) =
                move_indexed_file_to(&connection, keeper_id, &source, keep_dir, &hash)
            {
                outcome.failures.push(format!("{keeper_path}: {error}"));
                continue;
            }
            outcome.moved += 1;
        }
        for (_, path) in &unprotected {
            recycle_list.push(path.clone());
        }
    }
    if !recycle_list.is_empty() {
        for path in &recycle_list {
            match remove_indexed_path(&connection, path, true, Some(batch_id)) {
                Ok(()) => outcome.recycled += 1,
                Err(error) => outcome.failures.push(format!("{path}: {error}")),
            }
        }
    }
    Ok(outcome)
}

/// Outcome of a directory merge.
#[derive(serde::Serialize, Default)]
pub struct DirMergeOutcome {
    /// Losing copies recycled.
    pub recycled: usize,
    /// Winning copies moved into the target directory.
    pub moved: usize,
    /// Winning copies that already lived inside the target directory and
    /// stayed where they are.
    pub kept: usize,
    /// Directories removed after the merge left them empty.
    pub cleaned_dirs: Vec<String>,
    pub failures: Vec<String>,
}

/// Unified directory merge for the duplicate-directory view: the caller has
/// already decided a winner for every duplicate pair and passes each pair as
/// `(loser, winner)`. The loser is recycled behind the standard per-file
/// chain (index lookup, protection rules, content re-hash, audited move);
/// the winner ends up inside `target_dir` — moved there when it lives
/// elsewhere, left alone when it is already inside. A pair whose loser
/// cannot be recycled keeps its winner in place: moving it would only
/// create a fresh duplicate. After everything succeeded, every directory in
/// `cleanup_dirs` is removed best-effort — `remove_dir` refuses on its own
/// while non-duplicate files remain, so they are never at risk.
pub fn merge_dir_pair(
    database: &Path,
    pairs: &[(String, String)],
    target_dir: &str,
    cleanup_dirs: &[String],
) -> Result<DirMergeOutcome, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let target = absolute_path(Path::new(target_dir.trim()))?;
    let target_text = target.to_string_lossy().into_owned();
    let mut outcome = DirMergeOutcome::default();
    if pairs.is_empty() {
        return Ok(outcome);
    }
    let batch_id = new_batch_id();

    // Phase 1: recycle the losing copies (deduplicated across pairs).
    let mut recycled_ok: std::collections::HashSet<String> = Default::default();
    let mut seen: std::collections::HashSet<String> = Default::default();
    for (loser, _) in pairs {
        if !seen.insert(loser.clone()) {
            continue;
        }
        match remove_indexed_path(&connection, loser, true, Some(batch_id)) {
            Ok(()) => {
                outcome.recycled += 1;
                recycled_ok.insert(loser.clone());
            }
            Err(error) => outcome.failures.push(format!("{loser}: {error}")),
        }
    }

    // Phase 2: winners end up in the target directory — already there means
    // kept, otherwise moved (once each, only when the pair's loser was
    // recycled, otherwise the untouched twin would end up duplicated).
    let mut handled_winners: std::collections::HashSet<String> = Default::default();
    let mut all_moved = true;
    for (loser, winner) in pairs {
        if !recycled_ok.contains(loser) {
            outcome
                .failures
                .push(format!("{winner}: kept, its duplicate could not be recycled"));
            all_moved = false;
            continue;
        }
        if winner.starts_with(&target_text)
            && winner[target_text.len()..].starts_with(['/', '\\'])
        {
            outcome.kept += 1;
            continue;
        }
        if !handled_winners.insert(winner.clone()) {
            continue;
        }
        let row: Option<(i64, String)> = connection
            .query_row(
                "SELECT id,hash FROM files WHERE path=?1 AND present=1",
                params![winner],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((file_id, hash)) = row else {
            outcome
                .failures
                .push(format!("{winner}: not indexed or already gone"));
            all_moved = false;
            continue;
        };
        let source = PathBuf::from(winner);
        match hash_file(&source) {
            Ok(actual) if actual == hash => {}
            Ok(_) => {
                outcome
                    .failures
                    .push(format!("{winner}: content no longer matches the index"));
                all_moved = false;
                continue;
            }
            Err(error) => {
                outcome.failures.push(format!("{winner}: {error}"));
                all_moved = false;
                continue;
            }
        }
        match move_indexed_file_to(&connection, file_id, &source, &target, &hash) {
            Ok(()) => outcome.moved += 1,
            Err(error) => {
                outcome.failures.push(format!("{winner}: {error}"));
                all_moved = false;
            }
        }
    }

    // Phase 3: cleanup passed directories best-effort when everything
    // worked; remove_dir refuses while non-duplicate files remain.
    if all_moved && outcome.failures.is_empty() {
        for dir in cleanup_dirs {
            if let Ok(path) = absolute_path(Path::new(dir.trim())) {
                if path != target && !target.starts_with(&path) {
                    if fs::remove_dir(&path).is_ok() {
                        outcome
                            .cleaned_dirs
                            .push(path.to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    Ok(outcome)
}

/// Cached pixel-verification verdicts for duplicate-photo candidate pairs.
/// A cached row only counts while both files still carry the exact size and
/// mtime recorded at check time — any rescan that touches either file
/// invalidates the entry through the join, so stale rows are never served.
/// Pairs are canonicalized (lexicographically smaller path first) so the
/// same content pair hits one row regardless of orientation.
pub fn cached_pair_verdicts(
    database: &Path,
    pairs: &[(String, String)],
) -> Result<Vec<Option<bool>>, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut verdicts = Vec::with_capacity(pairs.len());
    for (first, second) in pairs {
        let (path_a, path_b) = canonical_pair(first, second);
        let row: Option<i64> = connection
            .query_row(
                "SELECT v.identical FROM photo_pair_verdicts v \
                 JOIN files f1 ON f1.path = v.path_a AND f1.present = 1 \
                 JOIN files f2 ON f2.path = v.path_b AND f2.present = 1 \
                 WHERE v.path_a = ?1 AND v.path_b = ?2 \
                   AND v.size_a = f1.size AND v.mtime_a = f1.modified \
                   AND v.size_b = f2.size AND v.mtime_b = f2.modified",
                params![path_a, path_b],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        verdicts.push(row.map(|identical| identical != 0));
    }
    Ok(verdicts)
}

/// Persist pixel-verification verdicts. Sizes and mtimes come from the
/// index (the same source the candidate list was built from); pairs with a
/// missing or absent file are skipped — they will re-verify next time.
/// Returns how many rows were written.
pub fn store_pair_verdicts(
    database: &Path,
    verdicts: &[(String, String, bool)],
) -> Result<usize, String> {
    let mut connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let checked_at = now_seconds()?;
    let tx = connection.transaction().map_err(|e| e.to_string())?;
    // Housekeeping: rows not refreshed for 90 days are for pairs that no
    // longer exist or stopped matching; drop them so the cache cannot grow
    // without bound.
    tx.execute(
        "DELETE FROM photo_pair_verdicts WHERE checked_at < ?1",
        params![checked_at - 90 * 86_400],
    )
    .map_err(|e| e.to_string())?;
    let mut stored = 0;
    for (first, second, identical) in verdicts {
        let (path_a, path_b) = canonical_pair(first, second);
        let row: Option<(i64, i64, i64, i64)> = tx
            .query_row(
                "SELECT f1.size, f1.modified, f2.size, f2.modified FROM files f1, files f2 \
                 WHERE f1.path = ?1 AND f1.present = 1 \
                   AND f2.path = ?2 AND f2.present = 1",
                params![path_a, path_b],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((size_a, mtime_a, size_b, mtime_b)) = row else {
            continue;
        };
        tx.execute(
            "INSERT INTO photo_pair_verdicts \
               (path_a, size_a, mtime_a, path_b, size_b, mtime_b, identical, checked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(path_a, path_b) DO UPDATE SET \
               size_a = excluded.size_a, mtime_a = excluded.mtime_a, \
               size_b = excluded.size_b, mtime_b = excluded.mtime_b, \
               identical = excluded.identical, checked_at = excluded.checked_at",
            params![
                path_a,
                size_a,
                mtime_a,
                path_b,
                size_b,
                mtime_b,
                i64::from(*identical),
                checked_at
            ],
        )
        .map_err(|e| e.to_string())?;
        stored += 1;
    }
    tx.commit().map_err(|e| e.to_string())?;
    Ok(stored)
}

/// Lexicographically smaller path first, so a content pair maps to one row
/// no matter which side of the candidate pair came first.
fn canonical_pair<'a>(first: &'a str, second: &'a str) -> (&'a str, &'a str) {
    if first <= second {
        (first, second)
    } else {
        (second, first)
    }
}

/// Move one indexed file into `keep_dir` (name kept; a collision appends a
/// numeric suffix), verifying content against the index first and updating
/// the indexed path so the next scan sees a rename instead of a disappearance.
fn move_indexed_file_to(
    connection: &Connection,
    file_id: i64,
    source: &Path,
    keep_dir: &Path,
    hash: &str,
) -> Result<(), String> {
    let name = source
        .file_name()
        .ok_or_else(|| "source has no file name".to_string())?;
    let extension = Path::new(name)
        .extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default();
    let full = name.to_string_lossy().into_owned();
    let stem = match full.rfind(&extension) {
        Some(index) if !extension.is_empty() => full[..index].to_string(),
        _ => full.clone(),
    };
    fs::create_dir_all(keep_dir).map_err(|e| format!("create keep directory: {e}"))?;
    let mut candidate = keep_dir.join(name);
    let mut suffix = 1_u32;
    while candidate.exists() {
        candidate = keep_dir.join(format!("{stem} ({suffix}){extension}"));
        suffix += 1;
    }
    move_verified(source, &candidate, hash)?;
    // A recycled copy leaves a dead index row (present=0) still holding this
    // path, and files.path is UNIQUE: rename the dead row aside so the moved
    // file can take the path over while the row itself survives for the
    // restore chain, which looks files up by id.
    connection
        .execute(
            "UPDATE files SET path = path || '#replaced#' || id              WHERE path = ?2 AND present = 0 AND id <> ?1",
            params![file_id, candidate.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
    connection
        .execute(
            "UPDATE files SET path=?2 WHERE id=?1",
            params![file_id, candidate.to_string_lossy()],
        )
        .map_err(|e| e.to_string())?;
    connection
        .execute(
            "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,'',?3,'moved',?4,NULL)",
            params![file_id, source.to_string_lossy(), hash, now_seconds()?],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn trash_paths(database: &Path, paths: &[String]) -> Result<BatchOutcome, String> {
    remove_paths(database, paths, true)
}

/// Permanently delete user-picked paths behind the same safety chain as
/// `trash_paths`.
pub fn delete_paths(database: &Path, paths: &[String]) -> Result<BatchOutcome, String> {
    remove_paths(database, paths, false)
}

fn remove_indexed_path(
    connection: &Connection,
    path: &str,
    to_trash: bool,
    batch_id: Option<i64>,
) -> Result<(), String> {
    let row: Option<(i64, String, i64, i64)> = connection
        .query_row(
            "SELECT id,hash,protected,present FROM files WHERE path=?1",
            params![path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((file_id, hash, protected, present)) = row else {
        return Err("文件不在索引中，请先扫描".to_string());
    };
    if present == 0 {
        return Err("文件已被标记为不存在，请刷新后重试".to_string());
    }
    if protected == 1 {
        return Err("受保护规则覆盖，已拒绝删除".to_string());
    }
    let source = PathBuf::from(path);
    if !source.is_file() {
        return Err("文件不存在或已被删除".to_string());
    }
    if hash_file(&source)? != hash {
        return Err("文件内容与索引记录不一致，请重新扫描后再试".to_string());
    }
    if to_trash {
        let destination = recycle_destination(connection, file_id, &source)?;
        move_verified(&source, &destination, &hash)?;
        connection.execute("INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,?3,?4,'trashed',?5,?6)", params![file_id, path, destination.to_string_lossy(), hash, now_seconds()?, batch_id]).map_err(|e| e.to_string())?;
    } else {
        fs::remove_file(&source).map_err(|e| format!("删除文件失败：{e}"))?;
        connection
            .execute(
                "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at,batch_id) VALUES(?1,?2,'',?3,'deleted',?4,?5)",
                params![file_id, path, hash, now_seconds()?, batch_id],
            )
            .map_err(|e| e.to_string())?;
    }
    connection
        .execute(
            "UPDATE files SET present=0, approved=0 WHERE id=?1",
            params![file_id],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Protection rules are substring matches with separators normalized, so a
/// rule stored as `D:\backup` also covers `D:/backup/...`; comparisons fold
/// case on Windows, where path casing carries no meaning. Substring (rather
/// than strict prefix) semantics is deliberate: over-protecting is safe,
/// silently un-protecting files an existing rule was covering is not.
fn is_protected_path(path_text: &str, rules: &[String]) -> bool {
    let path = normalize_path_text(path_text);
    rules.iter().any(|rule| {
        let rule = normalize_path_text(rule);
        !rule.is_empty() && path.contains(&rule)
    })
}

fn normalize_path_text(value: &str) -> String {
    let trimmed = value.trim().replace('\\', "/");
    #[cfg(target_os = "windows")]
    return trimmed.to_lowercase();
    #[cfg(not(target_os = "windows"))]
    return trimmed;
}

/// Permanently delete one file from the application recycle bin.
pub fn delete_trash(database: &Path, operation_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    delete_trash_with(&connection, operation_id)
}

fn delete_trash_with(connection: &Connection, operation_id: i64) -> Result<(), String> {
    let record: Option<(String, String)> = connection
        .query_row(
            "SELECT trash_path,state FROM operations WHERE id=?1",
            params![operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let Some((trashed, state)) = record else {
        return Err(format!("unknown operation id {operation_id}"));
    };
    if state != "trashed" {
        return Err("operation is not available for delete".to_string());
    }
    let trashed_path = PathBuf::from(&trashed);
    if trashed_path.exists() {
        fs::remove_file(&trashed_path).map_err(|e| format!("delete recycled file: {e}"))?;
    }
    let trash_root = PathBuf::from(required_setting(connection, "trash_path")?);
    cleanup_empty_parents(&trashed_path, &trash_root);
    connection
        .execute(
            "UPDATE operations SET state='deleted' WHERE id=?1",
            params![operation_id],
        )
        .map_err(|e| e.to_string())?;
    println!("Deleted permanently: {trashed}");
    Ok(())
}

/// Recycle-bin retention in days from settings; 0 disables auto-pruning.
pub fn trash_retention_days(connection: &Connection) -> i64 {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key='trash_retention_days'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|days| *days >= 0)
        .unwrap_or(30)
}

/// Permanently delete recycled files older than the configured retention.
pub fn prune_expired_trash(database: &Path) -> Result<u64, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    prune_expired_trash_with(&connection, now_seconds()?)
}

fn prune_expired_trash_with(connection: &Connection, now: i64) -> Result<u64, String> {
    let days = trash_retention_days(connection);
    if days <= 0 {
        return Ok(0);
    }
    let cutoff = now.saturating_sub(days.saturating_mul(86_400));
    let mut statement = connection
        .prepare("SELECT id FROM operations WHERE state='trashed' AND created_at < ?1")
        .map_err(|e| e.to_string())?;
    let ids = statement
        .query_map(params![cutoff], |row| row.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    let mut pruned = 0;
    for id in ids {
        match delete_trash_with(connection, id) {
            Ok(()) => pruned += 1,
            Err(error) => eprintln!("warning: could not prune operation {id}: {error}"),
        }
    }
    Ok(pruned)
}

fn cleanup_empty_parents(start: &Path, stop_at: &Path) {
    let mut current = start.parent().map(Path::to_path_buf);
    while let Some(directory) = current {
        if directory == stop_at || fs::remove_dir(&directory).is_err() {
            break;
        }
        current = directory.parent().map(Path::to_path_buf);
    }
}

/// Permanently delete every file currently in the application recycle bin.
pub fn empty_trash(database: &Path) -> Result<u64, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection
        .prepare("SELECT id FROM operations WHERE state='trashed'")
        .map_err(|e| e.to_string())?;
    let ids = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    let mut deleted = 0_u64;
    for id in &ids {
        // Best effort per file so one undeletable entry does not strand the
        // rest of the bin; the reused connection avoids a reopen per file.
        match delete_trash_with(&connection, *id) {
            Ok(()) => deleted += 1,
            Err(error) => eprintln!("warning: could not delete operation {id}: {error}"),
        }
    }
    println!("Recycle bin emptied: {} files", deleted);
    Ok(deleted)
}

/// Sorting options for the duplicate-group review queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSort {
    Size,
    Members,
    Path,
    /// Sort by the space actually freed when every removable copy goes:
    /// per-file size × (distinct physical copies − 1), so hardlinked
    /// name-groups (which free nothing) sink instead of floating to the top.
    Recoverable,
}

/// Coarse file-type buckets for the review filter; each maps to a fixed
/// extension list matched against the lowercased path.
pub fn kind_extensions(kind: &str) -> Option<&'static [&'static str]> {
    match kind {
        "image" => Some(&[
            "jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "svg",
            "ico",
        ]),
        "video" => Some(&[
            "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "3gp",
        ]),
        "audio" => Some(&["mp3", "wav", "flac", "aac", "m4a", "ogg", "wma", "opus"]),
        "document" => Some(&[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "csv", "json",
            "xml", "html", "htm", "epub",
        ]),
        "archive" => Some(&["zip", "rar", "7z", "tar", "gz", "bz2", "xz", "iso"]),
        _ => None,
    }
}

/// Filters and paging for `query_groups`.
pub struct GroupQuery<'a> {
    pub min_size: i64,
    pub path_contains: Option<&'a str>,
    pub sort: GroupSort,
    pub offset: i64,
    pub limit: i64,
    /// Coarse file-type bucket (`kind_extensions`); None = all.
    pub kind: Option<&'a str>,
    /// Directory filter: only groups with at least one copy under this path.
    pub dir_contains: Option<&'a str>,
}

#[derive(serde::Serialize)]
pub struct GroupFile {
    pub id: i64,
    pub path: String,
    pub protected: bool,
    pub approved: bool,
    pub modified: i64,
    /// True when another member of the same group shares this file's
    /// (device, inode): the "duplicate" is another name for the same physical
    /// file, so removing it frees no space.
    pub hardlinked: bool,
    /// Heuristic keeper suggestion (`keeper_score`); advisory only.
    pub suggested: bool,
}

#[derive(serde::Serialize)]
pub struct Group {
    pub hash: String,
    pub size: i64,
    /// Bytes actually freed when every removable copy goes: per-file size ×
    /// (distinct physical copies − 1); zero for hardlink-only groups.
    pub recoverable: i64,
    pub files: Vec<GroupFile>,
}

#[derive(serde::Serialize)]
pub struct GroupsPage {
    pub total: i64,
    pub groups: Vec<Group>,
}

/// Paged duplicate groups with optional size/path filters and ordering.
/// The filter runs in SQL (the `(hash,size,present)` index carries the
/// grouping), the total is computed over the same filtered set, and members
/// for the whole page are fetched in one query instead of one per group.
pub fn query_groups(database: &Path, query: &GroupQuery) -> Result<GroupsPage, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    query_groups_with(&connection, query)
}

fn query_groups_with(connection: &Connection, query: &GroupQuery) -> Result<GroupsPage, String> {
    let limit = query.limit.clamp(1, 200);
    let offset = query.offset.max(0);
    let min_size = query.min_size.max(0);
    // Row filters for kind/keyword/directory must stay at the GROUP level
    // ("the group contains a matching member"), not row level — filtering
    // rows first would collapse one-sided matches below the two-copy
    // minimum and silently drop the whole group.
    let mut values: Vec<rusqlite::types::Value> = vec![rusqlite::types::Value::Integer(min_size)];
    let mut having: Vec<String> = vec!["COUNT(*) > 1".to_string()];
    if let Some(extensions) = query.kind.and_then(kind_extensions) {
        // SUM over the 0/1 LIKE results: any match inside the group passes.
        let chain: Vec<String> = extensions
            .iter()
            .map(|extension| {
                values.push(rusqlite::types::Value::Text(format!("%.{extension}")));
                format!("SUM(lower(path) LIKE ?{})", values.len())
            })
            .collect();
        having.push(format!("({}) > 0", chain.join(" + ")));
    }
    if let Some(dir) = query
        .dir_contains
        .map(str::trim)
        .filter(|dir| !dir.is_empty())
    {
        values.push(rusqlite::types::Value::Text(format!(
            "%{}%",
            like_escape(dir)
        )));
        values.push(rusqlite::types::Value::Text("\\".to_string()));
        having.push(format!(
            "SUM(path LIKE ?{} ESCAPE ?{}) > 0",
            values.len() - 1,
            values.len()
        ));
    }
    let keyword = query.path_contains.map(|value| {
        format!("%{}%", like_escape(value.trim()))
    });
    if let Some(pattern) = &keyword {
        values.push(rusqlite::types::Value::Text(pattern.clone()));
        having.push(format!("SUM(path LIKE ?{} ESCAPE '\\') > 0", values.len()));
    }
    let group_sql = format!(
        "FROM files WHERE present=1 AND size>=?1 GROUP BY hash,size HAVING {}",
        having.join(" AND ")
    );
    // Distinct physical copies: hardlinked names collapse to one, so the
    // recoverable size of a hardlink-only group correctly comes out as zero.
    const PHYSICAL_SQL: &str =
        "COUNT(DISTINCT CASE WHEN dev!=0 OR inode!=0 THEN printf('%d:%d',dev,inode) ELSE 'i'||id END)";
    let total: i64 = connection
        .query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 {group_sql})"),
            rusqlite::params_from_iter(values.iter()),
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let order_sql = match query.sort {
        GroupSort::Size => "size DESC, members DESC",
        GroupSort::Members => "members DESC, size DESC",
        GroupSort::Path => "first_path",
        GroupSort::Recoverable => "freed DESC, size DESC, members DESC",
    };
    values.push(rusqlite::types::Value::Integer(limit));
    values.push(rusqlite::types::Value::Integer(offset));
    let mut statement = connection
        .prepare(&format!(
            "SELECT hash,size,COUNT(*) AS members,{PHYSICAL_SQL} AS physical, \
             MIN(path) AS first_path,{PHYSICAL_SQL}*size AS freed \
             {group_sql} ORDER BY {order_sql} LIMIT ?{} OFFSET ?{}",
            values.len() - 1,
            values.len()
        ))
        .map_err(|e| e.to_string())?;
    let keys = statement
        .query_map(rusqlite::params_from_iter(values.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);

    /// Group member plus its (device, inode) identity for hardlink marking.
    type MemberRow = (GroupFile, i64, i64);
    let mut members: HashMap<(String, i64), Vec<MemberRow>> = HashMap::new();
    if !keys.is_empty() {
        let mut conditions = String::new();
        let mut values: Vec<rusqlite::types::Value> = Vec::new();
        for (hash, size, _) in &keys {
            if !conditions.is_empty() {
                conditions.push_str(" OR ");
            }
            values.push(rusqlite::types::Value::Text(hash.clone()));
            values.push(rusqlite::types::Value::Integer(*size));
            conditions.push_str(&format!("(hash=?{} AND size=?{})", values.len() - 1, values.len()));
        }
        let mut statement = connection
            .prepare(&format!(
                "SELECT hash,size,id,path,protected,approved,modified,dev,inode FROM files \
                 WHERE present=1 AND ({conditions}) ORDER BY modified DESC, path ASC"
            ))
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(values.iter()), |row| {
                let hash: String = row.get(0)?;
                let size: i64 = row.get(1)?;
                let file = GroupFile {
                    id: row.get(2)?,
                    path: row.get(3)?,
                    protected: row.get::<_, i64>(4)? != 0,
                    approved: row.get::<_, i64>(5)? != 0,
                    modified: row.get(6)?,
                    hardlinked: false,
                    suggested: false,
                };
                Ok((hash, size, file, row.get::<_, i64>(7)?, row.get::<_, i64>(8)?))
            })
            .map_err(|e| e.to_string())?;
        for row in rows {
            let (hash, size, file, dev, inode) = row.map_err(|e| e.to_string())?;
            members.entry((hash, size)).or_default().push((file, dev, inode));
        }
    }

    let groups = keys
        .into_iter()
        .map(|(hash, size, physical)| {
            let members = members.remove(&(hash.clone(), size)).unwrap_or_default();
            // A (dev, inode) pair appearing twice inside one group means two
            // of the "duplicates" are names for the same physical file.
            let mut identities: HashMap<(i64, i64), usize> = HashMap::new();
            for (_, dev, inode) in &members {
                if *dev != 0 || *inode != 0 {
                    *identities.entry((*dev, *inode)).or_default() += 1;
                }
            }
            // Keeper suggestion: highest heuristic score among unprotected
            // members; every other member stays untouched (advisory only).
            let earliest = members.iter().map(|(f, _, _)| f.modified).min().unwrap_or(0);
            let latest = members.iter().map(|(f, _, _)| f.modified).max().unwrap_or(0);
            let mut best: Option<(i64, usize)> = None; // (score, member index)
            for (index, (file, _, _)) in members.iter().enumerate() {
                if file.protected {
                    continue;
                }
                let score = keeper_score(file, earliest, latest);
                if best.map(|(best_score, _)| score > best_score).unwrap_or(true) {
                    best = Some((score, index));
                }
            }
            let files = members
                .into_iter()
                .enumerate()
                .map(|(index, (mut file, dev, inode))| {
                    file.hardlinked =
                        identities.get(&(dev, inode)).copied().unwrap_or(0) > 1;
                    file.suggested = best.map(|(_, index_)| index_ == index).unwrap_or(false);
                    file
                })
                .collect();
            Group {
                hash,
                size,
                recoverable: size * (physical - 1).max(0),
                files,
            }
        })
        .collect();
    Ok(GroupsPage { total, groups })
}

/// Build the recycle-bin destination for one indexed file and create its
/// parent directories: `<trash>/<timestamp>-<file_id>/files/<file_id>/<name>`.
fn recycle_destination(
    connection: &Connection,
    file_id: i64,
    source: &Path,
) -> Result<PathBuf, String> {
    let trash_root = PathBuf::from(required_setting(connection, "trash_path")?);
    let batch = format!("{}-{}", now_seconds()?, file_id);
    let destination = trash_root
        .join(batch)
        .join("files")
        .join(file_id.to_string())
        .join(source.file_name().ok_or("source has no file name")?);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create recycle directory: {e}"))?;
    }
    Ok(destination)
}

fn move_verified(source: &Path, destination: &Path, expected_hash: &str) -> Result<(), String> {
    match fs::rename(source, destination) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_file(source, destination)?;
            copy_file(source, destination)?;
            if hash_file(destination)? != expected_hash {
                let _ = fs::remove_file(destination);
                return Err("copied file failed integrity check".to_string());
            }
            fs::remove_file(source)
                .map_err(|e| format!("verified copy exists but source could not be removed: {e}"))
        }
    }
}

fn copy_file(source: &Path, destination: &Path) -> Result<(), String> {
    let temporary = destination.with_extension("filelens-partial");
    let mut input = File::open(source).map_err(|e| e.to_string())?;
    let mut output = File::create(&temporary).map_err(|e| e.to_string())?;
    io::copy(&mut input, &mut output).map_err(|e| e.to_string())?;
    output.sync_all().map_err(|e| e.to_string())?;
    fs::rename(temporary, destination).map_err(|e| e.to_string())
}

fn status(database: &Path) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let files: i64 = connection
        .query_row("SELECT COUNT(*) FROM files WHERE present=1", [], |r| {
            r.get(0)
        })
        .map_err(|e| e.to_string())?;
    let duplicates: i64 = connection.query_row("SELECT COALESCE(SUM(n - 1),0) FROM (SELECT COUNT(*) n FROM files WHERE present=1 GROUP BY hash,size HAVING n > 1)", [], |r| r.get(0)).map_err(|e| e.to_string())?;
    let operations: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE state='trashed'",
            [],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    println!(
        "Indexed files: {files}\nDuplicate copies: {duplicates}\nIn recycle bin: {operations}"
    );
    Ok(())
}

pub fn trash_list(database: &Path) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection
        .prepare("SELECT id,source_path,trash_path,created_at FROM operations WHERE state='trashed' ORDER BY created_at DESC")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (id, source, trashed, created_at) = row.map_err(|e| e.to_string())?;
        println!("[{id}] {created_at}\t{source}\t{trashed}");
    }
    Ok(())
}

#[derive(serde::Serialize)]
pub struct DirCount {
    pub dir: String,
    pub count: i64,
}

/// Distinct parent directories of files that belong to any exact-duplicate
/// group, busiest first — the review page's directory filter options.
pub fn group_dirs(database: &Path) -> Result<Vec<DirCount>, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection
        .prepare(
            "SELECT path FROM files f WHERE present=1 \
             AND EXISTS (SELECT 1 FROM files g WHERE g.present=1 \
               AND g.hash=f.hash AND g.size=f.size AND g.id<>f.id)",
        )
        .map_err(|e| e.to_string())?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    drop(statement);
    let mut counts: HashMap<String, i64> = HashMap::new();
    for path in paths {
        let dir = match path.rsplit_once(['/', '\\']) {
            Some((parent, _)) => parent.to_string(),
            None => continue,
        };
        *counts.entry(dir).or_default() += 1;
    }
    let mut dirs: Vec<DirCount> = counts
        .into_iter()
        .map(|(dir, count)| DirCount { dir, count })
        .collect();
    dirs.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.dir.cmp(&b.dir)));
    Ok(dirs)
}

/// Export every member of the currently indexed duplicate groups as CSV for
/// record keeping before a cleanup. Returns the number of data rows written.
pub fn export_report(database: &Path, output: &Path) -> Result<usize, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut statement = connection
        .prepare(
            "SELECT f.hash, f.path, f.size, f.modified, f.approved \
             FROM files f \
             JOIN (SELECT hash, size FROM files WHERE present=1 GROUP BY hash, size HAVING COUNT(*) > 1) d \
               ON d.hash = f.hash AND d.size = f.size \
             WHERE f.present=1 \
             ORDER BY f.hash, f.path",
        )
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)? != 0,
            ))
        })
        .map_err(|e| e.to_string())?;

    let mut csv = String::from("hash,path,size,modified,approved\n");
    let mut count = 0_usize;
    for row in rows {
        let (hash, path, size, modified, approved) = row.map_err(|e| e.to_string())?;
        csv.push_str(&format!(
            "{},{},{},{},{}\n",
            hash,
            csv_field(&path),
            size,
            modified,
            approved
        ));
        count += 1;
    }
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("create output directory: {e}"))?;
        }
    fs::write(output, csv).map_err(|e| format!("write report: {e}"))?;
    Ok(count)
}

/// Cache-directory eviction: keep the directory at or below `max_bytes` by
/// deleting least-recently-modified files first. Returns the number of files
/// removed and bytes freed. A missing directory behaves as empty; entries
/// that cannot be read or removed are skipped, never fatal.
pub fn evict_lru_files(directory: &Path, max_bytes: u64) -> Result<(usize, u64), String> {
    let mut entries: Vec<(PathBuf, SystemTime, u64)> = Vec::new();
    if let Ok(reader) = fs::read_dir(directory) {
        for entry in reader.flatten() {
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            entries.push((entry.path(), modified, metadata.len()));
        }
    }
    let total: u64 = entries.iter().map(|(_, _, size)| size).sum();
    if total <= max_bytes {
        return Ok((0, 0));
    }
    entries.sort_by_key(|(_, modified, _)| *modified);
    let mut removed = 0_usize;
    let mut freed = 0_u64;
    for (path, _, size) in entries {
        if total - freed <= max_bytes {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            removed += 1;
            freed += size;
        }
    }
    Ok((removed, freed))
}

/// RFC 4180 quoting: fields containing commas, quotes or newlines get wrapped
/// in quotes with embedded quotes doubled.
fn csv_field(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r')
    {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn file_by_id(connection: &Connection, id: i64) -> Result<Option<IndexedFile>, String> {    connection
        .query_row(
            "SELECT id,path,size,hash,protected,approved,dev,inode FROM files WHERE id=?1 AND present=1",
            params![id],
            |r| {
                Ok(IndexedFile {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    size: r.get(2)?,
                    hash: r.get(3)?,
                    protected: r.get::<_, i64>(4)? != 0,
                    approved: r.get::<_, i64>(5)? != 0,
                    dev: r.get(6)?,
                    inode: r.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|e| e.to_string())
}

fn is_exact_duplicate(connection: &Connection, file: &IndexedFile) -> Result<bool, String> {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM files WHERE present=1 AND hash=?1 AND size=?2",
            params![file.hash, file.size],
            |r| r.get(0),
        )
        .map_err(|e| e.to_string())?;
    Ok(count > 1)
}

fn set_setting(connection: &Connection, key: &str, value: &str) -> Result<(), String> {
    connection.execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value]).map_err(|e| e.to_string()).map(|_| ())
}
fn required_setting(connection: &Connection, key: &str) -> Result<String, String> {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "database is not initialized; run init first".to_string())
}
fn ensure_initialized(connection: &Connection) -> Result<(), String> {
    let version = required_setting(connection, "schema_version")?;
    let version = version
        .parse::<i64>()
        .map_err(|_| format!("invalid schema version: {version}"))?;
    if version > SCHEMA_VERSION {
        return Err(format!("unsupported schema version: {version}"));
    }
    // Stepwise, transactional upgrades; each entry bumps the stored version
    // so an interrupted migration resumes instead of replaying.
    for (index, migration) in MIGRATIONS.iter().enumerate() {
        let target = index as i64 + 2; // migration N upgrades N+1 -> N+2
        if version < target {
            connection
                .execute_batch(&format!(
                    "BEGIN; {migration} UPDATE settings SET value='{target}' \
                     WHERE key='schema_version'; COMMIT;"
                ))
                .map_err(|e| format!("schema migration to v{target} failed: {e}"))?;
        }
    }
    // Cover trash/history orderings plus tables and indexes that were added
    // to `create_schema_sql` without a migration step (document similarity
    // predates the versioned upgrade path): `IF NOT EXISTS` keeps this
    // idempotent, so databases created before they existed pick them up on
    // open instead of failing on their first scan.
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS operations_state_created
                 ON operations(state, created_at);
             CREATE INDEX IF NOT EXISTS operations_created
                 ON operations(created_at);
             CREATE TABLE IF NOT EXISTS document_fingerprints (
               file_id INTEGER PRIMARY KEY,
               simhash INTEGER NOT NULL,
               token_count INTEGER NOT NULL,
               FOREIGN KEY(file_id) REFERENCES files(id) ON DELETE CASCADE
             );
             CREATE INDEX IF NOT EXISTS document_fingerprints_hash
                 ON document_fingerprints(simhash);
             CREATE INDEX IF NOT EXISTS files_hash_size_present
                 ON files(hash, size, present);",
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}
fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    // Canonicalize so scan-root prefixes keep matching stored paths across
    // sessions (links, `.`, `..`); dunce keeps Windows paths free of the
    // `\\?\` verbatim prefix, which would break LIKE prefix matching.
    dunce::canonicalize(&resolved).or(Ok(resolved))
}
fn unix_seconds(time: SystemTime) -> Result<i64, String> {
    i64::try_from(
        time.duration_since(UNIX_EPOCH)
            .map_err(|_| "timestamp before Unix epoch")?
            .as_secs(),
    )
    .map_err(|_| "timestamp out of range".to_string())
}
fn now_seconds() -> Result<i64, String> {
    unix_seconds(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("filelens-{name}-{nonce}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn evict_lru_files_keeps_directory_under_limit() {
        let directory = test_directory("cache-evict");
        fs::create_dir_all(&directory).unwrap();
        let write_cached = |name: &str, age_seconds: u64| {
            let path = directory.join(name);
            fs::write(&path, vec![b'x'; 100]).unwrap();
            let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - std::time::Duration::from_secs(age_seconds))
                .unwrap();
        };
        write_cached("old.png", 300);
        write_cached("middle.png", 200);
        write_cached("new.png", 100);

        // 300 bytes against a 150-byte cap: the two oldest go first.
        let (removed, freed) = evict_lru_files(&directory, 150).unwrap();
        assert_eq!((removed, freed), (2, 200));
        assert!(!directory.join("old.png").exists());
        assert!(!directory.join("middle.png").exists());
        assert!(directory.join("new.png").exists());

        // At or under the limit the pass is a no-op, and a missing
        // directory behaves as empty instead of erroring.
        assert_eq!(evict_lru_files(&directory, 150).unwrap(), (0, 0));
        assert_eq!(
            evict_lru_files(&directory.join("missing"), 100).unwrap(),
            (0, 0)
        );
    }

    #[test]
    fn restore_batch_restores_whole_bulk_action() {
        let directory = test_directory("restore-batch");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        // Three members in pair a: trashing two must keep one present copy,
        // exactly like the UI's mark-then-process flow.
        fs::write(source.join("a1.txt"), b"pair one").unwrap();
        fs::write(source.join("a2.txt"), b"pair one").unwrap();
        fs::write(source.join("a3.txt"), b"pair one").unwrap();
        fs::write(source.join("b1.txt"), b"pair two").unwrap();
        fs::write(source.join("b2.txt"), b"pair two").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let id_of = |name: &str| -> i64 {
            connection
                .query_row(
                    "SELECT id FROM files WHERE path LIKE ?1",
                    [format!("%{name}.txt")],
                    |row| row.get(0),
                )
                .unwrap()
        };
        // Batch removal of pair a shares one batch id; pair b loses one copy
        // file-by-file with no batch id and must not be touched by it.
        let (a1, a2, b1, b2) = (id_of("a1"), id_of("a2"), id_of("b1"), id_of("b2"));
        set_approval(&database, a1, true).unwrap();
        set_approval(&database, a2, true).unwrap();
        assert_eq!(trash_batch(&database, &[a1, a2]).succeeded, 2);
        set_approval(&database, b1, true).unwrap();
        set_approval(&database, b2, true).unwrap();
        trash(&database, b1).unwrap();
        let batch_id: i64 = connection
            .query_row(
                "SELECT batch_id FROM operations WHERE state='trashed' AND batch_id IS NOT NULL LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);

        let outcome = restore_batch(&database, batch_id);
        assert_eq!(outcome.succeeded, 2);
        assert!(outcome.failures.is_empty());
        assert!(source.join("a1.txt").exists());
        assert!(source.join("a2.txt").exists());
        assert!(!source.join("b1.txt").exists());
        // Restoring the same batch again is a no-op: no record is still
        // trashed.
        let outcome = restore_batch(&database, batch_id);
        assert_eq!(outcome.succeeded, 0);
        assert!(outcome.failures.is_empty());
    }

    #[test]
    fn hash_progress_reports_files_and_bytes() {
        let directory = test_directory("hash-progress");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("one.txt"), b"content one").unwrap();
        fs::write(source.join("two.txt"), b"content two").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();

        let progress = HashProgress::new();
        scan_with_control(
            &database,
            std::slice::from_ref(&source),
            &[],
            &[],
            0,
            true,
            &|| false,
            &|_, _, _| {},
            None,
            Some(&progress),
        )
        .unwrap();
        let (current, bytes) = progress.snapshot();
        assert!(bytes > 0);
        assert!(current.is_some());
    }

    #[test]
    fn approve_groups_except_keeper_marks_across_groups() {
        let directory = test_directory("bulk-mark");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("x_old.txt"), b"g1").unwrap();
        fs::write(source.join("x_new.txt"), b"g1").unwrap();
        fs::write(source.join("y_old.txt"), b"g2").unwrap();
        fs::write(source.join("y_new.txt"), b"g2").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE files SET modified = CASE WHEN path LIKE '%x_new%' OR path LIKE '%y_new%' THEN 2000 ELSE 1000 END, protected = CASE WHEN path LIKE '%y_old%' THEN 1 ELSE 0 END",
                [],
            )
            .unwrap();
        drop(connection);

        // Group one keeps the newest copy and marks the old one; group two's
        // only unprotected copy IS the keeper, so nothing is marked there.
        let outcome = approve_groups_except_keeper(&database, MarkStrategy::Newest, 0, "", None, "").unwrap();
        assert_eq!(outcome.succeeded, 1);
        assert!(outcome.failures.is_empty());

        let connection = open_database(&database).unwrap();
        let marked: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE present=1 AND approved=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        assert_eq!(marked, 1);
    }

    #[test]
    fn protect_preview_counts_and_samples_matches() {
        let directory = test_directory("protect-preview");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("keep_me.txt"), b"one").unwrap();
        fs::write(source.join("other.txt"), b"two").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let preview = protect_preview(&database, &["keep_me".to_string()]).unwrap();
        assert_eq!(preview.matched, 1);
        assert_eq!(preview.examples.len(), 1);
        assert!(preview.examples[0].ends_with("keep_me.txt"));

        let preview = protect_preview(&database, &[]).unwrap();
        assert_eq!(preview.matched, 0);
    }

    #[test]
    fn scan_error_log_keeps_recent_samples_and_total() {
        let log = ScanErrorLog::new();
        for index in 0..25 {
            log.record(&format!("/data/file-{index}.bin"), "permission denied");
        }
        let (total, recent) = log.snapshot();
        assert_eq!(total, 25);
        assert_eq!(recent.len(), 20);
        // The ring keeps the newest entries after wrapping past the cap.
        assert_eq!(recent.last().unwrap(), &("/data/file-24.bin".to_string(), "permission denied".to_string()));
        assert_eq!(recent.first().unwrap().0, "/data/file-5.bin");

        let empty = ScanErrorLog::new();
        assert_eq!(empty.snapshot(), (0, Vec::new()));
    }

    #[test]
    fn scan_approve_trash_and_restore_are_safe() {
        let directory = test_directory("lifecycle");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("keep.txt"), b"same content").unwrap();
        fs::write(source.join("copy.txt"), b"same content").unwrap();
        fs::write(source.join("other.txt"), b"different content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let copy_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path=?1",
                params![source.join("copy.txt").to_string_lossy()],
                |row| row.get(0),
            )
            .unwrap();
        let file = file_by_id(&connection, copy_id).unwrap().unwrap();
        assert!(is_exact_duplicate(&connection, &file).unwrap());
        drop(connection);

        set_approval(&database, copy_id, true).unwrap();
        trash(&database, copy_id).unwrap();
        assert!(!source.join("copy.txt").exists());

        let connection = open_database(&database).unwrap();
        let operation_id: i64 = connection
            .query_row("SELECT id FROM operations", [], |row| row.get(0))
            .unwrap();
        drop(connection);
        restore(&database, operation_id).unwrap();
        assert_eq!(fs::read(source.join("copy.txt")).unwrap(), b"same content");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn protected_files_cannot_be_approved() {
        let directory = test_directory("protected");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("protected-a.txt"), b"same content").unwrap();
        fs::write(source.join("protected-b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(
            &database,
            std::slice::from_ref(&source),
            &["protected".to_string()],
        )
        .unwrap();
        let connection = open_database(&database).unwrap();
        let id: i64 = connection
            .query_row("SELECT id FROM files LIMIT 1", [], |row| row.get(0))
            .unwrap();
        drop(connection);
        assert!(set_approval(&database, id, true).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn rescan_marks_missing_files_absent_and_skips_missing_roots() {
        let directory = test_directory("missing");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("keep.txt"), b"same content").unwrap();
        fs::write(source.join("copy.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        // A missing root must be skipped without aborting the scan.
        let roots = vec![source.clone(), directory.join("not-exists")];
        scan(&database, &roots, &[]).unwrap();

        // scanned_at has second resolution; make sure the second scan starts
        // strictly after the first one finished.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::remove_file(source.join("copy.txt")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        let present: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE present=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(present, 1, "deleted file should be marked absent");
        let duplicates: i64 = connection
            .query_row(
                "SELECT COALESCE(SUM(n - 1),0) FROM (SELECT COUNT(*) n FROM files WHERE present=1 GROUP BY hash,size HAVING n > 1)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(duplicates, 0, "no duplicate group should remain");
        let last_scan: String = connection
            .query_row(
                "SELECT value FROM settings WHERE key='last_scan_at'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(last_scan.parse::<i64>().is_ok());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn trash_can_be_deleted_permanently_and_emptied() {
        let directory = test_directory("trash-delete");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%b.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        set_approval(&database, b_id, true).unwrap();
        trash(&database, b_id).unwrap();

        let connection = open_database(&database).unwrap();
        let (operation_id, trash_path): (i64, String) = connection
            .query_row(
                "SELECT id,trash_path FROM operations WHERE state='trashed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        delete_trash(&database, operation_id).unwrap();
        assert!(!PathBuf::from(&trash_path).exists());
        let connection = open_database(&database).unwrap();
        let state: String = connection
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                params![operation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "deleted");
        drop(connection);

        // Recreate the duplicate, trash it again, then empty the bin.
        fs::write(source.join("b.txt"), b"same content").unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        set_approval(&database, b_id, true).unwrap();
        trash(&database, b_id).unwrap();
        let removed = empty_trash(&database).unwrap();
        assert_eq!(removed, 1);
        let connection = open_database(&database).unwrap();
        let trashed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE state='trashed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trashed, 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn approved_duplicates_can_be_deleted_directly() {
        let directory = test_directory("direct-delete");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%b.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);

        // Not approved yet: direct delete must refuse.
        assert!(delete_direct(&database, b_id).is_err());
        set_approval(&database, b_id, true).unwrap();
        delete_direct(&database, b_id).unwrap();
        assert!(!source.join("b.txt").exists());
        assert!(
            !recycle.join("files").exists(),
            "direct delete must not touch the recycle bin"
        );

        let connection = open_database(&database).unwrap();
        let (state, present): (String, i64) = connection
            .query_row(
                "SELECT o.state, f.present FROM operations o JOIN files f ON f.id=o.file_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "deleted");
        assert_eq!(present, 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn expired_recycle_items_are_pruned_after_retention() {
        let directory = test_directory("retention");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%b.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        set_approval(&database, b_id, true).unwrap();
        trash(&database, b_id).unwrap();

        // Backdate the operation beyond the default 30-day retention.
        let connection = open_database(&database).unwrap();
        let operation_id: i64 = connection
            .query_row(
                "SELECT id FROM operations WHERE state='trashed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(trash_retention_days(&connection), 30);
        connection
            .execute(
                "UPDATE operations SET created_at = created_at - 40*86400 WHERE id=?1",
                params![operation_id],
            )
            .unwrap();
        // Retention disabled: nothing is pruned.
        connection
            .execute(
                "INSERT INTO settings(key,value) VALUES('trash_retention_days','0')
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [],
            )
            .unwrap();
        drop(connection);
        assert_eq!(prune_expired_trash(&database).unwrap(), 0);

        // Retention enabled: the expired item is removed and marked deleted.
        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE settings SET value='30' WHERE key='trash_retention_days'",
                [],
            )
            .unwrap();
        drop(connection);
        assert_eq!(prune_expired_trash(&database).unwrap(), 1);
        assert!(
            !recycle.join("files").exists(),
            "pruned file and its now-empty directories should be gone"
        );
        let connection = open_database(&database).unwrap();
        let state: String = connection
            .query_row(
                "SELECT state FROM operations WHERE id=?1",
                params![operation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "deleted");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn similar_path_trash_verifies_protection_and_hash() {
        let directory = test_directory("similar-trash");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("plain.txt"), b"photo candidate").unwrap();
        fs::write(source.join("guarded.txt"), b"protected candidate").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(
            &database,
            std::slice::from_ref(&source),
            &["guarded".to_string()],
        )
        .unwrap();

        // Protected paths are refused and left untouched.
        let outcome = trash_paths(
            &database,
            &[source.join("guarded.txt").to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(outcome.succeeded, 0);
        assert_eq!(outcome.failures.len(), 1);
        assert!(outcome.failures[0].contains("受保护"));
        assert!(source.join("guarded.txt").exists());

        // Paths missing from the index are refused too.
        let stray = directory.join("stray.txt");
        fs::write(&stray, b"never scanned").unwrap();
        let outcome = trash_paths(&database, &[stray.to_string_lossy().into_owned()]).unwrap();
        assert_eq!(outcome.succeeded, 0);
        assert!(stray.exists());

        // A file whose content drifted from the index is refused.
        fs::write(source.join("plain.txt"), b"modified content").unwrap();
        let outcome = trash_paths(
            &database,
            &[source.join("plain.txt").to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(outcome.succeeded, 0);
        assert!(source.join("plain.txt").exists());

        // An intact path moves through the recycle bin with an operations record.
        fs::write(source.join("plain.txt"), b"photo candidate").unwrap();
        let outcome = trash_paths(
            &database,
            &[source.join("plain.txt").to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(outcome.succeeded, 1);
        assert!(outcome.failures.is_empty());
        assert!(!source.join("plain.txt").exists());
        let connection = open_database(&database).unwrap();
        let (state, trash_path): (String, String) = connection
            .query_row(
                "SELECT state,trash_path FROM operations",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        assert_eq!(state, "trashed");
        assert!(PathBuf::from(&trash_path).exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn similar_path_delete_records_operations() {
        let directory = test_directory("similar-delete");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("solo.txt"), b"unique content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let outcome = delete_paths(
            &database,
            &[source.join("solo.txt").to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(outcome.succeeded, 1);
        assert!(outcome.failures.is_empty());
        assert!(!source.join("solo.txt").exists());
        let connection = open_database(&database).unwrap();
        let (state, present): (String, i64) = connection
            .query_row(
                "SELECT state,present FROM operations JOIN files ON files.id=operations.file_id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(connection);
        assert_eq!(state, "deleted");
        assert_eq!(present, 0);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn scan_honors_exclude_rules_and_min_file_size() {
        let directory = test_directory("exclude-min-size");
        let source = directory.join("source");
        let cache = source.join("cache");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&cache).unwrap();
        // A duplicate pair inside a directory matching the exclude rule and a
        // duplicate pair below the size threshold: neither may be reported.
        fs::write(cache.join("a.txt"), b"cached pair").unwrap();
        fs::write(cache.join("b.txt"), b"cached pair").unwrap();
        fs::write(source.join("tiny-a.txt"), b"9 bytes!!").unwrap();
        fs::write(source.join("tiny-b.txt"), b"9 bytes!!").unwrap();
        // Above the threshold, not excluded: must be reported.
        fs::write(source.join("keep-a.txt"), b"report me please").unwrap();
        fs::write(source.join("keep-b.txt"), b"report me please").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        let summary = scan_with_options(
            &database,
            std::slice::from_ref(&source),
            &[],
            &["cache".to_string()],
            12,
            true,
        )
        .unwrap();
        assert_eq!(summary.new, 2);

        let connection = open_database(&database).unwrap();
        let cached: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%/cache/%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Excluded files never get a row.
        assert_eq!(cached, 0);
        // Below-threshold files are not seen by the scan, so any row from a
        // previous (threshold-free) run would be marked absent; here they
        // simply have no row at all.
        let tiny: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%tiny-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tiny, 0);
        let keep: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%keep-%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(keep, 2);
        drop(connection);

        let groups = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(groups.groups.len(), 1);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn v1_database_migrates_to_current_preserving_data() {
        let directory = test_directory("migration-v1-v2");
        let database = directory.join("index.db");
        // Build a hand-rolled v1 database: pre-quick_hash schema with data.
        let connection = open_database(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE files (
                   id INTEGER PRIMARY KEY,
                   path TEXT NOT NULL UNIQUE,
                   size INTEGER NOT NULL,
                   modified INTEGER NOT NULL,
                   hash TEXT NOT NULL,
                   protected INTEGER NOT NULL DEFAULT 0,
                   approved INTEGER NOT NULL DEFAULT 0,
                   present INTEGER NOT NULL DEFAULT 1,
                   scanned_at INTEGER NOT NULL
                 );
                 CREATE TABLE operations (
                   id INTEGER PRIMARY KEY,
                   file_id INTEGER NOT NULL,
                   source_path TEXT NOT NULL,
                   trash_path TEXT NOT NULL,
                   hash TEXT NOT NULL,
                   state TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   restored_at INTEGER
                 );
                 INSERT INTO settings(key,value) VALUES('schema_version','1');
                 INSERT INTO settings(key,value) VALUES('trash_path','/tmp/recycle');
                 INSERT INTO files(path,size,modified,hash,scanned_at)
                   VALUES('/data/keep.txt',42,100,'deadbeef',100);",
            )
            .unwrap();
        drop(connection);

        // Any command entry point migrates on open; groups also proves the
        // table shape is queryable afterwards.
        let _ = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();

        let connection = open_database(&database).unwrap();
        let version: i64 = connection
            .query_row(
                "SELECT value FROM settings WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap();
        // Stepwise migrations carry the old database all the way to the
        // current schema version, not just the next one.
        assert_eq!(version, SCHEMA_VERSION);
        let (rows, hashed): (i64, String) = connection
            .query_row(
                "SELECT COUNT(*), COALESCE(MAX(hash),'') FROM files WHERE path='/data/keep.txt'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(rows, 1);
        assert_eq!(hashed, "deadbeef");
        drop(connection);

        // A migrated database must survive a real scan: write_entry touches
        // the document-fingerprint table for every non-document file, and
        // that table shipped in create_schema_sql without a migration step.
        let root = directory.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("migrated.txt"), "migrated content\n").unwrap();
        scan(&database, &[root], &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let indexed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%migrated.txt' AND present=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(indexed, 1);
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn init_on_v1_database_migrates_instead_of_failing() {
        let directory = test_directory("init-migration-v1");
        let database = directory.join("index.db");
        // Hand-rolled v1 database, same shape the desktop app's first
        // release left behind.
        let connection = open_database(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE files (
                   id INTEGER PRIMARY KEY,
                   path TEXT NOT NULL UNIQUE,
                   size INTEGER NOT NULL,
                   modified INTEGER NOT NULL,
                   hash TEXT NOT NULL,
                   protected INTEGER NOT NULL DEFAULT 0,
                   approved INTEGER NOT NULL DEFAULT 0,
                   present INTEGER NOT NULL DEFAULT 1,
                   scanned_at INTEGER NOT NULL
                 );
                 CREATE TABLE operations (
                   id INTEGER PRIMARY KEY,
                   file_id INTEGER NOT NULL,
                   source_path TEXT NOT NULL,
                   trash_path TEXT NOT NULL,
                   hash TEXT NOT NULL,
                   state TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   restored_at INTEGER
                 );
                 INSERT INTO settings(key,value) VALUES('schema_version','1');
                 INSERT INTO settings(key,value) VALUES('trash_path','/tmp/recycle');
                 INSERT INTO files(path,size,modified,hash,scanned_at)
                   VALUES('/data/keep.txt',42,100,'deadbeef',100);",
            )
            .unwrap();
        drop(connection);

        // The desktop app routes every launch through init, so it must
        // upgrade the old schema rather than run current-schema DDL on it
        // (the quick_hash index would fail on a v1 files table).
        let recycle = directory.join("recycle");
        init(&database, &recycle).unwrap();

        let connection = open_database(&database).unwrap();
        let version: i64 = connection
            .query_row(
                "SELECT value FROM settings WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let (kept, quick_hash): (i64, Option<String>) = connection
            .query_row(
                "SELECT COUNT(*), quick_hash FROM files WHERE path='/data/keep.txt'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(kept, 1);
        assert_eq!(quick_hash, None);
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn large_files_use_two_stage_hashing() {
        let directory = test_directory("two-stage");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();

        let block = vec![0xCD_u8; (LARGE_FILE_QUICK_THRESHOLD + 1024) as usize];
        // All four files share the first QUICK_HASH_CHUNK bytes, so a naive
        // quick-hash-only grouping would merge them; full hashes must split
        // them into the two real content pairs.
        let mut different = block.clone();
        different[(LARGE_FILE_QUICK_THRESHOLD + 512) as usize] ^= 0xFF;
        fs::write(source.join("same-a.bin"), &block).unwrap();
        fs::write(source.join("same-b.bin"), &block).unwrap();
        fs::write(source.join("diff-a.bin"), &different).unwrap();
        fs::write(source.join("diff-b.bin"), &different).unwrap();

        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let groups = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(groups.groups.len(), 2, "same pair groups, diff pair groups too");
        for group in &groups.groups {
            assert_eq!(group.files.len(), 2);
            assert!(!group.hash.starts_with("q:"), "promoted pairs carry real hashes");
        }

        // A lone large file is stored with the quick marker; when its twin
        // appears in a later scan, both rows end up with real hashes and one
        // duplicate group.
        let lone_block = vec![0xEF_u8; (LARGE_FILE_QUICK_THRESHOLD + 1024) as usize];
        fs::write(source.join("lone-a.bin"), &lone_block).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        fs::write(source.join("lone-b.bin"), &lone_block).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let groups = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(groups.groups.len(), 3);
        let lone = groups
            .groups
            .iter()
            .find(|group| group.files.iter().any(|file| file.path.ends_with("lone-a.bin")))
            .expect("lone pair must group after its twin arrives");
        assert_eq!(lone.files.len(), 2);
        assert!(!lone.hash.starts_with("q:"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn export_report_writes_csv_of_duplicate_members() {
        let directory = test_directory("export-report");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        // Unique and comma-bearing names: only the quoted pair appears, and
        // the comma path survives the round-trip.
        fs::write(source.join("unique.txt"), b"one of a kind").unwrap();
        fs::write(source.join("pair,1.txt"), b"quoted pair").unwrap();
        fs::write(source.join("pair,2.txt"), b"quoted pair").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let output = directory.join("reports").join("report.csv");
        let rows = export_report(&database, &output).unwrap();
        assert_eq!(rows, 4);

        let csv = fs::read_to_string(&output).unwrap();
        let mut lines = csv.lines();
        assert_eq!(lines.next().unwrap(), "hash,path,size,modified,approved");
        let mut body = lines.collect::<Vec<_>>();
        body.sort();
        assert_eq!(body.len(), 4);
        assert!(body.iter().any(|line| line.contains("pair,1.txt")));
        assert!(body.iter().any(|line| line.contains("pair,2.txt")));
        // Comma paths are RFC 4180 quoted: the whole path sits in quotes.
        assert!(body
            .iter()
            .filter(|line| line.contains("pair,1.txt"))
            .all(|line| line.contains(",\"")));
        assert!(!body.iter().any(|line| line.contains("unique.txt")));
        let hash_of = |name: &str| {
            body.iter()
                .find(|line| line.contains(name))
                .map(|line| line.split(',').next().unwrap().to_string())
                .unwrap()
        };
        assert_eq!(hash_of("pair,1.txt"), hash_of("pair,2.txt"));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    // Hard-link identity comes from POSIX dev/inode metadata; on Windows the
    // feature degrades to "unknown" by design, so the marking can't be asserted.
    #[cfg(unix)]
    fn hardlinked_names_are_marked_in_groups() {
        let directory = test_directory("hardlinks");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        let database = directory.join("index.db");

        // link-a/link-b are two names for one physical file; copy-a/copy-b
        // are genuine duplicates sharing only content.
        fs::write(source.join("link-a.txt"), b"same inode").unwrap();
        fs::hard_link(source.join("link-a.txt"), source.join("link-b.txt")).unwrap();
        fs::write(source.join("copy-a.txt"), b"same content").unwrap();
        fs::write(source.join("copy-b.txt"), b"same content").unwrap();

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let groups = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Path,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();
        assert_eq!(groups.groups.len(), 2);
        let link_group = groups
            .groups
            .iter()
            .find(|group| group.files[0].path.contains("link-a"))
            .unwrap();
        assert!(link_group.files.iter().all(|file| file.hardlinked));
        let copy_group = groups
            .groups
            .iter()
            .find(|group| group.files[0].path.contains("copy-a"))
            .unwrap();
        assert!(copy_group.files.iter().all(|file| !file.hardlinked));
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn v2_database_migrates_to_current_preserving_data() {
        let directory = test_directory("migration-v2-v3");
        let database = directory.join("index.db");
        // Hand-rolled v2 database: pre-dev/inode schema with a data row.
        let connection = open_database(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 CREATE TABLE files (
                   id INTEGER PRIMARY KEY,
                   path TEXT NOT NULL UNIQUE,
                   size INTEGER NOT NULL,
                   modified INTEGER NOT NULL,
                   hash TEXT NOT NULL,
                   protected INTEGER NOT NULL DEFAULT 0,
                   approved INTEGER NOT NULL DEFAULT 0,
                   present INTEGER NOT NULL DEFAULT 1,
                   scanned_at INTEGER NOT NULL,
                   quick_hash TEXT
                 );
                 CREATE TABLE operations (
                   id INTEGER PRIMARY KEY,
                   file_id INTEGER NOT NULL,
                   source_path TEXT NOT NULL,
                   trash_path TEXT NOT NULL,
                   hash TEXT NOT NULL,
                   state TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   restored_at INTEGER
                 );
                 INSERT INTO settings(key,value) VALUES('schema_version','2');
                 INSERT INTO settings(key,value) VALUES('trash_path','/tmp/recycle');
                 INSERT INTO files(path,size,modified,hash,scanned_at)
                   VALUES('/data/keep.txt',42,100,'deadbeef',100);",
            )
            .unwrap();
        drop(connection);

        let _ = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 10,
            },
        )
        .unwrap();

        let connection = open_database(&database).unwrap();
        let version: i64 = connection
            .query_row(
                "SELECT value FROM settings WHERE key='schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
            .parse()
            .unwrap();
        // Stepwise migrations carry v2 all the way to the current version.
        assert_eq!(version, SCHEMA_VERSION);
        let row: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE dev=0 AND inode=0",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(row, 1);
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn batch_trash_shares_one_batch_id() {
        let directory = test_directory("batch-view");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        // One three-member pair (the batch keeps the required one copy)
        // and one two-member pair removed file-by-file afterwards.
        fs::write(source.join("a1.txt"), b"pair one").unwrap();
        fs::write(source.join("a2.txt"), b"pair one").unwrap();
        fs::write(source.join("a3.txt"), b"pair one").unwrap();
        fs::write(source.join("b1.txt"), b"pair two").unwrap();
        fs::write(source.join("b2.txt"), b"pair two").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let id_of = |name: &str| -> i64 {
            connection
                .query_row(
                    "SELECT id FROM files WHERE path LIKE ?1",
                    [format!("%{name}.txt")],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let (a1, a2, b1, b2) = (id_of("a1"), id_of("a2"), id_of("b1"), id_of("b2"));
        drop(connection);

        // One batch action removes both members of pair a (approval first,
        // exactly like the UI's mark-then-process flow).
        set_approval(&database, a1, true).unwrap();
        set_approval(&database, a2, true).unwrap();
        let outcome = trash_batch(&database, &[a1, a2]);
        assert_eq!(outcome.succeeded, 2);
        assert!(outcome.failures.is_empty());
        // Single-file removal of pair b (both approved while the pair is
        // still intact; only b1 is then removed, keeping one copy as the
        // safety rule requires). No batch id.
        set_approval(&database, b1, true).unwrap();
        set_approval(&database, b2, true).unwrap();
        trash(&database, b1).unwrap();

        let connection = open_database(&database).unwrap();
        let (batched, unbatched): (i64, i64) = connection
            .query_row(
                "SELECT \
                 SUM(batch_id IS NOT NULL), SUM(batch_id IS NULL) \
                 FROM operations WHERE state='trashed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(batched, 2, "one batch action writes one shared batch id");
        assert_eq!(unbatched, 1, "single removals stay unbatched");
        let distinct: i64 = connection
            .query_row(
                "SELECT COUNT(DISTINCT batch_id) FROM operations \
                 WHERE state='trashed' AND batch_id IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(distinct, 1);
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn scan_summary_reports_expired_prune_count() {
        let directory = test_directory("summary-prune");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%b.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        set_approval(&database, b_id, true).unwrap();
        trash(&database, b_id).unwrap();
        // Backdate beyond the 30-day retention so the next scan prunes it.
        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE operations SET created_at = created_at - 40*86400",
                [],
            )
            .unwrap();
        drop(connection);

        let summary = scan_with_control(
            &database,
            std::slice::from_ref(&source),
            &[],
            &[],
            0,
            true,
            &|| false,
            &|_, _, _| {},
            None,
            None,
        )
        .unwrap();
        assert_eq!(summary.pruned, 1);
        assert!(summary.failed_roots.is_empty());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn recycle_dir_keep_one_preserves_last_copy() {
        let directory = test_directory("dir-keep-one");
        let doomed = directory.join("doomed");
        let other = directory.join("other");
        let keep = directory.join("keep");
        fs::create_dir_all(&doomed).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(&keep).unwrap();
        // Content X: two copies inside the doomed dir only — a plain sweep
        // would erase it, so one copy must move into the keep dir first.
        fs::write(doomed.join("x1.png"), b"content X").unwrap();
        fs::write(doomed.join("x2.png"), b"content X").unwrap();
        // Content Y: one copy inside + one outside — recycling the inside
        // copy leaves the outside copy alive, so nothing needs moving.
        fs::write(doomed.join("y1.png"), b"content Y").unwrap();
        fs::write(other.join("y2.png"), b"content Y").unwrap();
        // Content Z: protected copies inside — untouchable by design.
        fs::write(doomed.join("z1.png"), b"content Z").unwrap();
        fs::write(doomed.join("z2.png"), b"content Z").unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(
            &database,
            &[doomed.clone(), other.clone()],
            &["z1.png".to_string()],
        )
        .unwrap();

        let outcome = recycle_dir_keep_one(&database, &doomed.to_string_lossy(), &keep).unwrap();
        assert_eq!(outcome.moved, 1, "content X keeps one via a move");
        // x2 (non-keeper of X), y1 (outside copy survives), and z2 (its
        // protected sibling z1 keeps content Z alive) are all recycled.
        assert_eq!(outcome.recycled, 3);

        // Content X survives exactly once, inside the keep dir.
        let kept: Vec<_> = fs::read_dir(&keep)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .collect();
        assert_eq!(kept.len(), 1, "one keeper moved: {kept:?}");
        assert_eq!(fs::read(&kept[0]).unwrap(), b"content X");
        // Content Y keeps its outside copy; the inside copy is recycled.
        assert!(!doomed.join("y1.png").exists());
        assert!(other.join("y2.png").exists());
        // Content Z: the protected copy survives in place; the unprotected
        // sibling was recycled (recoverable from the recycle bin).
        assert!(doomed.join("z1.png").exists());
        assert!(!doomed.join("z2.png").exists());
        // The moved file's index row points at its new home.
        let connection = open_database(&database).unwrap();
        let moved_path: String = connection
            .query_row(
                "SELECT path FROM files WHERE path LIKE ?1",
                [format!("%{}%", keep.to_string_lossy())],
                |row| row.get(0),
            )
            .unwrap();
        assert!(moved_path.starts_with(keep.to_string_lossy().as_ref()));
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn recoverable_sort_and_hardlink_correction() {
        let directory = test_directory("recoverable-sort");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        // Group A: four hardlinked names (one physical file) — frees nothing.
        fs::write(source.join("h1.txt"), vec![b'H'; 512]).unwrap();
        fs::hard_link(source.join("h1.txt"), source.join("h2.txt")).unwrap();
        fs::hard_link(source.join("h1.txt"), source.join("h3.txt")).unwrap();
        fs::hard_link(source.join("h1.txt"), source.join("h4.txt")).unwrap();
        // Group B: two genuine copies of different content — frees one full
        // copy.
        fs::write(source.join("r1.txt"), vec![b'R'; 512]).unwrap();
        fs::write(source.join("r2.txt"), vec![b'R'; 512]).unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        #[cfg(unix)]
        {
            // On POSIX the two groups report identical per-file size, so the
            // recoverable sort must sink the hardlink-only group to zero and
            // put the real duplicates first.
            let page = query_groups(
                &database,
                &GroupQuery {
                    min_size: 0,
                    path_contains: None,
                    sort: GroupSort::Recoverable,
                    offset: 0,
                    limit: 50,
                    kind: None,
                    dir_contains: None,
                },
            )
            .unwrap();
            assert_eq!(page.groups.len(), 2);
            assert_eq!(page.groups[0].recoverable, 512, "real copies first");
            assert_eq!(page.groups[0].files[0].path, source.join("r1.txt").to_string_lossy());
            assert_eq!(page.groups[1].recoverable, 0, "hardlinked names free nothing");
        }

        // Kind filter narrows the queue to the .txt groups only.
        let images = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                offset: 0,
                limit: 50,
                kind: Some("image"),
                dir_contains: None,
            },
        )
        .unwrap();
        assert_eq!(images.total, 0, "no image files were indexed");
        let text = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                offset: 0,
                limit: 50,
                kind: Some("document"),
                dir_contains: None,
            },
        )
        .unwrap();
        assert_eq!(text.total, 2, "both groups hold .txt documents");

        // Directory filter narrows the same queue to one parent directory.
        let dirs = group_dirs(&database).unwrap();
        assert!(dirs.len() >= 1);
        let source_dir = &dirs[0].dir;
        let scoped = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                offset: 0,
                limit: 50,
                kind: None,
                dir_contains: Some(source_dir),
            },
        )
        .unwrap();
        assert_eq!(scoped.total, 2, "all duplicates share one directory here");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn query_groups_filters_sorts_and_counts() {
        let directory = test_directory("query-groups");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        // Two exact-duplicate groups plus noise: big pair (2×4096), small
        // pair (2×9), and a unique file that must never appear.
        fs::write(source.join("big-a.bin"), vec![0xAB_u8; 4096]).unwrap();
        fs::write(source.join("big-b.bin"), vec![0xAB_u8; 4096]).unwrap();
        fs::create_dir_all(source.join("notes")).unwrap();
        fs::write(source.join("notes/小.txt"), b"tiny pair").unwrap();
        fs::write(source.join("notes/大.txt"), b"tiny pair").unwrap();
        fs::write(source.join("unique.txt"), b"one of a kind").unwrap();
        let database = directory.join("index.db");

        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let all = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.groups.len(), 2);
        // Size sort puts the big pair first; members come back path-ordered.
        assert_eq!(all.groups[0].size, 4096);
        assert_eq!(all.groups[0].files[0].path, source.join("big-a.bin").to_string_lossy());

        // Path filter narrows to the group containing a matching member.
        let filtered = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: Some("大.txt"),
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(filtered.total, 1);
        assert_eq!(filtered.groups.len(), 1);
        assert_eq!(filtered.groups[0].size, 9);

        // Min size excludes the small pair from both page and total.
        let big_only = query_groups(
            &database,
            &GroupQuery {
                min_size: 100,
                path_contains: None,
                sort: GroupSort::Size,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(big_only.total, 1);
        assert_eq!(big_only.groups[0].size, 4096);

        // Member-count sort with paging through the same set.
        let first_page = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Members,
                kind: None,
                dir_contains: None,
                offset: 0,
                limit: 1,
            },
        )
        .unwrap();
        assert_eq!(first_page.total, 2);
        assert_eq!(first_page.groups.len(), 1);
        let second_page = query_groups(
            &database,
            &GroupQuery {
                min_size: 0,
                path_contains: None,
                sort: GroupSort::Members,
                kind: None,
                dir_contains: None,
                offset: 1,
                limit: 1,
            },
        )
        .unwrap();
        assert_eq!(second_page.groups.len(), 1);
        assert_ne!(first_page.groups[0].hash, second_page.groups[0].hash);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn protection_rules_match_normalized_separators() {
        let rules = vec!["/data/backup".to_string(), "keep".to_string()];
        assert!(is_protected_path("/data/backup/old/a.txt", &rules));
        assert!(is_protected_path("/data/backup", &rules));
        // Trailing separator in the rule must not create a dead rule.
        assert!(is_protected_path("/data/backup/x.txt", &vec!["/data/backup/".to_string()]));
        // Substring semantics deliberately over-protects: a rule for
        // `/data/backup` also matches `/data/backup2` (safe direction).
        assert!(is_protected_path("/data/backup2/a.txt", &rules));
        // Plain name fragments keep matching anywhere in the path.
        assert!(is_protected_path("/mnt/store/keepme.txt", &rules));
        assert!(!is_protected_path("/mnt/store/other.txt", &rules));
    }

    #[test]
    fn photo_hash_is_stable_for_the_same_image() {
        let directory = test_directory("photo-hash");
        let path = directory.join("photo.png");
        let image = image::RgbImage::from_fn(32, 32, |x, y| {
            if x > y {
                image::Rgb([240, 240, 240])
            } else {
                image::Rgb([20, 20, 20])
            }
        });
        image.save(&path).unwrap();
        let (dhash, phash) = perceptual_hash(&path).unwrap();
        let (dhash_again, phash_again) = perceptual_hash(&path).unwrap();
        assert_eq!(dhash, dhash_again);
        assert_eq!(phash, phash_again);
        assert_eq!(fingerprint_parts(dhash).len(), 5);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn phash_distinguishes_unrelated_images() {
        let directory = test_directory("phash-distinct");
        let plain = directory.join("plain.png");
        let noisy = directory.join("noisy.png");
        image::RgbImage::from_fn(64, 64, |_, _| image::Rgb([200, 200, 200]))
            .save(&plain)
            .unwrap();
        image::RgbImage::from_fn(64, 64, |x, y| {
            image::Rgb([
                ((x * 7 + y * 13) % 256) as u8,
                ((x * 3 + y * 29) % 256) as u8,
                ((x * 11 + y * 5) % 256) as u8,
            ])
        })
        .save(&noisy)
        .unwrap();
        let (_, plain_phash) = perceptual_hash(&plain).unwrap();
        let (_, noisy_phash) = perceptual_hash(&noisy).unwrap();
        let distance = (plain_phash ^ noisy_phash).count_ones();
        assert!(distance > 8, "unrelated images too close: {distance}/64");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn similar_threshold_setting_clamps_and_persists() {
        let directory = test_directory("similar-threshold");
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        assert_eq!(12, set_similar_threshold(&database, 12).unwrap());
        assert_eq!(4, set_similar_threshold(&database, 0).unwrap());
        assert_eq!(20, set_similar_threshold(&database, 999).unwrap());
        let stored: i64 = {
            let connection = open_database(&database).unwrap();
            connection
                .query_row(
                    "SELECT value FROM settings WHERE key='similar_phash_max'",
                    [],
                    |r| r.get::<_, String>(0),
                )
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(20, stored);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn stale_absent_rows_are_pruned_but_history_anchors_survive() {
        let directory = test_directory("absent-prune");
        let source = directory.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("gone.txt"), b"vanishing content").unwrap();
        fs::write(source.join("keeper.txt"), b"kept content").unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        // Both files vanish externally; the scan marks their rows absent.
        // Absence marking compares scanned_at < scan_started with second
        // granularity, so the follow-up scan must land in a later second.
        fs::remove_file(source.join("gone.txt")).unwrap();
        fs::remove_file(source.join("keeper.txt")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        // Backdate the absent rows past the 90-day horizon, then anchor
        // keeper.txt the way a real recycle would: an operations row.
        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE files SET scanned_at = scanned_at - 91*86400 WHERE present = 0",
                [],
            )
            .unwrap();
        let keeper_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%keeper.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at) \
                 VALUES(?1, 'x', 'y', 'z', 'trashed', 0)",
                params![keeper_id],
            )
            .unwrap();
        drop(connection);

        let pruned = {
            let connection = open_database(&database).unwrap();
            let pruned = prune_absent_rows(&connection).unwrap();
            drop(connection);
            pruned
        };
        assert_eq!(1, pruned, "only the anchor-free absent row is pruned");
        let connection = open_database(&database).unwrap();
        let gone_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%gone.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let keeper_rows: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%keeper.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(0, gone_rows, "anchor-free absent row is deleted");
        assert_eq!(1, keeper_rows, "history anchor row survives");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn exif_capture_time_is_parsed_and_stored() {
        let directory = test_directory("exif-taken");
        let source = directory.join("source");
        fs::create_dir_all(&source).unwrap();
        let photo = source.join("shot.jpg");
        craft_exif_jpeg(
            &photo,
            "2023:07:01 12:00:00",
        );
        let expected = days_from_civil(2023, 7, 1) * 86_400 + 12 * 3_600;
        assert_eq!(expected, exif_taken_seconds(&photo));
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let stored: i64 = connection
            .query_row(
                "SELECT exif_taken FROM files WHERE path LIKE '%shot.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(expected, stored);
        let _ = fs::remove_dir_all(directory);
    }

    /// A minimal JPEG (SOI + APP1 + EOI) whose EXIF payload carries
    /// DateTimeOriginal for the given timestamp.
    fn craft_exif_jpeg(path: &Path, datetime: &str) {
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend(b"II");
        tiff.extend(42_u16.to_le_bytes());
        tiff.extend(8_u32.to_le_bytes());
        // IFD0: a single pointer to the Exif sub-IFD at offset 26.
        tiff.extend(1_u16.to_le_bytes());
        tiff.extend(0x8769_u16.to_le_bytes());
        tiff.extend(4_u16.to_le_bytes());
        tiff.extend(1_u32.to_le_bytes());
        tiff.extend(26_u32.to_le_bytes());
        tiff.extend(0_u32.to_le_bytes());
        // Exif sub-IFD at 26: DateTimeOriginal ASCII, value at offset 44.
        tiff.extend(1_u16.to_le_bytes());
        tiff.extend(0x9003_u16.to_le_bytes());
        tiff.extend(2_u16.to_le_bytes());
        tiff.extend((datetime.len() as u32 + 1).to_le_bytes());
        tiff.extend(44_u32.to_le_bytes());
        tiff.extend(0_u32.to_le_bytes());
        tiff.extend(datetime.as_bytes());
        tiff.push(0);

        let mut jpeg: Vec<u8> = vec![0xFF, 0xD8];
        jpeg.extend(0xFF_E1_u16.to_be_bytes());
        jpeg.extend(((6 + tiff.len() + 2) as u16).to_be_bytes());
        jpeg.extend(b"Exif\0\0");
        jpeg.extend(&tiff);
        jpeg.extend([0xFF, 0xD9]);
        fs::write(path, jpeg).unwrap();
    }

    #[test]
    fn scan_inherits_renamed_file_without_rehash() {
        let directory = test_directory("rename-detect");
        let source = directory.join("source");
        fs::create_dir_all(&source).unwrap();
        image::RgbImage::from_fn(48, 48, |x, y| {
            if x > y {
                image::Rgb([220, 220, 40])
            } else {
                image::Rgb([30, 30, 90])
            }
        })
        .save(source.join("pic.png"))
        .unwrap();
        fs::write(source.join("note.txt"), b"note").unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        let pic_id: i64 = connection
            .query_row("SELECT id FROM files WHERE path LIKE '%pic.png'", [], |r| r.get(0))
            .unwrap();
        let note_id: i64 = connection
            .query_row("SELECT id FROM files WHERE path LIKE '%note.txt'", [], |r| r.get(0))
            .unwrap();
        let fingerprint_id: i64 = connection
            .query_row("SELECT file_id FROM photo_fingerprints", [], |r| r.get(0))
            .unwrap();
        let pic_hash: String = connection
            .query_row("SELECT hash FROM files WHERE id=?1", params![pic_id], |r| r.get(0))
            .unwrap();
        drop(connection);

        // Rename both files and rescan: the rows must keep their identity
        // (same id, hash and fingerprint) under the new paths instead of
        // appearing as a new row plus an absent one.
        fs::rename(source.join("pic.png"), source.join("pic-moved.png")).unwrap();
        fs::rename(source.join("note.txt"), source.join("note-moved.txt")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(2, rows, "a rename must not duplicate or drop rows");
        let new_pic_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%pic-moved.png'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let new_note_id: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE '%note-moved.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pic_id, new_pic_id, "image row keeps its file_id");
        assert_eq!(note_id, new_note_id, "document row keeps its file_id");
        let fingerprint_id_after: i64 = connection
            .query_row("SELECT file_id FROM photo_fingerprints", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fingerprint_id, fingerprint_id_after, "no fingerprint rebuild");
        let hash_after: String = connection
            .query_row(
                "SELECT hash FROM files WHERE id=?1",
                params![new_pic_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pic_hash, hash_after, "content hash inherited, not recomputed");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn pair_verdict_cache_roundtrips_and_invalidates() {
        let directory = test_directory("pair-verdict-cache");
        let left = directory.join("left");
        fs::create_dir_all(&left).unwrap();
        fs::write(left.join("a.txt"), b"same").unwrap();
        fs::write(left.join("b.txt"), b"same").unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, std::slice::from_ref(&left), &[]).unwrap();
        let path_a = left.join("a.txt").to_string_lossy().into_owned();
        let path_b = left.join("b.txt").to_string_lossy().into_owned();

        // Store in one orientation, read back in the other: the cache is
        // canonicalized so both hit the same row.
        assert_eq!(
            1,
            store_pair_verdicts(&database, &[(path_a.clone(), path_b.clone(), true)]).unwrap()
        );
        let verdicts = cached_pair_verdicts(
            &database,
            &[
                (path_a.clone(), path_b.clone()),
                (path_b.clone(), path_a.clone()),
            ],
        )
        .unwrap();
        assert_eq!(vec![Some(true), Some(true)], verdicts);

        // Touching one file's content changes its size: the row must stop
        // being served.
        fs::write(left.join("a.txt"), b"same but longer").unwrap();
        scan(&database, std::slice::from_ref(&left), &[]).unwrap();
        let verdicts = cached_pair_verdicts(&database, &[(path_a.clone(), path_b.clone())]).unwrap();
        assert_eq!(vec![None], verdicts, "changed file must invalidate");

        // Restoring the original content re-validates the same verdict.
        fs::write(left.join("a.txt"), b"same").unwrap();
        scan(&database, std::slice::from_ref(&left), &[]).unwrap();
        assert_eq!(
            1,
            store_pair_verdicts(&database, &[(path_b.clone(), path_a.clone(), false)]).unwrap()
        );
        let verdicts = cached_pair_verdicts(&database, &[(path_a, path_b)]).unwrap();
        assert_eq!(vec![Some(false)], verdicts);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn merge_dir_pair_recycles_one_side_and_moves_the_other() {
        let directory = test_directory("dir-merge");
        let left = directory.join("left");
        let right = directory.join("right");
        fs::create_dir_all(&left).unwrap();
        fs::create_dir_all(&right).unwrap();
        // Two confirmed duplicate pairs across the directories, plus one
        // file unique to the left directory.
        fs::write(left.join("a.txt"), b"same").unwrap();
        fs::write(right.join("a.txt"), b"same").unwrap();
        fs::write(left.join("pair.txt"), b"pair").unwrap();
        fs::write(right.join("pair.txt"), b"pair").unwrap();
        fs::write(left.join("solo.txt"), b"left unique").unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, &[left.clone(), right.clone()], &[]).unwrap();

        // Merge one pair toward the left: the left copy loses (recycled),
        // the right file wins and takes its place. The right directory
        // keeps its other file, so the cleanup leaves it alone.
        let outcome = merge_dir_pair(
            &database,
            &[(
                left.join("a.txt").to_string_lossy().into_owned(),
                right.join("a.txt").to_string_lossy().into_owned(),
            )],
            &left.to_string_lossy(),
            &[right.to_string_lossy().into_owned()],
        )
        .unwrap();
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(1, outcome.recycled);
        assert_eq!(1, outcome.moved);
        assert_eq!(0, outcome.kept);
        assert!(outcome.cleaned_dirs.is_empty());
        assert_eq!(b"same", fs::read(left.join("a.txt")).unwrap().as_slice());
        assert!(!right.join("a.txt").exists());
        assert!(right.join("pair.txt").exists());
        assert!(left.join("solo.txt").exists(), "unrelated file untouched");

        // A protected loser blocks the whole pair: its winner stays put —
        // moving it would create a fresh duplicate inside the target.
        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE files SET protected=1 WHERE path LIKE '%left%pair.txt'",
                [],
            )
            .unwrap();
        drop(connection);
        let outcome = merge_dir_pair(
            &database,
            &[(
                left.join("pair.txt").to_string_lossy().into_owned(),
                right.join("pair.txt").to_string_lossy().into_owned(),
            )],
            &left.to_string_lossy(),
            &[right.to_string_lossy().into_owned()],
        )
        .unwrap();
        assert_eq!(0, outcome.recycled);
        assert_eq!(0, outcome.moved);
        assert_eq!(2, outcome.failures.len(), "both phases report the skip");
        assert!(left.join("pair.txt").exists());
        assert!(right.join("pair.txt").exists());

        // Unprotected again: the merge now empties the right directory and
        // the cleanup removes it, leaving the target with the moved files.
        let connection = open_database(&database).unwrap();
        connection
            .execute(
                "UPDATE files SET protected=0 WHERE path LIKE '%left%pair.txt'",
                [],
            )
            .unwrap();
        drop(connection);
        let outcome = merge_dir_pair(
            &database,
            &[(
                left.join("pair.txt").to_string_lossy().into_owned(),
                right.join("pair.txt").to_string_lossy().into_owned(),
            )],
            &left.to_string_lossy(),
            &[right.to_string_lossy().into_owned()],
        )
        .unwrap();
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        assert_eq!(1, outcome.recycled);
        assert_eq!(1, outcome.moved);
        assert_eq!(
            &[right.to_string_lossy().into_owned()],
            outcome.cleaned_dirs.as_slice()
        );
        assert!(!right.exists());
        assert_eq!(b"pair", fs::read(left.join("pair.txt")).unwrap().as_slice());
        // The index tracks the moved files at their new location.
        let connection = open_database(&database).unwrap();
        let live_in_left: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE present=1 AND path LIKE '%left%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(3, live_in_left);
        let live_in_right: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE present=1 AND path LIKE '%right%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(0, live_in_right);
        let trashed: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM operations WHERE state='trashed'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(2, trashed, "both replaced copies are recoverable");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn verify_photo_pairs_streams_verdicts_and_stops_on_request() {
        let directory = test_directory("pair-verify-batch");
        let same_a = directory.join("same-a.png");
        let same_b = directory.join("same-b.png");
        let other = directory.join("other.png");
        let image = image::RgbImage::from_fn(36, 20, |x, y| {
            image::Rgb([((x * 7) % 256) as u8, ((y * 11) % 256) as u8, 60])
        });
        image.save(&same_a).unwrap();
        image.save(&same_b).unwrap();
        image::RgbImage::from_fn(36, 20, |x, y| {
            image::Rgb([((x * 7) % 256) as u8, ((y * 11) % 256) as u8, 190])
        })
        .save(&other)
        .unwrap();
        let pa = same_a.to_string_lossy().to_string();
        let pb = same_b.to_string_lossy().to_string();
        let po = other.to_string_lossy().to_string();
        let mut pairs = Vec::new();
        for _ in 0..6 {
            pairs.push((pa.clone(), pb.clone()));
        }
        for _ in 0..3 {
            pairs.push((pa.clone(), po.clone()));
        }
        let verdicts = std::sync::Mutex::new(Vec::new());
        let (same, different) = verify_photo_pairs(
            &pairs,
            &|verdict: &PairVerdict| {
                verdicts
                    .lock()
                    .unwrap()
                    .push((verdict.second == pb, verdict.identical));
            },
            &|| false,
        );
        let collected = verdicts.lock().unwrap();
        assert_eq!(pairs.len(), collected.len(), "every pair gets a verdict");
        assert_eq!(6, same);
        assert_eq!(3, different);
        // A pair is identical exactly when its second side is same-b.png.
        for &(second_is_b, identical) in collected.iter() {
            assert_eq!(second_is_b, identical, "verdict must follow the pair");
        }
        drop(collected);

        // Cooperative stop: flipping the flag inside the callback keeps the
        // remaining pairs from being verified.
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (same, _different) = verify_photo_pairs(
            &vec![(pa.clone(), pb.clone()); 80],
            &|_verdict: &PairVerdict| {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            },
            &|| stop.load(std::sync::atomic::Ordering::Relaxed),
        );
        assert!(same < 80, "cancellation must leave most pairs unverified");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn photos_pixel_identical_checks_exact_pixels() {
        let directory = test_directory("pixel-identical");
        let copy = directory.join("copy.png");
        let other_size = directory.join("other-size.png");
        let other_pixels = directory.join("other-pixels.png");
        // Same pixels saved twice: identical verdict despite separate files.
        let image = image::RgbImage::from_fn(40, 24, |x, y| {
            image::Rgb([((x * 5) % 256) as u8, ((y * 9) % 256) as u8, 77])
        });
        image.save(&copy).unwrap();
        image.save(directory.join("copy2.png")).unwrap();
        assert!(photos_pixel_identical(
            &copy.to_string_lossy(),
            &directory.join("copy2.png").to_string_lossy()
        )
        .unwrap());
        // Different dimensions: never pixel-identical.
        image::RgbImage::from_fn(24, 40, |x, y| {
            image::Rgb([((x * 5) % 256) as u8, ((y * 9) % 256) as u8, 77])
        })
        .save(&other_size)
        .unwrap();
        assert!(!photos_pixel_identical(
            &copy.to_string_lossy(),
            &other_size.to_string_lossy()
        )
        .unwrap());
        // Same dimensions, different pixels: rejected.
        image::RgbImage::from_fn(40, 24, |x, y| {
            image::Rgb([((x * 5) % 256) as u8, ((y * 9) % 256) as u8, 200])
        })
        .save(&other_pixels)
        .unwrap();
        assert!(!photos_pixel_identical(
            &copy.to_string_lossy(),
            &other_pixels.to_string_lossy()
        )
        .unwrap());
        // Undecodable input surfaces an error instead of a silent verdict.
        let broken = directory.join("broken.png");
        fs::write(&broken, b"not an image").unwrap();
        assert!(photos_pixel_identical(&copy.to_string_lossy(), &broken.to_string_lossy()).is_err());
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn scan_fingerprints_images_with_phash_and_rebuilds_when_rows_vanish() {
        let directory = test_directory("phash-rebuild");
        let source = directory.join("source");
        fs::create_dir_all(&source).unwrap();
        image::RgbImage::from_fn(48, 48, |x, y| {
            if (x + y) % 2 == 0 {
                image::Rgb([250, 60, 60])
            } else {
                image::Rgb([10, 10, 90])
            }
        })
        .save(source.join("pic.png"))
        .unwrap();
        let database = directory.join("index.db");
        init(&database, &directory.join("recycle")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let first: i64 = connection
            .query_row("SELECT phash FROM photo_fingerprints", [], |r| r.get(0))
            .unwrap();
        assert_ne!(0, first, "pHash must be stored");
        // Simulate a fingerprint algorithm bump: rows removed, the image
        // itself untouched. The next scan must rebuild the fingerprint even
        // though size/mtime are unchanged.
        connection.execute("DELETE FROM photo_fingerprints", []).unwrap();
        drop(connection);
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let second: i64 = connection
            .query_row("SELECT phash FROM photo_fingerprints", [], |r| r.get(0))
            .unwrap();
        assert_eq!(first, second, "recomputed pHash must be deterministic");
    }

    #[test]
    fn hardlink_dedup_preserves_paths_and_records_the_operation() {
        let directory = test_directory("hardlink-dedup");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE ?1",
                ["%b.txt"],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        set_approval(&database, b, true).unwrap();
        hardlink(&database, b).unwrap();

        // Both paths still exist and hold the keeper's bytes.
        let kept = fs::read(source.join("a.txt")).unwrap();
        let linked = fs::read(source.join("b.txt")).unwrap();
        assert_eq!(kept, linked);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta_a = fs::metadata(source.join("a.txt")).unwrap();
            let meta_b = fs::metadata(source.join("b.txt")).unwrap();
            assert_eq!(meta_a.ino(), meta_b.ino(), "names must share one inode");
            assert_eq!(meta_a.nlink(), 2);
        }
        let connection = open_database(&database).unwrap();
        let (present, approved): (i64, i64) = connection
            .query_row(
                "SELECT COUNT(*), SUM(approved) FROM files WHERE present=1 AND path LIKE ?1",
                [format!("%b.txt")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!((present, approved), (1, 0));
        let (state, keeper): (String, String) = connection
            .query_row(
                "SELECT state,trash_path FROM operations WHERE source_path LIKE ?1",
                [format!("%b.txt")],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "hardlinked");
        assert!(keeper.ends_with("a.txt"), "operation records the keeper: {keeper}");
        drop(connection);
        // Re-running on an already-linked name: POSIX detects the shared
        // inode and refuses; Windows cannot tell and the idempotent swap
        // simply succeeds with the same end state.
        set_approval(&database, b, true).unwrap();
        #[cfg(unix)]
        {
            let error = hardlink(&database, b).unwrap_err();
            assert!(error.contains("already a hard link"), "{error}");
        }
        #[cfg(not(unix))]
        {
            hardlink(&database, b).unwrap();
        }
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn moved_onto_a_stale_path_does_not_wedge_the_scan() {
        let directory = test_directory("moved-onto-stale");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();

        fs::write(source.join("a.txt"), b"content alpha").unwrap();
        fs::write(source.join("b.txt"), b"content beta with more bytes").unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        // Delete b externally, then rename a onto b's old name. The index
        // still holds b as present=1 when the scan meets the renamed file;
        // retiring that stale row must precede the re-point or the path
        // UNIQUE constraint aborts this and every future scan.
        fs::remove_file(source.join("b.txt")).unwrap();
        fs::rename(source.join("a.txt"), source.join("b.txt")).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        let total: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM files WHERE path LIKE '%b.txt%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(total, 2, "stale row renamed aside, moved row re-pointed");
        let size: i64 = connection
            .query_row(
                "SELECT size FROM files WHERE path LIKE '%b.txt'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(size, 13, "the live row carries a's content, not b's");
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn quick_hash_promotion_survives_a_vanished_sibling() {
        let directory = test_directory("promotion-vanished");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();

        let block = vec![0xCD_u8; (LARGE_FILE_QUICK_THRESHOLD + 1024) as usize];
        fs::write(source.join("gone.bin"), &block).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        // The lone file vanishes and a same-size, same-head twin arrives:
        // the promotion path re-hashes the stale row, hits the missing file
        // and must record a per-file error instead of aborting the scan.
        fs::remove_file(source.join("gone.bin")).unwrap();
        let mut twin = block.clone();
        twin[(LARGE_FILE_QUICK_THRESHOLD + 512) as usize] ^= 0xFF;
        fs::write(source.join("twin.bin"), &twin).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();

        let connection = open_database(&database).unwrap();
        let hash: String = connection
            .query_row(
                "SELECT hash FROM files WHERE path LIKE '%twin.bin'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!hash.starts_with("q:"), "the twin must end up fully hashed");
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(windows)]
    #[test]
    fn hardlink_refreshes_the_windows_file_reference() {
        let directory = test_directory("hardlink-frn");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"same content").unwrap();
        fs::write(source.join("b.txt"), b"same content").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b: i64 = connection
            .query_row(
                "SELECT id FROM files WHERE path LIKE ?1",
                ["%b.txt"],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        set_approval(&database, b, true).unwrap();
        hardlink(&database, b).unwrap();

        // The linked name now resolves to the keeper's file reference; a
        // stale one would make rename detection re-point the keeper's own
        // row when the linked name is renamed later.
        let connection = open_database(&database).unwrap();
        let row_frn: i64 = connection
            .query_row(
                "SELECT frn FROM files WHERE path LIKE ?1",
                ["%b.txt"],
                |row| row.get(0),
            )
            .unwrap();
        drop(connection);
        let real_frn =
            i64::try_from(usn::file_reference_of(&source.join("b.txt")).unwrap()).unwrap();
        assert_ne!(real_frn, 0);
        assert_eq!(row_frn, real_frn, "row must carry the keeper's file reference");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn hardlink_rejects_diverged_content_without_touching_it() {
        let directory = test_directory("hardlink-tampered");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"original").unwrap();
        fs::write(source.join("b.txt"), b"original").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        let connection = open_database(&database).unwrap();
        let b: i64 = connection
            .query_row("SELECT id FROM files WHERE path LIKE ?1", ["%b.txt"], |r| r.get(0))
            .unwrap();
        drop(connection);
        fs::write(source.join("b.txt"), b"edited").unwrap();
        set_approval(&database, b, true).unwrap();
        let error = hardlink(&database, b).unwrap_err();
        assert!(error.contains("no longer matches indexed hash"), "{error}");
        assert_eq!(
            fs::read(source.join("b.txt")).unwrap(),
            b"edited",
            "the file must be untouched"
        );
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn strict_verify_roundtrips_and_keeps_the_chain_working() {
        let directory = test_directory("strict-verify");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("a.txt"), b"pair").unwrap();
        fs::write(source.join("b.txt"), b"pair").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        scan(&database, std::slice::from_ref(&source), &[]).unwrap();
        set_strict_verify(&database, true).unwrap();
        let connection = open_database(&database).unwrap();
        let enabled: String = connection
            .query_row(
                "SELECT value FROM settings WHERE key='strict_verify'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(enabled, "1");
        let b: i64 = connection
            .query_row("SELECT id FROM files WHERE path LIKE ?1", ["%b.txt"], |r| r.get(0))
            .unwrap();
        drop(connection);
        set_approval(&database, b, true).unwrap();
        // With strict mode on, the full trash chain must still succeed for
        // genuinely identical content.
        trash(&database, b).unwrap();
        assert!(!source.join("b.txt").exists());
        set_strict_verify(&database, false).unwrap();
        let connection = open_database(&database).unwrap();
        let enabled: String = connection
            .query_row(
                "SELECT value FROM settings WHERE key='strict_verify'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(enabled, "0");
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn usn_setting_on_still_scans_correctly_through_fallback() {
        let directory = test_directory("usn-fallback");
        let source = directory.join("source");
        let recycle = directory.join("recycle");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("keep.txt"), b"content").unwrap();
        fs::write(source.join("drop.txt"), b"ok").unwrap();
        let database = directory.join("index.db");
        init(&database, &recycle).unwrap();
        set_usn_scan(&database, true).unwrap();
        // Whether the USN listing is available (elevated) or refused
        // (non-admin), the scan must index exactly the right files.
        scan_with_options(
            &database,
            std::slice::from_ref(&source),
            &[],
            &[],
            4,
            false,
        )
        .unwrap();
        let connection = open_database(&database).unwrap();
        let indexed: i64 = connection
            .query_row("SELECT COUNT(*) FROM files WHERE present=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(indexed, 1, "only keep.txt passes the minimum size");
        drop(connection);
        let _ = fs::remove_dir_all(directory);
    }
}
