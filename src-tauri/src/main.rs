use std::path::PathBuf;

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
struct TrashItem {
    id: i64,
    created_at: i64,
    source_path: String,
    trash_path: String,
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
fn scan(database: String, roots: Vec<String>) -> Result<String, String> {
    let roots = roots.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    filelens::scan(&PathBuf::from(database), &roots, &[]).map(|_| "扫描完成，索引已更新。".into())
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
        .invoke_handler(tauri::generate_handler![
            initialize, scan, status, groups, approve, unapprove, trash, restore, trash_list
        ])
        .run(tauri::generate_context!())
        .expect("failed to run FileLens desktop application");
}
