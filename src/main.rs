use std::{
    collections::HashMap,
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};

/// Latest schema version this build understands. Older databases are
/// upgraded stepwise through MIGRATIONS on open; newer ones are rejected so a
/// downgrade can never misread an unknown schema.
const SCHEMA_VERSION: i64 = 3;
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
       inode INTEGER NOT NULL DEFAULT 0
     );
     CREATE INDEX IF NOT EXISTS files_hash_size_present ON files(hash, size, present);
     CREATE INDEX IF NOT EXISTS files_size_quick_hash ON files(size, quick_hash);
     CREATE TABLE IF NOT EXISTS photo_fingerprints (
       file_id INTEGER PRIMARY KEY,
       dhash INTEGER NOT NULL,
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
       FOREIGN KEY(file_id) REFERENCES files(id)
     );"
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
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")
        .map_err(|e| format!("configure database: {e}"))?;
    Ok(connection)
}

pub fn init(database: &Path, trash: &Path) -> Result<(), String> {
    fs::create_dir_all(trash).map_err(|e| format!("create recycle bin: {e}"))?;
    let connection = open_database(database)?;
    connection
        .execute_batch(create_schema_sql())
        .map_err(|e| format!("create schema: {e}"))?;
    set_setting(&connection, "schema_version", &SCHEMA_VERSION.to_string())?;
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
) -> Result<ScanSummary, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let trash = PathBuf::from(required_setting(&connection, "trash_path")?);
    let scan_started = now_seconds()?;
    let mut counters = ScanCounters::default();
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
            total,
        )?;
        scanned_roots.push(root.clone());
    }
    let removed = mark_missing_absent(&connection, &scanned_roots, scan_started)?;
    let pruned = prune_expired_trash_with(&connection, now_seconds()?)?;
    set_setting(&connection, "last_scan_at", &now_seconds()?.to_string())?;
    if !silent {
        println!(
            "Scanned: {} new, {} unchanged, {} updated, {} skipped, {} errors, {} missing, {} expired recycled.",
            counters.new,
            counters.unchanged,
            counters.updated,
            counters.skipped,
            counters.errors,
            removed,
            pruned
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
    photo: Option<u64>,
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
}

/// Hash and fingerprint one file on a worker thread. Pure file I/O: the
/// database is only touched by the writer thread.
fn process_file(path: PathBuf) -> WorkResult {
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
            (hash_file(&path)?, None)
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
        Ok(ProcessedEntry {
            path_text: path_text.clone(),
            size,
            modified,
            hash,
            quick_hash,
            dev,
            inode,
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
/// to the hashing workers. Uses its own read-only view of the database.
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
            if !file_type.is_file() || is_excluded(&path, trash, exclude) {
                let _ = result_tx.send(WorkResult::Skipped);
                continue;
            }
            // Below-threshold files are dropped from the report entirely:
            // they are skipped here (not marked seen), so a previous index
            // row is marked absent by mark_missing_absent after the scan.
            if min_file_size > 0 {
                let too_small = entry
                    .metadata()
                    .map(|metadata| metadata.len() < min_file_size)
                    .unwrap_or(false);
                if too_small {
                    let _ = result_tx.send(WorkResult::Skipped);
                    continue;
                }
            }
            if is_unchanged(&connection, &path) {
                let _ = result_tx.send(WorkResult::Unchanged(
                    path.to_string_lossy().into_owned(),
                ));
            } else if work_tx.send(path).is_err() {
                // Workers only exit early on cancellation.
                return Err("scan cancelled".to_string());
            }
        }
    }
    Ok(())
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
        "INSERT INTO files(path,size,modified,hash,protected,approved,present,scanned_at,quick_hash,dev,inode) VALUES(?1,?2,?3,?4,?5,0,1,?6,?7,?8,?9)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size,modified=excluded.modified,hash=excluded.hash,protected=excluded.protected,approved=0,present=1,scanned_at=excluded.scanned_at,quick_hash=excluded.quick_hash,dev=excluded.dev,inode=excluded.inode",
        params![entry.path_text, entry.size, entry.modified, entry.hash, is_protected as i64, now, entry.quick_hash, entry.dev, entry.inode],
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
    if let Some(photo) = entry.photo {
        let parts = fingerprint_parts(photo);
        connection.execute(
            "INSERT INTO photo_fingerprints(file_id,dhash,part_a,part_b,part_c,part_d,part_e) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![file_id, photo as i64, parts[0], parts[1], parts[2], parts[3], parts[4]],
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
                        let _ = result_tx.send(process_file(path));
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
                                promoted.hash = hash_file(Path::new(&promoted.path_text))?;
                                match write_entry(connection, &promoted, protect)? {
                                    IndexOutcome::New => counters.new += 1,
                                    IndexOutcome::Updated => counters.updated += 1,
                                    IndexOutcome::Unchanged => counters.unchanged += 1,
                                }
                            }
                            // Indexed rows promoted in an earlier scan still
                            // carry their quick marker; refresh them so a
                            // newly found twin groups under the real hash.
                            refresh_quick_hash_rows(connection, key.0, &quick)?;
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
// never splits what is actually one duplicate group.
fn refresh_quick_hash_rows(
    connection: &Connection,
    size: i64,
    quick: &str,
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
        let hash = hash_file(Path::new(&path))?;
        connection
            .execute(
                "UPDATE files SET hash=?1 WHERE path=?2",
                params![hash, path],
            )
            .map_err(|e| e.to_string())?;
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
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let count = file.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
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

fn perceptual_hash(path: &Path) -> Option<u64> {
    let image = image::ImageReader::open(path)
        .ok()?
        .decode()
        .ok()?
        .resize_exact(9, 8, image::imageops::FilterType::Triangle)
        .to_luma8();
    let mut hash = 0_u64;
    for y in 0..8 {
        for x in 0..8 {
            hash = (hash << 1) | u64::from(image.get_pixel(x, y)[0] > image.get_pixel(x + 1, y)[0]);
        }
    }
    Some(hash)
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
    let file =
        file_by_id(&connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if approved && file.protected {
        return Err("protected files cannot be approved".to_string());
    }
    if approved && !is_exact_duplicate(&connection, &file)? {
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

pub fn trash(database: &Path, file_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let file =
        file_by_id(&connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if file.protected || !file.approved || !is_exact_duplicate(&connection, &file)? {
        return Err(
            "file must be an approved, unprotected member of an exact duplicate group".to_string(),
        );
    }
    let source = PathBuf::from(&file.path);
    if hash_file(&source)? != file.hash {
        return Err("source no longer matches indexed hash; scan again".to_string());
    }
    let destination = recycle_destination(&connection, file.id, &source)?;
    move_verified(&source, &destination, &file.hash)?;
    connection.execute("INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at) VALUES(?1,?2,?3,?4,'trashed',?5)", params![file.id, file.path, destination.to_string_lossy(), file.hash, now_seconds()?]).map_err(|e| e.to_string())?;
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
    if source.exists() {
        return Err(format!(
            "restore refused: destination already exists: {}",
            source.display()
        ));
    }
    if hash_file(&trashed)? != hash {
        return Err("recycle-bin file failed integrity check".to_string());
    }
    if let Some(parent) = source.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create restore directory: {e}"))?;
    }
    move_verified(&trashed, &source, &hash)?;
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
    println!("Restored: {}", source.display());
    Ok(())
}

/// Permanently delete one approved duplicate instead of moving it to the
/// recycle bin. Safety checks mirror `trash`.
pub fn delete_direct(database: &Path, file_id: i64) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let file =
        file_by_id(&connection, file_id)?.ok_or_else(|| format!("unknown file id {file_id}"))?;
    if file.protected || !file.approved || !is_exact_duplicate(&connection, &file)? {
        return Err(
            "file must be an approved, unprotected member of an exact duplicate group".to_string(),
        );
    }
    let source = PathBuf::from(&file.path);
    if hash_file(&source)? != file.hash {
        return Err("source no longer matches indexed hash; scan again".to_string());
    }
    fs::remove_file(&source).map_err(|e| format!("delete file: {e}"))?;
    connection
        .execute(
            "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at) VALUES(?1,?2,'',?3,'deleted',?4)",
            params![file.id, file.path, file.hash, now_seconds()?],
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
pub struct BatchOutcome {
    pub succeeded: usize,
    pub failures: Vec<String>,
}

fn remove_paths(database: &Path, paths: &[String], to_trash: bool) -> Result<BatchOutcome, String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let mut outcome = BatchOutcome {
        succeeded: 0,
        failures: Vec::new(),
    };
    for path in paths {
        match remove_indexed_path(&connection, path, to_trash) {
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
pub fn trash_paths(database: &Path, paths: &[String]) -> Result<BatchOutcome, String> {
    remove_paths(database, paths, true)
}

/// Permanently delete user-picked paths behind the same safety chain as
/// `trash_paths`.
pub fn delete_paths(database: &Path, paths: &[String]) -> Result<BatchOutcome, String> {
    remove_paths(database, paths, false)
}

fn remove_indexed_path(connection: &Connection, path: &str, to_trash: bool) -> Result<(), String> {
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
        connection.execute("INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at) VALUES(?1,?2,?3,?4,'trashed',?5)", params![file_id, path, destination.to_string_lossy(), hash, now_seconds()?]).map_err(|e| e.to_string())?;
    } else {
        fs::remove_file(&source).map_err(|e| format!("删除文件失败：{e}"))?;
        connection
            .execute(
                "INSERT INTO operations(file_id,source_path,trash_path,hash,state,created_at) VALUES(?1,?2,'',?3,'deleted',?4)",
                params![file_id, path, hash, now_seconds()?],
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
}

/// Filters and paging for `query_groups`.
pub struct GroupQuery<'a> {
    pub min_size: i64,
    pub path_contains: Option<&'a str>,
    pub sort: GroupSort,
    pub offset: i64,
    pub limit: i64,
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
}

#[derive(serde::Serialize)]
pub struct Group {
    pub hash: String,
    pub size: i64,
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
    let pattern = format!(
        "%{}%",
        like_escape(query.path_contains.unwrap_or_default().trim())
    );
    // LIKE is ASCII-case-insensitive by default, close enough for a search box.
    let filter_sql =
        "FROM files WHERE present=1 GROUP BY hash,size \
         HAVING COUNT(*) > 1 AND size >= ?1 AND SUM(path LIKE ?2 ESCAPE '\\') > 0";
    let order_sql = match query.sort {
        GroupSort::Size => "size DESC, members DESC",
        GroupSort::Members => "members DESC, size DESC",
        GroupSort::Path => "first_path",
    };
    let total: i64 = connection
        .query_row(
            &format!("SELECT COUNT(*) FROM (SELECT 1 {filter_sql})"),
            params![min_size, pattern],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let mut statement = connection
        .prepare(&format!(
            "SELECT hash,size,COUNT(*) AS members,MIN(path) AS first_path {filter_sql} \
             ORDER BY {order_sql} LIMIT ?3 OFFSET ?4"
        ))
        .map_err(|e| e.to_string())?;
    let keys = statement
        .query_map(params![min_size, pattern, limit, offset], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
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
        for (hash, size) in &keys {
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
                 WHERE present=1 AND ({conditions}) ORDER BY path"
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
        .map(|(hash, size)| {
            let members = members.remove(&(hash.clone(), size)).unwrap_or_default();
            // A (dev, inode) pair appearing twice inside one group means two
            // of the "duplicates" are names for the same physical file.
            let mut identities: HashMap<(i64, i64), usize> = HashMap::new();
            for (_, dev, inode) in &members {
                if *dev != 0 || *inode != 0 {
                    *identities.entry((*dev, *inode)).or_default() += 1;
                }
            }
            let files = members
                .into_iter()
                .map(|(mut file, dev, inode)| {
                    file.hardlinked =
                        identities.get(&(dev, inode)).copied().unwrap_or(0) > 1;
                    file
                })
                .collect();
            Group { hash, size, files }
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
            "SELECT id,path,size,hash,protected,approved FROM files WHERE id=?1 AND present=1",
            params![id],
            |r| {
                Ok(IndexedFile {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    size: r.get(2)?,
                    hash: r.get(3)?,
                    protected: r.get::<_, i64>(4)? != 0,
                    approved: r.get::<_, i64>(5)? != 0,
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
    // Cover the trash/history orderings. `IF NOT EXISTS` keeps this idempotent
    // so databases created before the indexes existed pick them up on open.
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS operations_state_created
                 ON operations(state, created_at);
             CREATE INDEX IF NOT EXISTS operations_created
                 ON operations(created_at);",
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
    fn v2_database_migrates_to_v3_preserving_data() {
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
        assert_eq!(version, 3);
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
        )
        .unwrap();
        assert_eq!(summary.pruned, 1);
        assert!(summary.failed_roots.is_empty());
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
        let hash = perceptual_hash(&path).unwrap();
        assert_eq!(hash, perceptual_hash(&path).unwrap());
        assert_eq!(fingerprint_parts(hash).len(), 5);
        let _ = fs::remove_dir_all(directory);
    }
}
