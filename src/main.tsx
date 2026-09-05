import { useEffect, useRef, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { ScanProgress } from "./ui/components";
import {
  Detectors,
  History,
  Overview,
  Review,
  Settings,
  SimilarDocuments,
  SimilarPhotos,
  Sources,
  Trash,
} from "./ui/views";
import { friendlyError } from "./lib/format";
import type {
  DetectorStatus,
  Group,
  GroupFilters,
  GroupsPage,
  Page,
  ProjectState,
  ScanState,
  SimilarDocument,
  SimilarPhoto,
  Status,
  Toast,
  TrashItem,
} from "./lib/types";
import "./styles.css";

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
  const [excludeRules, setExcludeRules] = useState<string[]>([]);
  const [minFileSize, setMinFileSize] = useState(0);
  const [retentionDays, setRetentionDays] = useState(30);
  const [autoScan, setAutoScan] = useState(true);
  const [rootInput, setRootInput] = useState("");
  const [status, setStatus] = useState<Status | null>(null);
  const [groups, setGroups] = useState<Group[]>([]);
  const [groupsExhausted, setGroupsExhausted] = useState(true);
  const [groupsTotal, setGroupsTotal] = useState(0);
  const [groupFilters, setGroupFilters] = useState<GroupFilters>({
    search: "",
    minSize: 0,
    sort: "size",
  });
  const [trashItems, setTrashItems] = useState<TrashItem[]>([]);
  const [similarPhotos, setSimilarPhotos] = useState<SimilarPhoto[]>([]);
  const [similarDocuments, setSimilarDocuments] = useState<SimilarDocument[]>([]);
  const [detectors, setDetectors] = useState<DetectorStatus[]>([]);
  const [toasts, setToasts] = useState<Toast[]>([]);
  const [booting, setBooting] = useState(true);
  const [busyKeys, setBusyKeys] = useState<ReadonlySet<string>>(new Set());
  const [scanState, setScanState] = useState<ScanState>({
    state: "idle",
    processed: 0,
    total: 0,
    message: "",
    current_path: null,
    errors_total: 0,
    recent_errors: [],
  });
  const progressHistory = useRef<{ t: number; processed: number }[]>([]);
  const toastSeq = useRef(0);
  // Global busy = any tracked action in flight; per-section keys let each
  // button disable only for its own operation.
  const busy = busyKeys.size > 0;

  function setKeyBusy(key: string, on: boolean) {
    setBusyKeys((current) => {
      const next = new Set(current);
      if (on) next.add(key);
      else next.delete(key);
      return next;
    });
  }

  function notify(kind: Toast["kind"], text: string) {
    const id = ++toastSeq.current;
    // Keep the stack short; successes self-dismiss, errors stay until closed.
    setToasts((current) => [...current.slice(-3), { id, kind, text }]);
    if (kind === "ok") {
      window.setTimeout(() => dismissToast(id), 3600);
    }
  }

  function dismissToast(id: number) {
    setToasts((current) => current.filter((toast) => toast.id !== id));
  }

  useEffect(() => {
    invoke<ProjectState>("open_project")
      .then((project) => {
        setDatabase(project.database);
        setTrash(project.trash_path);
        setRoots(project.roots);
        setProtectRules(project.protect_rules);
        setExcludeRules(project.exclude_rules);
        setMinFileSize(project.min_file_size);
        setRetentionDays(project.trash_retention_days);
        setAutoScan(project.auto_scan);
        if (project.auto_scan && project.roots.length > 0) {
          void startScan(project.database, project.roots, project.protect_rules);
        } else {
          void refresh(project.database);
        }
      })
      .catch((error) =>
        notify("error", `自动打开本地项目失败：${String(error)}`),
      )
      .finally(() => setBooting(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
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
            notify(next.state === "failed" ? "error" : "ok", next.message);
            void refresh();
          }
        })
        .catch((error) =>
          setScanState({
            state: "failed",
            processed: 0,
            total: 0,
            message: String(error),
            current_path: null,
            errors_total: 0,
            recent_errors: [],
          }),
        );
    }, 500);
    return () => window.clearInterval(timer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [scanState.state]);

  async function execute(action: () => Promise<string>, key = "app") {
    setKeyBusy(key, true);
    try {
      notify("ok", await action());
    } catch (error) {
      notify("error", friendlyError(error));
    } finally {
      setKeyBusy(key, false);
    }
  }

  function groupsInvokeArgs(target: string, offset: number, limit: number) {
    return {
      database: target,
      offset,
      limit,
      minSize: groupFilters.minSize,
      pathContains: groupFilters.search.trim() || undefined,
      sort: groupFilters.sort,
    };
  }

  async function refresh(target = database) {
    if (!target) return;
    await execute(async () => {
      const [nextStatus, page, nextTrash, nextSimilarPhotos, nextDocuments, nextDetectors] = await Promise.all([
        invoke<Status>("status", { database: target }),
        // Reload as many groups as are already on screen so an action taken
        // deep in the list does not snap the queue back to its first page.
        invoke<GroupsPage>("groups", groupsInvokeArgs(target, 0, Math.max(GROUPS_PAGE, groups.length))),
        invoke<TrashItem[]>("trash_list", { database: target }),
        invoke<SimilarPhoto[]>("similar_photos", { database: target }),
        invoke<SimilarDocument[]>("similar_documents", { database: target }),
        invoke<DetectorStatus[]>("detector_status"),
      ]);
      setStatus(nextStatus);
      setGroups(page.groups);
      setGroupsTotal(page.total);
      setGroupsExhausted(page.groups.length >= page.total);
      setTrashItems(nextTrash);
      setSimilarPhotos(nextSimilarPhotos);
      setSimilarDocuments(nextDocuments);
      setDetectors(nextDetectors);
      return `索引已更新：${nextStatus.groups} 个精确重复组待审核。`;
    });
  }

  async function applyGroupFilters(next: GroupFilters) {
    if (!database) return;
    setGroupFilters(next);
    setKeyBusy("groups", true);
    try {
      const page = await invoke<GroupsPage>("groups", {
        database,
        offset: 0,
        limit: GROUPS_PAGE,
        minSize: next.minSize,
        pathContains: next.search.trim() || undefined,
        sort: next.sort,
      });
      setGroups(page.groups);
      setGroupsTotal(page.total);
      setGroupsExhausted(page.groups.length >= page.total);
    } catch (error) {
      notify("error", friendlyError(error));
    } finally {
      setKeyBusy("groups", false);
    }
  }

  async function initialize() {
    await execute(async () => {
      const result = await invoke<string>("save_project_config", {
        database,
        trash,
        roots,
        protectRules,
        excludeRules,
        minFileSize,
      });
      await refresh(database);
      return result;
    }, "init");
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
        excludeRules,
        minFileSize,
      });
      setScanState({
        state: "running",
        processed: 0,
        total: 0,
        message: result,
        current_path: null,
        errors_total: 0,
        recent_errors: [],
      });
      return result;
    }, "scan");
  }

  async function loadMoreGroups() {
    if (!database) return;
    setKeyBusy("groups", true);
    try {
      const page = await invoke<GroupsPage>(
        "groups",
        groupsInvokeArgs(database, groups.length, GROUPS_PAGE),
      );
      setGroups((current) => [...current, ...page.groups]);
      setGroupsTotal(page.total);
      setGroupsExhausted(groups.length + page.groups.length >= page.total);
    } catch (error) {
      notify("error", friendlyError(error));
    } finally {
      setKeyBusy("groups", false);
    }
  }

  async function cancelScan() {
    await execute(() => invoke<string>("cancel_scan"));
  }

  async function persistRoots(next: string[]) {
    if (!database) return;
    try {
      await invoke("save_roots", { database, roots: next, protectRules });
    } catch (error) {
      notify("error", friendlyError(error));
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
        {booting ? (
          <div className="loading-view">
            <span className="spinner" aria-hidden="true" />
            正在打开本地项目...
          </div>
        ) : (
          <>
            {scanState.state === "running" && (
              <ScanProgress
                scanState={scanState}
                history={progressHistory.current}
                onCancel={cancelScan}
                disabled={busy}
              />
            )}
            {/* Every page stays mounted so filters, selections and scroll
                positions survive switching; CSS hides inactive layers. */}
            <div className={page === "overview" ? "page-layer page-active" : "page-layer"}>
              <Overview
                status={status}
                groups={groups}
                selected={selected}
                onNavigate={setPage}
              />
            </div>
            <div className={page === "sources" ? "page-layer page-active" : "page-layer"}>
              <Sources
                roots={roots}
                rootInput={rootInput}
                setRootInput={setRootInput}
                addRoot={addRootPath}
                removeRoot={removeRootPath}
                scan={() => startScan(database, roots, protectRules)}
                disabled={busy || !database || scanState.state === "running"}
              />
            </div>
            <div className={page === "review" ? "page-layer page-active" : "page-layer"}>
              <Review
                database={database}
                groups={groups}
                totalGroups={groupsTotal}
                filters={groupFilters}
                onFilters={(next) => void applyGroupFilters(next)}
                busy={busy}
                busyKeys={busyKeys}
                execute={execute}
                refresh={refresh}
                hasMore={!groupsExhausted}
                onLoadMore={loadMoreGroups}
                indexedFiles={status?.files ?? 0}
                onNavigate={setPage}
              />
            </div>
            <div className={page === "similar" ? "page-layer page-active" : "page-layer"}>
              <SimilarPhotos
                database={database}
                photos={similarPhotos}
                busy={busy}
                busyKeys={busyKeys}
                execute={execute}
                refresh={refresh}
              />
            </div>
            <div className={page === "documents" ? "page-layer page-active" : "page-layer"}>
              <SimilarDocuments documents={similarDocuments} execute={execute} />
            </div>
            <div className={page === "detectors" ? "page-layer page-active" : "page-layer"}>
              <Detectors detectors={detectors} />
            </div>
            <div className={page === "trash" ? "page-layer page-active" : "page-layer"}>
              <Trash
                database={database}
                items={trashItems}
                busy={busy}
                busyKeys={busyKeys}
                execute={execute}
                refresh={refresh}
              />
            </div>
            <div className={page === "history" ? "page-layer page-active" : "page-layer"}>
              <History database={database} active={page === "history"} />
            </div>
            <div className={page === "settings" ? "page-layer page-active" : "page-layer"}>
              <Settings
                database={database}
                trash={trash}
                protectRules={protectRules}
                excludeRules={excludeRules}
                minFileSize={minFileSize}
                retentionDays={retentionDays}
                autoScan={autoScan}
                setDatabase={setDatabase}
                setTrash={setTrash}
                setProtectRules={setProtectRules}
                setExcludeRules={setExcludeRules}
                setMinFileSize={setMinFileSize}
                setRetentionDays={setRetentionDays}
                setAutoScan={setAutoScan}
                initialize={initialize}
                execute={execute}
                notify={(text) => notify("error", text)}
                busy={busy}
                busyKeys={busyKeys}
              />
            </div>
          </>
        )}
        {(toasts.length > 0 || busy) && (
          <div className="toast-stack" aria-live="polite">
            {toasts.map((toast) => (
              <div
                key={toast.id}
                className={toast.kind === "error" ? "toast error" : "toast"}
              >
                <span>{toast.text}</span>
                <button
                  className="toast-close"
                  aria-label="关闭通知"
                  onClick={() => dismissToast(toast.id)}
                >
                  ×
                </button>
              </div>
            ))}
            {busy && <div className="toast busy">正在处理，请不要关闭程序...</div>}
          </div>
        )}
      </main>
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<App />);
