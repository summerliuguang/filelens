import { useEffect, useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import "./styles.css";

type Page = "overview" | "sources" | "review" | "trash" | "settings";
type GroupFile = { id: number; path: string; protected: boolean; approved: boolean };
type Group = { hash: string; size: number; files: GroupFile[] };
type Status = { files: number; duplicates: number; inTrash: number };
type TrashItem = { id: number; created_at: number; source_path: string; trash_path: string };
type ProjectConfig = { trash_path: string; roots: string[]; protect_rules: string[] };

const nav: { id: Page; icon: string; label: string; caption: string }[] = [
  { id: "overview", icon: "◌", label: "概览", caption: "ARCHIVE HEALTH" },
  { id: "sources", icon: "⌁", label: "扫描来源", caption: "SCAN SOURCES" },
  { id: "review", icon: "⊞", label: "重复审核", caption: "REVIEW QUEUE" },
  { id: "trash", icon: "↶", label: "应用回收站", caption: "RECOVERY" },
  { id: "settings", icon: "⚙", label: "项目设置", caption: "PROJECT SETTINGS" },
];

function App() {
  const [page, setPage] = useState<Page>("overview");
  const [database, setDatabase] = useState("");
  const [trash, setTrash] = useState("");
  const [roots, setRoots] = useState<string[]>([]);
  const [protectRules, setProtectRules] = useState<string[]>([]);
  const [rootInput, setRootInput] = useState("");
  const [status, setStatus] = useState<Status | null>(null);
  const [groups, setGroups] = useState<Group[]>([]);
  const [trashItems, setTrashItems] = useState<TrashItem[]>([]);
  const [message, setMessage] = useState("请先在项目设置中创建或打开项目。");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const saved = localStorage.getItem("filelens.database");
    if (!saved) return;
    setDatabase(saved);
    invoke<ProjectConfig>("project_config", { database: saved }).then(config => {
      setTrash(config.trash_path); setRoots(config.roots); setProtectRules(config.protect_rules);
    }).catch(() => setMessage("未能打开上次项目，请在项目设置中确认路径。"));
  }, []);

  async function execute(action: () => Promise<string>) {
    setBusy(true);
    try { setMessage(await action()); } catch (error) { setMessage(`操作失败：${String(error)}`); } finally { setBusy(false); }
  }

  async function refresh() {
    if (!database) return;
    await execute(async () => {
      const [nextStatus, nextGroups, nextTrash] = await Promise.all([
        invoke<Status>("status", { database }), invoke<Group[]>("groups", { database }), invoke<TrashItem[]>("trash_list", { database }),
      ]);
      setStatus(nextStatus); setGroups(nextGroups); setTrashItems(nextTrash);
      return `索引已更新：${nextGroups.length} 个精确重复组待审核。`;
    });
  }

  async function initialize() {
    await execute(async () => {
      const result = await invoke<string>("save_project_config", { database, trash, roots, protectRules });
      localStorage.setItem("filelens.database", database);
      await refresh(); return result;
    });
  }

  async function scan() {
    await execute(async () => { const result = await invoke<string>("scan", { database, roots, protectRules }); await refresh(); return result; });
  }

  function addRoot() { const value = rootInput.trim(); if (value && !roots.includes(value)) setRoots([...roots, value]); setRootInput(""); }
  const active = nav.find(item => item.id === page)!;
  const selected = groups.reduce((total, group) => total + group.files.filter(file => file.approved).length, 0);

  return <div className="app-shell">
    <aside>
      <div className="brand"><span className="brand-mark">+</span><div><b>FileLens</b><small>本地文件整理</small></div></div>
      <nav>{nav.map(item => <button key={item.id} className={page === item.id ? "active" : ""} onClick={() => setPage(item.id)}><span>{item.icon}</span>{item.label}{item.id === "review" && status && status.duplicates > 0 && <em>{status.duplicates}</em>}</button>)}</nav>
      <div className="side-note"><span>安全模式</span><b>仅人工确认后移动</b><small>完整哈希二次校验</small></div>
    </aside>
    <main>
      <header><div><p className="eyebrow">{active.caption}</p><h1>{active.label}</h1></div><button className="secondary" disabled={busy || !database} onClick={refresh}>↻ 刷新</button></header>
      {page === "overview" && <Overview status={status} groups={groups} selected={selected} onNavigate={setPage} />}
      {page === "sources" && <Sources roots={roots} rootInput={rootInput} setRootInput={setRootInput} addRoot={addRoot} removeRoot={root => setRoots(roots.filter(item => item !== root))} scan={scan} disabled={busy || !database} />}
      {page === "review" && <Review database={database} groups={groups} busy={busy} execute={execute} refresh={refresh} />}
      {page === "trash" && <Trash database={database} items={trashItems} busy={busy} execute={execute} refresh={refresh} />}
      {page === "settings" && <Settings database={database} trash={trash} protectRules={protectRules} setDatabase={setDatabase} setTrash={setTrash} setProtectRules={setProtectRules} initialize={initialize} busy={busy} />}
      <footer className={message.startsWith("操作失败") ? "error" : ""}>{busy ? "正在处理，请不要关闭程序..." : message}</footer>
    </main>
  </div>;
}

function Overview({ status, groups, selected, onNavigate }: { status: Status | null; groups: Group[]; selected: number; onNavigate: (page: Page) => void }) {
  const recoverable = groups.reduce((total, group) => total + Math.max(0, group.files.length - 1) * group.size, 0);
  return <><section className="hero"><div><span className="signal">● 索引就绪</span><h2>让重复文件变得<br /><i>清晰、可控。</i></h2><p>FileLens 只处理经过完整 BLAKE3 内容哈希确认的重复文件。任何文件移动前都会再次校验。</p><button onClick={() => onNavigate("sources")}>管理扫描来源</button></div><div className="hero-orbit"><strong>{status?.duplicates ?? "-"}</strong><span>待审核副本</span><small>{formatBytes(recoverable)} 可释放空间</small></div></section><section className="metrics"><Metric value={status?.files ?? "-"} label="已索引文件" detail="本地数据库" /><Metric value={status?.duplicates ?? "-"} label="重复副本" detail="完整哈希确认" /><Metric value={selected} label="已确认处理" detail="等待移入回收站" /><Metric value={status?.inTrash ?? "-"} label="回收站文件" detail="可安全恢复" /></section><section className="panel action-panel"><div><p className="eyebrow">NEXT ACTION</p><h2>{groups.length ? "从重复组中选择要处理的副本" : "添加目录并开始首次扫描"}</h2><p>{groups.length ? "FileLens 从不自动删除，逐项确认后才会允许移动。" : "扫描只读取文件并建立索引，不会修改你的任何数据。"}</p></div><button onClick={() => onNavigate(groups.length ? "review" : "sources")}>{groups.length ? "打开审核队列" : "添加扫描目录"}</button></section></>;
}

function Metric({ value, label, detail }: { value: number | string; label: string; detail: string }) { return <article className="metric"><strong>{value}</strong><span>{label}</span><small>{detail}</small></article>; }

function Sources({ roots, rootInput, setRootInput, addRoot, removeRoot, scan, disabled }: { roots: string[]; rootInput: string; setRootInput: (value: string) => void; addRoot: () => void; removeRoot: (root: string) => void; scan: () => void; disabled: boolean }) {
  async function pickDirectory() { const selected = await open({ directory: true, multiple: false, title: "选择扫描目录" }); if (typeof selected === "string") setRootInput(selected); }
  return <section className="panel sources-page"><div className="section-head"><div><h2>本次扫描范围</h2><p>多个根目录会全局互相比较。应用回收站和常见缓存目录会自动排除。</p></div><button disabled={disabled || !roots.length} onClick={scan}>开始增量扫描</button></div><div className="add-source"><input value={rootInput} onChange={event => setRootInput(event.target.value)} onKeyDown={event => event.key === "Enter" && addRoot()} placeholder="输入目录，例如 D:\\NAS-Sync\\Photos" /><button className="secondary" onClick={pickDirectory}>选择目录</button><button className="secondary" onClick={addRoot}>添加</button></div><div className="source-list">{roots.length === 0 ? <Empty icon="⌁" text="还没有扫描目录" detail="添加本地同步目录后，即可建立内容索引。" /> : roots.map((root, index) => <article className="source" key={root}><span className="source-icon">{index + 1}</span><div><b>{root}</b><small>本地来源 · 全局比较已启用</small></div><button className="text-button" onClick={() => removeRoot(root)}>移除</button></article>)}</div></section>;
}

function Review({ database, groups, busy, execute, refresh }: { database: string; groups: Group[]; busy: boolean; execute: (action: () => Promise<string>) => Promise<void>; refresh: () => Promise<void> }) {
  return <section className="panel review-page"><div className="section-head"><div><h2>精确重复审核</h2><p>仅显示完整 BLAKE3 哈希一致的文件。请保留至少一个副本。</p></div><span className="pill">{groups.length} 个组</span></div>{groups.length === 0 ? <Empty icon="⊞" text="没有待审核的精确重复" detail="完成扫描后，重复组会按可释放空间显示在这里。" /> : <div className="groups">{groups.map((group, index) => <article className="group" key={group.hash}><div className="group-header"><div><span>重复组 {String(index + 1).padStart(2, "0")}</span><h3>{group.files.length} 个内容相同的文件</h3></div><div><b>{formatBytes(group.size)}</b><small>每个文件</small></div></div><div className="hash">BLAKE3 {group.hash}</div>{group.files.map(file => <div className="file-row" key={file.id}><div className="file-index">#{file.id}</div><div className="file-path"><b>{file.path.split(/[\\/]/).pop()}</b><span>{file.path}</span></div>{file.protected && <span className="protected">受保护</span>}<div className="file-actions">{file.approved ? <><span className="approved">已确认</span><button className="secondary" disabled={busy} onClick={() => execute(async () => { const result = await invoke<string>("unapprove", { database, fileId: file.id }); await refresh(); return result; })}>取消</button><button className="danger" disabled={busy} onClick={() => execute(async () => { const result = await invoke<string>("trash", { database, fileId: file.id }); await refresh(); return result; })}>移入回收站</button></> : <button className="secondary" disabled={busy || file.protected} onClick={() => execute(async () => { const result = await invoke<string>("approve", { database, fileId: file.id }); await refresh(); return result; })}>确认副本</button>}</div></div>)}</article>)}</div>}</section>;
}

function Trash({ database, items, busy, execute, refresh }: { database: string; items: TrashItem[]; busy: boolean; execute: (action: () => Promise<string>) => Promise<void>; refresh: () => Promise<void> }) {
  return <section className="panel trash-page"><div className="section-head"><div><h2>可恢复文件</h2><p>恢复时不会覆盖原路径已有文件，并会再次进行内容完整性检查。</p></div><span className="pill">{items.length} 个文件</span></div>{items.length === 0 ? <Empty icon="↶" text="应用回收站为空" detail="从审核队列移入的副本将显示在这里，默认建议保留 30 天。" /> : <div className="trash-list">{items.map(item => <article className="trash-item" key={item.id}><span className="source-icon">↶</span><div><b>{item.source_path.split(/[\\/]/).pop()}</b><span>原位置：{item.source_path}</span><small>移入时间：{new Date(item.created_at * 1000).toLocaleString("zh-CN")}</small></div><button disabled={busy} onClick={() => execute(async () => { const result = await invoke<string>("restore", { database, operationId: item.id }); await refresh(); return result; })}>恢复原位置</button></article>)}</div>}</section>;
}

function Settings({ database, trash, protectRules, setDatabase, setTrash, setProtectRules, initialize, busy }: { database: string; trash: string; protectRules: string[]; setDatabase: (value: string) => void; setTrash: (value: string) => void; setProtectRules: (value: string[]) => void; initialize: () => Promise<void>; busy: boolean }) {
  const [rule, setRule] = useState("");
  async function pickTrash() { const selected = await open({ directory: true, multiple: false, title: "选择本地应用回收站目录" }); if (typeof selected === "string") setTrash(selected); }
  return <section className="panel settings-page"><div className="section-head"><div><h2>项目与安全设置</h2><p>数据库记录扫描结果和操作日志；回收站必须位于客户端本地磁盘。</p></div></div><label>项目数据库路径<input value={database} onChange={event => setDatabase(event.target.value)} placeholder="例如 D:\\FileLens\\archive.db" /></label><label>应用回收站路径<div className="input-action"><input value={trash} onChange={event => setTrash(event.target.value)} placeholder="例如 E:\\FileLens-Recycle" /><button className="secondary" onClick={pickTrash}>选择目录</button></div></label><label>保护路径规则<div className="input-action"><input value={rule} onChange={event => setRule(event.target.value)} placeholder="例如 Originals" /><button className="secondary" onClick={() => { if (rule.trim() && !protectRules.includes(rule.trim())) setProtectRules([...protectRules, rule.trim()]); setRule(""); }}>添加</button></div></label><div className="rules">{protectRules.map(item => <span className="chip" key={item}>{item}<button onClick={() => setProtectRules(protectRules.filter(value => value !== item))}>×</button></span>)}</div><button disabled={busy || !database || !trash} onClick={initialize}>保存并打开项目</button><div className="safety"><b>安全承诺</b><span>扫描不会修改文件。移动只针对人工确认的精确重复副本，且执行前后均验证 BLAKE3 哈希。</span></div></section>;
}

function Empty({ icon, text, detail }: { icon: string; text: string; detail: string }) { return <div className="empty"><span>{icon}</span><b>{text}</b><p>{detail}</p></div>; }
function formatBytes(bytes: number) { if (bytes < 1024) return `${bytes} B`; const units = ["KB", "MB", "GB", "TB"]; let value = bytes / 1024; let unit = 0; while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; } return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`; }

createRoot(document.getElementById("root")!).render(<App />);
