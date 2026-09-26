import { useEffect, useMemo, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open, save } from "@tauri-apps/plugin-dialog";
import { ConfirmDialog, Empty, PreviewModal, SimilarCompareModal } from "./components";
import {
  LARGE_FILE_THRESHOLD,
  fileFolder,
  fileName,
  formatBytes,
  formatFileTime,
  friendlyError,
} from "../lib/format";
import type {
  DetectorStatus,
  Group,
  GroupFile,
  GroupFilters,
  GroupsPage,
  HistoryItem,
  Page,
  ProtectPreview,
  SimilarDocument,
  SimilarPhoto,
  SimilarPhotosPage,
  DuplicateVerifyState,
  Status,
  ThemeSetting,
  ThumbnailCacheStats,
  TrashItem,
  TrashUsage,
  ZeroByteFile,
} from "../lib/types";
import { HELP_FAQ, HELP_SECTIONS } from "../lib/help-content";

export function Overview({
  status,
  groups,
  selected,
  onNavigate,
  onOpenHelp,
}: {
  status: Status | null;
  groups: Group[];
  selected: number;
  onNavigate: (page: Page) => void;
  onOpenHelp: () => void;
}) {
  return (
    <>
      <section className="hero">
        <div>
          <span className="signal">
            {status?.last_scan_at
              ? `● 上次扫描：${new Date(status.last_scan_at * 1000).toLocaleString("zh-CN")}`
              : "● 等待首次扫描"}
          </span>
          <h2>
            让重复文件变得
            <br />
            <i>清晰、可控。</i>
          </h2>
          <p>
            FileLens 只处理经过完整 BLAKE3
            内容哈希确认的重复文件。任何文件移动前都会再次校验。
          </p>
          <div className="hero-actions">
            <button onClick={() => onNavigate("sources")}>管理扫描来源</button>
            <button className="secondary" onClick={onOpenHelp}>
              新手？查看使用帮助
            </button>
          </div>
        </div>
        <div className="hero-orbit">
          <strong>{status?.duplicates ?? "-"}</strong>
          <span>待审核副本</span>
          <small>{formatBytes(status?.recoverable_bytes ?? 0)} 可释放空间</small>
        </div>
      </section>
      <section className="metrics">
        <Metric
          value={status?.files ?? "-"}
          label="已索引文件"
          detail="本地数据库"
        />
        <Metric
          value={status?.duplicates ?? "-"}
          label="重复副本"
          detail="完整哈希确认"
        />
        <Metric
          value={status?.approved ?? selected}
          label="已确认处理"
          detail="等待移入回收站"
        />
        <Metric
          value={status?.in_trash ?? "-"}
          label="回收站文件"
          detail="可安全恢复"
        />
      </section>
      <section className="panel">
        <div className="section-head">
          <div>
            <h2>可清理空间</h2>
            <p>
              候选需要人工确认后才会释放；此处为逐文件估算，不含同一张照片被多对重复计入的部分。
            </p>
          </div>
        </div>
        <div className="metrics">
          <Metric
            value={formatBytes(status?.recoverable_bytes ?? 0)}
            label="精确重复"
            detail={`${status?.groups ?? 0} 个组（BLAKE3 确认）`}
          />
          <Metric
            value={formatBytes(status?.photo_candidate_bytes ?? 0)}
            label="照片候选"
            detail={`${status?.photo_candidates ?? 0} 张（重复/相似，人工确认）`}
          />
          <Metric
            value={formatBytes(status?.document_candidate_bytes ?? 0)}
            label="文档候选"
            detail={`${status?.document_candidates ?? 0} 个（近似文本，人工确认）`}
          />
        </div>
      </section>
      <section className="panel action-panel">
        <div>
          <p className="eyebrow">NEXT ACTION</p>
          <h2>
            {groups.length
              ? "从重复组中选择要处理的副本"
              : "添加目录并开始首次扫描"}
          </h2>
          <p>
            {groups.length
              ? "FileLens 从不自动删除，逐项确认后才会允许移动。"
              : "扫描只读取文件并建立索引，不会修改你的任何数据。"}
          </p>
        </div>
        <button
          onClick={() => onNavigate(groups.length ? "review" : "sources")}
        >
          {groups.length ? "打开审核队列" : "添加扫描目录"}
        </button>
      </section>
    </>
  );
}

function Metric({
  value,
  label,
  detail,
}: {
  value: number | string;
  label: string;
  detail: string;
}) {
  return (
    <article className="metric">
      <strong>{value}</strong>
      <span>{label}</span>
      <small>{detail}</small>
    </article>
  );
}

export function Sources({
  roots,
  rootInput,
  setRootInput,
  addRoot,
  removeRoot,
  scan,
  disabled,
  onOpenHelp,
}: {
  roots: string[];
  rootInput: string;
  setRootInput: (value: string) => void;
  addRoot: (value: string) => boolean;
  removeRoot: (root: string) => void;
  scan: () => void;
  disabled: boolean;
  onOpenHelp: () => void;
}) {
  const [pendingRemoveRoot, setPendingRemoveRoot] = useState<string | null>(null);
  async function pickDirectory() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择扫描目录",
    });
    if (typeof selected === "string") addRoot(selected);
  }
  function submitInput() {
    if (addRoot(rootInput)) setRootInput("");
  }
  return (
    <section className="panel sources-page">
      <div className="section-head">
        <div>
          <h2>本次扫描范围</h2>
          <p>多个根目录会全局互相比较。应用回收站和常见缓存目录会自动排除。</p>
        </div>
        <button disabled={disabled || !roots.length} onClick={scan}>
          开始增量扫描
        </button>
      </div>
      <div className="add-source">
        <input
          value={rootInput}
          onChange={(event) => setRootInput(event.target.value)}
          onKeyDown={(event) => event.key === "Enter" && submitInput()}
          placeholder="输入目录路径，或点击右侧按钮选择"
        />
        <button className="secondary" onClick={pickDirectory}>
          选择目录
        </button>
        <button className="secondary" onClick={submitInput}>
          添加
        </button>
      </div>
      <div className="source-list">
        {roots.length === 0 ? (
          <div className="onboarding">
            <p className="onboarding-title">三步开始清理重复文件</p>
            <ol className="onboarding-steps">
              <li>
                <b>添加扫描目录</b>
                <span>点击下方按钮选择文件夹，或把路径粘贴进输入框。</span>
              </li>
              <li>
                <b>开始增量扫描</b>
                <span>
                  点击右上角「开始增量扫描」；大文件先只比对指纹，不会长时间卡住。
                </span>
              </li>
              <li>
                <b>审核并清理</b>
                <span>
                  在「重复审核」里逐组确认，移入应用回收站的内容随时可整批恢复。
                </span>
              </li>
            </ol>
            <div className="onboarding-actions">
              <button onClick={() => void pickDirectory()}>选择第一个目录</button>
              <button className="text-button" onClick={onOpenHelp}>
                查看使用帮助
              </button>
            </div>
          </div>
        ) : (
          roots.map((root, index) => (
            <article className="source" key={root}>
              <span className="source-icon">{index + 1}</span>
              <div>
                <b>{root}</b>
                <small>本地来源 · 全局比较已启用</small>
              </div>
              <button
                className="text-button"
                onClick={() => setPendingRemoveRoot(root)}
              >
                移除
              </button>
            </article>
          ))
        )}
      </div>
      {pendingRemoveRoot && (
        <ConfirmDialog
          title="移除扫描目录"
          detail={
            <>
              {pendingRemoveRoot}
              <br />
              <small>
                该目录不再参与后续扫描，已索引的记录会保留；其中的文件不会受到任何影响。
              </small>
            </>
          }
          busy={false}
          onClose={() => setPendingRemoveRoot(null)}
          options={[
            {
              label: "移除",
              kind: "danger",
              action: () => {
                const root = pendingRemoveRoot;
                setPendingRemoveRoot(null);
                removeRoot(root);
              },
            },
          ]}
        />
      )}
    </section>
  );
}

export function Review({
  database,
  groups,
  totalGroups,
  filters,
  onFilters,
  busy,
  busyKeys,
  execute,
  refresh,
  hasMore,
  onLoadMore,
  indexedFiles,
  onNavigate,
  scanning,
  active,
  goTrash,
}: {
  database: string;
  groups: Group[];
  totalGroups: number;
  filters: GroupFilters;
  onFilters: (next: GroupFilters) => void;
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (
    action: () => Promise<string>,
    key?: string,
    toastAction?: { label: string; run: () => void },
  ) => Promise<void>;
  refresh: () => Promise<void>;
  hasMore: boolean;
  onLoadMore: () => Promise<void>;
  indexedFiles: number;
  onNavigate: (page: Page) => void;
  scanning: boolean;
  active: boolean;
  goTrash: () => void;
}) {
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  const [pendingRemove, setPendingRemove] = useState<GroupFile | null>(null);
  const [pendingBatch, setPendingBatch] = useState<Group | null>(null);
  const [searchInput, setSearchInput] = useState(filters.search);
  const [bulkStrategy, setBulkStrategy] = useState<
    "scored" | "newest" | "oldest" | "shortest"
  >("scored");
  const [cursor, setCursor] = useState(0);
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  const [groupDirs, setGroupDirs] = useState<{ dir: string; count: number }[]>(
    [],
  );
  const [pendingBulk, setPendingBulk] = useState<
    { kind: "mark" } | { kind: "recycle"; fileIds: number[] } | null
  >(null);

  const KIND_FILTERS: { value: string; label: string }[] = [
    { value: "all", label: "全部类型" },
    { value: "image", label: "图片" },
    { value: "video", label: "视频" },
    { value: "audio", label: "音频" },
    { value: "document", label: "文档" },
    { value: "archive", label: "压缩包" },
  ];

  // Directory filter options: parents of duplicated files, busiest first.
  useEffect(() => {
    if (!active || !database) return;
    invoke<{ dir: string; count: number }[]>("group_dirs", { database })
      .then(setGroupDirs)
      .catch(() => setGroupDirs([]));
  }, [active, database]);

  // Marked summary across the loaded page (advisory; bulk actions cover the
  // full filtered set).
  const marked = useMemo(() => {
    let count = 0;
    let bytes = 0;
    for (const group of groups) {
      for (const file of group.files) {
        if (file.approved) {
          count += 1;
          bytes += group.size;
        }
      }
    }
    return { count, bytes };
  }, [groups]);

  // Flattened file rows power keyboard navigation (J/K move, Space marks,
  // Enter opens the group dialog).
  const rows = useMemo(
    () => groups.flatMap((group) => group.files.map((file) => ({ group, file }))),
    [groups],
  );
  const rowIndexById = useMemo(
    () => new Map(rows.map((row, index) => [row.file.id, index])),
    [rows],
  );
  useEffect(() => {
    if (!active) return;
    const onKey = (event: KeyboardEvent) => {
      const target = event.target as HTMLElement | null;
      if (
        target instanceof HTMLInputElement ||
        target instanceof HTMLSelectElement ||
        target instanceof HTMLTextAreaElement
      ) {
        return;
      }
      const row = rows[cursor];
      if (event.key === "j" || event.key === "ArrowDown") {
        event.preventDefault();
        setCursor((current) => Math.min(current + 1, Math.max(rows.length - 1, 0)));
      } else if (event.key === "k" || event.key === "ArrowUp") {
        event.preventDefault();
        setCursor((current) => Math.max(current - 1, 0));
      } else if (event.key === " " && row) {
        event.preventDefault();
        execute(
          async () => {
            if (row.file.approved) {
              await invoke("unapprove", { database, fileId: row.file.id });
              await refresh();
              return "已取消删除标记，该副本将保留。";
            }
            if (row.file.protected) return "受保护路径无法标记。";
            await invoke("approve", { database, fileId: row.file.id });
            await refresh();
            return "已标记删除该副本，确认后才会执行。";
          },
          "cursor-mark",
        );
      } else if (event.key === "Enter" && row) {
        const marked = row.group.files.filter((file) => file.approved).length;
        if (marked > 0) setPendingBatch(row.group);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, cursor, rows, database, execute, refresh]);
  useEffect(() => {
    document
      .querySelector(`[data-row-index="${cursor}"]`)
      ?.scrollIntoView({ block: "nearest" });
  }, [cursor]);

  // Page through every group matching the current filters, not just the
  // loaded slice, so bulk actions cover the full result set.
  async function collectAllMatching(): Promise<Group[]> {
    const all: Group[] = [];
    let offset = 0;
    for (;;) {
      const page = await invoke<GroupsPage>("groups", {
        database,
        offset,
        limit: 200,
        minSize: filters.minSize,
        pathContains: filters.search,
        sort: filters.sort,
        kind: filters.kind,
        dirContains: filters.dir,
      });
      all.push(...page.groups);
      if (page.groups.length === 0 || all.length >= page.total) break;
      offset += 200;
    }
    return all;
  }

  const bulkStrategyLabel =
    bulkStrategy === "scored"
      ? "智能评分"
      : bulkStrategy === "newest"
        ? "保留最新"
        : bulkStrategy === "oldest"
          ? "保留最旧"
          : "保留最短路径";

  // Mark every duplicate of the "keeper" for later batch processing; pure
  // front-end choice over data the group already carries.
  function smartMark(group: Group, strategy: "newest" | "oldest" | "shortest") {
    const pool = group.files.filter((file) => !file.protected);
    if (pool.length < 2) return;
    let keeper = pool[0];
    for (const file of pool) {
      if (strategy === "newest" && file.modified > keeper.modified) keeper = file;
      if (strategy === "oldest" && file.modified < keeper.modified) keeper = file;
      if (strategy === "shortest" && file.path.length < keeper.path.length) keeper = file;
    }
    const targets = pool.filter((file) => file.id !== keeper.id && !file.approved);
    if (targets.length === 0) return;
    const label =
      strategy === "newest" ? "保留最新" : strategy === "oldest" ? "保留最旧" : "保留最短路径";
    void execute(async () => {
      for (const file of targets) {
        await invoke("approve", { database, fileId: file.id });
      }
      await refresh();
      return `已按「${label}」标记 ${targets.length} 个副本，确认后可整组处理。`;
    }, "smart");
  }

  // mode: "trash" recycles (recoverable), "delete" removes permanently.
  function confirmRemoval(file: GroupFile, mode: "trash" | "delete") {
    void execute(async () => {
      const result =
        mode === "trash"
          ? await invoke<string>("trash", { database, fileId: file.id })
          : await invoke<string>("delete_direct", { database, fileId: file.id });
      await refresh();
      return result;
    }, "process");
  }

  function confirmBatch(target: Group, mode: "trash" | "delete") {
    void execute(
      async () => {
        const fileIds = target.files
          .filter((file) => file.approved)
          .map((file) => file.id);
        return await invoke<string>("start_bulk", {
          database,
          kind: mode === "trash" ? "trash" : "delete",
          fileIds,
        });
      },
      "batch",
      mode === "trash" ? { label: "打开回收站", run: goTrash } : undefined,
    );
  }

  // Replace the marked duplicates with hard links to the kept copy: all
  // paths survive, the duplicated content is freed right away.
  function hardlinkBatch(target: Group) {
    void execute(async () => {
      const fileIds = target.files
        .filter((file) => file.approved)
        .map((file) => file.id);
      return await invoke<string>("start_bulk", {
        database,
        kind: "hardlink",
        fileIds,
      });
    }, "hardlink");
  }

  async function exportReport() {
    const target = await save({
      title: "导出重复文件报告",
      defaultPath: "filelens-report.csv",
      filters: [{ name: "CSV", extensions: ["csv"] }],
    });
    if (!target) return;
    await execute(
      () => invoke<string>("export_report", { database, path: target }),
      "export",
    );
  }

  return (
    <section className="panel review-page">
      <div className="section-head">
        <div>
          <h2>精确重复审核</h2>
          <p>
            仅显示完整 BLAKE3 哈希一致的文件。每组至少保留一份：<b>勾选的副本 = 要删除的副本</b>
            ，先标记再「移入回收站」，随时可从应用回收站恢复。键盘：J / K 选择行，空格标记或取消，Enter
            打开本组处理。
          </p>
        </div>
        <span className="pill">{totalGroups} 个组</span>
      </div>
      {totalGroups > 0 && (
        <div className="report-bar">
          <button
            className="secondary"
            disabled={busyKeys.has("export")}
            onClick={() => void exportReport()}
          >
            导出报告（CSV）
          </button>
        </div>
      )}
      <div className="filter-bar">
        <input
          value={searchInput}
          onChange={(event) => setSearchInput(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === "Enter") onFilters({ ...filters, search: searchInput });
          }}
          placeholder="按路径关键字筛选，回车应用"
          aria-label="按路径筛选重复组"
        />
        <select
          value={String(filters.minSize)}
          onChange={(event) =>
            onFilters({ ...filters, minSize: Number(event.target.value) })
          }
          aria-label="最小文件大小"
        >
          <option value="0">任意大小</option>
          <option value={1024 * 1024}>≥ 1 MB</option>
          <option value={10 * 1024 * 1024}>≥ 10 MB</option>
          <option value={100 * 1024 * 1024}>≥ 100 MB</option>
          <option value={1024 * 1024 * 1024}>≥ 1 GB</option>
        </select>
        <select
          value={filters.sort}
          onChange={(event) =>
            onFilters({ ...filters, sort: event.target.value as GroupFilters["sort"] })
          }
          aria-label="排序方式"
        >
          <option value="recoverable">按可释放空间</option>
          <option value="size">按文件大小</option>
          <option value="members">按副本数</option>
          <option value="path">按路径</option>
        </select>
        <select
          value={filters.kind}
          onChange={(event) =>
            onFilters({ ...filters, kind: event.target.value })
          }
          aria-label="按文件类型筛选"
        >
          {KIND_FILTERS.map((filter) => (
            <option key={filter.value} value={filter.value}>
              {filter.label}
            </option>
          ))}
        </select>
        {groupDirs.length > 1 && (
          <select
            value={filters.dir}
            onChange={(event) =>
              onFilters({ ...filters, dir: event.target.value })
            }
            aria-label="按目录筛选"
          >
            <option value="all">全部目录</option>
            {groupDirs.map((entry) => (
              <option key={entry.dir} value={entry.dir}>
                {entry.dir}（{entry.count}）
              </option>
            ))}
          </select>
        )}
        {marked.count > 0 && (
          <span className="pill">
            已标记 {marked.count} 个 · {formatBytes(marked.bytes)}
          </span>
        )}
        {totalGroups > 0 && (
          <>
            <select
              value={bulkStrategy}
              onChange={(event) =>
                setBulkStrategy(event.target.value as typeof bulkStrategy)
              }
              aria-label="批量保留策略"
            >
              <option value="scored">每组智能评分</option>
              <option value="newest">每组保留最新</option>
              <option value="oldest">每组保留最旧</option>
              <option value="shortest">每组保留最短路径</option>
            </select>
            <button
              className="secondary"
              disabled={busy}
              onClick={() => setPendingBulk({ kind: "mark" })}
            >
              全部组智能标记
            </button>
            <button
              className="secondary"
              disabled={busy}
              onClick={() => {
                void (async () => {
                  const all = await collectAllMatching();
                  const fileIds = all.flatMap((group) =>
                    group.files
                      .filter((file) => file.approved && !file.protected)
                      .map((file) => file.id),
                  );
                  setPendingBulk({ kind: "recycle", fileIds });
                })();
              }}
            >
              回收全部已标记
            </button>
          </>
        )}
        {(filters.search ||
          filters.minSize > 0 ||
          filters.sort !== "recoverable" ||
          filters.kind !== "all" ||
          filters.dir !== "all") && (
          <button
            className="text-button"
            onClick={() => {
              setSearchInput("");
              onFilters({
                search: "",
                minSize: 0,
                sort: "recoverable",
                kind: "all",
                dir: "all",
              });
            }}
          >
            重置筛选
          </button>
        )}
      </div>
      {groups.length === 0 ? (
        <Empty
          icon="⊞"
          text={
            scanning
              ? "扫描进行中"
              : totalGroups === 0
                ? "没有待审核的精确重复"
                : "当前筛选没有匹配的重复组"
          }
          detail={
            scanning
              ? "正在扫描与更新索引，完成后结果会自动出现在这里。"
              : totalGroups === 0
                ? indexedFiles === 0
                  ? "添加扫描目录并完成首次扫描后，重复组会显示在这里。"
                  : "完成扫描后，重复组会按可释放空间显示在这里。"
                : "试试放宽大小阈值或更换关键字。"
          }
          action={
            !scanning && totalGroups === 0 && indexedFiles === 0
              ? { label: "去添加扫描目录", onClick: () => onNavigate("sources") }
              : undefined
          }
        />
      ) : (
        <div className="groups">
          {groups.map((group, index) => {
            const isCollapsed = collapsed.has(group.hash);
            return (
            <article className="group" key={group.hash}>
              <div
                className="group-header"
                role="button"
                tabIndex={0}
                title={isCollapsed ? "展开本组" : "折叠本组"}
                onClick={() =>
                  setCollapsed((current) => {
                    const next = new Set(current);
                    if (next.has(group.hash)) next.delete(group.hash);
                    else next.add(group.hash);
                    return next;
                  })
                }
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    setCollapsed((current) => {
                      const next = new Set(current);
                      if (next.has(group.hash)) next.delete(group.hash);
                      else next.add(group.hash);
                      return next;
                    });
                  }
                }}
              >
                <div>
                  <span>
                    {isCollapsed ? "▸" : "▾"} 重复组{" "}
                    {String(index + 1).padStart(2, "0")}
                  </span>
                  <h3>{group.files.length} 个内容相同的文件</h3>
                </div>
                <div>
                  <b>{formatBytes(group.recoverable)}</b>
                  <small>
                    {group.recoverable === 0
                      ? "互为硬链接，释放 0"
                      : "本组可释放"}
                  </small>
                </div>
              </div>
              {!isCollapsed && (
                <>
              <div className="hash">BLAKE3 {group.hash}</div>
              {group.files.filter((file) => file.hardlinked).length >= 2 && (
                <div className="hardlink-note">
                  本组包含互为硬链接的文件名（同一物理文件的多个名字）：删除其中一个副本
                  <b>不会释放磁盘空间</b>，恢复时也只会还原一个名字。
                </div>
              )}
              {group.files.filter((file) => !file.protected).length > 1 && (
                <div className="smart-row">
                  <span>智能标记：</span>
                  <button
                    className="text-button"
                    disabled={busyKeys.has("smart")}
                    onClick={() => smartMark(group, "newest")}
                  >
                    保留最新
                  </button>
                  <button
                    className="text-button"
                    disabled={busyKeys.has("smart")}
                    onClick={() => smartMark(group, "oldest")}
                  >
                    保留最旧
                  </button>
                  <button
                    className="text-button"
                    disabled={busyKeys.has("smart")}
                    onClick={() => smartMark(group, "shortest")}
                  >
                    保留最短路径
                  </button>
                </div>
              )}
              {group.files.map((file) => {
                const rowIndex = rowIndexById.get(file.id);
                return (
                <div
                  className={rowIndex === cursor ? "file-row cursor" : "file-row"}
                  data-row-index={rowIndex}
                  key={file.id}
                >
                  <button
                    className="thumbnail-button"
                    title="点击放大预览"
                    onClick={() => setPreviewPath(file.path)}
                  >
                    <Thumbnail path={file.path} />
                  </button>
                  <div className="file-path">
                    <b>{fileName(file.path)}</b>
                    <span title={file.path}>{file.path}</span>
                    <small className="file-meta">
                      修改于 {formatFileTime(file.modified)} ·{" "}
                      {formatBytes(group.size)} · 所在位置 {fileFolder(file.path)}
                    </small>
                    {file.suggested && !file.approved && (
                      <span className="keep-pill">建议保留此副本</span>
                    )}
                  </div>
                  {file.protected && <span className="protected">受保护</span>}
                  {file.hardlinked && <span className="hardlinked">硬链接</span>}
                  <div className="file-actions">
                    {file.approved ? (
                      <>
                        <span className="approved">待删除</span>
                        <button
                          className="secondary"
                          disabled={busyKeys.has("unapprove")}
                          onClick={() =>
                            execute(async () => {
                              const result = await invoke<string>("unapprove", {
                                database,
                                fileId: file.id,
                              });
                              await refresh();
                              return "已取消删除标记，该副本将保留。";
                            }, "unapprove")
                          }
                        >
                          取消标记
                        </button>
                        <button
                          className="danger"
                          disabled={busy}
                          onClick={() => setPendingRemove(file)}
                        >
                          删除此副本
                        </button>
                      </>
                    ) : (
                      <button
                        className="secondary"
                        disabled={busyKeys.has("approve") || file.protected}
                        onClick={() =>
                          execute(async () => {
                            const result = await invoke<string>("approve", {
                              database,
                              fileId: file.id,
                            });
                            await refresh();
                            return "已标记删除该副本，确认后才会执行。";
                          }, "approve")
                        }
                      >
                        标记删除
                      </button>
                    )}
                  </div>
                </div>
                );
              })}
              {group.files.some((file) => file.approved) && (
                <div className="group-batch">
                  <button
                    className="danger"
                    disabled={busy}
                    onClick={() => setPendingBatch(group)}
                  >
                    处理本组已标记副本（{group.files.filter((f) => f.approved).length} 个）
                  </button>
                </div>
              )}
                </>
              )}
            </article>
            );
          })}
          {hasMore && (
            <div className="load-more">
              <button
                className="secondary"
                disabled={busy}
                onClick={() => void onLoadMore()}
              >
                加载更多重复组（还有 {Math.max(0, totalGroups - groups.length)} 组）
              </button>
            </div>
          )}
        </div>
      )}
      {previewPath && (
        <PreviewModal path={previewPath} onClose={() => setPreviewPath(null)} />
      )}
      {pendingRemove && (
        <ConfirmDialog
          title="选择删除方式"
          detail={
            <>
              {fileName(pendingRemove.path)} ·{" "}
              {formatBytes(groupSizeOf(pendingRemove, groups))}
            </>
          }
          note={
            groupSizeOf(pendingRemove, groups) >= LARGE_FILE_THRESHOLD
              ? "该文件超过 1 GB：移入回收站会先复制一份（耗时且占双倍空间），大文件建议直接删除。"
              : undefined
          }
          busy={busy}
          onClose={() => setPendingRemove(null)}
          options={[
            {
              label: "移入回收站（可恢复）",
              action: () => {
                const file = pendingRemove;
                setPendingRemove(null);
                confirmRemoval(file, "trash");
              },
            },
            {
              label: "直接永久删除",
              kind: "danger",
              action: () => {
                const file = pendingRemove;
                setPendingRemove(null);
                confirmRemoval(file, "delete");
              },
            },
          ]}
        />
      )}
      {pendingBatch && (
        <ConfirmDialog
          title={`处理 ${pendingBatch.files.filter((f) => f.approved).length} 个已标记副本`}
          detail={
            <>
              每个副本 {formatBytes(pendingBatch.size)}，共可释放{" "}
              {formatBytes(
                pendingBatch.size * pendingBatch.files.filter((f) => f.approved).length,
              )}
              。
            </>
          }
          note={
            pendingBatch.size >= LARGE_FILE_THRESHOLD
              ? "该组包含超过 1 GB 的大文件：移入回收站会先复制一份（耗时且占双倍空间），大文件建议直接删除。"
              : undefined
          }
          busy={busy}
          onClose={() => setPendingBatch(null)}
          options={[
            {
              label: "全部移入回收站（可恢复）",
              action: () => {
                const target = pendingBatch;
                setPendingBatch(null);
                confirmBatch(target, "trash");
              },
            },
            {
              label: "转为硬链接（无损去重）",
              action: () => {
                const target = pendingBatch;
                setPendingBatch(null);
                hardlinkBatch(target);
              },
            },
            {
              label: "全部直接永久删除",
              kind: "danger",
              action: () => {
                const target = pendingBatch;
                setPendingBatch(null);
                confirmBatch(target, "delete");
              },
            },
          ]}
        />
      )}
      {pendingBulk && (
        <ConfirmDialog
          title={
            pendingBulk.kind === "mark"
              ? "对筛选命中的全部重复组智能标记"
              : pendingBulk.fileIds.length > 0
                ? `移入回收站 ${pendingBulk.fileIds.length} 个已标记副本`
                : "没有可回收的副本"
          }
          detail={
            pendingBulk.kind === "mark" ? (
              <>
                将按「{bulkStrategyLabel}」在筛选命中的每一组（共{" "}
                {totalGroups} 组）保留一个副本，其余未受保护的副本全部标记为待处理。
                受保护路径永远不会被标记；标记后可在各组内核对，再用「回收全部已标记」统一处理。
              </>
            ) : pendingBulk.fileIds.length > 0 ? (
              <>
                这些副本将作为一个批次移入应用回收站，随时可整批恢复；每个文件移动前都会再次校验内容。
              </>
            ) : (
              <>当前筛选结果里没有已标记的副本。可先用「全部组智能标记」批量标记。</>
            )
          }
          busy={busy}
          onClose={() => setPendingBulk(null)}
          options={
            pendingBulk.kind === "recycle" && pendingBulk.fileIds.length === 0
              ? [{ label: "知道了", action: () => setPendingBulk(null) }]
              : [
                  {
                    label: pendingBulk.kind === "mark" ? "开始标记" : "移入回收站",
                    kind: pendingBulk.kind === "mark" ? undefined : "danger",
                    action: () => {
                      const current = pendingBulk;
                      setPendingBulk(null);
                      void execute(async () => {
                        if (current.kind === "mark") {
                          const result = await invoke<string>("approve_filtered", {
                            database,
                            strategy: bulkStrategy,
                            minSize: filters.minSize,
                            pathContains: filters.search,
                            kind: filters.kind,
                            dirContains: filters.dir,
                          });
                          await refresh();
                          return result;
                        }
                        const result = await invoke<string>("start_bulk", {
                          database,
                          kind: "trash",
                          fileIds: current.fileIds,
                        });
                        return result;
                      }, "bulk", { label: "打开回收站", run: goTrash });
                    },
                  },
                ]
          }
        />
      )}
    </section>
  );
}

function groupSizeOf(file: GroupFile, groups: Group[]) {
  const group = groups.find((candidate) =>
    candidate.files.some((member) => member.id === file.id),
  );
  return group?.size ?? 0;
}

function Thumbnail({ path }: { path: string }) {
  const [host, setHost] = useState<HTMLDivElement | null>(null);
  const [image, setImage] = useState<string | null>(null);
  // Lazy load: decode only when the row scrolls near the viewport, so a long
  // queue does not fire hundreds of image decodes at once. The wrapper must
  // generate a real box (no display:contents) — IntersectionObserver cannot
  // observe box-less elements and would never fire.
  useEffect(() => {
    setImage(null);
    if (!host) return;
    let cancelled = false;
    const load = () => {
      if (cancelled) return;
      invoke<string | null>("image_thumbnail", { path })
        .then((data) => {
          if (!cancelled) setImage(data);
        })
        .catch(() => {
          if (!cancelled) setImage(null);
        });
    };
    if (typeof IntersectionObserver === "undefined") {
      load();
      return () => {
        cancelled = true;
      };
    }
    const observer = new IntersectionObserver(
      (entries) => {
        if (entries.some((entry) => entry.isIntersecting)) {
          observer.disconnect();
          load();
        }
      },
      { rootMargin: "300px" },
    );
    observer.observe(host);
    return () => {
      cancelled = true;
      observer.disconnect();
    };
  }, [path, host]);
  return (
    <div
      ref={setHost}
      style={{ display: "flex", alignItems: "center", justifyContent: "center" }}
    >
      {image ? (
        <img className="thumbnail" src={image} alt="文件缩略图" />
      ) : (
        <span className="thumbnail thumbnail-placeholder">▧</span>
      )}
    </div>
  );
}

export function SimilarPhotos({
  database,
  photosPage,
  duplicatesPage,
  duplicateVerify,
  scanning,
  busy,
  busyKeys,
  execute,
  refresh,
}: {
  database: string;
  photosPage: SimilarPhotosPage;
  duplicatesPage: SimilarPhotosPage;
  /** Background per-pair pixel verification of the duplicate candidates. */
  duplicateVerify: DuplicateVerifyState | null;
  scanning: boolean;
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const photos = photosPage.pairs;
  const duplicates = duplicatesPage.pairs;
  const truncated = photosPage.truncated || duplicatesPage.truncated;
  // Fingerprints only nominate candidates; the duplicate view shows a pair
  // solely after the background pass confirmed its pixels are identical.
  const verifyMap = new Map(
    (duplicateVerify?.results ?? []).map((result) => [
      `${result.first}|${result.second}`,
      result.identical,
    ]),
  );
  const confirmedDuplicates = duplicates.filter(
    (photo) => verifyMap.get(`${photo.first_path}|${photo.second_path}`) === true,
  );
  const [visible, setVisible] = useState(60);
  const [comparePair, setComparePair] = useState<SimilarPhoto | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [pendingDelete, setPendingDelete] = useState(false);
  const [view, setView] = useState<"duplicate" | "similar">("similar");
  const [mode, setMode] = useState<"image" | "directory">("image");
  const [dirFilter, setDirFilter] = useState("all");
  const [pendingDirTrash, setPendingDirTrash] = useState<{
    dir: string;
    paths: string[];
    /** Pairs whose BOTH sides live in `dir`: recycling all of them would
     * erase the last copy, so the dialog offers a keep-one flow. */
    sameDirPairs: SimilarPhoto[];
  } | null>(null);
  const [keepDir, setKeepDir] = useState<string | null>(null);
  const [pendingMerge, setPendingMerge] = useState<{
    dirA: string;
    dirB: string;
    pairs: SimilarPhoto[];
    target: { kind: "a" | "b" | "custom"; custom: string };
    conditions: MergeCondition[];
  } | null>(null);
  const [mergePrefixInput, setMergePrefixInput] = useState("");
  // Natural pixel sizes per path for the merge wizard's resolution
  // condition, probed once per dialog open.
  const [mergeDims, setMergeDims] = useState<Record<string, [number, number]>>({});
  const [showConsistentOnly, setShowConsistentOnly] = useState(false);
  const [pairSort, setPairSort] = useState<"distance" | "size" | "time">(
    "distance",
  );
  const [dirGroupSort, setDirGroupSort] = useState<"pairs" | "bytes">("pairs");

  // Short two-segment name for group headers; the full path stays in title.
  function shortDir(dir: string) {
    const parts = dir.split(/[\\/]/).filter(Boolean);
    return parts.length > 2 ? `…/${parts.slice(-2).join("/")}` : parts.join("/");
  }

  // duplicate: identical pixels, differing bytes (a copy with drifted EXIF
  // or metadata). similar: near-identical pixels, dHash distance 1-4.
  const base = view === "duplicate" ? confirmedDuplicates : photos;
  const verifyRunning =
    view === "duplicate" && (duplicateVerify?.running ?? false);  const directoryView = view === "duplicate" && mode === "directory";

  function switchView(next: "duplicate" | "similar") {
    setView(next);
    setDirFilter("all");
    setVisible(60);
    setSelected(new Set());
  }

  function switchMode(next: "image" | "directory") {
    setMode(next);
    setVisible(60);
  }

  // Directories touched by any candidate photo, busiest first.
  const directories = [...new Set(base.flatMap((photo) => [fileFolder(photo.first_path), fileFolder(photo.second_path)]))]
    .map((dir) => ({
      dir,
      count: base.filter(
        (photo) =>
          fileFolder(photo.first_path) === dir ||
          fileFolder(photo.second_path) === dir,
      ).length,
    }))
    .sort((a, b) => b.count - a.count);

  const matched =
    dirFilter === "all"
      ? base
      : base.filter(
          (photo) =>
            fileFolder(photo.first_path) === dirFilter ||
            fileFolder(photo.second_path) === dirFilter,
        );
  const shown = (() => {
    const sorted = [...matched];
    if (pairSort === "size") {
      sorted.sort(
        (a, b) =>
          Math.max(b.first_size, b.second_size) -
          Math.max(a.first_size, a.second_size),
      );
    } else if (pairSort === "time") {
      sorted.sort(
        (a, b) =>
          Math.max(b.first_modified, b.second_modified) -
          Math.max(a.first_modified, a.second_modified),
      );
    }
    return sorted.slice(0, visible);
  })();

  // 重复目录模式：跨目录的重复照片按「目录对」分组，两侧照片对齐成两列；
  // 同一目录内部的对留在「重复图片」模式。
  const { dirGroups, sameDirPairs, capped } = (() => {
    if (!directoryView) {
      return { dirGroups: [] as DirPairGroup[], sameDirPairs: 0, capped: false };
    }
    const map = new Map<string, { dirA: string; dirB: string; pairs: SimilarPhoto[] }>();
    let sameDirPairs = 0;
    for (const photo of base) {
      const firstDir = fileFolder(photo.first_path);
      const secondDir = fileFolder(photo.second_path);
      if (firstDir === secondDir) {
        sameDirPairs += 1;
        continue;
      }
      const [dirA, dirB] =
        firstDir < secondDir ? [firstDir, secondDir] : [secondDir, firstDir];
      const key = `${dirA}\u0000${dirB}`;
      let group = map.get(key);
      if (!group) {
        group = { dirA, dirB, pairs: [] };
        map.set(key, group);
      }
      group.pairs.push(
        firstDir === dirA
          ? photo
          : {
              ...photo,
              first_path: photo.second_path,
              first_size: photo.second_size,
              first_modified: photo.second_modified,
              second_path: photo.first_path,
              second_size: photo.first_size,
              second_modified: photo.first_modified,
            },
      );
    }
    const all = [...map.values()].sort((a, b) => b.pairs.length - a.pairs.length);
    let budget = visible;
    const dirGroups: DirPairGroup[] = [];
    let capped = false;
    for (const group of all) {
      if (budget <= 0) {
        capped = true;
        break;
      }
      const pairs = group.pairs.slice(0, budget);
      if (pairs.length < group.pairs.length) capped = true;
      budget -= pairs.length;
      dirGroups.push({ dirA: group.dirA, dirB: group.dirB, pairs });
    }
    return { dirGroups, sameDirPairs, capped };
  })();

  const visibleDirGroups = (() => {
    let groups = showConsistentOnly
      ? dirGroups.filter((group) => groupConsistency(group.pairs).consistent)
      : [...dirGroups];
    const sideBytes = (group: DirPairGroup, side: "first" | "second") => {
      const seen = new Set<string>();
      let bytes = 0;
      for (const pair of group.pairs) {
        const path = side === "first" ? pair.first_path : pair.second_path;
        const size = side === "first" ? pair.first_size : pair.second_size;
        if (!seen.has(path)) {
          seen.add(path);
          bytes += Math.max(size, 0);
        }
      }
      return bytes;
    };
    if (dirGroupSort === "bytes") {
      groups.sort(
        (a, b) =>
          Math.max(sideBytes(b, "first"), sideBytes(b, "second")) -
          Math.max(sideBytes(a, "first"), sideBytes(a, "second")),
      );
    }
    return groups.map((group) => ({
      ...group,
      leftBytes: sideBytes(group, "first"),
      rightBytes: sideBytes(group, "second"),
    }));
  })();

  // 两侧各一张、无一对多关系时，目录内容结构才算完全一致。
  function groupConsistency(pairs: SimilarPhoto[]): {
    consistent: boolean;
    reason: string;
  } {
    const left = new Set(pairs.map((p) => p.first_path));
    const right = new Set(pairs.map((p) => p.second_path));
    if (pairs.length !== left.size || pairs.length !== right.size) {
      return { consistent: false, reason: "结构不一致：存在一对多" };
    }
    if (left.size !== right.size) {
      return { consistent: false, reason: "结构不一致：两侧数量不同" };
    }
    return { consistent: true, reason: "两目录内容结构一致" };
  }

  function askMergeGroup(group: DirPairGroup) {
    // Detect naming prefixes across BOTH sides, busiest first; known
    // camera/screenshot styles start checked.
    const counts = new Map<string, { label: string; count: number }>();
    const dimensionPaths = new Set<string>();
    for (const photo of group.pairs) {
      for (const path of [photo.first_path, photo.second_path]) {
        dimensionPaths.add(path);
        const prefix = fileNamePrefix(fileName(path));
        if (!prefix) continue;
        const key = prefix.toLowerCase();
        const entry = counts.get(key);
        if (entry) entry.count += 1;
        else counts.set(key, { label: prefix, count: 1 });
      }
    }
    // The resolution condition needs natural pixel sizes: probe the headers
    // once per dialog in the background and fill the cache as results land.
    setMergeDims({});
    for (const path of dimensionPaths) {
      invoke<[number, number] | null>("image_dimensions", { path })
        .then((dims) => {
          if (dims) {
            setMergeDims((current) => ({ ...current, [path]: dims }));
          }
        })
        .catch(() => {});
    }
    setPendingMerge({
      dirA: group.dirA,
      dirB: group.dirB,
      pairs: group.pairs,
      target: { kind: "a", custom: "" },
      conditions: [
        {
          kind: "prefix",
          enabled: true,
          prefixes: [...counts.entries()]
            .sort((a, b) => b[1].count - a[1].count)
            .map(([key, { label, count }]) => ({
              key,
              label,
              count,
              checked: MERGE_DEFAULT_PREFIXES.has(key),
            })),
        },
        { kind: "exif", enabled: true },
        { kind: "resolution", enabled: false },
        { kind: "time", enabled: true },
        { kind: "prefer", enabled: false, side: null },
      ],
    });
    setMergePrefixInput("");
  }

  function patchMergeConditions(
    update: (list: MergeCondition[]) => MergeCondition[],
  ) {
    setPendingMerge((current) =>
      current ? { ...current, conditions: update(current.conditions) } : current,
    );
  }

  function moveConditionOrder(from: number, to: number) {
    patchMergeConditions((list) => {
      if (to < 0 || to >= list.length) return list;
      const next = [...list];
      const [item] = next.splice(from, 1);
      next.splice(to, 0, item);
      return next;
    });
  }

  function setConditionEnabled(index: number, enabled: boolean) {
    patchMergeConditions((list) =>
      list.map((condition, i) => (i === index ? { ...condition, enabled } : condition)),
    );
  }

  function setPreferSide(index: number, side: "first" | "second" | null) {
    patchMergeConditions((list) =>
      list.map(
        (condition, i) =>
          condition.kind === "prefer" && i === index ? { ...condition, side } : condition,
      ),
    );
  }

  function toggleMergePrefix(key: string) {
    patchMergeConditions((list) =>
      list.map((condition) =>
        condition.kind === "prefix"
          ? {
              ...condition,
              prefixes: condition.prefixes.map((option) =>
                option.key === key
                  ? { ...option, checked: !option.checked }
                  : option,
              ),
            }
          : condition,
      ),
    );
  }

  function addMergePrefix(raw: string) {
    const prefix = raw.trim().replace(/[^\u4e00-\u9fa5A-Za-z]+$/, "");
    if (!prefix) return;
    const key = prefix.toLowerCase();
    patchMergeConditions((list) =>
      list.map((condition) => {
        if (condition.kind !== "prefix") return condition;
        if (condition.prefixes.some((option) => option.key === key)) {
          return {
            ...condition,
            enabled: true,
            prefixes: condition.prefixes.map((option) =>
              option.key === key ? { ...option, checked: true } : option,
            ),
          };
        }
        return {
          ...condition,
          enabled: true,
          prefixes: [
            ...condition.prefixes,
            { key, label: prefix, count: 0, checked: true },
          ],
        };
      }),
    );
  }

  function pickMergeTarget() {
    void (async () => {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "选择合并目标目录",
      });
      if (typeof selected === "string") {
        setPendingMerge((current) =>
          current
            ? { ...current, target: { kind: "custom", custom: selected } }
            : current,
        );
      }
    })();
  }

  function toggleSelection(path: string) {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  }

  function removeSelected(mode: "trash" | "delete") {
    const paths = [...selected];
    void execute(async () => {
      const command = mode === "trash" ? "trash_paths" : "delete_paths";
      const result = await invoke<string>(command, { database, paths });
      setSelected(new Set());
      await refresh();
      return result;
    }, "similar");
  }

  // Every candidate photo living inside the selected directory — a pair can
  // contribute one or both sides. Goes through trash_paths, so protection
  // rules and the hash re-check still apply to each file.
  function askRecycleDirectory(dir: string) {
    const sameDirPairs = base.filter(
      (photo) =>
        fileFolder(photo.first_path) === dir &&
        fileFolder(photo.second_path) === dir,
    );
    const paths = [
      ...new Set(
        matched
          .flatMap((photo) => [photo.first_path, photo.second_path])
          .filter((path) => fileFolder(path) === dir),
      ),
    ];
    if (paths.length > 0) {
      setKeepDir(null);
      setPendingDirTrash({ dir, paths, sameDirPairs });
    }
  }

  return (
    <section className="panel review-page">
      <div className="section-head">
        <div>
          <h2>{view === "duplicate" ? "重复照片" : "相似照片"}</h2>
          <p>
            {view === "duplicate"
              ? mode === "directory"
                ? "按目录对照显示跨目录的重复照片，两列互为副本；两个目录内容结构一致时，可整侧移除（走保护规则与内容校验，可在回收站恢复）。列表只包含逐像素校验确认完全一致的候选。"
                : "这里只显示逐像素校验确认完全一致的重复照片（字节不同、复制导致信息略异的副本）；指纹相近但像素不同的候选会被自动排除。"
              : "仅供人工查看。点击照片并排放大对比，勾选后可批量移入回收站或删除。"}
          </p>
        </div>
        <div className="head-actions">
          <div className="theme-choice" role="tablist" aria-label="照片检测视图">
            <button
              type="button"
              className={view === "duplicate" ? "on" : ""}
              onClick={() => switchView("duplicate")}
            >
              重复（{confirmedDuplicates.length} 对）
            </button>
            <button
              type="button"
              className={view === "similar" ? "on" : ""}
              onClick={() => switchView("similar")}
            >
              相似（{photos.length} 对）
            </button>
          </div>
          {view === "duplicate" && (
            <div className="theme-choice" role="tablist" aria-label="重复查看模式">
              <button
                type="button"
                className={mode === "image" ? "on" : ""}
                onClick={() => switchMode("image")}
              >
                重复图片
              </button>
              <button
                type="button"
                className={mode === "directory" ? "on" : ""}
                onClick={() => switchMode("directory")}
              >
                重复目录
              </button>
            </div>
          )}
          {view === "duplicate" && mode === "directory" && (
            <select
              value={dirGroupSort}
              onChange={(event) =>
                setDirGroupSort(event.target.value as typeof dirGroupSort)
              }
              aria-label="目录组排序"
            >
              <option value="pairs">按对数排序</option>
              <option value="bytes">按可释放空间</option>
            </select>
          )}
          {!directoryView && (
            <select
              value={pairSort}
              onChange={(event) =>
                setPairSort(event.target.value as typeof pairSort)
              }
              aria-label="配对排序"
            >
              <option value="distance">按相似程度</option>
              <option value="size">按文件大小</option>
              <option value="time">按修改时间</option>
            </select>
          )}
          {!directoryView && directories.length > 1 && (
            <select
              value={dirFilter}
              onChange={(event) => {
                setDirFilter(event.target.value);
                setVisible(60);
              }}
              aria-label="按目录筛选候选"
            >
              <option value="all">全部目录（{base.length} 对）</option>
              {directories.map(({ dir, count }) => (
                <option key={dir} value={dir}>
                  {dir}（{count} 对）
                </option>
              ))}
            </select>
          )}
          {!directoryView && dirFilter !== "all" && (
            <button
              className="secondary"
              disabled={busy}
              onClick={() => askRecycleDirectory(dirFilter)}
            >
              回收该目录全部候选
            </button>
          )}
          {selected.size > 0 && (
            <span className="pill">已选 {selected.size} 个</span>
          )}
          {selected.size > 0 && (
            <button
              className="danger"
              disabled={busyKeys.has("similar")}
              onClick={() => setPendingDelete(true)}
            >
              删除选中文件
            </button>
          )}
          <span className="pill">
            {directoryView
              ? `${visibleDirGroups.length} 组目录`
              : `${matched.length} 对`}
          </span>
          {directoryView && (
            <label className="dir-filter-toggle">
              <input
                type="checkbox"
                checked={showConsistentOnly}
                onChange={(event) => setShowConsistentOnly(event.target.checked)}
              />
              只看结构一致
            </label>
          )}
          {view === "duplicate" && duplicateVerify?.running && (
            <span
              className="pill"
              title="指纹候选只有近似意义，正在后台对每对候选做逐像素校验；确认完全一致后才会显示在列表中。"
            >
              逐像素校验中 {duplicateVerify.done}/{duplicateVerify.total}
            </span>
          )}
          {view === "duplicate" &&
            duplicateVerify &&
            !duplicateVerify.running &&
            duplicateVerify.different > 0 && (
              <span
                className="pill"
                title={`逐像素校验完成：${duplicateVerify.same} 对完全一致；${duplicateVerify.different} 对只是相似照片，已从重复视图排除。`}
              >
                已排除 {duplicateVerify.different} 对相似候选
              </span>
            )}
          {truncated && (
            <span
              className="pill"
              title="感知指纹相近的配对数量很大，仅保留距离最小的前 5000 对；按目录筛选或缩小扫描范围可获得完整结果。"
            >
              已截断至 5000 对
            </span>
          )}
        </div>
      </div>
      {base.length === 0 ? (
        <Empty
          icon="◒"
          text={
            scanning
              ? "扫描进行中"
              : view === "duplicate"
                ? duplicates.length === 0
                  ? "没有重复照片候选"
                  : verifyRunning
                    ? "正在逐像素校验候选…"
                    : "没有像素级完全一致的重复照片"
                : "没有高置信度相似照片"
          }
          detail={
            scanning
              ? "正在扫描与重建指纹，完成后结果会自动出现在这里。"
              : view === "duplicate"
                ? duplicates.length === 0
                  ? "完成扫描后，像素完全一致但字节不同的照片（复制导致信息略异）会显示在这里。"
                  : verifyRunning
                    ? `指纹候选共 ${duplicates.length} 对，已确认 ${confirmedDuplicates.length} 对完全一致；确认后立即显示，请稍候。`
                    : `共校验 ${duplicateVerify?.total ?? 0} 对指纹候选，其中 ${duplicateVerify?.different ?? 0} 对只是相似照片，已从本视图排除。`
                : "完成扫描后，这里会显示重压缩或缩放后的同源照片候选。"
          }
        />
      ) : directoryView ? (
        visibleDirGroups.length === 0 ? (
          <Empty
            icon="◒"
            text={
              showConsistentOnly && dirGroups.length > 0
                ? "没有结构一致的目录组"
                : "没有跨目录的重复照片"
            }
            detail={
              showConsistentOnly && dirGroups.length > 0
                ? "取消勾选「只看结构一致」可查看其余目录组。"
                : sameDirPairs > 0
                  ? `同一目录内还有 ${sameDirPairs} 对重复照片，切换到「重复图片」模式查看。`
                  : "两个不同目录间的重复照片会在这里按目录分组、两列对照显示。"
            }
          />
        ) : (
          <div className="dir-groups">
            {visibleDirGroups.map((group) => {
              const leftPaths = [...new Set(group.pairs.map((p) => p.first_path))];
              const rightPaths = [...new Set(group.pairs.map((p) => p.second_path))];
              const info = groupConsistency(group.pairs);
              return (
                <article
                  className="dir-group"
                  key={`${group.dirA}|${group.dirB}`}
                >
                  <div className="dir-group-head">
                    <span className="pill">{group.pairs.length} 对</span>
                    <span
                      className={info.consistent ? "keep-pill" : "pill"}
                      title={info.reason}
                    >
                      {info.reason}
                    </span>
                    <span className="dir-group-hint">
                      两侧互为副本；点「合并到一侧」按保留条件把它们集中到一个目录
                    </span>
                    <div className="dir-group-actions">
                      <button
                        className="secondary"
                        disabled={busy}
                        onClick={() => askMergeGroup(group)}
                      >
                        合并到一侧
                      </button>
                    </div>
                  </div>
                  <div className="dir-pair-cols">
                    <div className="dir-side-head">
                      <b title={group.dirA}>{shortDir(group.dirA)}</b>
                      <span className="dir-side-meta">
                        {leftPaths.length} 张 · {formatBytes(group.leftBytes)}
                      </span>
                    </div>
                    <div className="dir-side-head">
                      <b title={group.dirB}>{shortDir(group.dirB)}</b>
                      <span className="dir-side-meta">
                        {rightPaths.length} 张 · {formatBytes(group.rightBytes)}
                      </span>
                    </div>
                  </div>
                  <div className="dir-pair-list">
                    {group.pairs.map((photo) => (
                      <div
                        className="dir-pair-row"
                        key={`${photo.first_path}|${photo.second_path}`}
                      >
                        <div className="dir-slot">
                          <label className="photo-check">
                            <input
                              type="checkbox"
                              checked={selected.has(photo.first_path)}
                              onChange={() => toggleSelection(photo.first_path)}
                            />
                          </label>
                          <button
                            className="thumbnail-button"
                            title="点击并排放大对比"
                            onClick={() => setComparePair(photo)}
                          >
                            <Thumbnail path={photo.first_path} />
                          </button>
                          <PhotoInfo
                            path={photo.first_path}
                            size={photo.first_size}
                            modified={photo.first_modified}
                          />
                        </div>
                        <div className="dir-slot">
                          <label className="photo-check">
                            <input
                              type="checkbox"
                              checked={selected.has(photo.second_path)}
                              onChange={() => toggleSelection(photo.second_path)}
                            />
                          </label>
                          <button
                            className="thumbnail-button"
                            title="点击并排放大对比"
                            onClick={() => setComparePair(photo)}
                          >
                            <Thumbnail path={photo.second_path} />
                          </button>
                          <PhotoInfo
                            path={photo.second_path}
                            size={photo.second_size}
                            modified={photo.second_modified}
                          />
                        </div>
                      </div>
                    ))}
                  </div>
                </article>
              );
            })}
            {capped && (
              <div className="load-more">
                <button
                  className="secondary"
                  onClick={() => setVisible((count) => count + 120)}
                >
                  显示更多
                </button>
              </div>
            )}
          </div>
        )
      ) : (
        <div className="similar-list">
          {shown.map((photo, index) => {
            const keep = suggestKeep(photo);
            return (
              <article
                className="photo-pair"
                key={`${photo.first_path}-${photo.second_path}`}
              >
                <div className="similar-photos">
                  <PhotoSlot
                    photo={photo}
                    side="first"
                    onCompare={setComparePair}
                    selected={selected}
                    onToggle={toggleSelection}
                  />
                  <PhotoSlot
                    photo={photo}
                    side="second"
                    onCompare={setComparePair}
                    selected={selected}
                    onToggle={toggleSelection}
                  />
                </div>
                <div className="photo-pair-info">
                  <b>候选 {index + 1}</b>
                  {keep && (
                    <span className="keep-pill">
                      建议保留{keep.side === "first" ? "左" : "右"}图 · {keep.reason}
                    </span>
                  )}
                  <PhotoInfo path={photo.first_path} size={photo.first_size} modified={photo.first_modified} />
                  <PhotoInfo path={photo.second_path} size={photo.second_size} modified={photo.second_modified} />
                  <small>
                    {view === "duplicate"
                      ? "已逐像素校验：两张完全一致"
                      : `dHash 差异 ${photo.distance}/64 · pHash 差异 ${photo.phash_distance}/64`}
                  </small>
                </div>
              </article>
            );
          })}
          {visible < matched.length && (
            <div className="load-more">
              <button
                className="secondary"
                onClick={() => setVisible((count) => count + 120)}
              >
                显示更多（还有 {matched.length - visible} 对）
              </button>
            </div>
          )}
        </div>
      )}
      {comparePair && (
        <SimilarCompareModal
          photo={comparePair}
          expectIdentical={view === "duplicate"}
          onClose={() => setComparePair(null)}
        />
      )}
      {pendingDirTrash && (
        <ConfirmDialog
          title={`回收目录 ${pendingDirTrash.dir} 中的候选照片`}
          detail={
            <>
              将把该目录中出现的 {pendingDirTrash.paths.length} 张候选照片移入应用回收站（可恢复）；移动前逐张校验内容与保护规则。
              {pendingDirTrash.sameDirPairs.length > 0 && (
                <>
                  <br />
                  <small>
                    注意：其中 {pendingDirTrash.sameDirPairs.length} 对的两张都在该目录——全部移除会让这些照片失去最后一个副本。建议先选择一个保留目录，每对会先移一张过去保留。
                  </small>
                  <br />
                  {keepDir ? (
                    <b>保留目录：{keepDir}</b>
                  ) : (
                    <button
                      className="secondary"
                      onClick={() => {
                        void (async () => {
                          const selected = await open({
                            directory: true,
                            multiple: false,
                            title: "选择保留目录",
                          });
                          if (typeof selected === "string") setKeepDir(selected);
                        })();
                      }}
                    >
                      选择保留目录
                    </button>
                  )}
                </>
              )}
            </>
          }
          busy={busy}
          onClose={() => {
            setPendingDirTrash(null);
            setKeepDir(null);
          }}
          options={
            pendingDirTrash.sameDirPairs.length > 0
              ? [
                  ...(keepDir
                    ? [
                        {
                          label: "移入回收站（同目录对保留一张）",
                          action: () => {
                            const { dir } = pendingDirTrash;
                            const keep = keepDir;
                            setPendingDirTrash(null);
                            setKeepDir(null);
                            void execute(async () => {
                              const result = await invoke<string>(
                                "recycle_dir_keep_one",
                                { database, dir, keepDir: keep },
                              );
                              await refresh();
                              return result;
                            }, "similar");
                          },
                        },
                      ]
                    : []),
                  {
                    label: "全部移入回收站（部分对将无副本）",
                    kind: "danger" as const,
                    action: () => {
                      const { dir, paths } = pendingDirTrash;
                      setPendingDirTrash(null);
                      void execute(async () => {
                        const result = await invoke<string>("trash_paths", {
                          database,
                          paths,
                        });
                        await refresh();
                        return result;
                      }, "similar");
                    },
                  },
                  {
                    label: "先不处理",
                    action: () => setPendingDirTrash(null),
                  },
                ]
              : [
                  {
                    label: "移入回收站",
                    action: () => {
                      const { dir, paths } = pendingDirTrash;
                      setPendingDirTrash(null);
                      void execute(async () => {
                        const result = await invoke<string>("trash_paths", {
                          database,
                          paths,
                        });
                        await refresh();
                        return result;
                      }, "similar");
                    },
                  },
                ]
          }
        />
      )}
      {pendingMerge &&
        (() => {
          const targetDir =
            pendingMerge.target.kind === "a"
              ? pendingMerge.dirA
              : pendingMerge.target.kind === "b"
                ? pendingMerge.dirB
                : pendingMerge.target.custom.trim();
          const decisions = pendingMerge.pairs.map((photo) => ({
            photo,
            winner: decideMergeWinner(
              photo,
              pendingMerge.conditions,
              targetDir,
              mergeDims,
            ),
          }));
          const backendPairs: [string, string][] = decisions.map(
            ({ photo, winner }) => [
              winner === photo.first_path ? photo.second_path : photo.first_path,
              winner,
            ],
          );
          const moveCount = new Set(
            backendPairs
              .filter(([, winner]) => !insideDir(winner, targetDir))
              .map(([, winner]) => winner),
          ).size;
          const keptCount = new Set(
            backendPairs
              .filter(([, winner]) => insideDir(winner, targetDir))
              .map(([, winner]) => winner),
          ).size;
          const recycleCount = new Set(
            backendPairs.map(([loser]) => loser),
          ).size;
          const cleanupDirs =
            pendingMerge.target.kind === "a"
              ? [pendingMerge.dirB]
              : pendingMerge.target.kind === "b"
                ? [pendingMerge.dirA]
                : [pendingMerge.dirA, pendingMerge.dirB];
          const invalidTarget =
            targetDir.trim() === "" || !looksAbsolutePath(targetDir.trim());
          const relativeTargetHint =
            pendingMerge.target.kind === "custom" &&
            targetDir.trim() !== "" &&
            !looksAbsolutePath(targetDir.trim());
          return (
            <ConfirmDialog
              title={`合并到一侧（共 ${pendingMerge.pairs.length} 对重复照片）`}
              detail={
                <>
                  <span className="merge-field">
                    <b>目标文件夹（保留的照片都集中到这里）</b>
                    <label className="merge-radio">
                      <input
                        type="radio"
                        checked={pendingMerge.target.kind === "a"}
                        onChange={() =>
                          setPendingMerge((current) =>
                            current
                              ? { ...current, target: { kind: "a", custom: "" } }
                              : current,
                          )
                        }
                      />
                      左侧目录：{pendingMerge.dirA}
                    </label>
                    <label className="merge-radio">
                      <input
                        type="radio"
                        checked={pendingMerge.target.kind === "b"}
                        onChange={() =>
                          setPendingMerge((current) =>
                            current
                              ? { ...current, target: { kind: "b", custom: "" } }
                              : current,
                          )
                        }
                      />
                      右侧目录：{pendingMerge.dirB}
                    </label>
                    <label className="merge-radio">
                      <input
                        type="radio"
                        checked={pendingMerge.target.kind === "custom"}
                        onChange={() =>
                          setPendingMerge((current) =>
                            current
                              ? {
                                  ...current,
                                  target: {
                                    kind: "custom",
                                    custom: current.target.custom,
                                  },
                                }
                              : current,
                          )
                        }
                      />
                      其他位置（选择现有目录，或输入新路径自动创建）
                    </label>
                    {pendingMerge.target.kind === "custom" && (
                      <span className="merge-target-row">
                        <input
                          value={pendingMerge.target.custom}
                          onChange={(event) =>
                            setPendingMerge((current) =>
                              current
                                ? {
                                    ...current,
                                    target: {
                                      kind: "custom",
                                      custom: event.target.value,
                                    },
                                  }
                                : current,
                            )
                          }
                          placeholder="输入完整路径，不存在会自动创建"
                        />
                        <button className="secondary" onClick={pickMergeTarget}>
                          选择目录…
                        </button>
                        {relativeTargetHint && (
                          <small className="merge-target-warning">
                            请输入绝对路径（如 D:\Photos 或 /home/user/Photos），相对路径无法定位。
                          </small>
                        )}
                      </span>
                    )}
                  </span>
                  <span className="merge-field">
                    <b>
                      保留条件（按优先级从上到下判断，先能分出胜负的条件生效）
                    </b>
                    {pendingMerge.conditions.map((condition, index) => (
                      <span className="merge-condition" key={condition.kind}>
                        <span className="merge-order">
                          <button
                            className="text-button"
                            disabled={index === 0}
                            aria-label="上移"
                            onClick={() => moveConditionOrder(index, index - 1)}
                          >
                            ↑
                          </button>
                          <button
                            className="text-button"
                            disabled={
                              index === pendingMerge.conditions.length - 1
                            }
                            aria-label="下移"
                            onClick={() => moveConditionOrder(index, index + 1)}
                          >
                            ↓
                          </button>
                        </span>
                        <label className="merge-enable" title="启用该条件">
                          <input
                            type="checkbox"
                            checked={condition.enabled}
                            onChange={(event) =>
                              setConditionEnabled(index, event.target.checked)
                            }
                          />
                        </label>
                        <span className="merge-condition-body">
                          <b>
                            {condition.kind === "prefix"
                              ? "前缀匹配（保留文件名开头匹配的照片）"
                              : condition.kind === "exif"
                                ? "EXIF 拍摄时间（有拍摄信息的优先，更早的原图优先）"
                                : condition.kind === "resolution"
                                  ? "分辨率较高（保留像素尺寸更大的一张）"
                                  : condition.kind === "time"
                                    ? "时间较新（保留修改时间较晚的照片）"
                                    : "优先指定目录"}
                          </b>
                          {condition.kind === "prefix" && condition.enabled && (
                            <>
                              <span className="merge-prefixes">
                                {condition.prefixes.length === 0 && (
                                  <small>未识别出字母/中文前缀。</small>
                                )}
                                {condition.prefixes.map((option) => (
                                  <label
                                    key={option.key}
                                    className={option.checked ? "on" : ""}
                                  >
                                    <input
                                      type="checkbox"
                                      checked={option.checked}
                                      onChange={() =>
                                        toggleMergePrefix(option.key)
                                      }
                                    />
                                    {option.label}
                                    {option.count > 0 && (
                                      <small>{option.count} 张</small>
                                    )}
                                  </label>
                                ))}
                              </span>
                              <span className="merge-add-prefix">
                                <input
                                  value={mergePrefixInput}
                                  onChange={(event) =>
                                    setMergePrefixInput(event.target.value)
                                  }
                                  onKeyDown={(event) => {
                                    if (event.key === "Enter") {
                                      addMergePrefix(mergePrefixInput);
                                      setMergePrefixInput("");
                                    }
                                  }}
                                  placeholder="自定义前缀，如 IMG"
                                />
                                <button
                                  className="secondary"
                                  onClick={() => {
                                    addMergePrefix(mergePrefixInput);
                                    setMergePrefixInput("");
                                  }}
                                >
                                  添加
                                </button>
                              </span>
                            </>
                          )}
                          {condition.kind === "prefer" && condition.enabled && (
                            <select
                              value={condition.side ?? ""}
                              onChange={(event) =>
                                setPreferSide(
                                  index,
                                  (event.target.value || null) as
                                    | "first"
                                    | "second"
                                    | null,
                                )
                              }
                            >
                              <option value="">选择优先保留的目录</option>
                              <option value="first">
                                左侧目录（{shortDir(pendingMerge.dirA)}）
                              </option>
                              <option value="second">
                                右侧目录（{shortDir(pendingMerge.dirB)}）
                              </option>
                            </select>
                          )}
                        </span>
                      </span>
                    ))}
                    <small>
                      没有条件能分出胜负时，保留目标文件夹里已有的那张；不启用任何条件时同理。
                    </small>
                  </span>
                  <small>
                    将移动 {moveCount} 张照片到目标文件夹；{keptCount}{" "}
                    张已在目标文件夹，原地保留；回收落选副本 {recycleCount}{" "}
                    张（可从应用回收站恢复）。目录里不属于重复的照片不受影响；被清空的来源目录会自动删除。
                  </small>
                </>
              }
              busy={busy}
              onClose={() => setPendingMerge(null)}
              options={[
                {
                  label: `合并（回收 ${recycleCount} 张）`,
                  disabled: invalidTarget || backendPairs.length === 0,
                  action: () => {
                    const merge = pendingMerge;
                    const pairs = backendPairs;
                    const dir = targetDir;
                    const cleanup = cleanupDirs;
                    setPendingMerge(null);
                    setMergePrefixInput("");
                    void execute(async () => {
                      const result = await invoke<string>("merge_dir_pair", {
                        database,
                        pairs,
                        targetDir: dir,
                        cleanupDirs: cleanup,
                      });
                      await refresh();
                      return result;
                    }, "similar");
                  },
                },
                {
                  label: "先不处理",
                  action: () => {
                    setPendingMerge(null);
                    setMergePrefixInput("");
                  },
                },
              ]}
            />
          );
        })()}
      {pendingDelete && (
        <ConfirmDialog
          title={`处理选中的 ${selected.size} 个文件`}
          detail="相似照片是视觉判断，可能包含误报；建议先逐张预览确认。"
          busy={busy}
          onClose={() => setPendingDelete(false)}
          options={[
            {
              label: "移入回收站（可恢复）",
              action: () => {
                setPendingDelete(false);
                removeSelected("trash");
              },
            },
            {
              label: "直接永久删除",
              kind: "danger",
              action: () => {
                setPendingDelete(false);
                removeSelected("delete");
              },
            },
          ]}
        />
      )}
    </section>
  );
}

function PhotoSlot({
  photo,
  side,
  onCompare,
  selected,
  onToggle,
}: {
  photo: SimilarPhoto;
  side: "first" | "second";
  onCompare: (photo: SimilarPhoto) => void;
  selected: Set<string>;
  onToggle: (path: string) => void;
}) {
  const path = side === "first" ? photo.first_path : photo.second_path;
  return (
    <div className="photo-slot">
      <label className="photo-check">
        <input
          type="checkbox"
          checked={selected.has(path)}
          onChange={() => onToggle(path)}
        />
      </label>
      <button
        className="thumbnail-button"
        title="点击并排放大对比"
        onClick={() => onCompare(photo)}
      >
        <Thumbnail path={path} />
      </button>
    </div>
  );
}

// Naming prefix of a photo file: the leading run of letters/CJK before the
// first digit/space/underscore ("IMG_2023..." → "IMG", "微信图片_2023" →
// "微信图片", "1695317846821.jpg" → ""). Camera and screenshot styles carry
// the original capture metadata, so the selective merge prefers them.
function fileNamePrefix(name: string): string {
  const match = name.match(/^[A-Za-z一-龥]+/);
  return match ? match[0] : "";
}

// Prefixes that mark original captures or screenshots rather than re-saved
// copies; they start checked in the selective merge dialog.
const MERGE_DEFAULT_PREFIXES = new Set([
  "img",
  "dsc",
  "screenshot",
  "pxl",
  "photo",
  "wechat",
  "mmexport",
  "微信图片",
]);

// Retention rules for merging a directory pair, evaluated top-down: the
// first enabled rule that tells the two copies apart picks the winner.
type MergeCondition =
  | {
      kind: "prefix";
      enabled: boolean;
      prefixes: { key: string; label: string; count: number; checked: boolean }[];
    }
  | { kind: "exif"; enabled: boolean }
  | { kind: "resolution"; enabled: boolean }
  | { kind: "time"; enabled: boolean }
  | { kind: "prefer"; enabled: boolean; side: "first" | "second" | null };

function insideDir(path: string, dir: string): boolean {
  // Folder strings come from fileFolder (forward slashes) while photo paths
  // keep the raw separators from the index, so normalize both sides.
  const normalize = (value: string) => {
    const unified = value.split("\\").join("/").replace(/\/+$/, "");
    // Windows paths are case-insensitive: a root registered as "D:\Photos"
    // must still match the picker's "d:/photos". POSIX paths stay
    // case-sensitive.
    return /^[a-z]:\//i.test(unified) ? unified.toLowerCase() : unified;
  };
  const base = normalize(dir);
  if (!base) return false;
  const candidate = normalize(path);
  return candidate.startsWith(base) && candidate[base.length] === "/";
}

// The backend resolves the merge target with the process CWD as fallback,
// so a relative input would silently move files somewhere unexpected.
// Require an absolute path (Windows drive or POSIX root) up front.
function looksAbsolutePath(value: string): boolean {
  return /^(?:[A-Za-z]:[\\/]|\/)/.test(value);
}

// Walk the enabled conditions in priority order. Without a verdict the copy
// already inside the target folder wins, else the left side's file — so the
// merge always produces exactly one winner per pair.
function decideMergeWinner(
  photo: SimilarPhoto,
  conditions: MergeCondition[],
  targetDir: string,
  dims: Record<string, [number, number]>,
): string {
  for (const condition of conditions) {
    if (!condition.enabled) continue;
    if (condition.kind === "prefix") {
      const checked = new Set(
        condition.prefixes
          .filter((option) => option.checked)
          .map((option) => option.key),
      );
      if (checked.size === 0) continue;
      const first = fileNamePrefix(fileName(photo.first_path)).toLowerCase();
      const second = fileNamePrefix(fileName(photo.second_path)).toLowerCase();
      const firstHit = first !== "" && checked.has(first);
      const secondHit = second !== "" && checked.has(second);
      if (firstHit !== secondHit) {
        return firstHit ? photo.first_path : photo.second_path;
      }
    } else if (condition.kind === "exif") {
      // The side carrying capture metadata beats a re-save that lost it;
      // when both carry it, the earlier capture is the original.
      const first = photo.first_taken;
      const second = photo.second_taken;
      if (first > 0 && second > 0 && first !== second) {
        return first < second ? photo.first_path : photo.second_path;
      }
      if (first > 0 && second <= 0) return photo.first_path;
      if (second > 0 && first <= 0) return photo.second_path;
    } else if (condition.kind === "resolution") {
      // Higher natural pixel count wins; pairs missing a header probe fall
      // through to the next condition.
      const first = dims[photo.first_path];
      const second = dims[photo.second_path];
      if (first && second) {
        const firstPixels = first[0] * first[1];
        const secondPixels = second[0] * second[1];
        if (firstPixels !== secondPixels) {
          return firstPixels > secondPixels
            ? photo.first_path
            : photo.second_path;
        }
      }
    } else if (condition.kind === "time") {
      if (photo.first_modified !== photo.second_modified) {
        return photo.first_modified > photo.second_modified
          ? photo.first_path
          : photo.second_path;
      }
    } else if (condition.side) {
      return condition.side === "first" ? photo.first_path : photo.second_path;
    }
  }
  const firstIn = insideDir(photo.first_path, targetDir);
  const secondIn = insideDir(photo.second_path, targetDir);
  if (!firstIn && secondIn) return photo.second_path;
  return photo.first_path;
}

// Cheap retention hint for a similar pair: prefer the larger file (likely
// the higher-quality original), break ties by recency.
function suggestKeep(
  photo: SimilarPhoto,
): { side: "first" | "second"; reason: string } | null {
  if (photo.first_size !== photo.second_size) {
    return photo.first_size > photo.second_size
      ? { side: "first", reason: "文件更大" }
      : { side: "second", reason: "文件更大" };
  }
  if (photo.first_modified !== photo.second_modified) {
    return photo.first_modified > photo.second_modified
      ? { side: "first", reason: "修改更近" }
      : { side: "second", reason: "修改更近" };
  }
  return null;
}

function PhotoInfo({
  path,
  size,
  modified,
}: {
  path: string;
  size: number;
  modified: number;
}) {
  return (
    <span className="photo-info" title={path}>
      {fileName(path)} · {formatBytes(size)} · {formatFileTime(modified)}
    </span>
  );
}

type DirPairGroup = { dirA: string; dirB: string; pairs: SimilarPhoto[] };

function DocumentInfo({
  path,
  size,
  modified,
  execute,
}: {
  path: string;
  size: number;
  modified: number;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
}) {
  return (
    <div className="document-info">
      <PhotoInfo path={path} size={size} modified={modified} />
      <button
        className="secondary"
        onClick={() =>
          void invoke("reveal_in_manager", { path }).catch((error) =>
            console.error("reveal_in_manager failed:", error),
          )
        }
      >
        定位
      </button>
      <button
        className="secondary"
        onClick={() =>
          void execute(() => invoke<string>("open_file", { path }), "open-doc")
        }
      >
        打开
      </button>
    </div>
  );
}

export function SimilarDocuments({
  documents,
  execute,
  scanning,
}: {
  documents: SimilarDocument[];
  execute: (action: () => Promise<string>) => Promise<void>;
  scanning: boolean;
}) {
  const [visible, setVisible] = useState(60);
  const shown = documents.slice(0, visible);
  return (
    <section className="panel review-page">
      <div className="section-head">
        <div>
          <h2>相似文档</h2>
          <p>文本近似仅供人工查看，不能作为自动处理依据。</p>
        </div>
        <span className="pill">{documents.length} 对</span>
      </div>
      {documents.length === 0 ? (
        <Empty
          icon="≡"
          text={scanning ? "扫描进行中" : "没有高置信度相似文档"}
          detail={
            scanning
              ? "正在扫描与更新索引，完成后结果会自动出现在这里。"
              : "扫描 TXT、Markdown、CSV、JSON、XML 或 HTML 后会显示候选。"
          }
        />
      ) : (
        <div className="similar-list">
          {shown.map((document, index) => (
            <article
              className="similar-pair"
              key={`${document.first_path}-${document.second_path}`}
            >
              <div>
                <b>候选 {index + 1}</b>
                <DocumentInfo
                  path={document.first_path}
                  size={document.first_size}
                  modified={document.first_modified}
                  execute={execute}
                />
                <DocumentInfo
                  path={document.second_path}
                  size={document.second_size}
                  modified={document.second_modified}
                  execute={execute}
                />
              </div>
              <small>SimHash 差异 {document.distance}/64</small>
            </article>
          ))}
          {visible < documents.length && (
            <div className="load-more">
              <button
                className="secondary"
                onClick={() => setVisible((count) => count + 120)}
              >
                显示更多（还有 {documents.length - visible} 对）
              </button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export function Detectors({ detectors }: { detectors: DetectorStatus[] }) {
  return <section className="panel review-page"><div className="section-head"><div><h2>检测能力</h2><p>所有处理均在本地进行。未启用的能力不会以低质量算法替代。</p></div></div>{detectors.length === 0 ? <Empty icon="◎" text="检测能力列表为空" detail="打开项目或完成初始化后会显示当前支持的检测能力。" /> : <div className="source-list">{detectors.map(detector => <article className="source" key={detector.name}><span className="source-icon">{detector.available ? "✓" : "·"}</span><div><b>{detector.name}</b><small>{detector.detail}</small></div><span className={detector.available ? "approved" : "protected"}>{detector.available ? "已启用" : "待接入"}</span></article>)}</div>}</section>;
}

export function Trash({
  database,
  items,
  busy,
  busyKeys,
  execute,
  refresh,
  trashMaxBytes,
}: {
  database: string;
  items: TrashItem[];
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
  trashMaxBytes: number;
}) {
  const [pendingEmpty, setPendingEmpty] = useState(false);
  const [pendingPrune, setPendingPrune] = useState(false);
  const [pendingItem, setPendingItem] = useState<TrashItem | null>(null);
  const [pendingBatchRestore, setPendingBatchRestore] = useState<{
    id: number;
    count: number;
  } | null>(null);
  const [pendingBatchDelete, setPendingBatchDelete] = useState<{
    id: number;
    count: number;
  } | null>(null);
  // Real disk usage of the recycle bin, walked when the page loads.
  const [usage, setUsage] = useState<TrashUsage | null>(null);
  useEffect(() => {
    let cancelled = false;
    invoke<TrashUsage>("trash_usage", { database })
      .then((next) => {
        if (!cancelled) setUsage(next);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [database, items.length]);
  const expiredCount = items.filter((item) => item.expired).length;
  const overLimit = trashMaxBytes > 0 && (usage?.bytes ?? 0) > trashMaxBytes;
  return (
    <section className="panel trash-page">
      <div className="section-head">
        <div>
          <h2>可恢复文件</h2>
          <p>恢复时不会覆盖原路径已有文件，并会再次进行内容完整性检查。</p>
        </div>
        <div className="head-actions">
          <span className="pill">
            {items.length} 个文件
            {usage && ` · 占用 ${formatBytes(usage.bytes)}`}
          </span>
          {overLimit && (
            <span className="pill warn">
              已超过设置的容量提醒上限（{formatBytes(trashMaxBytes)}），建议恢复或清理。
            </span>
          )}
          {expiredCount > 0 && (
            <button
              className="secondary"
              disabled={busy}
              onClick={() => setPendingPrune(true)}
            >
              清理过期文件（{expiredCount}）
            </button>
          )}
          {items.length > 0 && (
            <button
              className="danger"
              disabled={busy}
              onClick={() => setPendingEmpty(true)}
            >
              清空回收站
            </button>
          )}
        </div>
      </div>
      {items.length === 0 ? (
        <Empty
          icon="↶"
          text="应用回收站为空"
          detail="从审核队列移入的副本将显示在这里，默认建议保留 30 天。"
        />
      ) : (
        <div className="trash-list">
          {groupTrashByBatch(items).map((segment) => (
            <div className="trash-batch" key={segment.key}>
              {segment.batch && (
                <div className="trash-batch-head">
                  <b>批次清理</b>
                  <span>
                    {new Date(segment.batch.time * 1000).toLocaleString("zh-CN")} ·{" "}
                    {segment.items.length} 个文件 · 同一次批量操作
                  </span>
                  <button
                    className="secondary"
                    disabled={busyKeys.has("restore-batch")}
                    onClick={() =>
                      setPendingBatchRestore({
                        id: segment.batch!.id,
                        count: segment.items.length,
                      })
                    }
                  >
                    恢复整批
                  </button>
                  <button
                    className="danger"
                    disabled={busyKeys.has("delete-batch")}
                    onClick={() =>
                      setPendingBatchDelete({
                        id: segment.batch!.id,
                        count: segment.items.length,
                      })
                    }
                  >
                    永久删除整批
                  </button>
                </div>
              )}
              {segment.items.map((item) => (
            <article className="trash-item" key={item.id}>
              <span className="source-icon">↶</span>
              <div>
                <b>{fileName(item.source_path)}</b>
                <span>原位置：{item.source_path}</span>
                <small>
                  移入时间：
                  {new Date(item.created_at * 1000).toLocaleString("zh-CN")}
                  {item.expired && " · 已超过保留期"}
                </small>
              </div>
              <div className="trash-actions">
                <button
                  disabled={busyKeys.has("restore")}
                  onClick={() =>
                    execute(async () => {
                      const result = await invoke<string>("restore", {
                        database,
                        operationId: item.id,
                      });
                      await refresh();
                      return result;
                    }, "restore")
                  }
                >
                  恢复原位置
                </button>
                <button
                  className="secondary"
                  disabled={busyKeys.has("restore")}
                  title="选择另一个目录恢复（原位置已删除或不想放回原处时使用）"
                  onClick={() => {
                    void (async () => {
                      const target = await open({
                        directory: true,
                        multiple: false,
                        title: "选择恢复目录",
                      });
                      if (typeof target !== "string") return;
                      await execute(async () => {
                        const result = await invoke<string>("restore_to", {
                          database,
                          operationId: item.id,
                          targetDir: target,
                        });
                        await refresh();
                        return result;
                      }, "restore");
                    })();
                  }}
                >
                  恢复到…
                </button>
                <button
                  className="danger"
                  disabled={busy}
                  onClick={() => setPendingItem(item)}
                >
                  永久删除
                </button>
              </div>
            </article>
              ))}
            </div>
          ))}
        </div>
      )}
      {pendingPrune && (
        <ConfirmDialog
          title={`清理 ${expiredCount} 个超期回收文件`}
          detail="这些文件已超过保留期，清理后将永久删除、无法恢复。"
          busy={busy}
          onClose={() => setPendingPrune(false)}
          options={[
            {
              label: "确认清理",
              kind: "danger",
              action: () => {
                setPendingPrune(false);
                void execute(async () => {
                  const result = await invoke<string>("trash_prune_expired", {
                    database,
                  });
                  await refresh();
                  return result;
                }, "prune");
              },
            },
          ]}
        />
      )}
      {pendingEmpty && (
        <ConfirmDialog
          title="清空回收站"
          detail={`回收站中的 ${items.length} 个文件将被永久删除，此操作不可恢复。`}
          busy={busy}
          onClose={() => setPendingEmpty(false)}
          options={[
            {
              label: "永久删除全部",
              kind: "danger",
              action: () => {
                setPendingEmpty(false);
                void execute(async () => {
                  const result = await invoke<string>("trash_empty", {
                    database,
                  });
                  await refresh();
                  return result;
                }, "empty");
              },
            },
          ]}
        />
      )}
      {pendingBatchRestore && (
        <ConfirmDialog
          title={`恢复整批 ${pendingBatchRestore.count} 个文件`}
          detail="这批文件将恢复到各自的原位置；原位置已被占用或内容校验失败的文件会被跳过并在结果中提示。"
          busy={busy}
          onClose={() => setPendingBatchRestore(null)}
          options={[
            {
              label: "恢复整批",
              action: () => {
                const batchId = pendingBatchRestore.id;
                setPendingBatchRestore(null);
                void execute(async () => {
                  const result = await invoke<string>("restore_batch", {
                    database,
                    batchId,
                  });
                  await refresh();
                  return result;
                }, "restore-batch");
              },
            },
          ]}
        />
      )}
      {pendingBatchDelete && (
        <ConfirmDialog
          title={`永久删除整批 ${pendingBatchDelete.count} 个文件`}
          detail="这批文件将从应用回收站中直接永久删除。"
          note="删除后将无法再恢复，请确认这批副本不再需要。"
          busy={busy}
          onClose={() => setPendingBatchDelete(null)}
          options={[
            {
              label: "永久删除整批",
              kind: "danger",
              action: () => {
                const batchId = pendingBatchDelete.id;
                setPendingBatchDelete(null);
                void execute(async () => {
                  const result = await invoke<string>("delete_trash_batch", {
                    database,
                    batchId,
                  });
                  await refresh();
                  return result;
                }, "delete-batch");
              },
            },
          ]}
        />
      )}
      {pendingItem && (
        <ConfirmDialog
          title="永久删除该文件"
          detail={
            <>
              {fileName(pendingItem.source_path)}
              <br />
              <small>{pendingItem.source_path}</small>
            </>
          }
          note="删除后将无法再恢复。"
          busy={busy}
          onClose={() => setPendingItem(null)}
          options={[
            {
              label: "永久删除",
              kind: "danger",
              action: () => {
                const target = pendingItem;
                setPendingItem(null);
                void execute(async () => {
                  const result = await invoke<string>("trash_delete", {
                    database,
                    operationId: target.id,
                  });
                  await refresh();
                  return result;
                }, "trash-delete");
              },
            },
          ]}
        />
      )}
    </section>
  );
}

// Segments the recycle list into batches: consecutive items sharing a
// batch_id form one user action; unbatched (single) removals stand alone.
function groupTrashByBatch(items: TrashItem[]): { key: string; batch: { id: number; time: number } | null; items: TrashItem[] }[] {
  const segments: { key: string; batch: { id: number; time: number } | null; items: TrashItem[] }[] = [];
  for (const item of items) {
    const last = segments[segments.length - 1];
    if (item.batch_id !== null && last?.batch?.id === item.batch_id) {
      last.items.push(item);
      continue;
    }
    segments.push({
      key: item.batch_id !== null ? `batch-${item.batch_id}` : `item-${item.id}`,
      batch: item.batch_id !== null ? { id: item.batch_id, time: item.created_at } : null,
      items: [item],
    });
  }
  return segments;
}

const HISTORY_PAGE = 100;
const HISTORY_FILTERS: { value: string; label: string }[] = [
  { value: "", label: "全部" },
  { value: "trashed", label: "已入回收站" },
  { value: "restored", label: "已恢复" },
  { value: "deleted", label: "已永久删除" },
  { value: "hardlinked", label: "已转硬链接" },
];
// One-shot cleanup helpers: empty directories found by the last scan and
// zero-byte files (which share one hash and would otherwise form a single
// noisy duplicate group).
export function Cleanup({
  database,
  active,
  busy,
  execute,
  refresh,
}: {
  database: string;
  active: boolean;
  busy: boolean;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const [emptyDirs, setEmptyDirs] = useState<string[]>([]);
  const [emptyLoaded, setEmptyLoaded] = useState(false);
  const [emptySelected, setEmptySelected] = useState<ReadonlySet<string>>(
    new Set(),
  );
  const [zeroFiles, setZeroFiles] = useState<ZeroByteFile[]>([]);
  const [zeroLoaded, setZeroLoaded] = useState(false);
  const [zeroSelected, setZeroSelected] = useState<ReadonlySet<string>>(
    new Set(),
  );
  const [pendingDirs, setPendingDirs] = useState<number | null>(null);
  const [pendingZeroTrash, setPendingZeroTrash] = useState<number | null>(null);

  function loadEmptyDirs() {
    invoke<string[]>("empty_dirs", { database })
      .then((next) => {
        setEmptyDirs(next);
        setEmptySelected(new Set(next));
        setEmptyLoaded(true);
      })
      .catch(() => setEmptyLoaded(true));
  }
  function loadZeroFiles() {
    invoke<ZeroByteFile[]>("zero_byte_files", { database })
      .then((next) => {
        setZeroFiles(next);
        setZeroSelected(new Set(next.map((file) => file.path)));
        setZeroLoaded(true);
      })
      .catch(() => setZeroLoaded(true));
  }

  // Load when the page becomes visible; the component stays mounted so
  // selections survive switching away and back, like the history page.
  useEffect(() => {
    if (!active || !database) return;
    loadEmptyDirs();
    loadZeroFiles();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [active, database]);

  function toggle(
    set: (next: ReadonlySet<string>) => void,
    current: ReadonlySet<string>,
    path: string,
  ) {
    const next = new Set(current);
    if (next.has(path)) next.delete(path);
    else next.add(path);
    set(next);
  }

  function removeSelectedEmptyDirs() {
    const paths = emptyDirs.filter((path) => emptySelected.has(path));
    setPendingDirs(null);
    void execute(async () => {
      const result = await invoke<string>("remove_empty_dirs", {
        database,
        dirs: paths,
      });
      loadEmptyDirs();
      await refresh();
      return result;
    }, "cleanup");
  }

  function trashSelectedZeroFiles() {
    const paths = zeroFiles
      .filter((file) => zeroSelected.has(file.path))
      .map((file) => file.path);
    setPendingZeroTrash(null);
    void execute(async () => {
      const result = await invoke<string>("trash_paths", {
        database,
        paths,
      });
      loadZeroFiles();
      await refresh();
      return result;
    }, "cleanup");
  }

  return (
    <section className="panel cleanup-page">
      <div className="section-head">
        <div>
          <h2>清理工具</h2>
          <p>
            两类占着位置却没有内容的候选：空文件夹与零字节文件。删除前都会重新核实当前状态。
          </p>
        </div>
        <div className="head-actions">
          <button
            className="secondary"
            disabled={busy}
            onClick={() => {
              loadEmptyDirs();
              loadZeroFiles();
            }}
          >
            重新检查
          </button>
        </div>
      </div>

      <div className="cleanup-section">
        <div className="section-head">
          <div>
            <h3>空文件夹（{emptyDirs.length}）</h3>
            <p>
              来自上一次扫描的结果；删除时会逐个重新确认仍然为空，且不会触碰扫描根目录与受保护目录。文件夹本身不可恢复（但本来就是空的）。
            </p>
          </div>
          {emptyDirs.length > 0 && (
            <div className="head-actions">
              <button
                className="secondary"
                disabled={busy || emptySelected.size === 0}
                onClick={() => setPendingDirs(emptySelected.size)}
              >
                删除选中的空文件夹（{emptySelected.size}）
              </button>
            </div>
          )}
        </div>
        {emptyDirs.length === 0 ? (
          <Empty
            icon="▢"
            text={emptyLoaded ? "没有发现空文件夹" : "正在读取上一次扫描的结果…"}
            detail="扫描完成后，完全为空的目录会出现在这里。"
          />
        ) : (
          <div className="cleanup-list">
            {emptyDirs.map((path) => (
              <label className="cleanup-row" key={path}>
                <input
                  type="checkbox"
                  checked={emptySelected.has(path)}
                  onChange={() =>
                    toggle(setEmptySelected, emptySelected, path)
                  }
                />
                <div>
                  <b>{fileName(path)}</b>
                  <small>{path}</small>
                </div>
              </label>
            ))}
          </div>
        )}
      </div>

      <div className="cleanup-section">
        <div className="section-head">
          <div>
            <h3>零字节文件（{zeroFiles.length}）</h3>
            <p>
              完全没有内容的文件，不会占用空间，但会污染重复列表。移入应用回收站（可恢复），走完整的保护规则与内容校验。
            </p>
          </div>
          {zeroFiles.length > 0 && (
            <div className="head-actions">
              <button
                className="secondary"
                disabled={busy || zeroSelected.size === 0}
                onClick={() => setPendingZeroTrash(zeroSelected.size)}
              >
                移入回收站（{zeroSelected.size}）
              </button>
            </div>
          )}
        </div>
        {zeroFiles.length === 0 ? (
          <Empty
            icon="▢"
            text={zeroLoaded ? "没有零字节文件" : "正在读取…"}
            detail="0 字节的文件会出现在这里，可一键移入回收站。"
          />
        ) : (
          <div className="cleanup-list">
            {zeroFiles.map((file) => (
              <label className="cleanup-row" key={file.path}>
                <input
                  type="checkbox"
                  checked={zeroSelected.has(file.path)}
                  onChange={() =>
                    toggle(setZeroSelected, zeroSelected, file.path)
                  }
                />
                <div>
                  <b>{fileName(file.path)}</b>
                  <small>
                    {file.path} · 修改于 {formatFileTime(file.modified)}
                  </small>
                </div>
              </label>
            ))}
          </div>
        )}
      </div>

      {pendingDirs !== null && (
        <ConfirmDialog
          title={`删除 ${pendingDirs} 个空文件夹`}
          detail="删除时会逐个重新确认仍然为空；任何不再为空、已被删除、属于扫描根目录或受保护的文件夹都会被跳过并在结果中提示。"
          note="文件夹本身删除后不可恢复（它们当前都是空的）。"
          busy={busy}
          onClose={() => setPendingDirs(null)}
          options={[
            {
              label: "删除空文件夹",
              kind: "danger",
              action: removeSelectedEmptyDirs,
            },
          ]}
        />
      )}
      {pendingZeroTrash !== null && (
        <ConfirmDialog
          title={`移入回收站 ${pendingZeroTrash} 个零字节文件`}
          detail="这些文件会按标准安全链移入应用回收站，随时可以恢复。"
          busy={busy}
          onClose={() => setPendingZeroTrash(null)}
          options={[
            {
              label: "移入回收站",
              action: trashSelectedZeroFiles,
            },
          ]}
        />
      )}
    </section>
  );
}

export function History({ database, active }: { database: string; active: boolean }) {
  const [items, setItems] = useState<HistoryItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [exhausted, setExhausted] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);
  const [stateFilter, setStateFilter] = useState("");

  // Reload only while the page is visible: the component stays mounted so
  // scroll position and loaded pages survive switching away and back.
  useEffect(() => {
    if (!active || !database) return;
    setLoading(true);
    setError("");
    invoke<HistoryItem[]>("history", {
      database,
      offset: 0,
      limit: HISTORY_PAGE,
      stateFilter: stateFilter || undefined,
    })
      .then((next) => {
        setItems(next);
        setExhausted(next.length < HISTORY_PAGE);
        setError("");
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [database, active, stateFilter]);

  function loadMore() {
    if (loadingMore) return;
    setLoadingMore(true);
    invoke<HistoryItem[]>("history", {
      database,
      offset: items.length,
      limit: HISTORY_PAGE,
      stateFilter: stateFilter || undefined,
    })
      .then((next) => {
        setItems((current) => [...current, ...next]);
        setExhausted(next.length < HISTORY_PAGE);
        setError("");
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoadingMore(false));
  }

  const stateBadge: Record<string, { label: string; className: string }> = {
    trashed: { label: "已入回收站", className: "protected" },
    restored: { label: "已恢复", className: "approved" },
    deleted: { label: "已永久删除", className: "deleted" },
    hardlinked: { label: "已转硬链接", className: "approved" },
    moved: { label: "已移入保留目录", className: "approved" },
  };

  return (
    <section className="panel history-page">
      <div className="section-head">
        <div>
          <h2>操作历史</h2>
          <p>
            每一次移入回收站、恢复和永久删除都有记录，按时间倒序显示，可供追溯核对。
          </p>
        </div>
        <span className="pill">{items.length} 条记录</span>
      </div>
      <div className="filter-bar">
        {HISTORY_FILTERS.map((filter) => (
          <button
            key={filter.value}
            className={stateFilter === filter.value ? "secondary on" : "secondary"}
            onClick={() => setStateFilter(filter.value)}
          >
            {filter.label}
          </button>
        ))}
      </div>
      {loading ? (
        <div className="history-empty">正在加载...</div>
      ) : error ? (
        <div className="history-empty">{friendlyError(error)}</div>
      ) : items.length === 0 ? (
        <Empty
          icon="⧗"
          text="还没有操作记录"
          detail="移入回收站、恢复或永久删除的每一步都会记录在这里。"
        />
      ) : (
        <div className="trash-list">
          {items.map((item) => {
            const badge = stateBadge[item.state] ?? {
              label: item.state,
              className: "protected",
            };
            return (
              <article className="trash-item" key={item.id}>
                <span className="source-icon">⧗</span>
                <div>
                  <b>{fileName(item.source_path)}</b>
                  <span title={item.source_path}>{item.source_path}</span>
                  <small>
                    {new Date(item.created_at * 1000).toLocaleString("zh-CN")}
                    {item.state === "trashed" && item.trash_path
                      ? ` · 现存于回收站（${item.trash_path}）`
                      : ""}
                    {item.state === "restored" && item.restored_at
                      ? ` · 恢复于 ${new Date(item.restored_at * 1000).toLocaleString("zh-CN")}`
                      : ""}
                  </small>
                </div>
                <span className={badge.className}>{badge.label}</span>
              </article>
            );
          })}
          {!exhausted && (
            <div className="load-more">
              <button
                className="secondary"
                disabled={loadingMore}
                onClick={loadMore}
              >
                {loadingMore ? "加载中..." : "加载更早的记录"}
              </button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export function Settings({
  database,
  trash,
  theme,
  setTheme,
  settingsTab,
  onSettingsTabChange,
  protectRules,
  excludeRules,
  minFileSize,
  retentionDays,
  trashMaxBytes,
  autoScan,
  strictVerify,
  setStrictVerify,
  usnScan,
  setUsnScan,
  watchScan,
  setWatchScan,
  similarThreshold,
  setSimilarThreshold,
  setDatabase,
  setTrash,
  setProtectRules,
  setExcludeRules,
  setMinFileSize,
  setRetentionDays,
  setTrashMaxBytes,
  setAutoScan,
  initialize,
  execute,
  refresh,
  notify,
  busy,
  busyKeys,
}: {
  database: string;
  trash: string;
  theme: ThemeSetting;
  setTheme: (value: ThemeSetting) => void;
  settingsTab: "project" | "help";
  onSettingsTabChange: (tab: "project" | "help") => void;
  protectRules: string[];
  excludeRules: string[];
  minFileSize: number;
  retentionDays: number;
  trashMaxBytes: number;
  autoScan: boolean;
  strictVerify: boolean;
  setStrictVerify: (value: boolean) => void;
  usnScan: boolean;
  setUsnScan: (value: boolean) => void;
  watchScan: boolean;
  setWatchScan: (value: boolean) => void;
  similarThreshold: number;
  setSimilarThreshold: (value: number) => void;
  setDatabase: (value: string) => void;
  setTrash: (value: string) => void;
  setProtectRules: (value: string[]) => void;
  setExcludeRules: (value: string[]) => void;
  setMinFileSize: (value: number) => void;
  setRetentionDays: (value: number) => void;
  setTrashMaxBytes: (value: number) => void;
  setAutoScan: (value: boolean) => void;
  initialize: () => Promise<void>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
  notify: (message: string) => void;
  busy: boolean;
  busyKeys: ReadonlySet<string>;
}) {
  const [rule, setRule] = useState("");
  const [protectPreview, setProtectPreview] = useState<ProtectPreview | null>(
    null,
  );
  const [excludeInput, setExcludeInput] = useState("");
  const [minSizeInput, setMinSizeInput] = useState(
    minFileSize ? String(Math.round(minFileSize / (1024 * 1024))) : "",
  );
  const [retentionInput, setRetentionInput] = useState(String(retentionDays));
  // The capacity reminder is entered in MB; the setting stores bytes.
  const [maxBytesInput, setMaxBytesInput] = useState(
    trashMaxBytes ? String(Math.round(trashMaxBytes / (1024 * 1024))) : "",
  );
  const [cacheStats, setCacheStats] = useState<ThumbnailCacheStats | null>(null);
  const [thresholdInput, setThresholdInput] = useState(String(similarThreshold));

  useEffect(() => {
    setThresholdInput(String(similarThreshold));
  }, [similarThreshold]);

  // The threshold reshapes the similar-photo queue immediately: persist,
  // mirror to state, then refresh so the list reflects the new cutoff.
  function saveSimilarThreshold() {
    const value = Number.parseInt(thresholdInput.trim(), 10);
    if (Number.isNaN(value) || value < 4 || value > 20) {
      notify("相似判定阈值需为 4 到 20 之间的整数。");
      setThresholdInput(String(similarThreshold));
      return;
    }
    if (value === similarThreshold) return;
    void execute(async () => {
      const stored = await invoke<number>("set_similar_threshold", {
        database,
        maxDistance: value,
      });
      setSimilarThreshold(stored);
      await refresh();
      return `相似判定阈值已设为 ${stored}，相似照片列表已按新阈值刷新。`;
    }, "similar-threshold");
  }

  // The cache panel loads lazily (one directory walk) and refreshes after a
  // clear; failures leave the panel blank instead of disturbing the user.
  useEffect(() => {
    if (cacheStats) return;
    invoke<ThumbnailCacheStats>("thumbnail_cache_stats")
      .then(setCacheStats)
      .catch(() => setCacheStats(null));
  }, [cacheStats]);

  function clearThumbnailCache() {
    void execute(async () => {
      const result = await invoke<string>("thumbnail_cache_clear");
      setCacheStats(null);
      return result;
    }, "cache-clear");
  }

  useEffect(() => {
    setRetentionInput(String(retentionDays));
  }, [retentionDays]);

  function saveRetention() {
    const days = Number.parseInt(retentionInput, 10);
    if (Number.isNaN(days) || days < 0 || days > 3650) {
      notify("保留天数需为 0 到 3650 之间的整数。");
      setRetentionInput(String(retentionDays));
      return;
    }
    if (days === retentionDays) return;
    void execute(async () => {
      const result = await invoke<string>("set_trash_retention", {
        database,
        days,
      });
      setRetentionDays(days);
      return result;
    }, "retention");
  }

  function toggleAutoScan() {
    const next = !autoScan;
    void execute(async () => {
      const result = await invoke<string>("set_auto_scan", {
        database,
        enabled: next,
      });
      setAutoScan(next);
      return result;
    }, "auto-scan");
  }

  function toggleStrictVerify() {
    const next = !strictVerify;
    void execute(async () => {
      const result = await invoke<string>("set_strict_verify", {
        database,
        enabled: next,
      });
      setStrictVerify(next);
      return result;
    }, "strict-verify");
  }

  function toggleUsnScan() {
    const next = !usnScan;
    void execute(async () => {
      const result = await invoke<string>("set_usn_scan", {
        database,
        enabled: next,
      });
      setUsnScan(next);
      return result;
    }, "usn-scan");
  }

  function toggleWatchScan() {
    const next = !watchScan;
    void execute(async () => {
      await invoke("set_watch_scan", {
        database,
        enabled: next,
      });
      setWatchScan(next);
      return next
        ? "实时监控已开启：扫描目录有变化时会自动增量扫描。"
        : "实时监控已关闭。";
    }, "watch-scan");
  }

  // The input is in MB for readability; the setting stores bytes.
  function saveMinSize() {
    const trimmed = minSizeInput.trim();
    const megabytes = trimmed === "" ? 0 : Number.parseInt(trimmed, 10);
    if (Number.isNaN(megabytes) || megabytes < 0 || megabytes > 1024 * 1024) {
      notify("最小文件大小需为 0 到 1048576 之间的整数（MB）。");
      setMinSizeInput(minFileSize ? String(Math.round(minFileSize / (1024 * 1024))) : "");
      return;
    }
    setMinFileSize(megabytes * 1024 * 1024);
  }
  function saveTrashMaxBytes() {
    const trimmed = maxBytesInput.trim();
    const megabytes = trimmed === "" ? 0 : Number.parseInt(trimmed, 10);
    if (Number.isNaN(megabytes) || megabytes < 0 || megabytes > 1024 * 1024) {
      notify("容量提醒上限需为 0 到 1048576 之间的整数（MB）。");
      setMaxBytesInput(trashMaxBytes ? String(Math.round(trashMaxBytes / (1024 * 1024))) : "");
      return;
    }
    setTrashMaxBytes(megabytes * 1024 * 1024);
  }
  async function pickTrash() {
    const selected = await open({
      directory: true,
      multiple: false,
      title: "选择本地应用回收站目录",
    });
    if (typeof selected === "string") setTrash(selected);
  }
  return (
    <section className="panel settings-page">
      <div className="section-head">
        <div>
          <h2>{settingsTab === "help" ? "使用帮助" : "项目与安全设置"}</h2>
          <p>
            {settingsTab === "help"
              ? "从扫描到清理的完整操作说明，以及常见问题。"
              : "数据库记录扫描结果和操作日志；回收站必须位于客户端本地磁盘。"}
          </p>
        </div>
        <div className="head-actions">
          <div className="theme-choice" role="tablist" aria-label="设置页视图">
            <button
              type="button"
              className={settingsTab === "project" ? "on" : ""}
              onClick={() => onSettingsTabChange("project")}
            >
              项目设置
            </button>
            <button
              type="button"
              className={settingsTab === "help" ? "on" : ""}
              onClick={() => onSettingsTabChange("help")}
            >
              使用帮助
            </button>
          </div>
        </div>
      </div>
      {settingsTab === "help" ? (
        <HelpPage />
      ) : (
        <>
      <label>
        界面主题
        <div className="theme-choice" role="radiogroup" aria-label="界面主题">
          {(
            [
              ["system", "跟随系统"],
              ["light", "日间模式"],
              ["dark", "夜间模式"],
            ] as [ThemeSetting, string][]
          ).map(([value, label]) => (
            <button
              key={value}
              type="button"
              className={theme === value ? "on" : ""}
              aria-pressed={theme === value}
              onClick={() => setTheme(value)}
            >
              {label}
            </button>
          ))}
        </div>
        <span className="field-hint">「跟随系统」随操作系统的深浅色设置自动切换。</span>
      </label>
      <label>
        项目数据库路径
        <input
          value={database}
          onChange={(event) => setDatabase(event.target.value)}
          placeholder="默认自动管理，可输入自定义 .db 文件完整路径"
        />
      </label>
      <label>
        应用回收站路径
        <div className="input-action">
          <input
            value={trash}
            onChange={(event) => setTrash(event.target.value)}
            placeholder="选择本地磁盘上的回收站目录"
          />
          <button className="secondary" onClick={pickTrash}>
            选择目录
          </button>
        </div>
      </label>
      <label>
        保护路径规则
        <div className="input-action">
          <input
            value={rule}
            onChange={(event) => setRule(event.target.value)}
            placeholder="例如 Originals"
          />
            <button
              className="secondary"
              onClick={() => {
                if (rule.trim() && !protectRules.includes(rule.trim())) {
                  setProtectRules([...protectRules, rule.trim()]);
                  setProtectPreview(null);
                }
                setRule("");
              }}
            >
              添加
            </button>
        </div>
      </label>
      <div className="rules">
        {protectRules.map((item) => (
          <span className="chip" key={item}>
            {item}
            <button
              onClick={() => {
                setProtectRules(protectRules.filter((value) => value !== item));
                setProtectPreview(null);
              }}
            >
              ×
            </button>
          </span>
        ))}
      </div>
      {protectRules.length > 0 && (
        <div className="protect-preview">
          <button
            className="secondary"
            disabled={busy}
            onClick={() =>
              void execute(async () => {
                const preview = await invoke<ProtectPreview>(
                  "protect_preview",
                  { database, protectRules },
                );
                setProtectPreview(preview);
                return `当前规则将保护 ${preview.matched} 个已索引文件。`;
              }, "protect-preview")
            }
          >
            预览保护范围
          </button>
          {protectPreview && (
            <div className="protect-preview-result">
              <b>
                当前规则将保护 {protectPreview.matched} 个已索引文件
                {protectPreview.matched === 0 && "——没有匹配任何路径，请检查规则拼写"}
              </b>
              {protectPreview.examples.length > 0 && (
                <ul>
                  {protectPreview.examples.map((example) => (
                    <li key={example}>{example}</li>
                  ))}
                </ul>
              )}
            </div>
          )}
        </div>
      )}
      <label>
        排除目录规则
        <div className="input-action">
          <input
            value={excludeInput}
            onChange={(event) => setExcludeInput(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === "Enter") {
                const trimmed = excludeInput.trim();
                if (trimmed && !excludeRules.includes(trimmed))
                  setExcludeRules([...excludeRules, trimmed]);
                setExcludeInput("");
              }
            }}
            placeholder="例如 node_modules"
          />
          <button
            className="secondary"
            onClick={() => {
              const trimmed = excludeInput.trim();
              if (trimmed && !excludeRules.includes(trimmed))
                setExcludeRules([...excludeRules, trimmed]);
              setExcludeInput("");
            }}
          >
            添加
          </button>
        </div>
        <small className="field-hint">
          路径包含任一规则的目录与文件将被扫描跳过；下次扫描生效。
        </small>
      </label>
      <div className="rules">
        {excludeRules.map((item) => (
          <span className="chip" key={item}>
            {item}
            <button
              onClick={() =>
                setExcludeRules(excludeRules.filter((value) => value !== item))
              }
            >
              ×
            </button>
          </span>
        ))}
      </div>
      <label>
        最小文件大小（MB）
        <input
          value={minSizeInput}
          onChange={(event) => setMinSizeInput(event.target.value)}
          onBlur={saveMinSize}
          onKeyDown={(event) => event.key === "Enter" && saveMinSize()}
          inputMode="numeric"
          placeholder="0"
        />
        <small className="field-hint">
          小于该大小的文件不参与重复检测；填 0 或留空表示不过滤。下次扫描生效。
        </small>
      </label>
      <label>
        回收站保留天数
        <input
          value={retentionInput}
          onChange={(event) => setRetentionInput(event.target.value)}
          onBlur={saveRetention}
          onKeyDown={(event) => event.key === "Enter" && saveRetention()}
          inputMode="numeric"
          placeholder="30"
        />
        <small className="field-hint">
          超过该天数仍未恢复的回收站文件，会在下一次扫描结束时自动永久清理；填
          0 表示不自动清理。
        </small>
      </label>
      <label>
        回收站容量提醒上限（MB）
        <input
          value={maxBytesInput}
          onChange={(event) => setMaxBytesInput(event.target.value)}
          onBlur={saveTrashMaxBytes}
          onKeyDown={(event) => event.key === "Enter" && saveTrashMaxBytes()}
          inputMode="numeric"
          placeholder="0"
        />
        <small className="field-hint">
          回收站实际占用超过该值时，回收站页会显示提醒横幅；仅提醒，不会自动删除任何文件；填
          0 表示不提醒。
        </small>
      </label>
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={autoScan}
          onChange={toggleAutoScan}
        />
        <span>
          启动时自动增量扫描
          <small>
            打开应用后用已保存的扫描目录自动更新索引；扫描只读取文件，可随时取消。
          </small>
        </span>
      </label>
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={strictVerify}
          onChange={toggleStrictVerify}
        />
        <span>
          删除前逐字节复核（严格模式）
          <small>
            开启后，永久删除或移入回收站前会把副本与保留副本逐字节比对，彻底排除内容不一致；代价是每个文件多读一遍。
          </small>
        </span>
      </label>
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={usnScan}
          onChange={toggleUsnScan}
        />
        <span>
          USN 快速扫描（Windows 实验性）
          <small>
            以管理员身份运行时从 NTFS 变更日志读取文件列表，加速大目录扫描；权限不足时自动回退普通扫描，不影响结果。
          </small>
        </span>
      </label>
      <label>
        相似判定阈值（pHash 距离上限，4–20）
        <input
          value={thresholdInput}
          onChange={(event) => setThresholdInput(event.target.value)}
          onBlur={saveSimilarThreshold}
          onKeyDown={(event) => event.key === "Enter" && saveSimilarThreshold()}
          inputMode="numeric"
          placeholder="10"
        />
        <small className="field-hint">
          越小越严格（相似候选更少更准），越大越宽松；修改后相似照片列表立即按新阈值刷新。
        </small>
      </label>
      <label className="toggle-row">
        <input
          type="checkbox"
          checked={watchScan}
          onChange={toggleWatchScan}
        />
        <span>
          实时监控扫描目录（实验性）
          <small>
            扫描目录里出现新增、修改或移动后，静置约三秒自动做一次增量扫描，无需手动刷新；关闭应用后监控随之停止。
          </small>
        </span>
      </label>
      <button
        disabled={busyKeys.has("app") || busyKeys.has("init") || !database || !trash}
        onClick={() => void initialize()}
      >
        保存并打开项目
      </button>
      <div className="cache-panel">
        <b>预览缓存</b>
        <span>
          {cacheStats
            ? `缩略图与预览缓存占用 ${formatBytes(cacheStats.bytes)}（${cacheStats.files} 个文件）。`
            : "缓存统计不可用。"}
        </span>
        <button
          className="secondary"
          disabled={busyKeys.has("cache-clear") || !cacheStats || cacheStats.files === 0}
          onClick={clearThumbnailCache}
        >
          清理缓存
        </button>
      </div>
      <div className="safety">
        <b>安全承诺</b>
        <span>
          扫描不会修改文件。移动只针对人工确认的精确重复副本，且执行前后均验证
          BLAKE3 哈希。
        </span>
      </div>
        </>
      )}
    </section>
  );
}

// The guide's wording lives in src/lib/help-content.ts; this renderer only
// knows the two inline conventions used there: **bold** and [[key]].
function renderInline(text: string): ReactNode {
  const parts = text.split(/(\*\*[^*]+\*\*|\[\[[^\]]+\]\])/g).filter(Boolean);
  return parts.map((part, index) => {
    if (part.startsWith("**") && part.endsWith("**")) {
      return <b key={index}>{part.slice(2, -2)}</b>;
    }
    if (part.startsWith("[[") && part.endsWith("]]")) {
      return <kbd key={index}>{part.slice(2, -2)}</kbd>;
    }
    return part;
  });
}

function HelpPage() {
  return (
    <div className="help-page">
      {HELP_SECTIONS.map((section) => (
        <section className="help-section" key={section.title}>
          <h3>{section.title}</h3>
          {section.blocks.map((block, index) =>
            block.kind === "p" ? (
              <p key={index}>{renderInline(block.text)}</p>
            ) : block.kind === "ol" ? (
              <ol key={index}>
                {block.items.map((item, itemIndex) => (
                  <li key={itemIndex}>{renderInline(item)}</li>
                ))}
              </ol>
            ) : (
              <ul key={index}>
                {block.items.map((item, itemIndex) => (
                  <li key={itemIndex}>{renderInline(item)}</li>
                ))}
              </ul>
            ),
          )}
        </section>
      ))}
      <section className="help-section" key="faq">
        <h3>常见问题</h3>
        {HELP_FAQ.map((item) => (
          <details className="help-faq" key={item.q}>
            <summary>{item.q}</summary>
            <p>{renderInline(item.a)}</p>
          </details>
        ))}
      </section>
    </div>
  );
}

