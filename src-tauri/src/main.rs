// Hide the console window in release builds (it would otherwise shadow the
// app with a black terminal); keep it in debug so logs stay visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use notify::{EventKind, Watcher as _};
use image::ImageDecoder as _;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use tauri::Manager;

#[derive(Serialize, Clone)]
struct BulkState {
    running: bool,
    kind: String,
    done: u64,
    total: u64,
    message: String,
}

#[derive(Serialize)]
struct DetectorStatus {
    name: String,
    available: bool,
    detail: String,
}
#[derive(Serialize)]
struct TrashItem {
    id: i64,
    created_at: i64,
    source_path: String,
    trash_path: String,
    expired: bool,
    /// Shared id of one bulk user action; null for single-file removals.
    batch_id: Option<i64>,
}

#[derive(Serialize)]
struct HistoryItem {
    id: i64,
    created_at: i64,
    source_path: String,
    trash_path: String,
    state: String,
    restored_at: Option<i64>,
}

#[derive(Serialize)]
struct ProjectState {
    database: String,
    trash_path: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
    exclude_rules: Vec<String>,
    min_file_size: i64,
    trash_retention_days: i64,
    auto_scan: bool,
    strict_verify: bool,
    usn_scan: bool,
    watch_scan: bool,
    similar_threshold: i64,
}

#[derive(Clone, Serialize)]
struct ScanErrorSample {
    path: String,
    error: String,
}

#[derive(Clone, Serialize)]
struct ScanState {
    state: String,
    processed: u64,
    total: u64,
    message: String,
    current_path: Option<String>,
    /// Most recently started full hash plus cumulative bytes hashed this
    /// scan, so a multi-GB file never looks like a stalled progress bar.
    hashing_path: Option<String>,
    hashing_bytes: u64,
    errors_total: u64,
    recent_errors: Vec<ScanErrorSample>,
}

struct ScanTask {
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<ScanState>>,
    errors: Arc<filelens::ScanErrorLog>,
    hash_progress: Arc<filelens::HashProgress>,
}

/// A long bulk sweep (recycle / hardlink / permanent delete) running on its
/// own thread, so the webview never blocks on thousands of files.
struct BulkTask {
    kind: String,
    progress: Arc<Mutex<BulkProgress>>,
}

/// The real-time directory watcher: one background thread per enable,
/// cancelled through the flag when the setting is turned off.
struct WatchTask {
    cancel: Arc<AtomicBool>,
}

#[derive(Clone, Serialize)]
struct BulkProgress {
    done: u64,
    total: u64,
    finished: bool,
    message: String,
}

fn open_database(path: &str) -> Result<Connection, String> {
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    // WAL lets UI reads proceed while the scan thread writes; busy_timeout
    // absorbs the brief lock contention that remains.
    connection
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;")
        .map_err(|error| error.to_string())?;
    Ok(connection)
}

fn write_project_pointer(app: &tauri::AppHandle, database: &str) -> Result<(), String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    fs::create_dir_all(&data_dir).map_err(|error| format!("create app data directory: {error}"))?;
    let pointer = serde_json::to_string(database).map_err(|error| error.to_string())?;
    fs::write(data_dir.join("project.json"), pointer)
        .map_err(|error| format!("save project pointer: {error}"))
}

fn setting_value(connection: &Connection, key: &str) -> Option<String> {
    connection
        .query_row(
            "SELECT value FROM settings WHERE key=?1",
            params![key],
            |row| row.get(0),
        )
        .ok()
}

fn setting_flag(connection: &Connection, key: &str, default: bool) -> bool {
    setting_value(connection, key)
        .map(|value| value == "1")
        .unwrap_or(default)
}

#[tauri::command]
fn open_project(app: tauri::AppHandle) -> Result<ProjectState, String> {
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("resolve app data directory: {error}"))?;
    fs::create_dir_all(&data_dir).map_err(|error| format!("create app data directory: {error}"))?;
    let pointer = data_dir.join("project.json");
    let database = match fs::read_to_string(&pointer) {
        Ok(saved) => PathBuf::from(
            serde_json::from_str::<String>(&saved).map_err(|error| error.to_string())?,
        ),
        Err(_) => data_dir.join("filelens.db"),
    };
    let connection = open_database(&database.to_string_lossy())?;
    let stored_trash: Option<String> = connection
        .query_row(
            "SELECT value FROM settings WHERE key='trash_path'",
            [],
            |row| row.get(0),
        )
        .ok();
    drop(connection);
    let trash = stored_trash
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("recycle"));
    filelens::init(&database, &trash)?;
    write_project_pointer(&app, &database.to_string_lossy())?;
    let connection = open_database(&database.to_string_lossy())?;
    Ok(ProjectState {
        database: database.to_string_lossy().into_owned(),
        trash_path: trash.to_string_lossy().into_owned(),
        roots: setting_list(&connection, "roots")?,
        protect_rules: setting_list(&connection, "protect_rules")?,
        exclude_rules: setting_list(&connection, "exclude_rules")?,
        min_file_size: setting_value(&connection, "min_file_size")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        trash_retention_days: filelens::trash_retention_days(&connection),
        auto_scan: setting_flag(&connection, "auto_scan_on_start", true),
        strict_verify: setting_flag(&connection, "strict_verify", false),
        usn_scan: setting_flag(&connection, "usn_scan", false),
        watch_scan: setting_flag(&connection, "watch_scan", false),
        similar_threshold: setting_value(&connection, "similar_phash_max")
            .and_then(|value| value.parse().ok())
            .unwrap_or(10),
    })
}

#[tauri::command]
fn save_project_config(
    app: tauri::AppHandle,
    scan_slot: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
    watch_slot: tauri::State<'_, Arc<Mutex<Option<WatchTask>>>>,
    database: String,
    trash: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
    exclude_rules: Vec<String>,
    min_file_size: i64,
) -> Result<String, String> {
    filelens::init(&PathBuf::from(&database), &PathBuf::from(&trash))?;
    let min_size = min_file_size.max(0).to_string();
    {
        let connection = open_database(&database)?;
        save_setting_list(&connection, "roots", &roots)?;
        save_setting_list(&connection, "protect_rules", &protect_rules)?;
        save_setting_list(&connection, "exclude_rules", &exclude_rules)?;
        connection
            .execute(
                "INSERT INTO settings(key,value) VALUES('min_file_size',?1) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![min_size],
            )
            .map_err(|error| error.to_string())?;
    }
    // Keep the real-time watcher in step with the saved roots (a no-op when
    // the feature is off).
    if setting_flag(&open_database(&database)?, "watch_scan", false) {
        start_watcher(&scan_slot, &watch_slot, &database)?;
    }
    write_project_pointer(&app, &database)?;
    Ok("项目设置已保存。".into())
}

#[tauri::command]
fn save_roots(
    scan_slot: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
    watch_slot: tauri::State<'_, Arc<Mutex<Option<WatchTask>>>>,
    database: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
) -> Result<String, String> {
    {
        let connection = open_database(&database)?;
        save_setting_list(&connection, "roots", &roots)?;
        save_setting_list(&connection, "protect_rules", &protect_rules)?;
    }
    // Keep the real-time watcher in step with the saved roots (a no-op when
    // the feature is off).
    if setting_flag(&open_database(&database)?, "watch_scan", false) {
        start_watcher(&scan_slot, &watch_slot, &database)?;
    }
    Ok("扫描目录已保存。".into())
}

#[tauri::command]
fn start_scan(
    task: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
    database: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
    exclude_rules: Vec<String>,
    min_file_size: i64,
) -> Result<String, String> {
    launch_scan(
        &task,
        &database,
        &roots,
        &protect_rules,
        &exclude_rules,
        min_file_size,
    )?;
    Ok("扫描任务已在后台启动。".into())
}

/// True while a registered scan task is still running. A finished scan left
/// in the slot by a watcher-triggered run (which the UI never polls) must
/// not block new scans or the background verification forever.
fn scan_task_running(task: &Mutex<Option<ScanTask>>) -> bool {
    task.lock()
        .ok()
        .and_then(|slot| {
            slot.as_ref()
                .and_then(|current| current.state.lock().ok())
                .map(|state| state.state == "running")
        })
        .unwrap_or(false)
}

/// Spawn the background scan. Shared by the manual command and the
/// real-time watcher (which skips quietly when a scan is already running).
fn launch_scan(
    task: &Mutex<Option<ScanTask>>,
    database: &str,
    roots: &[String],
    protect_rules: &[String],
    exclude_rules: &[String],
    min_file_size: i64,
) -> Result<(), String> {
    let mut task = task.lock().map_err(|_| "scan task lock failed")?;
    if let Some(existing) = task.as_ref() {
        // A completed/cancelled/failed task may linger in the slot when no
        // UI polled its final state; replacing it is safe, a running one is
        // not.
        let running = existing
            .state
            .lock()
            .map(|state| state.state == "running")
            .unwrap_or(true);
        if running {
            return Err("已有扫描任务正在运行".into());
        }
    }
    {
        let connection = open_database(database)?;
        save_setting_list(&connection, "roots", roots)?;
        save_setting_list(&connection, "protect_rules", protect_rules)?;
        save_setting_list(&connection, "exclude_rules", exclude_rules)?;
    }
    let roots = roots.iter().map(PathBuf::from).collect::<Vec<_>>();
    let protect_rules: Vec<String> = protect_rules.to_vec();
    let exclude_rules: Vec<String> = exclude_rules.to_vec();
    let database: String = database.to_string();
    let cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(ScanState {
        state: "running".into(),
        processed: 0,
        total: 0,
        message: "正在准备扫描...".into(),
        current_path: None,
        hashing_path: None,
        hashing_bytes: 0,
        errors_total: 0,
        recent_errors: Vec::new(),
    }));
    let errors = Arc::new(filelens::ScanErrorLog::new());
    let hash_progress = Arc::new(filelens::HashProgress::new());
    let worker_cancelled = cancelled.clone();
    let worker_state = state.clone();
    let worker_errors = errors.clone();
    let worker_hash = hash_progress.clone();
    std::thread::spawn(move || {
        let progress_state = worker_state.clone();
        let result = filelens::scan_with_control(
            &PathBuf::from(database),
            &roots,
            &protect_rules,
            &exclude_rules,
            min_file_size.max(0) as u64,
            true,
            &|| worker_cancelled.load(Ordering::Relaxed),
            &|processed, total, current_path| {
                if let Ok(mut current) = progress_state.lock() {
                    current.processed = processed;
                    current.total = total;
                    current.current_path = current_path.map(str::to_string);
                }
            },
            Some(&worker_errors),
            Some(&worker_hash),
        );
        if let Ok(mut current) = worker_state.lock() {
            match result {
                Ok(summary) => {
                    current.state = "completed".into();
                    let mut message = "扫描完成，索引已更新。".to_string();
                    if summary.fingerprint_rebuild > 0 {
                        message.push_str(&format!(
                            " 本次重建了 {} 张照片的指纹（指纹格式升级后的一次性处理）。",
                            summary.fingerprint_rebuild
                        ));
                    }
                    if summary.pruned > 0 {
                        message.push_str(&format!(
                            " 已清理 {} 个超期回收文件。",
                            summary.pruned
                        ));
                    }
                    if summary.index_pruned > 0 {
                        message.push_str(&format!(
                            " 已清理 {} 条 90 天未更新的缺席索引记录。",
                            summary.index_pruned
                        ));
                    }
                    if !summary.failed_roots.is_empty() {
                        message.push_str(&format!(
                            " 跳过 {} 个无效扫描目录。",
                            summary.failed_roots.len()
                        ));
                    }
                    if summary.errors > 0 {
                        message.push_str(&format!(
                            " {} 个条目处理失败，可在历史进度或日志中查看。",
                            summary.errors
                        ));
                    }
                    current.message = message;
                }
                Err(error) if error == "scan cancelled" => {
                    current.state = "cancelled".into();
                    current.message = "扫描已取消，已完成的索引仍会保留。".into();
                }
                Err(error) => {
                    current.state = "failed".into();
                    current.message = error;
                }
            }
        }
    });
    *task = Some(ScanTask {
        cancelled,
        state,
        errors,
        hash_progress,
    });
    Ok(())
}

#[tauri::command]
fn scan_state(
    task: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
) -> Result<ScanState, String> {
    let mut task = task.lock().map_err(|_| "scan task lock failed")?;
    let Some(current) = task.as_ref() else {
        return Ok(ScanState {
            state: "idle".into(),
            processed: 0,
            total: 0,
            message: "没有正在运行的扫描任务。".into(),
            current_path: None,
            hashing_path: None,
            hashing_bytes: 0,
            errors_total: 0,
            recent_errors: Vec::new(),
        });
    };
    let mut state = current
        .state
        .lock()
        .map_err(|_| "scan state lock failed")?
        .clone();
    let (hashing_path, hashing_bytes) = current.hash_progress.snapshot();
    state.hashing_path = hashing_path;
    state.hashing_bytes = hashing_bytes;
    let (errors_total, recent) = current.errors.snapshot();
    state.errors_total = errors_total;
    state.recent_errors = recent
        .into_iter()
        .map(|(path, error)| ScanErrorSample { path, error })
        .collect();
    if state.state != "running" {
        *task = None;
    }
    Ok(state)
}

#[tauri::command]
fn cancel_scan(task: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>) -> Result<String, String> {
    let task = task.lock().map_err(|_| "scan task lock failed")?;
    let Some(current) = task.as_ref() else {
        return Err("没有正在运行的扫描任务".into());
    };
    current.cancelled.store(true, Ordering::Relaxed);
    Ok("正在请求取消，当前文件处理完成后会停止。".into())
}

fn setting_list(connection: &Connection, key: &str) -> Result<Vec<String>, String> {
    let value: Option<String> = connection
        .query_row("SELECT value FROM settings WHERE key=?1", [key], |row| {
            row.get(0)
        })
        .ok();
    value
        .map(|value| serde_json::from_str(&value).map_err(|error| error.to_string()))
        .transpose()
        .map(|value| value.unwrap_or_default())
}

fn save_setting_list(connection: &Connection, key: &str, values: &[String]) -> Result<(), String> {
    let value = serde_json::to_string(values).map_err(|error| error.to_string())?;
    connection
        .execute("INSERT INTO settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, value])
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[tauri::command]
fn approve(database: String, file_id: i64) -> Result<String, String> {
    filelens::set_approval(&PathBuf::from(database), file_id, true).map(|_| "副本已确认。".into())
}

#[tauri::command]
fn unapprove(database: String, file_id: i64) -> Result<String, String> {
    filelens::set_approval(&PathBuf::from(database), file_id, false).map(|_| "已取消确认。".into())
}

#[tauri::command]
fn trash(database: String, file_id: i64) -> Result<String, String> {
    filelens::trash(&PathBuf::from(database), file_id).map(|_| "文件已移入应用回收站。".into())
}

#[tauri::command]
fn trash_approved(database: String, file_ids: Vec<i64>) -> Result<String, String> {
    let outcome = filelens::trash_batch(&PathBuf::from(&database), &file_ids);
    Ok(format!(
        "已移动 {} 个，{} 个失败：{}",
        outcome.succeeded,
        outcome.failures.len(),
        outcome.failures.join("；")
    ))
}

#[tauri::command]
fn delete_direct(database: String, file_id: i64) -> Result<String, String> {
    filelens::delete_direct(&PathBuf::from(database), file_id)
        .map(|_| "文件已永久删除。".into())
}

#[tauri::command]
fn delete_direct_batch(database: String, file_ids: Vec<i64>) -> Result<String, String> {
    let outcome = filelens::delete_approved_batch(&PathBuf::from(&database), &file_ids);
    Ok(format!(
        "已删除 {} 个，{} 个失败：{}",
        outcome.succeeded,
        outcome.failures.len(),
        outcome.failures.join("；")
    ))
}

/// Recycle user-selected similar candidates (not exact duplicates) after the
/// core-library safety chain verifies each path against the index.
#[tauri::command]
fn trash_paths(database: String, paths: Vec<String>) -> Result<String, String> {
    let outcome = filelens::trash_paths(&PathBuf::from(&database), &paths)?;
    Ok(format_batch_outcome("已移入回收站", outcome))
}

/// Permanently delete user-selected similar candidates behind the same
/// safety chain (index lookup, protection check, hash re-verification).
#[tauri::command]
fn delete_paths(database: String, paths: Vec<String>) -> Result<String, String> {
    let outcome = filelens::delete_paths(&PathBuf::from(&database), &paths)?;
    Ok(format_batch_outcome("已永久删除", outcome))
}

fn format_batch_outcome(verb: &str, outcome: filelens::BatchOutcome) -> String {
    if outcome.failures.is_empty() {
        format!("{verb} {} 个文件。", outcome.succeeded)
    } else {
        format!(
            "{verb} {} 个，{} 个失败：{}",
            outcome.succeeded,
            outcome.failures.len(),
            outcome.failures.join("；")
        )
    }
}

#[tauri::command]
fn trash_delete(database: String, operation_id: i64) -> Result<String, String> {
    filelens::delete_trash(&PathBuf::from(database), operation_id)
        .map(|_| "文件已永久删除。".into())
}

#[tauri::command]
fn trash_empty(database: String) -> Result<String, String> {
    filelens::empty_trash(&PathBuf::from(database))
        .map(|count| format!("已清空回收站，永久删除 {count} 个文件。"))
}

// Thumbnails and full previews share one cache directory; 500 MB is generous
// for 320 px tiles while keeping long-term disk growth bounded.
const THUMBNAIL_CACHE_MAX_BYTES: u64 = 500 * 1024 * 1024;

fn thumbnail_cache_key(path: &Path) -> String {
    let metadata = fs::metadata(path).ok();
    let modified = metadata
        .as_ref()
        .and_then(|m| m.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let size = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
    blake3::hash(format!("{}|{modified}|{size}", path.display()).as_bytes())
        .to_hex()[..20]
        .to_string()
}

/// Caps concurrent image decodes: `DynamicImage::from_decoder` holds the
/// full decoded raster in memory (a 60 MP photo is ~240 MB of RGBA), so a
/// photo grid firing dozens of tile requests must not decode all of them
/// at once. Disk cache hits return before acquiring a slot.
const MAX_CONCURRENT_DECODES: usize = 3;
static DECODE_SLOTS: std::sync::Mutex<usize> = std::sync::Mutex::new(MAX_CONCURRENT_DECODES);
static DECODE_SLOTS_COND: std::sync::Condvar = std::sync::Condvar::new();

/// RAII permit: releasing happens automatically on scope exit, covering
/// every early return in `render_image`.
struct DecodePermit(std::sync::MutexGuard<'static, usize>);
impl Drop for DecodePermit {
    fn drop(&mut self) {
        *self.0 += 1;
        DECODE_SLOTS_COND.notify_one();
    }
}

fn acquire_decode_permit() -> DecodePermit {
    let mut slots = DECODE_SLOTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    while *slots == 0 {
        slots = DECODE_SLOTS_COND
            .wait(slots)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
    }
    *slots -= 1;
    DecodePermit(slots)
}

/// Decode an image, apply EXIF orientation, downscale and return base64 PNG.
/// Results are cached on disk keyed by path/mtime/size; a full or read-only
/// cache disk must not break previews, so cache errors are ignored.
fn render_image(
    app: &tauri::AppHandle,
    source: &Path,
    cache_prefix: &str,
    max_dimension: u32,
) -> Result<Option<String>, String> {
    let cache_dir = app
        .path()
        .app_cache_dir()
        .map_err(|error| error.to_string())?
        .join("thumbnails");
    let cached = cache_dir.join(format!(
        "{cache_prefix}-{}.png",
        thumbnail_cache_key(source)
    ));
    if let Ok(bytes) = fs::read(&cached) {
        return Ok(Some(format!(
            "data:image/png;base64,{}",
            STANDARD.encode(bytes)
        )));
    }
    // Only actual decodes take a slot; cache hits above return without one.
    let _decode_permit = acquire_decode_permit();
    let reader = match image::ImageReader::open(source) {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let mut decoder = match reader.into_decoder() {
        Ok(decoder) => decoder,
        Err(_) => return Ok(None),
    };
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut image = match image::DynamicImage::from_decoder(decoder) {
        Ok(image) => image,
        Err(_) => return Ok(None),
    };
    image.apply_orientation(orientation);
    let thumbnail = image.thumbnail(max_dimension, max_dimension);
    let mut output = std::io::Cursor::new(Vec::new());
    let bytes = match thumbnail.write_to(&mut output, image::ImageFormat::Png) {
        Ok(()) => output.into_inner(),
        Err(_) => return Ok(None),
    };
    let _ = fs::create_dir_all(&cache_dir);
    let temporary = cached.with_extension("tmp");
    if fs::write(&temporary, &bytes).is_ok() {
        let _ = fs::rename(&temporary, &cached);
        // Best-effort bound on cache growth; eviction failure must not
        // break the preview that was just rendered.
        let _ = filelens::evict_lru_files(&cache_dir, THUMBNAIL_CACHE_MAX_BYTES);
    }
    Ok(Some(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(bytes)
    )))
}

#[tauri::command]
async fn image_thumbnail(
    app: tauri::AppHandle,
    path: String,
) -> Result<Option<String>, String> {
    let source = PathBuf::from(&path);
    // 560 px so the larger similar-photo tiles stay crisp on scaled displays.
    tauri::async_runtime::spawn_blocking(move || render_image(&app, &source, "th", 560))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn image_preview(app: tauri::AppHandle, path: String) -> Result<Option<String>, String> {
    let source = PathBuf::from(&path);
    tauri::async_runtime::spawn_blocking(move || render_image(&app, &source, "l", 1400))
        .await
        .map_err(|error| error.to_string())?
}

#[tauri::command]
fn open_file(path: String) -> Result<String, String> {
    let path = PathBuf::from(&path);
    if !path.is_file() {
        return Err("文件不存在或已删除".into());
    }
    // explorer.exe (not `cmd /C start`) so cmd metacharacters in the path are
    // never interpreted by a shell.
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("explorer").arg(&path).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open").arg(&path).spawn();
    result
        .map(|_| "已调用系统默认程序打开文件。".into())
        .map_err(|error| format!("打开文件失败：{error}"))
}

#[tauri::command]
fn restore(database: String, operation_id: i64) -> Result<String, String> {
    filelens::restore(&PathBuf::from(database), operation_id).map(|_| "文件已恢复至原位置。".into())
}

#[tauri::command]
async fn restore_batch(database: String, batch_id: i64) -> Result<String, String> {
    // Per-file hashing + moving can take a while on large batches.
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = filelens::restore_batch(&PathBuf::from(&database), batch_id);
        if outcome.failures.is_empty() {
            Ok(format!("已恢复整批 {} 个文件至原位置。", outcome.succeeded))
        } else {
            Ok(format!(
                "已恢复 {} 个，{} 个失败：{}",
                outcome.succeeded,
                outcome.failures.len(),
                outcome.failures.join("；")
            ))
        }
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Settings-page preview: how many indexed files the unsaved rule set would
/// shield, with sample paths.
#[tauri::command]
async fn protect_preview(
    database: String,
    protect_rules: Vec<String>,
) -> Result<filelens::ProtectPreview, String> {
    // Streams every indexed path and re-hashes nothing, but still touches
    // the whole table; keep it off the main thread.
    tauri::async_runtime::spawn_blocking(move || {
        filelens::protect_preview(&PathBuf::from(&database), &protect_rules)
    })
    .await
    .map_err(|error| error.to_string())?
}

/// Apply one keep-strategy to every duplicate group matching the review
/// filters, not just the loaded page.
#[tauri::command]
fn approve_filtered(
    database: String,
    strategy: String,
    min_size: i64,
    path_contains: String,
    kind: Option<String>,
    dir_contains: Option<String>,
) -> Result<String, String> {
    let strategy = match strategy.as_str() {
        "scored" => filelens::MarkStrategy::Scored,
        "newest" => filelens::MarkStrategy::Newest,
        "oldest" => filelens::MarkStrategy::Oldest,
        "shortest" => filelens::MarkStrategy::Shortest,
        other => return Err(format!("未知的保留策略：{other}")),
    };
    let outcome = filelens::approve_groups_except_keeper(
        &PathBuf::from(&database),
        strategy,
        min_size.max(0),
        &path_contains,
        kind.as_deref().filter(|value| *value != "all"),
        dir_contains.as_deref().unwrap_or(""),
    )?;
    if outcome.failures.is_empty() {
        Ok(format!("已标记 {} 个副本待处理。", outcome.succeeded))
    } else {
        Ok(format!(
            "已标记 {} 个副本，{} 个失败：{}",
            outcome.succeeded,
            outcome.failures.len(),
            outcome.failures.join("；")
        ))
    }
}

#[tauri::command]
async fn export_report(database: String, path: String) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let rows = filelens::export_report(&PathBuf::from(&database), &PathBuf::from(&path))?;
        Ok(format!("已导出 {rows} 条重复记录到 {}", path))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn reveal_in_manager(path: String) -> Result<String, String> {
    let target = PathBuf::from(&path);
    if !target.exists() {
        return Err("文件不存在或已删除".into());
    }
    #[cfg(target_os = "windows")]
    let result = {
        // explorer /select, expects backslash separators.
        let windows_path = path.replace('/', "\\");
        std::process::Command::new("explorer")
            .arg(format!("/select,{windows_path}"))
            .spawn()
    };
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").args(["-R", &path]).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let result = std::process::Command::new("xdg-open")
        .arg(target.parent().unwrap_or(&target))
        .spawn();
    result
        .map(|_| "已在文件管理器中定位。".into())
        .map_err(|error| format!("打开文件管理器失败：{error}"))
}

#[derive(Serialize)]
struct ThumbnailCacheStats {
    files: u64,
    bytes: u64,
}

fn thumbnail_cache_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, String> {
    let cache_root = app
        .path()
        .app_cache_dir()
        .map_err(|error| format!("resolve app cache directory: {error}"))?;
    Ok(cache_root.join("thumbnails"))
}

// Pure reporting/cleanup over the cache directory; no business logic, so it
// lives in the command layer like the thumbnail writer itself.
#[tauri::command]
fn thumbnail_cache_stats(app: tauri::AppHandle) -> Result<ThumbnailCacheStats, String> {
    let directory = thumbnail_cache_dir(&app)?;
    let mut files = 0_u64;
    let mut bytes = 0_u64;
    if let Ok(entries) = fs::read_dir(&directory) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata()
                && metadata.is_file() {
                    files += 1;
                    bytes += metadata.len();
                }
        }
    }
    Ok(ThumbnailCacheStats { files, bytes })
}

#[tauri::command]
fn thumbnail_cache_clear(app: tauri::AppHandle) -> Result<String, String> {
    let directory = thumbnail_cache_dir(&app)?;
    if directory.exists() {
        fs::remove_dir_all(&directory).map_err(|error| format!("清理缓存失败：{error}"))?;
    }
    fs::create_dir_all(&directory).map_err(|error| format!("重建缓存目录失败：{error}"))?;
    Ok("缩略图与预览缓存已清理，重新浏览图片时会自动重建。".into())
}

#[tauri::command]
async fn status(database: String) -> Result<filelens::StatusSnapshot, String> {
    // Full fingerprint pairing on every refresh(): keep it off the main
    // thread or the UI stutters while the overview recomputes.
    tauri::async_runtime::spawn_blocking(move || {
        filelens::project_status(&PathBuf::from(&database))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn groups(
    database: String,
    offset: Option<i64>,
    limit: Option<i64>,
    min_size: Option<i64>,
    path_contains: Option<String>,
    sort: Option<String>,
    kind: Option<String>,
    dir_contains: Option<String>,
) -> Result<filelens::GroupsPage, String> {
    // `GroupQuery` borrows the filter strings, so build it inside the
    // blocking closure from the moved, owned parameters.
    tauri::async_runtime::spawn_blocking(move || {
        let query = filelens::GroupQuery {
            min_size: min_size.unwrap_or(0),
            path_contains: path_contains
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty()),
            sort: match sort.as_deref() {
                Some("members") => filelens::GroupSort::Members,
                Some("path") => filelens::GroupSort::Path,
                Some("recoverable") => filelens::GroupSort::Recoverable,
                _ => filelens::GroupSort::Size,
            },
            offset: offset.unwrap_or(0),
            limit: limit.unwrap_or(50),
            kind: kind.as_deref().filter(|value| *value != "all"),
            dir_contains: dir_contains
                .as_deref()
                .filter(|value| !value.is_empty() && *value != "all"),
        };
        filelens::query_groups(&PathBuf::from(&database), &query)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
async fn group_dirs(database: String) -> Result<Vec<filelens::DirCount>, String> {
    tauri::async_runtime::spawn_blocking(move || filelens::group_dirs(&PathBuf::from(&database)))
        .await
        .map_err(|error| error.to_string())?
}

/// kind = "duplicate": both fingerprints exactly equal (dHash 0 + pHash 0) —
/// a near-certain pixel copy, still subject to the background per-pair pixel
/// verification (start_duplicate_verify) before the UI shows it. kind =
/// "similar": near-identical pixels (dHash distance 1-4) with the stored
/// pHash gate. Both exclude byte-identical exact duplicates.
#[tauri::command]
async fn similar_photos(
    database: String,
    kind: Option<String>,
) -> Result<filelens::SimilarPhotosPage, String> {
    tauri::async_runtime::spawn_blocking(move || {
        filelens::similar_photo_pairs(&PathBuf::from(&database), kind.as_deref())
    })
    .await
    .map_err(|error| error.to_string())?
}
/// Definitive pixel comparison for one candidate pair. The duplicate-photos
/// view is fingerprint-based, so it can contain near-identical burst shots;
/// the compare modal runs this before the user deletes either side.
#[tauri::command]
async fn verify_photo_pair(first: String, second: String) -> Result<bool, String> {
    // Decodes both images at full resolution — easily hundreds of MB of
    // RGBA for large photos; never decode on the main thread.
    tauri::async_runtime::spawn_blocking(move || {
        filelens::photos_pixel_identical(&first, &second)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[derive(Clone, Serialize)]
struct VerifyPairVerdict {
    first: String,
    second: String,
    identical: bool,
}

#[derive(Clone, Serialize, Default)]
struct DuplicateVerifyState {
    running: bool,
    done: usize,
    total: usize,
    same: usize,
    different: usize,
    results: Vec<VerifyPairVerdict>,
}

struct DuplicateVerifyTask {
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<DuplicateVerifyState>>,
}

/// Verify every duplicate-view candidate pixel by pixel on background
/// threads. Idempotent: while a run is active, starting again just reports
/// its current state. The verdicts stream into `results` as they are
/// computed, so the UI can filter progressively.
#[tauri::command]
async fn start_duplicate_verify(
    state: tauri::State<'_, Mutex<Option<DuplicateVerifyTask>>>,
    scan_slot: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
    database: String,
) -> Result<DuplicateVerifyState, String> {
    {
        let slot = state
            .lock()
            .map_err(|_| "duplicate verify lock failed")?;
        if let Some(task) = slot.as_ref() {
            if !task.cancel.load(Ordering::Relaxed) {
                return Ok(task
                    .state
                    .lock()
                    .map_err(|_| "duplicate verify lock failed")?
                    .clone());
            }
        }
    }
    // The pre-work loads the whole photo fingerprint table and runs one
    // query per pair against the verdict cache — keep it off the main
    // thread and never hold the task slot across the await.
    let prepared = {
        let database = database.clone();
        tauri::async_runtime::spawn_blocking(
            move || -> Result<(DuplicateVerifyState, Vec<(String, String)>), String> {
                let pairs = filelens::duplicate_photo_candidates(Path::new(&database))?;
                // Serve cached verdicts instantly; only pairs whose either file
                // changed since the last check go through the decoder again.
                // Fresh verdicts are written back so the next pass is a hit.
                let cached = filelens::cached_pair_verdicts(Path::new(&database), &pairs)?;
                let mut seeded = Vec::new();
                let mut pending = Vec::new();
                for ((first, second), verdict) in pairs.iter().zip(&cached) {
                    match verdict {
                        Some(identical) => seeded.push(VerifyPairVerdict {
                            first: first.clone(),
                            second: second.clone(),
                            identical: *identical,
                        }),
                        None => pending.push((first.clone(), second.clone())),
                    }
                }
                let mut initial = DuplicateVerifyState {
                    running: true,
                    total: pairs.len(),
                    ..Default::default()
                };
                for verdict in &seeded {
                    initial.done += 1;
                    if verdict.identical {
                        initial.same += 1;
                    } else {
                        initial.different += 1;
                    }
                }
                initial.results = seeded;
                Ok((initial, pending))
            },
        )
        .await
        .map_err(|error| error.to_string())??
    };
    let (initial, pending) = prepared;
    let task_state = Arc::new(Mutex::new(initial));
    let cancel = Arc::new(AtomicBool::new(false));
    let mut slot = state
        .lock()
        .map_err(|_| "duplicate verify lock failed")?;
    // Another start may have slipped in while the pre-work ran.
    if let Some(task) = slot.as_ref() {
        if !task.cancel.load(Ordering::Relaxed) {
            return Ok(task
                .state
                .lock()
                .map_err(|_| "duplicate verify lock failed")?
                .clone());
        }
    }
    *slot = Some(DuplicateVerifyTask {
        cancel: cancel.clone(),
        state: task_state.clone(),
    });
    drop(slot);
    let task_state_for_thread = task_state.clone();
    let scan_slot_for_thread = scan_slot.inner().clone();
    std::thread::spawn(move || {
        let database_for_store = database.clone();
        // Workers publish from several threads; buffer verdicts and flush to
        // the cache in batches instead of paying a connection per pair.
        let buffer: std::sync::Mutex<Vec<(String, String, bool)>> =
            std::sync::Mutex::new(Vec::new());
        let flush = |buffer: &std::sync::Mutex<Vec<(String, String, bool)>>| {
            if let Ok(mut buffered) = buffer.lock() {
                if buffered.is_empty() {
                    return;
                }
                let batch: Vec<_> = buffered.drain(..).collect();
                let _ = filelens::store_pair_verdicts(Path::new(&database_for_store), &batch);
            }
        };
        // Pairs whose verdict has been published drop out here, so an
        // interrupted pass can resume with exactly what is left.
        let remaining: std::sync::Mutex<std::collections::HashSet<(String, String)>> =
            std::sync::Mutex::new(pending.iter().cloned().collect());
        let publish = |verdict: &filelens::PairVerdict| {
            if let Ok(mut left) = remaining.lock() {
                left.remove(&(verdict.first.clone(), verdict.second.clone()));
            }
            if let Ok(mut buffered) = buffer.lock() {
                buffered.push((
                    verdict.first.clone(),
                    verdict.second.clone(),
                    verdict.identical,
                ));
                if buffered.len() >= 64 {
                    let batch: Vec<_> = buffered.drain(..).collect();
                    let _ =
                        filelens::store_pair_verdicts(Path::new(&database_for_store), &batch);
                }
            }
            if let Ok(mut guard) = task_state_for_thread.lock() {
                guard.done += 1;
                if verdict.identical {
                    guard.same += 1;
                } else {
                    guard.different += 1;
                }
                guard.results.push(VerifyPairVerdict {
                    first: verdict.first.clone(),
                    second: verdict.second.clone(),
                    identical: verdict.identical,
                });
            }
        };
        let should_stop = || {
            cancel.load(Ordering::Relaxed) || scan_task_running(&scan_slot_for_thread)
        };
        while !cancel.load(Ordering::Relaxed) {
            // A running scan keeps every core busy with hashing: yield until
            // it finishes, then continue with the remaining pairs.
            if scan_task_running(&scan_slot_for_thread) {
                std::thread::sleep(std::time::Duration::from_millis(1500));
                continue;
            }
            // Snapshot instead of drain: pairs stay in `remaining` until
            // their verdict is published, so an interrupted pass (a scan
            // starting mid-batch) resumes with exactly what is left.
            let slice: Vec<(String, String)> = match remaining.lock() {
                Ok(left) => left.iter().cloned().collect(),
                Err(_) => break,
            };
            if slice.is_empty() {
                break;
            }
            let _ = filelens::verify_photo_pairs(&slice, &publish, &should_stop);
        }
        flush(&buffer);
        if let Ok(mut guard) = task_state_for_thread.lock() {
            guard.running = false;
        }
    });
    task_state
        .lock()
        .map_err(|_| "duplicate verify lock failed".to_string())
        .map(|guard| guard.clone())
}

#[tauri::command]
fn duplicate_verify_state(
    state: tauri::State<'_, Mutex<Option<DuplicateVerifyTask>>>,
) -> Result<DuplicateVerifyState, String> {
    let slot = state
        .lock()
        .map_err(|_| "duplicate verify lock failed")?;
    Ok(slot
        .as_ref()
        .and_then(|task| task.state.lock().ok().map(|guard| guard.clone()))
        .unwrap_or_default())
}

#[tauri::command]
fn cancel_duplicate_verify(
    state: tauri::State<'_, Mutex<Option<DuplicateVerifyTask>>>,
) -> Result<(), String> {
    let slot = state
        .lock()
        .map_err(|_| "duplicate verify lock failed")?;
    if let Some(task) = slot.as_ref() {
        task.cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

#[tauri::command]
async fn similar_documents(database: String) -> Result<Vec<filelens::SimilarDocument>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        filelens::similar_document_pairs(&PathBuf::from(&database))
    })
    .await
    .map_err(|error| error.to_string())?
}

#[tauri::command]
fn detector_status() -> Vec<DetectorStatus> {
    vec![
        DetectorStatus {
            name: "照片重复/相似".into(),
            available: true,
            detail: "本地 dHash：像素一致的重复（复制导致信息略异）与高置信度相似候选".into(),
        },
        DetectorStatus {
            name: "文档近似".into(),
            available: true,
            detail: "TXT、Markdown、CSV、JSON、XML、HTML 的本地 SimHash".into(),
        },
        DetectorStatus {
            name: "音频转码相似".into(),
            available: false,
            detail: "需要随安装包提供 Chromaprint/FFmpeg 解码器；当前不会产生不可靠结果".into(),
        },
        DetectorStatus {
            name: "视频转码与片段包含".into(),
            available: false,
            detail: "需要随安装包提供 FFmpeg 解码器与帧指纹任务；当前不会产生不可靠结果".into(),
        },
    ]
}

#[tauri::command]
async fn trash_list(database: String) -> Result<Vec<TrashItem>, String> {
    tauri::async_runtime::spawn_blocking(move || trash_list_blocking(&database))
        .await
        .map_err(|error| error.to_string())?
}

fn trash_list_blocking(database: &str) -> Result<Vec<TrashItem>, String> {
    let connection = open_database(database)?;
    let retention = filelens::trash_retention_days(&connection);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0);
    let mut statement = connection.prepare("SELECT id,created_at,source_path,trash_path,batch_id FROM operations WHERE state='trashed' ORDER BY created_at DESC").map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            let created_at: i64 = row.get(1)?;
            Ok(TrashItem {
                id: row.get(0)?,
                created_at,
                source_path: row.get(2)?,
                trash_path: row.get(3)?,
                expired: retention > 0 && now.saturating_sub(created_at) > retention * 86_400,
                batch_id: row.get(4)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn history(
    database: String,
    offset: Option<i64>,
    limit: Option<i64>,
    state_filter: Option<String>,
) -> Result<Vec<HistoryItem>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    tauri::async_runtime::spawn_blocking(move || history_blocking(&database, offset, limit, state_filter))
        .await
        .map_err(|error| error.to_string())?
}

fn history_blocking(
    database: &str,
    offset: i64,
    limit: i64,
    state_filter: Option<String>,
) -> Result<Vec<HistoryItem>, String> {
    let connection = open_database(database)?;
    // Optional state filter (trashed/restored/deleted/hardlinked); an
    // unrecognized value is ignored rather than erroring.
    let state_sql = state_filter
        .as_deref()
        .filter(|state| {
            matches!(*state, "trashed" | "restored" | "deleted" | "hardlinked")
        })
        .map(|state| format!("WHERE state='{state}'"))
        .unwrap_or_default();
    let mut statement = connection
        .prepare(&format!(
            "SELECT id,created_at,source_path,trash_path,state,restored_at \
             FROM operations {state_sql} ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2"
        ))
        .map_err(|error| error.to_string())?;
    statement
        .query_map(params![limit, offset], |row| {
            Ok(HistoryItem {
                id: row.get(0)?,
                created_at: row.get(1)?,
                source_path: row.get(2)?,
                trash_path: row.get(3)?,
                state: row.get(4)?,
                restored_at: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn restore_to(
    database: String,
    operation_id: i64,
    target_dir: String,
) -> Result<String, String> {
    filelens::restore_to(&PathBuf::from(database), operation_id, Path::new(&target_dir))
        .map(|_| "文件已恢复到指定目录。".into())
}

#[tauri::command]
fn trash_prune_expired(database: String) -> Result<String, String> {
    filelens::prune_expired_trash(&PathBuf::from(database))
        .map(|count| format!("已清理 {count} 个过期回收文件。"))
}

#[tauri::command]
fn set_trash_retention(database: String, days: i64) -> Result<String, String> {
    if !(0..=3650).contains(&days) {
        return Err("保留天数需在 0 到 3650 之间（0 表示不自动清理）".into());
    }
    let connection = open_database(&database)?;
    connection
        .execute(
            "INSERT INTO settings(key,value) VALUES('trash_retention_days',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![days.to_string()],
        )
        .map_err(|error| error.to_string())?;
    if days == 0 {
        Ok("已关闭回收站自动清理。".into())
    } else {
        Ok(format!("回收站文件将保留 {days} 天，超期后在下一次扫描时自动清理。"))
    }
}

#[tauri::command]
fn set_auto_scan(database: String, enabled: bool) -> Result<String, String> {
    let connection = open_database(&database)?;
    connection
        .execute(
            "INSERT INTO settings(key,value) VALUES('auto_scan_on_start',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![if enabled { "1" } else { "0" }],
        )
        .map_err(|error| error.to_string())?;
    Ok(if enabled {
        "已开启：启动时将自动进行增量扫描。".into()
    } else {
        "已关闭启动自动扫描。".into()
    })
}

/// Replace approved duplicates with hard links to their kept copies: every
/// path survives, the duplicated content is freed immediately. Cross-volume
/// or otherwise unlinkable copies are skipped and reported per file.
#[tauri::command]
fn hardlink_approved(database: String, file_ids: Vec<i64>) -> Result<String, String> {
    let outcome = filelens::hardlink_batch(&PathBuf::from(&database), &file_ids);
    Ok(format!(
        "已转为硬链接 {} 个，{} 个失败：{}",
        outcome.succeeded,
        outcome.failures.len(),
        outcome.failures.join("；")
    ))
}

#[tauri::command]
fn set_strict_verify(database: String, enabled: bool) -> Result<String, String> {
    filelens::set_strict_verify(&PathBuf::from(&database), enabled)?;
    Ok(if enabled {
        "已开启严格模式：删除前将逐字节复核内容。".into()
    } else {
        "已关闭严格模式。".into()
    })
}

/// Real-time watch mode: persist the preference and (re)start a background
/// directory watcher per enable. Changes settle for three seconds, then a
/// debounced incremental scan runs unless one is already in progress.
#[tauri::command]
fn set_watch_scan(
    scan_slot: tauri::State<'_, Arc<Mutex<Option<ScanTask>>>>,
    watch_slot: tauri::State<'_, Arc<Mutex<Option<WatchTask>>>>,
    database: String,
    enabled: bool,
) -> Result<(), String> {
    filelens::set_watch_scan(Path::new(&database), enabled)?;
    if enabled {
        start_watcher(&scan_slot, &watch_slot, &database)?;
    } else if let Some(task) = watch_slot
        .lock()
        .map_err(|_| "watch lock failed")?
        .as_ref()
    {
        task.cancel.store(true, Ordering::Relaxed);
    }
    Ok(())
}

/// (Re)start the directory watcher with the currently saved roots and
/// rules. Called on enable and whenever the roots change, so a newly added
/// scan root is picked up without touching the toggle.
fn start_watcher(
    scan_slot: &Arc<Mutex<Option<ScanTask>>>,
    watch_slot: &Arc<Mutex<Option<WatchTask>>>,
    database: &str,
) -> Result<(), String> {
    // Hold the slot lock across the whole restart: two overlapping calls
    // (settings save racing the toggle) must not interleave, or the first
    // task's cancel flag would be set after the second task replaced it,
    // orphaning a watcher thread that keeps launching scans forever.
    let mut slot = watch_slot.lock().map_err(|_| "watch lock failed")?;
    // A previous watcher is cancelled before anything else: a re-enable
    // restarts it with the current roots and rules.
    if let Some(task) = slot.as_ref() {
        task.cancel.store(true, Ordering::Relaxed);
    }
    let (roots, protect_rules, exclude_rules, min_file_size) = {
        let connection = open_database(database)?;
        (
            setting_list(&connection, "roots")?,
            setting_list(&connection, "protect_rules")?,
            setting_list(&connection, "exclude_rules")?,
            setting_value(&connection, "min_file_size")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        )
    };
    if roots.is_empty() {
        *slot = None;
        return Ok(());
    }
    let database: String = database.to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    // Register the task before spawning: the thread clears the slot itself
    // whenever it exits (early failure or shutdown), so the slot never
    // claims a live watcher that is actually dead.
    *slot = Some(WatchTask { cancel: cancel.clone() });
    let slot_for_thread = Arc::clone(watch_slot);
    let watcher_cancel = cancel.clone();
    let scan_slot = Arc::clone(scan_slot);
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(watcher) => watcher,
            Err(_) => {
                clear_watch_task(&slot_for_thread, &watcher_cancel);
                return;
            }
        };
        let mut watched_any = false;
        for root in &roots {
            // A root that does not exist (yet) is skipped rather than fatal:
            // the remaining roots stay monitored, and re-toggling the
            // setting picks the missing one up later.
            if Path::new(root).is_dir()
                && watcher
                    .watch(Path::new(root), notify::RecursiveMode::Recursive)
                    .is_ok()
            {
                watched_any = true;
            }
        }
        if !watched_any {
            clear_watch_task(&slot_for_thread, &watcher_cancel);
            return;
        }
        let relevant = |kind: &EventKind| !matches!(kind, EventKind::Access(_));
        let mut dirty = false;
        loop {
            if watcher_cancel.load(Ordering::Relaxed) {
                break;
            }
            match rx.recv_timeout(std::time::Duration::from_millis(2000)) {
                Ok(Ok(event)) => {
                    if relevant(&event.kind) {
                        dirty = true;
                    }
                }
                Ok(Err(_)) => dirty = true,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if !dirty {
                        continue;
                    }
                    // Settle window: keep collecting until the tree stays
                    // quiet for three seconds, so bulk copies produce one
                    // scan instead of hundreds.
                    loop {
                        match rx.recv_timeout(std::time::Duration::from_millis(3000)) {
                            Ok(_) => {}
                            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                        }
                        if watcher_cancel.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    dirty = false;
                    if watcher_cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    let _ = launch_scan(
                        &scan_slot,
                        &database,
                        &roots,
                        &protect_rules,
                        &exclude_rules,
                        min_file_size,
                    );
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        clear_watch_task(&slot_for_thread, &watcher_cancel);
    });
    Ok(())
}

/// Remove the watch task from the slot only when it still holds *this*
/// task — a restart may already have replaced it with a newer watcher.
fn clear_watch_task(slot: &Mutex<Option<WatchTask>>, cancel: &Arc<AtomicBool>) {
    if let Ok(mut guard) = slot.lock() {
        let still_current = guard
            .as_ref()
            .is_some_and(|task| Arc::ptr_eq(&task.cancel, cancel));
        if still_current {
            *guard = None;
        }
    }
}

/// Similar-photo decision threshold (pHash distance cap for the "similar"
/// view); returns the stored clamped value.
#[tauri::command]
fn set_similar_threshold(database: String, max_distance: i64) -> Result<i64, String> {
    filelens::set_similar_threshold(Path::new(&database), max_distance)
}

#[tauri::command]
fn set_usn_scan(database: String, enabled: bool) -> Result<String, String> {
    filelens::set_usn_scan(&PathBuf::from(&database), enabled)?;
    Ok(if enabled {
        "已开启 USN 快速扫描（需要管理员权限运行，否则自动回退普通扫描）。".into()
    } else {
        "已关闭 USN 快速扫描。".into()
    })
}

/// Recycle the candidate photos of one directory; pairs whose both sides
/// live in that directory keep one copy alive by first moving it into the
/// user-chosen keep directory.
#[tauri::command]
fn recycle_dir_keep_one(
    database: String,
    dir: String,
    keep_dir: String,
) -> Result<String, String> {
    let outcome = filelens::recycle_dir_keep_one(&PathBuf::from(&database), &dir, Path::new(&keep_dir))?;
    let mut message = format!(
        "已移入回收站 {} 个，{} 张移入保留目录。",
        outcome.recycled, outcome.moved
    );
    if !outcome.failures.is_empty() {
        message.push_str(&format!(
            " 失败 {} 个：{}",
            outcome.failures.len(),
            outcome.failures.join("；")
        ));
    }
    Ok(message)
}

/// Directory merge from the duplicate-directory view: for every pair the
/// frontend has decided a winner via the user's retention rules — the loser
/// is recycled, the winner ends up in the target folder. Emptied source
/// directories are cleaned up best-effort.
#[tauri::command]
fn merge_dir_pair(
    database: String,
    pairs: Vec<(String, String)>,
    target_dir: String,
    cleanup_dirs: Vec<String>,
) -> Result<String, String> {
    let outcome =
        filelens::merge_dir_pair(&PathBuf::from(&database), &pairs, &target_dir, &cleanup_dirs)?;
    let mut message = format!(
        "已移入回收站 {} 个，{} 张移动到目标目录，{} 张原地保留。",
        outcome.recycled, outcome.moved, outcome.kept
    );
    if !outcome.cleaned_dirs.is_empty() {
        message.push_str(&format!(
            " 已删除清空的空目录 {} 个。",
            outcome.cleaned_dirs.len()
        ));
    }
    if !outcome.failures.is_empty() {
        message.push_str(&format!(
            " 失败 {} 个：{}",
            outcome.failures.len(),
            outcome.failures.join("；")
        ));
    }
    Ok(message)
}

/// Kick off a bulk sweep (kind: "trash" | "hardlink" | "delete") on a
/// background thread with live progress; the frontend polls `bulk_state`.
#[tauri::command]
fn start_bulk(
    bulk: tauri::State<'_, Mutex<Option<BulkTask>>>,
    database: String,
    kind: String,
    file_ids: Vec<i64>,
) -> Result<String, String> {
    if file_ids.is_empty() {
        return Err("没有选择要处理的文件".into());
    }
    if !matches!(kind.as_str(), "trash" | "hardlink" | "delete") {
        return Err(format!("未知的批量类型：{kind}"));
    }
    let mut slot = bulk.lock().map_err(|_| "bulk task lock failed")?;
    if slot
        .as_ref()
        .map(|task| !task.progress.lock().map(|p| p.finished).unwrap_or(true))
        .unwrap_or(false)
    {
        return Err("已有批量任务正在运行".into());
    }
    let total = file_ids.len() as u64;
    let progress = Arc::new(Mutex::new(BulkProgress {
        done: 0,
        total,
        finished: false,
        message: "正在准备批量操作...".into(),
    }));
    let worker_progress = progress.clone();
    let task_database = database.clone();
    let task_kind = kind.clone();
    std::thread::spawn(move || {
        let report = {
            let worker_progress = worker_progress.clone();
            move |done: usize, total_count: usize| {
                if let Ok(mut guard) = worker_progress.lock() {
                    guard.done = done as u64;
                    guard.total = total_count as u64;
                    guard.message = format!("正在处理 {}/{}...", done, total_count);
                }
            }
        };
        let outcome = match task_kind.as_str() {
            "trash" => {
                filelens::trash_batch_with_progress(&PathBuf::from(&task_database), &file_ids, Some(&report))
            }
            "hardlink" => {
                filelens::hardlink_batch_with_progress(&PathBuf::from(&task_database), &file_ids, Some(&report))
            }
            _ => {
                filelens::delete_approved_batch_with_progress(&PathBuf::from(&task_database), &file_ids, Some(&report))
            }
        };
        let mut summary = format!(
            "批量{}完成：成功 {} 个，失败 {} 个。",
            if task_kind == "trash" {
                "移入回收站"
            } else if task_kind == "hardlink" {
                "硬链接替换"
            } else {
                "永久删除"
            },
            outcome.succeeded,
            outcome.failures.len()
        );
        if !outcome.failures.is_empty() {
            let preview: Vec<String> = outcome.failures.iter().take(3).cloned().collect();
            summary.push_str(&format!(" 失败原因：{}", preview.join("；")));
            if outcome.failures.len() > 3 {
                summary.push_str(&format!(" 等 {} 条", outcome.failures.len()));
            }
        }
        if let Ok(mut guard) = worker_progress.lock() {
            guard.done = total;
            guard.finished = true;
            guard.message = summary;
        }
    });
    *slot = Some(BulkTask { kind, progress });
    Ok("批量任务已在后台启动，进度见提示。".into())
}

/// Snapshot of the bulk sweep for polling; `finished` stays true until a new
/// sweep starts, so a missed poll can still read the outcome.
#[tauri::command]
fn bulk_state(bulk: tauri::State<'_, Mutex<Option<BulkTask>>>) -> Result<BulkState, String> {
    let slot = bulk.lock().map_err(|_| "bulk task lock failed")?;
    Ok(match slot.as_ref() {
        Some(task) => {
            let progress = task
                .progress
                .lock()
                .map_err(|_| "bulk progress lock failed")?
                .clone();
            BulkState {
                running: !progress.finished,
                kind: task.kind.clone(),
                done: progress.done,
                total: progress.total,
                message: progress.message,
            }
        }
        None => BulkState {
            running: false,
            kind: String::new(),
            done: 0,
            total: 0,
            message: String::new(),
        },
    })
}

fn main() {
    tauri::Builder::default()
        .manage(Arc::new(Mutex::new(None::<ScanTask>)))
        .manage(Mutex::new(None::<BulkTask>))
        .manage(Arc::new(Mutex::new(None::<WatchTask>)))
        .manage(Mutex::new(None::<DuplicateVerifyTask>))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            open_project,
            save_project_config,
            save_roots,
            start_scan,
            scan_state,
            cancel_scan,
            status,
            groups,
            similar_photos,
            verify_photo_pair,
            start_duplicate_verify,
            duplicate_verify_state,
            cancel_duplicate_verify,
            similar_documents,
            detector_status,
            approve,
            unapprove,
            trash,
            trash_approved,
            trash_delete,
            trash_empty,
            delete_direct,
            delete_direct_batch,
            delete_paths,
            trash_paths,
            trash_prune_expired,
            history,
            set_trash_retention,
            set_auto_scan,
            open_file,
            image_preview,
            image_thumbnail,
            thumbnail_cache_stats,
            thumbnail_cache_clear,
            export_report,
            reveal_in_manager,
            restore,
            restore_batch,
            protect_preview,
            approve_filtered,
            hardlink_approved,
            set_strict_verify,
            set_similar_threshold,
            set_usn_scan,
            set_watch_scan,
            start_bulk,
            bulk_state,
            restore_to,
            group_dirs,
            recycle_dir_keep_one,
            merge_dir_pair,
            trash_list
        ])
        .run(tauri::generate_context!())
        .expect("failed to run FileLens desktop application");
}
