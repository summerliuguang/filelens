// Hide the console window in release builds (it would otherwise shadow the
// app with a black terminal); keep it in debug so logs stay visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use image::ImageDecoder as _;
use rusqlite::{Connection, params};
use serde::Serialize;
use std::path::Path;
use tauri::Manager;

#[derive(Serialize)]
struct Status {
    files: i64,
    duplicates: i64,
    approved: i64,
    in_trash: i64,
    last_scan_at: Option<i64>,
    groups: i64,
    recoverable_bytes: i64,
    photo_candidates: i64,
    photo_candidate_bytes: i64,
    document_candidates: i64,
    document_candidate_bytes: i64,
}

#[derive(Serialize)]
struct SimilarPhoto {
    first_path: String,
    second_path: String,
    distance: u32,
    phash_distance: u32,
    first_size: i64,
    first_modified: i64,
    second_size: i64,
    second_modified: i64,
}
#[derive(Serialize)]
struct SimilarPhotosPage {
    pairs: Vec<SimilarPhoto>,
    /// Pairs kept after the hard cap; when truncated the true total is only
    /// known to be larger.
    total: usize,
    truncated: bool,
}

#[derive(Serialize, Clone)]
struct BulkState {
    running: bool,
    kind: String,
    done: u64,
    total: u64,
    message: String,
}

#[derive(Serialize)]
struct SimilarDocument {
    first_path: String,
    second_path: String,
    distance: u32,
    first_size: i64,
    first_modified: i64,
    second_size: i64,
    second_modified: i64,
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
    })
}

#[tauri::command]
fn save_project_config(
    app: tauri::AppHandle,
    database: String,
    trash: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
    exclude_rules: Vec<String>,
    min_file_size: i64,
) -> Result<String, String> {
    filelens::init(&PathBuf::from(&database), &PathBuf::from(&trash))?;
    let connection = open_database(&database)?;
    save_setting_list(&connection, "roots", &roots)?;
    save_setting_list(&connection, "protect_rules", &protect_rules)?;
    save_setting_list(&connection, "exclude_rules", &exclude_rules)?;
    let min_size = min_file_size.max(0).to_string();
    connection
        .execute(
            "INSERT INTO settings(key,value) VALUES('min_file_size',?1) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![min_size],
        )
        .map_err(|error| error.to_string())?;
    write_project_pointer(&app, &database)?;
    Ok("项目设置已保存。".into())
}

#[tauri::command]
fn save_roots(
    database: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
) -> Result<String, String> {
    let connection = open_database(&database)?;
    save_setting_list(&connection, "roots", &roots)?;
    save_setting_list(&connection, "protect_rules", &protect_rules)?;
    Ok("扫描目录已保存。".into())
}

#[tauri::command]
fn start_scan(
    task: tauri::State<'_, Mutex<Option<ScanTask>>>,
    database: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
    exclude_rules: Vec<String>,
    min_file_size: i64,
) -> Result<String, String> {
    let mut task = task.lock().map_err(|_| "scan task lock failed")?;
    if task.is_some() {
        return Err("已有扫描任务正在运行".into());
    }
    {
        let connection = open_database(&database)?;
        save_setting_list(&connection, "roots", &roots)?;
        save_setting_list(&connection, "protect_rules", &protect_rules)?;
        save_setting_list(&connection, "exclude_rules", &exclude_rules)?;
    }
    let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
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
    Ok("扫描任务已在后台启动。".into())
}

#[tauri::command]
fn scan_state(task: tauri::State<'_, Mutex<Option<ScanTask>>>) -> Result<ScanState, String> {
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
fn cancel_scan(task: tauri::State<'_, Mutex<Option<ScanTask>>>) -> Result<String, String> {
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
fn restore_batch(database: String, batch_id: i64) -> Result<String, String> {
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
}

/// Settings-page preview: how many indexed files the unsaved rule set would
/// shield, with sample paths.
#[tauri::command]
fn protect_preview(
    database: String,
    protect_rules: Vec<String>,
) -> Result<filelens::ProtectPreview, String> {
    filelens::protect_preview(&PathBuf::from(&database), &protect_rules)
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
fn export_report(database: String, path: String) -> Result<String, String> {
    let rows = filelens::export_report(&PathBuf::from(&database), &PathBuf::from(&path))?;
    Ok(format!("已导出 {rows} 条重复记录到 {}", path))
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
fn status(database: String) -> Result<Status, String> {
    let connection = open_database(&database)?;
    let files = connection
        .query_row("SELECT COUNT(*) FROM files WHERE present=1", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    let duplicates = connection.query_row("SELECT COALESCE(SUM(n - 1),0) FROM (SELECT COUNT(*) n FROM files WHERE present=1 GROUP BY hash,size HAVING n > 1)", [], |row| row.get(0)).map_err(|error| error.to_string())?;
    let approved = connection
        .query_row(
            "SELECT COUNT(*) FROM files WHERE present=1 AND approved=1",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    // Hardlink-aware recoverable size: names sharing one physical file count
    // once, so hardlink-only groups do not inflate the estimate.
    let recoverable_bytes = connection.query_row(
        "SELECT COALESCE(SUM((physical - 1) * size),0) FROM (SELECT size, \
         COUNT(DISTINCT CASE WHEN dev!=0 OR inode!=0 THEN printf('%d:%d',dev,inode) ELSE 'i'||id END) AS physical \
         FROM files WHERE present=1 GROUP BY hash,size HAVING COUNT(*) > 1)",
        [],
        |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    let in_trash = connection
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE state='trashed'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let last_scan_at = connection
        .query_row(
            "SELECT value FROM settings WHERE key='last_scan_at'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|value| value.parse::<i64>().ok());
    let groups = connection
        .query_row(
            "SELECT COUNT(*) FROM (SELECT 1 FROM files WHERE present=1 \
             GROUP BY hash,size HAVING COUNT(*) > 1)",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    // Photo / document candidates: distinct removable files across all
    // accepted pairs, with their combined size (advisory — a human still
    // confirms every removal).
    let photo_entries = load_fingerprints(&connection, "photo_fingerprints", "dhash", Some("phash"))?;
    let (photo_pairs, _) = similar_pairs(&photo_entries, &PHOTO_CHUNKS, 0, 4, Some(PHASH_MAX_DISTANCE));
    let (photo_candidates, photo_candidate_bytes) = candidate_totals(&photo_entries, &photo_pairs);
    let document_entries = load_fingerprints(&connection, "document_fingerprints", "simhash", None)?;
    let (document_pairs, _) = similar_pairs(&document_entries, &DOCUMENT_CHUNKS, 0, 8, None);
    let (document_candidates, document_candidate_bytes) =
        candidate_totals(&document_entries, &document_pairs);
    Ok(Status {
        files,
        duplicates,
        approved,
        in_trash,
        last_scan_at,
        groups,
        recoverable_bytes,
        photo_candidates,
        photo_candidate_bytes,
        document_candidates,
        document_candidate_bytes,
    })
}

/// Distinct files across accepted pairs and their combined size.
fn candidate_totals(entries: &[FingerprintEntry], pairs: &[(usize, usize, u32)]) -> (i64, i64) {
    let mut seen: HashSet<usize> = HashSet::new();
    let (mut count, mut bytes) = (0_i64, 0_i64);
    for &(a, b, _) in pairs {
        for index in [a, b] {
            if seen.insert(index) {
                count += 1;
                bytes += entries[index].size.max(0);
            }
        }
    }
    (count, bytes)
}

#[tauri::command]
fn groups(
    database: String,
    offset: Option<i64>,
    limit: Option<i64>,
    min_size: Option<i64>,
    path_contains: Option<String>,
    sort: Option<String>,
    kind: Option<String>,
    dir_contains: Option<String>,
) -> Result<filelens::GroupsPage, String> {
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
    filelens::query_groups(&PathBuf::from(database), &query)
}

#[derive(Serialize)]
struct GroupDir {
    dir: String,
    count: i64,
}

#[tauri::command]
fn group_dirs(database: String) -> Result<Vec<GroupDir>, String> {
    Ok(filelens::group_dirs(&PathBuf::from(database))?
        .into_iter()
        .map(|entry| GroupDir {
            dir: entry.dir,
            count: entry.count,
        })
        .collect())
}

struct FingerprintEntry {
    path: String,
    hash: String,
    value: u64,
    /// Secondary fingerprint (pHash for photos, absent for documents).
    extra: u64,
    size: i64,
    modified: i64,
}

/// Disjoint 64-bit chunks used for candidate bucketing. Pigeonhole: with N
/// chunks, any pair differing in at most N-1 bits must share at least one
/// chunk value, so the bucket join is exact for the thresholds below.
const PHOTO_CHUNKS: [(u32, u64); 5] = [
    (51, 0x1fff),
    (38, 0x1fff),
    (25, 0x1fff),
    (12, 0x1fff),
    (0, 0x0fff),
];
const DOCUMENT_CHUNKS: [(u32, u64); 8] = [
    (56, 0xff),
    (48, 0xff),
    (40, 0xff),
    (32, 0xff),
    (24, 0xff),
    (16, 0xff),
    (8, 0xff),
    (0, 0xff),
];
const SIMILAR_MAX_BUCKET: usize = 256;
const SIMILAR_RESULT_LIMIT: usize = 5000;
/// pHash (DCT) secondary threshold: common practice for "same photo,
/// recompressed/resized" is well inside 10/64.
const PHASH_MAX_DISTANCE: u32 = 10;

fn load_fingerprints(
    connection: &Connection,
    table: &str,
    column: &str,
    extra_column: Option<&str>,
) -> Result<Vec<FingerprintEntry>, String> {
    let extra_expression = extra_column
        .map(|column| format!("f.{column}"))
        .unwrap_or_else(|| "0".to_string());
    let mut statement = connection
        .prepare(&format!(
            "SELECT a.path, a.hash, f.{column}, {extra_expression}, a.size, a.modified \
             FROM {table} f JOIN files a ON a.id = f.file_id WHERE a.present = 1"
        ))
        .map_err(|e| e.to_string())?;
    statement
        .query_map([], |row| {
            Ok(FingerprintEntry {
                path: row.get(0)?,
                hash: row.get(1)?,
                value: row.get::<_, i64>(2)? as u64,
                extra: row.get::<_, i64>(3)? as u64,
                size: row.get(4)?,
                modified: row.get(5)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn similar_pairs(
    entries: &[FingerprintEntry],
    chunks: &[(u32, u64)],
    min_distance: u32,
    max_distance: u32,
    extra_max_distance: Option<u32>,
) -> (Vec<(usize, usize, u32)>, usize) {
    let mut seen: HashSet<(usize, usize)> = HashSet::new();
    let mut result: Vec<(usize, usize, u32)> = Vec::new();
    for &(shift, mask) in chunks {
        let mut buckets: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            buckets
                .entry((entry.value >> shift) & mask)
                .or_default()
                .push(index);
        }
        for members in buckets.into_values() {
            // Oversized buckets (e.g. thousands of near-black thumbnails)
            // would pair quadratically; these clusters are skipped.
            if members.len() < 2 || members.len() > SIMILAR_MAX_BUCKET {
                continue;
            }
            for i in 0..members.len() {
                for j in (i + 1)..members.len() {
                    let key = (members[i].min(members[j]), members[i].max(members[j]));
                    if seen.contains(&key) {
                        continue;
                    }
                    let (first, second) = (&entries[key.0], &entries[key.1]);
                    // Byte-identical duplicates belong to the exact-duplicate
                    // review, never to the photo detectors.
                    if first.hash == second.hash {
                        seen.insert(key);
                        continue;
                    }
                    let distance = (first.value ^ second.value).count_ones();
                    if distance < min_distance || distance > max_distance {
                        continue;
                    }
                    if let Some(max) = extra_max_distance {
                        // Secondary fingerprint must agree too: cuts the
                        // gradient-hash false positives substantially.
                        if (first.extra ^ second.extra).count_ones() > max {
                            seen.insert(key);
                            continue;
                        }
                    }
                    seen.insert(key);
                    result.push((key.0, key.1, distance));
                }
            }
        }
    }
    result.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| entries[a.0].path.cmp(&entries[b.0].path))
    });
    let total = result.len();
    result.truncate(SIMILAR_RESULT_LIMIT);
    (result, total)
}

/// kind = "duplicate": both fingerprints exactly equal (dHash 0 + pHash 0) —
/// a near-certain pixel copy, still subject to the background per-pair pixel
/// verification (start_duplicate_verify) before the UI shows it. kind =
/// "similar": near-identical pixels (dHash distance 1-4) with a pHash gate.
/// Both exclude byte-identical exact duplicates.
#[tauri::command]
fn similar_photos(
    database: String,
    kind: Option<String>,
) -> Result<SimilarPhotosPage, String> {
    let connection = open_database(&database)?;
    let entries = load_fingerprints(&connection, "photo_fingerprints", "dhash", Some("phash"))?;
    let (min_distance, max_distance, extra_max_distance) = if kind.as_deref() == Some("duplicate")
    {
        (0, 0, Some(0))
    } else {
        (1, 4, Some(PHASH_MAX_DISTANCE))
    };
    let (pairs, total) = similar_pairs(
        &entries,
        &PHOTO_CHUNKS,
        min_distance,
        max_distance,
        extra_max_distance,
    );
    Ok(SimilarPhotosPage {
        pairs: pairs
            .into_iter()
            .map(|(a, b, distance)| SimilarPhoto {
                first_path: entries[a].path.clone(),
                second_path: entries[b].path.clone(),
                distance,
                phash_distance: (entries[a].extra ^ entries[b].extra).count_ones(),
                first_size: entries[a].size,
                first_modified: entries[a].modified,
                second_size: entries[b].size,
                second_modified: entries[b].modified,
            })
            .collect(),
        total,
        truncated: total > SIMILAR_RESULT_LIMIT,
    })
}

/// Definitive pixel comparison for one candidate pair. The duplicate-photos
/// view is fingerprint-based, so it can contain near-identical burst shots;
/// the compare modal runs this before the user deletes either side.
#[tauri::command]
fn verify_photo_pair(first: String, second: String) -> Result<bool, String> {
    filelens::photos_pixel_identical(&first, &second)
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

/// Recompute the duplicate-view candidate list: both fingerprints exactly
/// equal. Shared by the query command and the background verification task so
/// the verified set always matches what the UI fetched.
fn duplicate_photo_candidates(database: &str) -> Result<Vec<(String, String)>, String> {
    let connection = open_database(database)?;
    let entries = load_fingerprints(&connection, "photo_fingerprints", "dhash", Some("phash"))?;
    let (pairs, _) = similar_pairs(&entries, &PHOTO_CHUNKS, 0, 0, Some(0));
    Ok(pairs
        .into_iter()
        .map(|(a, b, _)| (entries[a].path.clone(), entries[b].path.clone()))
        .collect())
}

/// Verify every duplicate-view candidate pixel by pixel on background
/// threads. Idempotent: while a run is active, starting again just reports
/// its current state. The verdicts stream into `results` as they are
/// computed, so the UI can filter progressively.
#[tauri::command]
fn start_duplicate_verify(
    state: tauri::State<'_, Mutex<Option<DuplicateVerifyTask>>>,
    database: String,
) -> Result<DuplicateVerifyState, String> {
    let mut slot = state
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
    let pairs = duplicate_photo_candidates(&database)?;
    // Serve cached verdicts instantly; only pairs whose either file changed
    // since the last check go through the decoder again. Fresh verdicts are
    // written back so the next pass is a cache hit.
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
    let task_state = Arc::new(Mutex::new(initial));
    let cancel = Arc::new(AtomicBool::new(false));
    *slot = Some(DuplicateVerifyTask {
        cancel: cancel.clone(),
        state: task_state.clone(),
    });
    drop(slot);
    let task_state_for_thread = task_state.clone();
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
        let publish = |verdict: &filelens::PairVerdict| {
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
        let should_stop = || cancel.load(Ordering::Relaxed);
        let _ = filelens::verify_photo_pairs(&pending, &publish, &should_stop);
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
fn similar_documents(database: String) -> Result<Vec<SimilarDocument>, String> {
    let connection = open_database(&database)?;
    let entries = load_fingerprints(&connection, "document_fingerprints", "simhash", None)?;
    let (pairs, _) = similar_pairs(&entries, &DOCUMENT_CHUNKS, 0, 8, None);
    Ok(pairs
        .into_iter()
        .map(|(a, b, distance)| SimilarDocument {
            first_path: entries[a].path.clone(),
            second_path: entries[b].path.clone(),
            distance,
            first_size: entries[a].size,
            first_modified: entries[a].modified,
            second_size: entries[b].size,
            second_modified: entries[b].modified,
        })
        .collect())
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
fn trash_list(database: String) -> Result<Vec<TrashItem>, String> {
    let connection = open_database(&database)?;
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
fn history(
    database: String,
    offset: Option<i64>,
    limit: Option<i64>,
    state_filter: Option<String>,
) -> Result<Vec<HistoryItem>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let connection = open_database(&database)?;
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
        .manage(Mutex::new(None::<ScanTask>))
        .manage(Mutex::new(None::<BulkTask>))
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
            set_usn_scan,
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
