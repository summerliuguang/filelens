use std::process::Command;

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

fn cli(arguments: &[String]) -> Result<String, String> {
    let root = std::env::current_dir().map_err(|error| error.to_string())?;
    let output = Command::new("cargo")
        .current_dir(root)
        .args(["run", "--quiet", "--"])
        .args(arguments)
        .output()
        .map_err(|error| format!("start FileLens core: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

#[tauri::command]
fn initialize(database: String, trash: String) -> Result<String, String> {
    cli(&["init".into(), database, trash])
}

#[tauri::command]
fn scan(database: String, roots: Vec<String>) -> Result<String, String> {
    let mut arguments = vec!["scan".into(), database];
    for root in roots {
        arguments.push("--root".into());
        arguments.push(root);
    }
    cli(&arguments)
}

#[tauri::command]
fn status(database: String) -> Result<Status, String> {
    let output = cli(&["status".into(), database])?;
    let numbers: Vec<i64> = output
        .lines()
        .filter_map(|line| line.split(':').nth(1)?.trim().parse().ok())
        .collect();
    if numbers.len() == 3 {
        Ok(Status {
            files: numbers[0],
            duplicates: numbers[1],
            in_trash: numbers[2],
        })
    } else {
        Err("unexpected core status response".into())
    }
}

#[tauri::command]
fn groups(database: String) -> Result<Vec<Group>, String> {
    let output = cli(&["groups".into(), database])?;
    parse_groups(&output)
}

#[tauri::command]
fn approve(database: String, file_id: i64) -> Result<String, String> {
    cli(&[
        "approve".into(),
        database,
        "--file-id".into(),
        file_id.to_string(),
    ])
}
#[tauri::command]
fn unapprove(database: String, file_id: i64) -> Result<String, String> {
    cli(&[
        "unapprove".into(),
        database,
        "--file-id".into(),
        file_id.to_string(),
    ])
}
#[tauri::command]
fn trash(database: String, file_id: i64) -> Result<String, String> {
    cli(&[
        "trash".into(),
        database,
        "--file-id".into(),
        file_id.to_string(),
    ])
}

#[tauri::command]
fn restore(database: String, operation_id: i64) -> Result<String, String> {
    cli(&[
        "restore".into(),
        database,
        "--operation-id".into(),
        operation_id.to_string(),
    ])
}

#[tauri::command]
fn trash_list(database: String) -> Result<Vec<TrashItem>, String> {
    let output = cli(&["trash-list".into(), database])?;
    output
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let end = line.find(']').ok_or("invalid recycle-bin item")?;
            let id = line[1..end].parse().map_err(|_| "invalid recycle-bin id")?;
            let parts: Vec<_> = line[end + 1..].trim().split('\t').collect();
            if parts.len() != 3 {
                return Err("invalid recycle-bin item".into());
            }
            Ok(TrashItem {
                id,
                created_at: parts[0].parse().map_err(|_| "invalid recycle-bin date")?,
                source_path: parts[1].into(),
                trash_path: parts[2].into(),
            })
        })
        .collect()
}

fn parse_groups(output: &str) -> Result<Vec<Group>, String> {
    let mut groups = Vec::new();
    let mut current: Option<Group> = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Group ") {
            if let Some(group) = current.take() {
                groups.push(group);
            }
            let parts: Vec<_> = trimmed.split_whitespace().collect();
            let size = parts
                .get(6)
                .and_then(|value| value.parse().ok())
                .ok_or("invalid group size")?;
            let hash = parts.last().ok_or("invalid group hash")?.to_string();
            current = Some(Group {
                hash,
                size,
                files: Vec::new(),
            });
        } else if trimmed.starts_with('[') {
            let end = trimmed.find(']').ok_or("invalid file item")?;
            let id = trimmed[1..end].parse().map_err(|_| "invalid file id")?;
            let remaining = trimmed[end + 1..].trim();
            let protected =
                remaining.ends_with(" [protected]") || remaining.contains(" [protected] ");
            let approved = remaining.ends_with(" [approved]") || remaining.contains(" [approved] ");
            let path = remaining
                .replace(" [protected]", "")
                .replace(" [approved]", "");
            if let Some(group) = &mut current {
                group.files.push(GroupFile {
                    id,
                    path,
                    protected,
                    approved,
                });
            }
        }
    }
    if let Some(group) = current {
        groups.push(group);
    }
    Ok(groups)
}

fn main() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            initialize, scan, status, groups, approve, unapprove, trash, restore, trash_list
        ])
        .run(tauri::generate_context!())
        .expect("failed to run FileLens desktop application");
}
