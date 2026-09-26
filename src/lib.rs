#[allow(dead_code)]
#[path = "main.rs"]
mod implementation;

pub use implementation::{
    approve_groups_except_keeper, delete_approved_batch, delete_approved_batch_with_progress,
    delete_direct, delete_paths, delete_trash, delete_trash_batch, duplicate_photo_candidates,
    empty_dirs, empty_trash, evict_lru_files, export_report, group_dirs, hardlink,
    hardlink_batch, hardlink_batch_with_progress, init, keeper_score,
    cached_pair_verdicts, image_dimensions, merge_dir_pair, photos_pixel_identical,
    protect_preview, project_status, prune_expired_trash, query_groups, recycle_dir_keep_one,
    remove_empty_dirs, restore, restore_batch, restore_to, similar_document_pairs,
    similar_photo_pairs, store_pair_verdicts, move_back, trash_usage, verify_photo_pairs,
    zero_byte_files,
    scan, scan_with_control, scan_with_options, set_approval, set_similar_threshold,
    set_strict_verify, set_usn_scan, set_watch_scan,
    trash, trash_batch, trash_batch_with_progress, trash_list, trash_paths,
    trash_retention_days, BatchOutcome, DirCount, DirKeepOutcome, EmptyDirOutcome, Group,
    GroupFile, GroupQuery, GroupSort, GroupsPage, HashProgress, MarkStrategy, PairVerdict,
    ProtectPreview, ScanErrorLog, DirMergeOutcome, ScanSummary, SimilarDocument, SimilarPhoto,
    SimilarPhotosPage, StatusSnapshot, TrashUsage, ZeroByteFile,
};
