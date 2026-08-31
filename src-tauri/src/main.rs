use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use rusqlite::{Connection, params};
use serde::Serialize;

#[derive(Serialize)]
struct Status {
    files: i64,
    duplicates: i64,
    in_trash: i64,
}
#[derive(Serialize)]
struct GroupFile {
    id: i64,
    path: String,
    protected: bool,
    approved: bool,
}
#[derive(Serialize)]
struct Group {
    hash: String,
    size: i64,
    files: Vec<GroupFile>,
}

#[derive(Serialize)]
struct SimilarPhoto {
    first_path: String,
    second_path: String,
    distance: u32,
}
#[derive(Serialize)]
struct SimilarDocument {
    first_path: String,
    second_path: String,
    distance: u32,
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
}

#[derive(Serialize)]
struct ProjectConfig {
    trash_path: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
}

#[derive(Clone, Serialize)]
struct ScanState {
    state: String,
    processed: u64,
    message: String,
}

struct ScanTask {
    cancelled: Arc<AtomicBool>,
    state: Arc<Mutex<ScanState>>,
}

fn open_database(path: &str) -> Result<Connection, String> {
    Connection::open(path).map_err(|error| error.to_string())
}

#[tauri::command]
fn initialize(database: String, trash: String) -> Result<String, String> {
    filelens::init(&PathBuf::from(database), &PathBuf::from(trash))
        .map(|_| "项目已创建或打开。".into())
}

#[tauri::command]
fn project_config(database: String) -> Result<ProjectConfig, String> {
    let connection = open_database(&database)?;
    let trash_path = connection
        .query_row(
            "SELECT value FROM settings WHERE key='trash_path'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(ProjectConfig {
        trash_path,
        roots: setting_list(&connection, "roots")?,
        protect_rules: setting_list(&connection, "protect_rules")?,
    })
}

#[tauri::command]
fn save_project_config(
    database: String,
    trash: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
) -> Result<String, String> {
    filelens::init(&PathBuf::from(&database), &PathBuf::from(&trash))?;
    let connection = open_database(&database)?;
    save_setting_list(&connection, "roots", &roots)?;
    save_setting_list(&connection, "protect_rules", &protect_rules)?;
    Ok("项目设置已保存。".into())
}

#[tauri::command]
fn start_scan(
    task: tauri::State<'_, Mutex<Option<ScanTask>>>,
    database: String,
    roots: Vec<String>,
    protect_rules: Vec<String>,
) -> Result<String, String> {
    let mut task = task.lock().map_err(|_| "scan task lock failed")?;
    if task.is_some() {
        return Err("已有扫描任务正在运行".into());
    }
    let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    let cancelled = Arc::new(AtomicBool::new(false));
    let state = Arc::new(Mutex::new(ScanState {
        state: "running".into(),
        processed: 0,
        message: "正在扫描文件...".into(),
    }));
    let worker_cancelled = cancelled.clone();
    let worker_state = state.clone();
    std::thread::spawn(move || {
        let progress_state = worker_state.clone();
        let result = filelens::scan_with_control(
            &PathBuf::from(database),
            &roots,
            &protect_rules,
            &|| worker_cancelled.load(Ordering::Relaxed),
            &|processed| {
                if let Ok(mut current) = progress_state.lock() {
                    current.processed = processed;
                }
            },
        );
        if let Ok(mut current) = worker_state.lock() {
            match result {
                Ok(()) => {
                    current.state = "completed".into();
                    current.message = "扫描完成，索引已更新。".into();
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
    *task = Some(ScanTask { cancelled, state });
    Ok("扫描任务已在后台启动。".into())
}

#[tauri::command]
fn scan_state(task: tauri::State<'_, Mutex<Option<ScanTask>>>) -> Result<ScanState, String> {
    let mut task = task.lock().map_err(|_| "scan task lock failed")?;
    let Some(current) = task.as_ref() else {
        return Ok(ScanState {
            state: "idle".into(),
            processed: 0,
            message: "没有正在运行的扫描任务。".into(),
        });
    };
    let state = current
        .state
        .lock()
        .map_err(|_| "scan state lock failed")?
        .clone();
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
    for file_id in file_ids {
        filelens::trash(&PathBuf::from(&database), file_id)?;
        moved += 1;
    }
    Ok(format!("已将 {moved} 个确认副本移入应用回收站。"))
}

#[tauri::command]
fn image_thumbnail(path: String) -> Result<Option<String>, String> {
    let reader = match image::ImageReader::open(path) {
        Ok(reader) => reader,
        Err(_) => return Ok(None),
    };
    let image = match reader.decode() {
        Ok(image) => image.thumbnail(320, 240),
        Err(_) => return Ok(None),
    };
    let mut output = std::io::Cursor::new(Vec::new());
    image
        .write_to(&mut output, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(Some(format!(
        "data:image/png;base64,{}",
        STANDARD.encode(output.into_inner())
    )))
}

#[tauri::command]
fn restore(database: String, operation_id: i64) -> Result<String, String> {
    filelens::restore(&PathBuf::from(database), operation_id).map(|_| "文件已恢复至原位置。".into())
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
    let in_trash = connection
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE state='trashed'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(Status {
        files,
        duplicates,
        in_trash,
    })
}

#[tauri::command]
fn groups(database: String) -> Result<Vec<Group>, String> {
    let connection = open_database(&database)?;
    let mut statement = connection.prepare("SELECT hash,size FROM files WHERE present=1 GROUP BY hash,size HAVING COUNT(*) > 1 ORDER BY size DESC").map_err(|error| error.to_string())?;
    let keys = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut result = Vec::new();
    for key in keys {
        let (hash, size) = key.map_err(|error| error.to_string())?;
        let mut members = connection.prepare("SELECT id,path,protected,approved FROM files WHERE present=1 AND hash=?1 AND size=?2 ORDER BY path").map_err(|error| error.to_string())?;
        let files = members
            .query_map(params![hash, size], |row| {
                Ok(GroupFile {
                    id: row.get(0)?,
                    path: row.get(1)?,
                    protected: row.get::<_, i64>(2)? != 0,
                    approved: row.get::<_, i64>(3)? != 0,
                })
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        result.push(Group { hash, size, files });
    }
    Ok(result)
}

#[tauri::command]
fn similar_photos(database: String) -> Result<Vec<SimilarPhoto>, String> {
    let connection = open_database(&database)?;
    let mut statement = connection.prepare(
        "SELECT DISTINCT a.path,b.path,p1.dhash,p2.dhash
         FROM photo_fingerprints p1
         JOIN photo_fingerprints p2 ON p1.file_id < p2.file_id
           AND (p1.part_a=p2.part_a OR p1.part_b=p2.part_b OR p1.part_c=p2.part_c OR p1.part_d=p2.part_d OR p1.part_e=p2.part_e)
         JOIN files a ON a.id=p1.file_id
         JOIN files b ON b.id=p2.file_id
         WHERE a.present=1 AND b.present=1 AND a.hash != b.hash
         LIMIT 5000",
    ).map_err(|error| error.to_string())?;
    let candidates = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
            ))
        })
        .map_err(|error| error.to_string())?;
    let mut result = Vec::new();
    for candidate in candidates {
        let (first_path, second_path, first_hash, second_hash) =
            candidate.map_err(|error| error.to_string())?;
        let distance = (first_hash ^ second_hash).count_ones();
        if distance <= 4 {
            result.push(SimilarPhoto {
                first_path,
                second_path,
                distance,
            });
        }
    }
    Ok(result)
}

#[tauri::command]
fn similar_documents(database: String) -> Result<Vec<SimilarDocument>, String> {
    let connection = open_database(&database)?;
    let mut statement = connection.prepare("SELECT a.path,b.path,d1.simhash,d2.simhash FROM document_fingerprints d1 JOIN document_fingerprints d2 ON d1.file_id<d2.file_id JOIN files a ON a.id=d1.file_id JOIN files b ON b.id=d2.file_id WHERE a.present=1 AND b.present=1 AND a.hash!=b.hash LIMIT 5000").map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
            ))
        })
        .map_err(|e| e.to_string())?;
    let mut result = Vec::new();
    for row in rows {
        let (first_path, second_path, first, second) = row.map_err(|e| e.to_string())?;
        let distance = (first ^ second).count_ones();
        if distance <= 8 {
            result.push(SimilarDocument {
                first_path,
                second_path,
                distance,
            });
        }
    }
    Ok(result)
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
    let mut statement = connection.prepare("SELECT id,created_at,source_path,trash_path FROM operations WHERE state='trashed' ORDER BY created_at DESC").map_err(|error| error.to_string())?;
    statement
        .query_map([], |row| {
            Ok(TrashItem {
                id: row.get(0)?,
                created_at: row.get(1)?,
                source_path: row.get(2)?,
                trash_path: row.get(3)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}

fn main() {
    tauri::Builder::default()
        .manage(Mutex::new(None::<ScanTask>))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            initialize,
            project_config,
            save_project_config,
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
            image_thumbnail,
            restore,
            trash_list
        ])
        .run(tauri::generate_context!())
        .expect("failed to run FileLens desktop application");
}
