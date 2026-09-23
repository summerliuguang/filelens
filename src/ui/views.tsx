import { useEffect, useState } from "react";
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
  Status,
  ThemeSetting,
  ThumbnailCacheStats,
  TrashItem,
} from "../lib/types";

export function Overview({
  status,
  groups,
  selected,
  onNavigate,
}: {
  status: Status | null;
  groups: Group[];
  selected: number;
  onNavigate: (page: Page) => void;
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
          <button onClick={() => onNavigate("sources")}>管理扫描来源</button>
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
}: {
  roots: string[];
  rootInput: string;
  setRootInput: (value: string) => void;
  addRoot: (value: string) => boolean;
  removeRoot: (root: string) => void;
  scan: () => void;
  disabled: boolean;
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
            <button onClick={() => void pickDirectory()}>选择第一个目录</button>
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
}: {
  database: string;
  groups: Group[];
  totalGroups: number;
  filters: GroupFilters;
  onFilters: (next: GroupFilters) => void;
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
  hasMore: boolean;
  onLoadMore: () => Promise<void>;
  indexedFiles: number;
  onNavigate: (page: Page) => void;
}) {
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  const [pendingRemove, setPendingRemove] = useState<GroupFile | null>(null);
  const [pendingBatch, setPendingBatch] = useState<Group | null>(null);
  const [searchInput, setSearchInput] = useState(filters.search);
  const [bulkStrategy, setBulkStrategy] = useState<
    "newest" | "oldest" | "shortest"
  >("newest");
  const [pendingBulk, setPendingBulk] = useState<
    { kind: "mark" } | { kind: "recycle"; fileIds: number[] } | null
  >(null);

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
      });
      all.push(...page.groups);
      if (page.groups.length === 0 || all.length >= page.total) break;
      offset += 200;
    }
    return all;
  }

  const bulkStrategyLabel =
    bulkStrategy === "newest"
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
    void execute(async () => {
      const fileIds = target.files
        .filter((file) => file.approved)
        .map((file) => file.id);
      const result =
        mode === "trash"
          ? await invoke<string>("trash_approved", { database, fileIds })
          : await invoke<string>("delete_direct_batch", { database, fileIds });
      await refresh();
      return result;
    }, "batch");
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
            仅显示完整 BLAKE3 哈希一致的文件。每组至少保留一个副本；点击文件可放大预览。
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
          <option value="size">按大小</option>
          <option value="members">按副本数</option>
          <option value="path">按路径</option>
        </select>
        {totalGroups > 0 && (
          <>
            <select
              value={bulkStrategy}
              onChange={(event) =>
                setBulkStrategy(event.target.value as typeof bulkStrategy)
              }
              aria-label="批量保留策略"
            >
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
        {(filters.search || filters.minSize > 0 || filters.sort !== "size") && (
          <button
            className="text-button"
            onClick={() => {
              setSearchInput("");
              onFilters({ search: "", minSize: 0, sort: "size" });
            }}
          >
            重置筛选
          </button>
        )}
      </div>
      {groups.length === 0 ? (
        <Empty
          icon="⊞"
          text={totalGroups === 0 ? "没有待审核的精确重复" : "当前筛选没有匹配的重复组"}
          detail={
            totalGroups === 0
              ? indexedFiles === 0
                ? "添加扫描目录并完成首次扫描后，重复组会显示在这里。"
                : "完成扫描后，重复组会按可释放空间显示在这里。"
              : "试试放宽大小阈值或更换关键字。"
          }
          action={
            totalGroups === 0 && indexedFiles === 0
              ? { label: "去添加扫描目录", onClick: () => onNavigate("sources") }
              : undefined
          }
        />
      ) : (
        <div className="groups">
          {groups.map((group, index) => (
            <article className="group" key={group.hash}>
              <div className="group-header">
                <div>
                  <span>重复组 {String(index + 1).padStart(2, "0")}</span>
                  <h3>{group.files.length} 个内容相同的文件</h3>
                </div>
                <div>
                  <b>{formatBytes(group.size)}</b>
                  <small>每个文件</small>
                </div>
              </div>
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
              {group.files.map((file) => (
                <div className="file-row" key={file.id}>
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
              ))}
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
            </article>
          ))}
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
                          });
                          await refresh();
                          return result;
                        }
                        const result = await invoke<string>("trash_approved", {
                          database,
                          fileIds: current.fileIds,
                        });
                        await refresh();
                        return result;
                      }, "bulk");
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
  const [image, setImage] = useState<string | null>(null);
  useEffect(() => {
    void invoke<string | null>("image_thumbnail", { path })
      .then(setImage)
      .catch(() => setImage(null));
  }, [path]);
  return image ? (
    <img
      className="thumbnail"
      src={image}
      alt="文件缩略图"
    />
  ) : (
    <span className="thumbnail thumbnail-placeholder">▧</span>
  );
}

export function SimilarPhotos({
  database,
  photos,
  duplicates,
  busy,
  busyKeys,
  execute,
  refresh,
}: {
  database: string;
  photos: SimilarPhoto[];
  duplicates: SimilarPhoto[];
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const [visible, setVisible] = useState(60);
  const [comparePair, setComparePair] = useState<SimilarPhoto | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [pendingDelete, setPendingDelete] = useState(false);
  const [view, setView] = useState<"duplicate" | "similar">("similar");
  const [dirFilter, setDirFilter] = useState("all");
  const [pendingDirTrash, setPendingDirTrash] = useState<{
    dir: string;
    paths: string[];
  } | null>(null);

  // duplicate: identical pixels, differing bytes (a copy with drifted EXIF
  // or metadata). similar: near-identical pixels, dHash distance 1-4.
  const base = view === "duplicate" ? duplicates : photos;

  function switchView(next: "duplicate" | "similar") {
    setView(next);
    setDirFilter("all");
    setVisible(60);
    setSelected(new Set());
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
  const shown = matched.slice(0, visible);

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
    const paths = [
      ...new Set(
        matched
          .flatMap((photo) => [photo.first_path, photo.second_path])
          .filter((path) => fileFolder(path) === dir),
      ),
    ];
    if (paths.length > 0) setPendingDirTrash({ dir, paths });
  }

  return (
    <section className="panel review-page">
      <div className="section-head">
        <div>
          <h2>{view === "duplicate" ? "重复照片" : "相似照片"}</h2>
          <p>
            {view === "duplicate"
              ? "像素完全一致、但复制时 EXIF 等信息略有不同的照片。仅供人工查看，确认后可移入回收站。"
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
              重复（{duplicates.length} 对）
            </button>
            <button
              type="button"
              className={view === "similar" ? "on" : ""}
              onClick={() => switchView("similar")}
            >
              相似（{photos.length} 对）
            </button>
          </div>
          {directories.length > 1 && (
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
          {dirFilter !== "all" && (
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
          <span className="pill">{matched.length} 对</span>
        </div>
      </div>
      {base.length === 0 ? (
        <Empty
          icon="◒"
          text={view === "duplicate" ? "没有重复照片候选" : "没有高置信度相似照片"}
          detail={
            view === "duplicate"
              ? "完成扫描后，像素完全一致但字节不同的照片（复制导致信息略异）会显示在这里。"
              : "完成扫描后，这里会显示重压缩或缩放后的同源照片候选。"
          }
        />
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
                    {photo.distance === 0 ? "像素级一致（字节不同）" : `dHash 差异 ${photo.distance}/64`}
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
          onClose={() => setComparePair(null)}
        />
      )}
      {pendingDirTrash && (
        <ConfirmDialog
          title={`回收目录 ${pendingDirTrash.dir} 中的候选照片`}
          detail={`将把该目录中出现的 ${pendingDirTrash.paths.length} 张候选照片移入应用回收站（可恢复）。若某一对的两张都在该目录，两张都会被移入；移动前逐张校验内容与保护规则。`}
          busy={busy}
          onClose={() => setPendingDirTrash(null)}
          options={[
            {
              label: "移入回收站",
              action: () => {
                const { paths } = pendingDirTrash;
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
          ]}
        />
      )}
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
}: {
  documents: SimilarDocument[];
  execute: (action: () => Promise<string>) => Promise<void>;
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
          text="没有高置信度相似文档"
          detail="扫描 TXT、Markdown、CSV、JSON、XML 或 HTML 后会显示候选。"
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
}: {
  database: string;
  items: TrashItem[];
  busy: boolean;
  busyKeys: ReadonlySet<string>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const [pendingEmpty, setPendingEmpty] = useState(false);
  const [pendingPrune, setPendingPrune] = useState(false);
  const [pendingItem, setPendingItem] = useState<TrashItem | null>(null);
  const [pendingBatchRestore, setPendingBatchRestore] = useState<{
    id: number;
    count: number;
  } | null>(null);
  const expiredCount = items.filter((item) => item.expired).length;
  return (
    <section className="panel trash-page">
      <div className="section-head">
        <div>
          <h2>可恢复文件</h2>
          <p>恢复时不会覆盖原路径已有文件，并会再次进行内容完整性检查。</p>
        </div>
        <div className="head-actions">
          <span className="pill">{items.length} 个文件</span>
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
export function History({ database, active }: { database: string; active: boolean }) {
  const [items, setItems] = useState<HistoryItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [exhausted, setExhausted] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);

  // Reload only while the page is visible: the component stays mounted so
  // scroll position and loaded pages survive switching away and back.
  useEffect(() => {
    if (!active || !database) return;
    setLoading(true);
    invoke<HistoryItem[]>("history", { database, offset: 0, limit: HISTORY_PAGE })
      .then((next) => {
        setItems(next);
        setExhausted(next.length < HISTORY_PAGE);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [database, active]);

  function loadMore() {
    if (loadingMore) return;
    setLoadingMore(true);
    invoke<HistoryItem[]>("history", {
      database,
      offset: items.length,
      limit: HISTORY_PAGE,
    })
      .then((next) => {
        setItems((current) => [...current, ...next]);
        setExhausted(next.length < HISTORY_PAGE);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoadingMore(false));
  }

  const stateBadge: Record<string, { label: string; className: string }> = {
    trashed: { label: "已入回收站", className: "protected" },
    restored: { label: "已恢复", className: "approved" },
    deleted: { label: "已永久删除", className: "deleted" },
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
  protectRules,
  excludeRules,
  minFileSize,
  retentionDays,
  autoScan,
  setDatabase,
  setTrash,
  setProtectRules,
  setExcludeRules,
  setMinFileSize,
  setRetentionDays,
  setAutoScan,
  initialize,
  execute,
  notify,
  busy,
  busyKeys,
}: {
  database: string;
  trash: string;
  theme: ThemeSetting;
  setTheme: (value: ThemeSetting) => void;
  protectRules: string[];
  excludeRules: string[];
  minFileSize: number;
  retentionDays: number;
  autoScan: boolean;
  setDatabase: (value: string) => void;
  setTrash: (value: string) => void;
  setProtectRules: (value: string[]) => void;
  setExcludeRules: (value: string[]) => void;
  setMinFileSize: (value: number) => void;
  setRetentionDays: (value: number) => void;
  setAutoScan: (value: boolean) => void;
  initialize: () => Promise<void>;
  execute: (action: () => Promise<string>, key?: string) => Promise<void>;
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
  const [cacheStats, setCacheStats] = useState<ThumbnailCacheStats | null>(null);

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
          <h2>项目与安全设置</h2>
          <p>数据库记录扫描结果和操作日志；回收站必须位于客户端本地磁盘。</p>
        </div>
      </div>
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
    </section>
  );
}

