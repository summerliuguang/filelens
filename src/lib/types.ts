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
};

export type Group = { hash: string; size: number; files: GroupFile[] };

export type GroupsPage = { total: number; groups: Group[] };

export type GroupFilters = {
  search: string;
  minSize: number;
  sort: "size" | "members" | "path";
};

export type Toast = { id: number; kind: "ok" | "error"; text: string };

export type Status = {
  files: number;
  duplicates: number;
  approved: number;
  in_trash: number;
  last_scan_at: number | null;
  groups: number;
  recoverable_bytes: number;
};

export type TrashItem = {
  id: number;
  created_at: number;
  source_path: string;
  trash_path: string;
  expired: boolean;
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
};

export type ScanErrorSample = { path: string; error: string };

export type ScanState = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  processed: number;
  total: number;
  message: string;
  current_path: string | null;
  errors_total: number;
  recent_errors: ScanErrorSample[];
};

export type SimilarPhoto = {
  first_path: string;
  second_path: string;
  distance: number;
  first_size: number;
  first_modified: number;
  second_size: number;
  second_modified: number;
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
