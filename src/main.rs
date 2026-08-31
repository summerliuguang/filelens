use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use rusqlite::{Connection, OptionalExtension, params};

const SCHEMA_VERSION: i64 = 1;
const COPY_BUFFER_SIZE: usize = 8 * 1024 * 1024;

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
        } => scan(&database, &roots, &protect),
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
        .execute_batch(
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
           scanned_at INTEGER NOT NULL
         );
         CREATE INDEX IF NOT EXISTS files_hash_size_present ON files(hash, size, present);
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
         );",
        )
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
    scan_with_control(database, roots, protect, &|| false, &|_| {})
}

pub fn scan_with_control(
    database: &Path,
    roots: &[PathBuf],
    protect: &[String],
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(u64),
) -> Result<(), String> {
    let connection = open_database(database)?;
    ensure_initialized(&connection)?;
    let trash = PathBuf::from(required_setting(&connection, "trash_path")?);
    let mut counters = ScanCounters::default();
    for root in roots {
        if cancelled() {
            return Err("scan cancelled".to_string());
        }
        let root = absolute_path(root)?;
        if !root.is_dir() {
            return Err(format!("scan root is not a directory: {}", root.display()));
        }
        scan_directory(
            &connection,
            &root,
            &trash,
            protect,
            &mut counters,
            cancelled,
            progress,
        )?;
    }
    println!(
        "Scanned: {} new, {} unchanged, {} updated, {} skipped, {} errors.",
        counters.new, counters.unchanged, counters.updated, counters.skipped, counters.errors
    );
    Ok(())
}

#[derive(Default)]
struct ScanCounters {
    new: u64,
    unchanged: u64,
    updated: u64,
    skipped: u64,
    errors: u64,
}

fn scan_directory(
    connection: &Connection,
    root: &Path,
    trash: &Path,
    protect: &[String],
    counters: &mut ScanCounters,
    cancelled: &dyn Fn() -> bool,
    progress: &dyn Fn(u64),
) -> Result<(), String> {
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        if cancelled() {
            return Err("scan cancelled".to_string());
        }
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                eprintln!("warning: cannot read {}: {error}", directory.display());
                counters.errors += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            if cancelled() {
                return Err("scan cancelled".to_string());
            }
            let path = entry.path();
            let file_type = match entry.file_type() {
                Ok(value) => value,
                Err(_) => {
                    counters.errors += 1;
                    continue;
                }
            };
            if file_type.is_symlink() {
                counters.skipped += 1;
                continue;
            }
            if file_type.is_dir() {
                if is_excluded(&path, trash) {
                    counters.skipped += 1;
                } else {
                    directories.push(path);
                }
                continue;
            }
            if !file_type.is_file() || is_excluded(&path, trash) {
                counters.skipped += 1;
                continue;
            }
            match index_file(connection, &path, protect) {
                Ok(IndexOutcome::New) => counters.new += 1,
                Ok(IndexOutcome::Unchanged) => counters.unchanged += 1,
                Ok(IndexOutcome::Updated) => counters.updated += 1,
                Err(error) => {
                    eprintln!("warning: {}: {error}", path.display());
                    counters.errors += 1;
                }
            }
            progress(
                counters.new
                    + counters.unchanged
                    + counters.updated
                    + counters.skipped
                    + counters.errors,
            );
        }
    }
    Ok(())
}

fn is_excluded(path: &Path, trash: &Path) -> bool {
    let value = path.to_string_lossy();
    path.starts_with(trash)
        || value.contains("/.git/")
        || value.contains("\\.git\\")
        || value.contains("/node_modules/")
        || value.contains("\\node_modules\\")
        || value.contains("/$RECYCLE.BIN/")
        || value.contains("\\$RECYCLE.BIN\\")
}

enum IndexOutcome {
    New,
    Unchanged,
    Updated,
}

fn index_file(
    connection: &Connection,
    path: &Path,
    protect: &[String],
) -> Result<IndexOutcome, String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let size = i64::try_from(metadata.len()).map_err(|_| "file is too large".to_string())?;
    let modified = unix_seconds(metadata.modified().map_err(|e| e.to_string())?)?;
    let path_text = absolute_path(path)?.to_string_lossy().into_owned();
    let current: Option<(i64, i64)> = connection
        .query_row(
            "SELECT size, modified FROM files WHERE path=?1",
            params![path_text],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if current == Some((size, modified)) {
        connection
            .execute(
                "UPDATE files SET present=1 WHERE path=?1",
                params![path_text],
            )
            .map_err(|e| e.to_string())?;
        save_photo_fingerprint(connection, &path_text, path)?;
        return Ok(IndexOutcome::Unchanged);
    }
    let hash = hash_file(path)?;
    let after = fs::metadata(path).map_err(|e| e.to_string())?;
    if after.len() != metadata.len()
        || unix_seconds(after.modified().map_err(|e| e.to_string())?)? != modified
    {
        return Err("file changed while hashing; retry on next scan".to_string());
    }
    let is_protected = protect.iter().any(|rule| path_text.contains(rule));
    let now = now_seconds()?;
    let outcome = if current.is_some() {
        IndexOutcome::Updated
    } else {
        IndexOutcome::New
    };
    connection.execute(
        "INSERT INTO files(path,size,modified,hash,protected,approved,present,scanned_at) VALUES(?1,?2,?3,?4,?5,0,1,?6)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size,modified=excluded.modified,hash=excluded.hash,protected=excluded.protected,approved=0,present=1,scanned_at=excluded.scanned_at",
        params![path_text, size, modified, hash, is_protected as i64, now],
    ).map_err(|e| e.to_string())?;
    connection
        .execute(
            "DELETE FROM photo_fingerprints WHERE file_id=(SELECT id FROM files WHERE path=?1)",
            params![path_text],
        )
        .map_err(|e| e.to_string())?;
    save_photo_fingerprint(connection, &path_text, path)?;
    save_document_fingerprint(connection, &path_text, path)?;
    Ok(outcome)
}

fn save_photo_fingerprint(
    connection: &Connection,
    path_text: &str,
    path: &Path,
) -> Result<(), String> {
    let file_id: i64 = connection
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![path_text],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let existing: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM photo_fingerprints WHERE file_id=?1",
            params![file_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if existing.is_some() {
        return Ok(());
    }
    let Some(hash) = perceptual_hash(path) else {
        return Ok(());
    };
    let parts = fingerprint_parts(hash);
    connection.execute(
        "INSERT INTO photo_fingerprints(file_id,dhash,part_a,part_b,part_c,part_d,part_e) VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![file_id, hash as i64, parts[0], parts[1], parts[2], parts[3], parts[4]],
    ).map_err(|e| e.to_string())?;
    Ok(())
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

fn save_document_fingerprint(
    connection: &Connection,
    path_text: &str,
    path: &Path,
) -> Result<(), String> {
    let file_id: i64 = connection
        .query_row(
            "SELECT id FROM files WHERE path=?1",
            params![path_text],
            |row| row.get(0),
        )
        .map_err(|e| e.to_string())?;
    let Some((hash, count)) = document_simhash(path) else {
        return Ok(());
    };
    connection.execute("INSERT INTO document_fingerprints(file_id,simhash,token_count) VALUES(?1,?2,?3) ON CONFLICT(file_id) DO UPDATE SET simhash=excluded.simhash,token_count=excluded.token_count", params![file_id, hash as i64, count]).map_err(|e| e.to_string())?;
    Ok(())
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
    let trash_root = PathBuf::from(required_setting(&connection, "trash_path")?);
    let batch = format!("{}-{}", now_seconds()?, file.id);
    let destination = trash_root
        .join(batch)
        .join("files")
        .join(file.id.to_string())
        .join(source.file_name().ok_or("source has no file name")?);
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("create recycle directory: {e}"))?;
    }
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

fn file_by_id(connection: &Connection, id: i64) -> Result<Option<IndexedFile>, String> {
    connection
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
    if version.parse::<i64>().ok() == Some(SCHEMA_VERSION) {
        Ok(())
    } else {
        Err(format!("unsupported schema version: {version}"))
    }
}
fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())
            .map(|cwd| cwd.join(path))
    }
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
