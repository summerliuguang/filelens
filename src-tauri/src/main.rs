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
}

#[derive(Serialize)]
struct SimilarPhoto {
    first_path: String,
    second_path: String,
    distance: u32,
    first_size: i64,
    first_modified: i64,
    second_size: i64,
    second_modified: i64,
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
    errors_total: u64,
    recent_errors: Vec<ScanErrorSample>,
}

struct ScanTask {
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<ScanState>>,
    errors: Arc<filelens::ScanErrorLog>,
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
        errors_total: 0,
        recent_errors: Vec::new(),
    }));
    let errors = Arc::new(filelens::ScanErrorLog::new());
    let worker_cancelled = cancelled.clone();
    let worker_state = state.clone();
    let worker_errors = errors.clone();
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
        );
        if let Ok(mut current) = worker_state.lock() {
            match result {
                Ok(summary) => {
                    current.state = "completed".into();
                    let mut message = "扫描完成，索引已更新。".to_string();
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
    *task = Some(ScanTask { cancelled, state, errors });
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
            errors_total: 0,
            recent_errors: Vec::new(),
        });
    };
    let mut state = current
        .state
        .lock()
        .map_err(|_| "scan state lock failed")?
        .clone();
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
    let mut moved = 0;
    let mut failed: Vec<String> = Vec::new();
    for file_id in file_ids {
        match filelens::trash(&PathBuf::from(&database), file_id) {
            Ok(()) => moved += 1,
            Err(error) => failed.push(format!("文件 #{file_id}：{error}")),
        }
    }
    if failed.is_empty() {
        Ok(format!("已将 {moved} 个确认副本移入应用回收站。"))
    } else {
        Ok(format!(
            "已移动 {moved} 个，{} 个失败：{}",
            failed.len(),
            failed.join("；")
        ))
    }
}

#[tauri::command]
fn delete_direct(database: String, file_id: i64) -> Result<String, String> {
    filelens::delete_direct(&PathBuf::from(database), file_id)
        .map(|_| "文件已永久删除。".into())
}

#[tauri::command]
fn delete_direct_batch(database: String, file_ids: Vec<i64>) -> Result<String, String> {
    let mut deleted = 0;
    let mut failed: Vec<String> = Vec::new();
    for file_id in file_ids {
        match filelens::delete_direct(&PathBuf::from(&database), file_id) {
            Ok(()) => deleted += 1,
            Err(error) => failed.push(format!("文件 #{file_id}：{error}")),
        }
    }
    if failed.is_empty() {
        Ok(format!("已永久删除 {deleted} 个文件。"))
    } else {
        Ok(format!(
            "已删除 {deleted} 个，{} 个失败：{}",
            failed.len(),
            failed.join("；")
        ))
    }
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
    tauri::async_runtime::spawn_blocking(move || render_image(&app, &source, "t", 320))
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
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    files += 1;
                    bytes += metadata.len();
                }
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
    let recoverable_bytes = connection.query_row("SELECT COALESCE(SUM((n - 1) * size),0) FROM (SELECT COUNT(*) AS n, size FROM files WHERE present=1 GROUP BY hash,size HAVING n > 1)", [], |row| row.get(0)).map_err(|error| error.to_string())?;
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
    Ok(Status {
        files,
        duplicates,
        approved,
        in_trash,
        last_scan_at,
        groups,
        recoverable_bytes,
    })
}

#[tauri::command]
fn groups(
    database: String,
    offset: Option<i64>,
    limit: Option<i64>,
    min_size: Option<i64>,
    path_contains: Option<String>,
    sort: Option<String>,
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
            _ => filelens::GroupSort::Size,
        },
        offset: offset.unwrap_or(0),
        limit: limit.unwrap_or(50),
    };
    filelens::query_groups(&PathBuf::from(database), &query)
}

struct FingerprintEntry {
    path: String,
    hash: String,
    value: u64,
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

fn load_fingerprints(
    connection: &Connection,
    table: &str,
    column: &str,
) -> Result<Vec<FingerprintEntry>, String> {
    let mut statement = connection
        .prepare(&format!(
            "SELECT a.path, a.hash, f.{column}, a.size, a.modified FROM {table} f \
             JOIN files a ON a.id = f.file_id WHERE a.present = 1"
        ))
        .map_err(|e| e.to_string())?;
    statement
        .query_map([], |row| {
            Ok(FingerprintEntry {
                path: row.get(0)?,
                hash: row.get(1)?,
                value: row.get::<_, i64>(2)? as u64,
                size: row.get(3)?,
                modified: row.get(4)?,
            })
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

fn similar_pairs(
    entries: &[FingerprintEntry],
    chunks: &[(u32, u64)],
    max_distance: u32,
) -> Vec<(usize, usize, u32)> {
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
                    if first.hash == second.hash {
                        seen.insert(key);
                        continue;
                    }
                    let distance = (first.value ^ second.value).count_ones();
                    if distance <= max_distance {
                        seen.insert(key);
                        result.push((key.0, key.1, distance));
                    }
                }
            }
        }
    }
    result.sort_by(|a, b| {
        a.2.cmp(&b.2)
            .then_with(|| entries[a.0].path.cmp(&entries[b.0].path))
    });
    result.truncate(SIMILAR_RESULT_LIMIT);
    result
}

#[tauri::command]
fn similar_photos(database: String) -> Result<Vec<SimilarPhoto>, String> {
    let connection = open_database(&database)?;
    let entries = load_fingerprints(&connection, "photo_fingerprints", "dhash")?;
    Ok(similar_pairs(&entries, &PHOTO_CHUNKS, 4)
        .into_iter()
        .map(|(a, b, distance)| SimilarPhoto {
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
fn similar_documents(database: String) -> Result<Vec<SimilarDocument>, String> {
    let connection = open_database(&database)?;
    let entries = load_fingerprints(&connection, "document_fingerprints", "simhash")?;
    Ok(similar_pairs(&entries, &DOCUMENT_CHUNKS, 8)
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
            name: "照片相似".into(),
            available: true,
            detail: "本地 dHash，高置信度只读候选".into(),
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
    let mut statement = connection.prepare("SELECT id,created_at,source_path,trash_path FROM operations WHERE state='trashed' ORDER BY created_at DESC").map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            let created_at: i64 = row.get(1)?;
            Ok(TrashItem {
                id: row.get(0)?,
                created_at,
                source_path: row.get(2)?,
                trash_path: row.get(3)?,
                expired: retention > 0 && now.saturating_sub(created_at) > retention * 86_400,
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
) -> Result<Vec<HistoryItem>, String> {
    let limit = limit.unwrap_or(100).clamp(1, 500);
    let offset = offset.unwrap_or(0).max(0);
    let connection = open_database(&database)?;
    let mut statement = connection
        .prepare(
            "SELECT id,created_at,source_path,trash_path,state,restored_at \
             FROM operations ORDER BY created_at DESC, id DESC LIMIT ?1 OFFSET ?2",
        )
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

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(None::<ScanTask>))
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
            restore,
            trash_list
        ])
        .run(tauri::generate_context!())
        .expect("failed to run FileLens desktop application");
}
