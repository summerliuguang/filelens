import { useEffect, useRef, useState, type ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./styles.css";

type Page =
  | "overview"
  | "sources"
  | "review"
  | "similar"
  | "documents"
  | "detectors"
  | "trash"
  | "history"
  | "settings";
type GroupFile = {
  id: number;
  path: string;
  protected: boolean;
  approved: boolean;
  modified: number;
};
type Group = { hash: string; size: number; files: GroupFile[] };
type Status = {
  files: number;
  duplicates: number;
  approved: number;
  in_trash: number;
  last_scan_at: number | null;
  groups: number;
  recoverable_bytes: number;
};
type TrashItem = {
  id: number;
  created_at: number;
  source_path: string;
  trash_path: string;
  expired: boolean;
};
type HistoryItem = {
  id: number;
  created_at: number;
  source_path: string;
  trash_path: string;
  state: "trashed" | "restored" | "deleted" | string;
  restored_at: number | null;
};
type ProjectConfig = {
  trash_path: string;
  roots: string[];
  protect_rules: string[];
};
type ProjectState = ProjectConfig & {
  database: string;
  trash_retention_days: number;
  auto_scan: boolean;
};
type ScanState = {
  state: "idle" | "running" | "completed" | "cancelled" | "failed";
  processed: number;
  total: number;
  message: string;
};
type SimilarPhoto = {
  first_path: string;
  second_path: string;
  distance: number;
  first_size: number;
  first_modified: number;
  second_size: number;
  second_modified: number;
};
type SimilarDocument = {
  first_path: string;
  second_path: string;
  distance: number;
  first_size: number;
  first_modified: number;
  second_size: number;
  second_modified: number;
};
type DetectorStatus = { name: string; available: boolean; detail: string };

const nav: { id: Page; icon: string; label: string; caption: string }[] = [
  { id: "overview", icon: "◌", label: "概览", caption: "ARCHIVE HEALTH" },
  { id: "sources", icon: "⌁", label: "扫描来源", caption: "SCAN SOURCES" },
  { id: "review", icon: "⊞", label: "重复审核", caption: "REVIEW QUEUE" },
  { id: "similar", icon: "◒", label: "相似照片", caption: "SIMILAR PHOTOS" },
  { id: "documents", icon: "≡", label: "相似文档", caption: "SIMILAR DOCUMENTS" },
  { id: "detectors", icon: "◉", label: "检测能力", caption: "DETECTORS" },
  { id: "trash", icon: "↶", label: "应用回收站", caption: "RECOVERY" },
  { id: "history", icon: "⧗", label: "操作历史", caption: "OPERATION LOG" },
  { id: "settings", icon: "⚙", label: "项目设置", caption: "PROJECT SETTINGS" },
];

const GROUPS_PAGE = 50;

function App() {
  const [page, setPage] = useState<Page>("overview");
  const [database, setDatabase] = useState("");
  const [trash, setTrash] = useState("");
  const [roots, setRoots] = useState<string[]>([]);
  const [protectRules, setProtectRules] = useState<string[]>([]);
  const [retentionDays, setRetentionDays] = useState(30);
  const [autoScan, setAutoScan] = useState(true);
  const [rootInput, setRootInput] = useState("");
  const [status, setStatus] = useState<Status | null>(null);
  const [groups, setGroups] = useState<Group[]>([]);
  const [groupsExhausted, setGroupsExhausted] = useState(true);
  const [trashItems, setTrashItems] = useState<TrashItem[]>([]);
  const [similarPhotos, setSimilarPhotos] = useState<SimilarPhoto[]>([]);
  const [similarDocuments, setSimilarDocuments] = useState<SimilarDocument[]>([]);
  const [detectors, setDetectors] = useState<DetectorStatus[]>([]);
  const [message, setMessage] = useState("正在打开本地项目...");
  const [busy, setBusy] = useState(false);
  const [scanState, setScanState] = useState<ScanState>({
    state: "idle",
    processed: 0,
    total: 0,
    message: "",
  });
  const progressHistory = useRef<{ t: number; processed: number }[]>([]);

  useEffect(() => {
    invoke<ProjectState>("open_project")
      .then((project) => {
        setDatabase(project.database);
        setTrash(project.trash_path);
        setRoots(project.roots);
        setProtectRules(project.protect_rules);
        setRetentionDays(project.trash_retention_days);
        setAutoScan(project.auto_scan);
        if (project.auto_scan && project.roots.length > 0) {
          void startScan(project.database, project.roots, project.protect_rules);
        } else {
          setMessage("本地项目已打开。");
          void refresh(project.database);
        }
      })
      .catch((error) => setMessage(`自动打开本地项目失败：${String(error)}`));
  }, []);

  useEffect(() => {
    if (scanState.state !== "running") return;
    progressHistory.current = [];
    const timer = window.setInterval(() => {
      invoke<ScanState>("scan_state")
        .then((next) => {
          const now = Date.now();
          const history = progressHistory.current;
          history.push({ t: now, processed: next.processed });
          while (history.length > 1 && now - history[0].t > 8000) history.shift();
          setScanState(next);
          if (next.state !== "running") {
            setMessage(next.message);
            void refresh();
          }
        })
        .catch((error) =>
          setScanState({
            state: "failed",
            processed: 0,
            total: 0,
            message: String(error),
          }),
        );
    }, 500);
    return () => window.clearInterval(timer);
  }, [scanState.state]);

  async function execute(action: () => Promise<string>) {
    setBusy(true);
    try {
      setMessage(await action());
    } catch (error) {
      setMessage(friendlyError(error));
    } finally {
      setBusy(false);
    }
  }

  async function refresh(target = database) {
    if (!target) return;
    await execute(async () => {
      const [nextStatus, nextGroups, nextTrash, nextSimilarPhotos, nextDocuments, nextDetectors] = await Promise.all([
        invoke<Status>("status", { database: target }),
        invoke<Group[]>("groups", { database: target, offset: 0, limit: GROUPS_PAGE }),
        invoke<TrashItem[]>("trash_list", { database: target }),
        invoke<SimilarPhoto[]>("similar_photos", { database: target }),
        invoke<SimilarDocument[]>("similar_documents", { database: target }),
        invoke<DetectorStatus[]>("detector_status"),
      ]);
      setStatus(nextStatus);
      setGroups(nextGroups);
      setGroupsExhausted(nextGroups.length < GROUPS_PAGE);
      setTrashItems(nextTrash);
      setSimilarPhotos(nextSimilarPhotos);
      setSimilarDocuments(nextDocuments);
      setDetectors(nextDetectors);
      return `索引已更新：${nextStatus.groups} 个精确重复组待审核。`;
    });
  }

  async function initialize() {
    await execute(async () => {
      const result = await invoke<string>("save_project_config", {
        database,
        trash,
        roots,
        protectRules,
      });
      await refresh(database);
      return result;
    });
  }

  async function startScan(
    targetDatabase: string,
    targetRoots: string[],
    targetRules: string[],
  ) {
    await execute(async () => {
      const result = await invoke<string>("start_scan", {
        database: targetDatabase,
        roots: targetRoots,
        protectRules: targetRules,
      });
      setScanState({ state: "running", processed: 0, total: 0, message: result });
      return result;
    });
  }

  async function loadMoreGroups() {
    if (!database) return;
    await execute(async () => {
      const next = await invoke<Group[]>("groups", {
        database,
        offset: groups.length,
        limit: GROUPS_PAGE,
      });
      setGroups((current) => [...current, ...next]);
      setGroupsExhausted(next.length < GROUPS_PAGE);
      return `已加载 ${groups.length + next.length} 个重复组。`;
    });
  }

  async function cancelScan() {
    await execute(() => invoke<string>("cancel_scan"));
  }

  async function persistRoots(next: string[]) {
    if (!database) return;
    try {
      await invoke("save_roots", { database, roots: next, protectRules });
    } catch (error) {
      setMessage(friendlyError(error));
    }
  }

  function addRootPath(value: string) {
    const trimmed = value.trim();
    if (!trimmed || roots.includes(trimmed)) return false;
    const next = [...roots, trimmed];
    setRoots(next);
    void persistRoots(next);
    return true;
  }
  function removeRootPath(root: string) {
    const next = roots.filter((item) => item !== root);
    setRoots(next);
    void persistRoots(next);
  }
  const active = nav.find((item) => item.id === page)!;
  const selected = groups.reduce(
    (total, group) =>
      total + group.files.filter((file) => file.approved).length,
    0,
  );

  return (
    <div className="app-shell">
      <aside>
        <div className="brand">
          <span className="brand-mark">+</span>
          <div>
            <b>FileLens</b>
            <small>本地文件整理</small>
          </div>
        </div>
        <nav>
          {nav.map((item) => (
            <button
              key={item.id}
              className={page === item.id ? "active" : ""}
              onClick={() => setPage(item.id)}
            >
              <span>{item.icon}</span>
              {item.label}
              {item.id === "review" && status && status.duplicates > 0 && (
                <em>{status.duplicates}</em>
              )}
              {item.id === "trash" && trashItems.length > 0 && (
                <em>{trashItems.length}</em>
              )}
            </button>
          ))}
        </nav>
        <div className="side-note">
          <span>安全模式</span>
          <b>仅人工确认后移动</b>
          <small>完整哈希二次校验</small>
        </div>
      </aside>
      <main>
        <header>
          <div>
            <p className="eyebrow">{active.caption}</p>
            <h1>{active.label}</h1>
          </div>
          <button
            className="secondary"
            disabled={busy || !database}
            onClick={() => refresh()}
          >
            ↻ 刷新
          </button>
        </header>
        {scanState.state === "running" && <ScanProgress scanState={scanState} history={progressHistory.current} onCancel={cancelScan} disabled={busy} />}
        {page === "overview" && (
          <Overview
            status={status}
            groups={groups}
            selected={selected}
            onNavigate={setPage}
          />
        )}
        {page === "sources" && (
          <Sources
            roots={roots}
            rootInput={rootInput}
            setRootInput={setRootInput}
            addRoot={addRootPath}
            removeRoot={removeRootPath}
            scan={() => startScan(database, roots, protectRules)}
            disabled={busy || !database || scanState.state === "running"}
          />
        )}
        {page === "review" && (
          <Review
            database={database}
            groups={groups}
            totalGroups={status ? status.groups : groups.length}
            busy={busy}
            execute={execute}
            refresh={refresh}
            hasMore={!groupsExhausted}
            onLoadMore={loadMoreGroups}
          />
        )}
        {page === "similar" && (
          <SimilarPhotos
            database={database}
            photos={similarPhotos}
            busy={busy}
            execute={execute}
            refresh={refresh}
          />
        )}
        {page === "documents" && <SimilarDocuments documents={similarDocuments} />}
        {page === "detectors" && <Detectors detectors={detectors} />}
        {page === "trash" && (
          <Trash
            database={database}
            items={trashItems}
            busy={busy}
            execute={execute}
            refresh={refresh}
          />
        )}
        {page === "history" && <History database={database} />}
        {page === "settings" && (
          <Settings
            database={database}
            trash={trash}
            protectRules={protectRules}
            retentionDays={retentionDays}
            autoScan={autoScan}
            setDatabase={setDatabase}
            setTrash={setTrash}
            setProtectRules={setProtectRules}
            setRetentionDays={setRetentionDays}
            setAutoScan={setAutoScan}
            initialize={initialize}
            execute={execute}
            notify={setMessage}
            busy={busy}
          />
        )}
        <footer className={message.startsWith("操作失败") ? "error" : ""}>
          {busy ? "正在处理，请不要关闭程序..." : message}
        </footer>
      </main>
    </div>
  );
}

function ScanProgress({
  scanState,
  history,
  onCancel,
  disabled,
}: {
  scanState: ScanState;
  history: { t: number; processed: number }[];
  onCancel: () => void;
  disabled: boolean;
}) {
  const percent =
    scanState.total > 0
      ? Math.min(100, Math.round((scanState.processed / scanState.total) * 100))
      : null;
  let rate: number | null = null;
  if (history.length >= 2) {
    const seconds = (history[history.length - 1].t - history[0].t) / 1000;
    const delta = history[history.length - 1].processed - history[0].processed;
    if (seconds > 0 && delta > 0) rate = delta / seconds;
  }
  const etaSeconds =
    rate !== null && scanState.total > scanState.processed
      ? (scanState.total - scanState.processed) / rate
      : null;
  return (
    <section className="scan-progress">
      <div className="scan-progress-head">
        <b>后台扫描中</b>
        <span>
          已处理 {scanState.processed.toLocaleString()}
          {scanState.total > 0
            ? ` / ${scanState.total.toLocaleString()} 个条目`
            : " 个条目"}
          {rate !== null && ` · ${formatRate(rate)}`}
          {etaSeconds !== null && ` · ${formatEta(etaSeconds)}`}
        </span>
      </div>
      <div className="progress-track">
        <i
          className={percent === null ? "indeterminate" : ""}
          style={percent === null ? undefined : { width: `${percent}%` }}
        />
      </div>
      <div className="scan-progress-foot">
        <span>
          {percent === null ? "正在统计文件数量..." : `${percent}%`}
        </span>
        <button className="secondary" disabled={disabled} onClick={onCancel}>
          取消扫描
        </button>
      </div>
    </section>
  );
}

function Overview({
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

function Sources({
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
          <Empty
            icon="⌁"
            text="还没有扫描目录"
            detail="添加本地同步目录后，即可建立内容索引。"
          />
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

function Review({
  database,
  groups,
  totalGroups,
  busy,
  execute,
  refresh,
  hasMore,
  onLoadMore,
}: {
  database: string;
  groups: Group[];
  totalGroups: number;
  busy: boolean;
  execute: (action: () => Promise<string>) => Promise<void>;
  refresh: () => Promise<void>;
  hasMore: boolean;
  onLoadMore: () => Promise<void>;
}) {
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  const [pendingRemove, setPendingRemove] = useState<GroupFile | null>(null);
  const [pendingBatch, setPendingBatch] = useState<Group | null>(null);

  // mode: "trash" recycles (recoverable), "delete" removes permanently.
  function confirmRemoval(file: GroupFile, mode: "trash" | "delete") {
    void execute(async () => {
      const result =
        mode === "trash"
          ? await invoke<string>("trash", { database, fileId: file.id })
          : await invoke<string>("delete_direct", { database, fileId: file.id });
      await refresh();
      return result;
    });
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
    });
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
      {groups.length === 0 ? (
        <Empty
          icon="⊞"
          text="没有待审核的精确重复"
          detail="完成扫描后，重复组会按可释放空间显示在这里。"
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
                  <div className="file-actions">
                    {file.approved ? (
                      <>
                        <span className="approved">待删除</span>
                        <button
                          className="secondary"
                          disabled={busy}
                          onClick={() =>
                            execute(async () => {
                              const result = await invoke<string>("unapprove", {
                                database,
                                fileId: file.id,
                              });
                              await refresh();
                              return "已取消删除标记，该副本将保留。";
                            })
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
                        disabled={busy || file.protected}
                        onClick={() =>
                          execute(async () => {
                            const result = await invoke<string>("approve", {
                              database,
                              fileId: file.id,
                            });
                            await refresh();
                            return "已标记删除该副本，确认后才会执行。";
                          })
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
                加载更多重复组
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

function SimilarPhotos({
  database,
  photos,
  busy,
  execute,
  refresh,
}: {
  database: string;
  photos: SimilarPhoto[];
  busy: boolean;
  execute: (action: () => Promise<string>) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const [visible, setVisible] = useState(60);
  const [previewPath, setPreviewPath] = useState<string | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [pendingDelete, setPendingDelete] = useState(false);
  const shown = photos.slice(0, visible);

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
    });
  }

  return (
    <section className="panel review-page">
      <div className="section-head">
        <div>
          <h2>高置信度相似照片</h2>
          <p>仅供人工查看。点击照片放大预览，勾选后可批量移入回收站或删除。</p>
        </div>
        <div className="head-actions">
          {selected.size > 0 && (
            <span className="pill">已选 {selected.size} 个</span>
          )}
          {selected.size > 0 && (
            <button className="danger" disabled={busy} onClick={() => setPendingDelete(true)}>
              删除选中文件
            </button>
          )}
          <span className="pill">{photos.length} 对</span>
        </div>
      </div>
      {photos.length === 0 ? (
        <Empty
          icon="◒"
          text="没有高置信度相似照片"
          detail="完成扫描后，这里会显示重压缩或缩放后的同源照片候选。"
        />
      ) : (
        <div className="similar-list">
          {shown.map((photo, index) => (
            <article className="similar-pair" key={`${photo.first_path}-${photo.second_path}`}>
              <div className="similar-photos">
                <PhotoSlot
                  photo={photo}
                  side="first"
                  previewPath={previewPath}
                  onPreview={setPreviewPath}
                  selected={selected}
                  onToggle={toggleSelection}
                />
                <PhotoSlot
                  photo={photo}
                  side="second"
                  previewPath={previewPath}
                  onPreview={setPreviewPath}
                  selected={selected}
                  onToggle={toggleSelection}
                />
              </div>
              <div>
                <b>候选 {index + 1}</b>
                <PhotoInfo path={photo.first_path} size={photo.first_size} modified={photo.first_modified} />
                <PhotoInfo path={photo.second_path} size={photo.second_size} modified={photo.second_modified} />
              </div>
              <small>dHash 差异 {photo.distance}/64</small>
            </article>
          ))}
          {visible < photos.length && (
            <div className="load-more">
              <button
                className="secondary"
                onClick={() => setVisible((count) => count + 120)}
              >
                显示更多（还有 {photos.length - visible} 对）
              </button>
            </div>
          )}
        </div>
      )}
      {previewPath && (
        <PreviewModal path={previewPath} onClose={() => setPreviewPath(null)} />
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
  previewPath,
  onPreview,
  selected,
  onToggle,
}: {
  photo: SimilarPhoto;
  side: "first" | "second";
  previewPath: string | null;
  onPreview: (path: string) => void;
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
        title="点击放大预览"
        onClick={() => onPreview(path)}
      >
        <Thumbnail path={path} />
      </button>
    </div>
  );
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

function SimilarDocuments({ documents }: { documents: SimilarDocument[] }) {
  const [visible, setVisible] = useState(60);
  const shown = documents.slice(0, visible);
  return <section className="panel review-page"><div className="section-head"><div><h2>相似文档</h2><p>文本近似仅供人工查看，不能作为自动处理依据。</p></div><span className="pill">{documents.length} 对</span></div>{documents.length === 0 ? <Empty icon="≡" text="没有高置信度相似文档" detail="扫描 TXT、Markdown、CSV、JSON、XML 或 HTML 后会显示候选。" /> : <div className="similar-list">{shown.map((document, index) => <article className="similar-pair" key={`${document.first_path}-${document.second_path}`}><div><b>候选 {index + 1}</b><PhotoInfo path={document.first_path} size={document.first_size} modified={document.first_modified} /><PhotoInfo path={document.second_path} size={document.second_size} modified={document.second_modified} /></div><small>SimHash 差异 {document.distance}/64</small></article>)}{visible < documents.length && (<div className="load-more"><button className="secondary" onClick={() => setVisible((count) => count + 120)}>显示更多（还有 {documents.length - visible} 对）</button></div>)}</div>}</section>;
}

function Detectors({ detectors }: { detectors: DetectorStatus[] }) {
  return <section className="panel review-page"><div className="section-head"><div><h2>检测能力</h2><p>所有处理均在本地进行。未启用的能力不会以低质量算法替代。</p></div></div><div className="source-list">{detectors.map(detector => <article className="source" key={detector.name}><span className="source-icon">{detector.available ? "✓" : "·"}</span><div><b>{detector.name}</b><small>{detector.detail}</small></div><span className={detector.available ? "approved" : "protected"}>{detector.available ? "已启用" : "待接入"}</span></article>)}</div></section>;
}

function Trash({
  database,
  items,
  busy,
  execute,
  refresh,
}: {
  database: string;
  items: TrashItem[];
  busy: boolean;
  execute: (action: () => Promise<string>) => Promise<void>;
  refresh: () => Promise<void>;
}) {
  const [pendingEmpty, setPendingEmpty] = useState(false);
  const [pendingPrune, setPendingPrune] = useState(false);
  const [pendingItem, setPendingItem] = useState<TrashItem | null>(null);
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
          {items.map((item) => (
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
                  disabled={busy}
                  onClick={() =>
                    execute(async () => {
                      const result = await invoke<string>("restore", {
                        database,
                        operationId: item.id,
                      });
                      await refresh();
                      return result;
                    })
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
                });
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
                });
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
                });
              },
            },
          ]}
        />
      )}
    </section>
  );
}

const HISTORY_PAGE = 100;

function History({ database }: { database: string }) {
  const [items, setItems] = useState<HistoryItem[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState("");
  const [exhausted, setExhausted] = useState(false);
  const [loadingMore, setLoadingMore] = useState(false);

  useEffect(() => {
    if (!database) return;
    setLoading(true);
    invoke<HistoryItem[]>("history", { database, offset: 0, limit: HISTORY_PAGE })
      .then((next) => {
        setItems(next);
        setExhausted(next.length < HISTORY_PAGE);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [database]);

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

function Settings({
  database,
  trash,
  protectRules,
  retentionDays,
  autoScan,
  setDatabase,
  setTrash,
  setProtectRules,
  setRetentionDays,
  setAutoScan,
  initialize,
  execute,
  notify,
  busy,
}: {
  database: string;
  trash: string;
  protectRules: string[];
  retentionDays: number;
  autoScan: boolean;
  setDatabase: (value: string) => void;
  setTrash: (value: string) => void;
  setProtectRules: (value: string[]) => void;
  setRetentionDays: (value: number) => void;
  setAutoScan: (value: boolean) => void;
  initialize: () => Promise<void>;
  execute: (action: () => Promise<string>) => Promise<void>;
  notify: (message: string) => void;
  busy: boolean;
}) {
  const [rule, setRule] = useState("");
  const [retentionInput, setRetentionInput] = useState(String(retentionDays));

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
    });
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
    });
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
              if (rule.trim() && !protectRules.includes(rule.trim()))
                setProtectRules([...protectRules, rule.trim()]);
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
              onClick={() =>
                setProtectRules(protectRules.filter((value) => value !== item))
              }
            >
              ×
            </button>
          </span>
        ))}
      </div>
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
      <button disabled={busy || !database || !trash} onClick={initialize}>
        保存并打开项目
      </button>
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

function Empty({
  icon,
  text,
  detail,
}: {
  icon: string;
  text: string;
  detail: string;
}) {
  return (
    <div className="empty">
      <span>{icon}</span>
      <b>{text}</b>
      <p>{detail}</p>
    </div>
  );
}

type ConfirmOption = {
  label: string;
  kind?: "primary" | "secondary" | "danger";
  action: () => void;
};

// Unified confirmation dialog: explicit cancel button (the only exit in the
// earlier review dialogs was clicking the backdrop), Escape to close, and
// ARIA roles. All destructive flows go through this component.
function ConfirmDialog({
  title,
  detail,
  note,
  options,
  busy,
  onClose,
}: {
  title: string;
  detail?: ReactNode;
  note?: string;
  options: ConfirmOption[];
  busy: boolean;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onClick={onClose} role="presentation">
      <div
        className="modal modal-confirm"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(event) => event.stopPropagation()}
      >
        <h3>{title}</h3>
        {detail && <p>{detail}</p>}
        {note && <p className="modal-note">{note}</p>}
        <div className="modal-actions-row">
          {options.map((option) => (
            <button
              key={option.label}
              className={option.kind ?? "secondary"}
              disabled={busy}
              onClick={option.action}
            >
              {option.label}
            </button>
          ))}
          <button
            className="text-button modal-cancel"
            disabled={busy}
            onClick={onClose}
          >
            取消
          </button>
        </div>
      </div>
    </div>
  );
}
function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

function formatFileTime(unixSeconds: number) {
  return new Date(unixSeconds * 1000).toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

const LARGE_FILE_THRESHOLD = 1024 * 1024 * 1024;

function fileFolder(path: string) {
  const parts = path.split(/[\\/]/);
  return parts.slice(0, -1).join("/");
}

function fileName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}

function PreviewModal({
  path,
  onClose,
}: {
  path: string;
  onClose: () => void;
}) {
  const [image, setImage] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    setImage(null);
    setFailed(false);
    void invoke<string | null>("image_preview", { path })
      .then((result) => {
        if (result) setImage(result);
        else setFailed(true);
      })
      .catch(() => setFailed(true));
  }, [path]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onClick={onClose} role="presentation">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label={`预览 ${fileName(path)}`}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="modal-head">
          <div>
            <b>{fileName(path)}</b>
            <span>{fileFolder(path)}</span>
          </div>
          <div className="modal-actions">
            <button
              className="secondary"
              onClick={() =>
                void invoke("open_file", { path }).catch((error) =>
                  console.error("open_file failed:", error),
                )
              }
            >
              用系统程序打开
            </button>
            <button className="secondary" onClick={onClose}>
              关闭
            </button>
          </div>
        </div>
        {image && <img className="preview-image" src={image} alt={fileName(path)} />}
        {failed && (
          <div className="preview-fallback">
            <b>此文件无法预览</b>
            <span>只有图片支持应用内预览，其他类型请用系统程序打开。</span>
          </div>
        )}
        {!image && !failed && <div className="preview-loading">正在加载预览...</div>}
      </div>
    </div>
  );
}

function friendlyError(error: unknown) {
  const raw = String(error);
  const rules: [RegExp, string][] = [
    [/permission denied|access is denied/i, "没有访问权限，请检查文件或目录的读取权限。"],
    [/database is locked|database table is locked/i, "数据库正被其他操作占用，请稍后重试。"],
    [/no such file|os error 2/i, "文件或目录不存在，可能已被移动或删除。"],
    [/no longer matches indexed hash/, "文件内容与索引记录不一致，请重新扫描后再试。"],
    [/file changed while hashing/, "扫描期间文件内容发生了变化，将在下次扫描时重试。"],
    [/must be an approved/, "只有已确认的精确重复副本才能移入回收站。"],
    [/is not a directory/, "扫描目录无效或已不存在，请重新添加。"],
    [/restore refused/, "原位置已存在文件，恢复被拒绝以避免覆盖。"],
    [/unknown file id/, "该文件已不在索引中，请刷新后重试。"],
  ];
  for (const [pattern, text] of rules) {
    if (pattern.test(raw)) return text;
  }
  return `操作失败：${raw}`;
}

function formatRate(rate: number) {
  return `${rate.toLocaleString("zh-CN", { maximumFractionDigits: 0 })} 个/秒`;
}

function formatEta(seconds: number) {
  if (seconds < 60) return `预计剩余 ${Math.ceil(seconds)} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `预计剩余 ${minutes} 分 ${Math.ceil(seconds % 60)} 秒`;
  return `预计剩余 ${Math.floor(minutes / 60)} 时 ${minutes % 60} 分`;
}

createRoot(document.getElementById("root")!).render(<App />);
