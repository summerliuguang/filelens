#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{
    approve_groups_except_keeper, delete_approved_batch, delete_direct, delete_paths,
    delete_trash, empty_trash, evict_lru_files, export_report, init, protect_preview,
    prune_expired_trash, query_groups, restore, restore_batch, scan, scan_with_control,
    scan_with_options, set_approval, trash, trash_batch, trash_list, trash_paths,
    trash_retention_days, BatchOutcome, Group, GroupFile, GroupQuery, GroupSort, GroupsPage,
    HashProgress, MarkStrategy, ProtectPreview, ScanErrorLog, ScanSummary,
};
