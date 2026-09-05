#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{
    delete_direct, delete_paths, delete_trash, empty_trash, init, prune_expired_trash,
    query_groups, restore, scan, scan_with_control, set_approval, trash, trash_list, trash_paths,
    trash_retention_days, Group, GroupFile, GroupQuery, GroupSort, GroupsPage, BatchOutcome,
    ScanSummary,
};
