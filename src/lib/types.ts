// Shared data types mirrored from the Rust command layer. Rust serializes
// struct fields as snake_case verbatim — keep every field name identical or
// the frontend silently reads undefined.

export type Page =
  | "overview"
  | "sources"
  | "review"
  | "similar"
  | "documents"
  | "detectors"
  | "trash"
  | "history"
  | "settings";

export type GroupFile = {
  id: number;
  path: string;
  protected: boolean;
  approved: boolean;
  modified: number;
  /** Another group member shares this file's (dev, inode): removing it frees no space. */
  hardlinked: boolean;
  /** Heuristic keeper suggestion; advisory only. */
  suggested: boolean;
};

export type Group = {
  hash: string;
  size: number;
  /** Bytes actually freed by removing all removable copies (hardlink-aware). */
  recoverable: number;
  files: GroupFile[];
};

export type GroupsPage = { total: number; groups: Group[] };

export type GroupFilters = {
  search: string;
  minSize: number;
  sort: "recoverable" | "size" | "members" | "path";
  kind: string;
  dir: string;
};

export type Toast = {
  id: number;
  kind: "ok" | "error";
  text: string;
  /** Optional follow-up (e.g. "open the recycle bin") rendered as a button. */
  action?: { label: string; run: () => void };
};

export type Status = {
  files: number;
  duplicates: number;
  approved: number;
  in_trash: number;
  last_scan_at: number | null;
  groups: number;
  recoverable_bytes: number;
  photo_candidates: number;
  photo_candidate_bytes: number;
  document_candidates: number;
  document_candidate_bytes: number;
};

export type SimilarPhotosPage = {
  pairs: SimilarPhoto[];
  total: number;
  truncated: boolean;
};

export type BulkState = {
  running: boolean;
  kind: string;
  done: number;
  total: number;
  message: string;
};

export type TrashItem = {
  id: number;
  created_at: number;
  source_path: string;
  trash_path: string;
  expired: boolean;
  /** Shared id of one bulk user action; null for single-file removals. */
  batch_id: number | null;
};

export type HistoryItem = {
  id: number;
  created_at: number;
  source_path: string;
  trash_path: string;
  state: "trashed" | "restored" | "deleted" | string;
  restored_at: number | null;
};

export type ProjectConfig = {
  trash_path: string;
  roots: string[];
  protect_rules: string[];
};

export type ProjectState = ProjectConfig & {
  database: string;
  exclude_rules: string[];
  min_file_size: number;
  trash_retention_days: number;
  auto_scan: boolean;
  strict_verify: boolean;
  usn_scan: boolean;
  watch_scan: boolean;
  similar_threshold: number;
};

export type ScanErrorSample = { path: string; error: string };

export type ThemeSetting = "system" | "light" | "dark";

export type ProtectPreview = { matched: number; examples: string[] };

export type ScanState = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  processed: number;
  total: number;
  message: string;
  current_path: string | null;
  /** Most recently started full hash + cumulative bytes hashed this scan. */
  hashing_path: string | null;
  hashing_bytes: number;
  errors_total: number;
  recent_errors: ScanErrorSample[];
};

export type SimilarPhoto = {
  first_path: string;
  second_path: string;
  distance: number;
  phash_distance: number;
  first_size: number;
  first_modified: number;
  second_size: number;
  second_modified: number;
  /** EXIF capture time of each side (unix seconds; 0 = unknown). */
  first_taken: number;
  second_taken: number;
};

/** Background pixel verification of the duplicate-photo candidates. */
export type DuplicateVerifyState = {
  running: boolean;
  done: number;
  total: number;
  same: number;
  different: number;
  results: {
    first: string;
    second: string;
    identical: boolean;
  }[];
};

export type SimilarDocument = {
  first_path: string;
  second_path: string;
  distance: number;
  first_size: number;
  first_modified: number;
  second_size: number;
  second_modified: number;
};

export type DetectorStatus = { name: string; available: boolean; detail: string };

export type ThumbnailCacheStats = { files: number; bytes: number };
