#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{
    delete_approved_batch, delete_direct, delete_paths, delete_trash, empty_trash, export_report,
    init, prune_expired_trash, query_groups, restore, scan, scan_with_control, scan_with_options,
    set_approval, trash, trash_batch, trash_list, trash_paths, trash_retention_days, Group,
    GroupFile, GroupQuery, GroupSort, GroupsPage, BatchOutcome, ScanErrorLog, ScanSummary,
};
