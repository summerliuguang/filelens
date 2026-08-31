import { useState } from "react";
import { createRoot } from "react-dom/client";
import { invoke } from "@tauri-apps/api/core";
import "./styles.css";

type GroupFile = { id: number; path: string; protected: boolean; approved: boolean };
type Group = { hash: string; size: number; files: GroupFile[] };
type Status = { files: number; duplicates: number; inTrash: number };

const defaultDatabase = "";

function App() {
  const [database, setDatabase] = useState(defaultDatabase);
  const [trash, setTrash] = useState("");
  const [roots, setRoots] = useState<string[]>([]);
  const [rootInput, setRootInput] = useState("");
  const [status, setStatus] = useState<Status | null>(null);
  const [groups, setGroups] = useState<Group[]>([]);
  const [message, setMessage] = useState("请选择项目数据库和本地回收站，然后添加扫描目录。");
  const [busy, setBusy] = useState(false);

  async function execute(action: () => Promise<string>) {
    setBusy(true);
    try {
      setMessage(await action());
    } catch (error) {
      setMessage(`操作失败：${String(error)}`);
    } finally {
      setBusy(false);
    }
  }

  async function refresh() {
    await execute(async () => {
      const [nextStatus, nextGroups] = await Promise.all([
        invoke<Status>("status", { database }),
        invoke<Group[]>("groups", { database }),
      ]);
      setStatus(nextStatus);
      setGroups(nextGroups);
      return `已加载 ${nextGroups.length} 个精确重复组。`;
    });
  }

  function addRoot() {
    const value = rootInput.trim();
    if (value && !roots.includes(value)) setRoots([...roots, value]);
    setRootInput("");
  }

  return <main>
    <header>
      <div><p className="eyebrow">LOCAL-FIRST ARCHIVE CARE</p><h1>FileLens</h1></div>
      <button className="secondary" disabled={busy || !database} onClick={refresh}>刷新数据</button>
    </header>
    <section className="setup card">
      <div><label>项目数据库</label><input value={database} onChange={e => setDatabase(e.target.value)} placeholder="例如 D:\\FileLens\\archive.db" /></div>
      <div><label>本地应用回收站</label><input value={trash} onChange={e => setTrash(e.target.value)} placeholder="例如 E:\\FileLens-Recycle" /></div>
      <button disabled={busy || !database || !trash} onClick={() => execute(() => invoke("initialize", { database, trash }))}>创建/打开项目</button>
    </section>
    <section className="metrics">
      <article><span>已索引文件</span><strong>{status?.files ?? "-"}</strong></article>
      <article><span>可审阅重复副本</span><strong>{status?.duplicates ?? "-"}</strong></article>
      <article><span>应用回收站</span><strong>{status?.inTrash ?? "-"}</strong></article>
    </section>
    <section className="card scan">
      <div className="section-heading"><div><p className="eyebrow">SCAN SOURCES</p><h2>扫描目录</h2></div><button disabled={busy || !database || roots.length === 0} onClick={() => execute(async () => { const result = await invoke<string>("scan", { database, roots }); await refresh(); return result; })}>开始增量扫描</button></div>
      <div className="add-root"><input value={rootInput} onChange={e => setRootInput(e.target.value)} onKeyDown={e => e.key === "Enter" && addRoot()} placeholder="输入本地同步目录或 SMB 映射盘路径" /><button className="secondary" onClick={addRoot}>添加目录</button></div>
      <div className="roots">{roots.length === 0 ? <span>尚未添加目录</span> : roots.map(root => <span className="chip" key={root}>{root}<button aria-label="删除目录" onClick={() => setRoots(roots.filter(item => item !== root))}>×</button></span>)}</div>
    </section>
    <section className="card review">
      <div className="section-heading"><div><p className="eyebrow">REVIEW QUEUE</p><h2>精确重复</h2></div><span className="hint">完整 BLAKE3 哈希确认，移动前仍会再次校验</span></div>
      {groups.length === 0 ? <div className="empty">完成扫描后，高置信度的精确重复组会显示在这里。</div> : groups.map((group, index) => <article className="group" key={group.hash}>
        <div className="group-title"><b>重复组 {index + 1}</b><span>{group.files.length} 个相同文件</span><span>{formatBytes(group.size)} / 文件</span></div>
        {group.files.map(file => <div className="file" key={file.id}><div className="file-path"><b>#{file.id}</b><span>{file.path}</span>{file.protected && <i>受保护</i>}</div><div className="file-actions">{file.approved ? <button className="secondary" disabled={busy} onClick={() => execute(async () => { const result = await invoke<string>("unapprove", { database, fileId: file.id }); await refresh(); return result; })}>取消审批</button> : <button className="secondary" disabled={busy || file.protected} onClick={() => execute(async () => { const result = await invoke<string>("approve", { database, fileId: file.id }); await refresh(); return result; })}>确认副本</button>}{file.approved && <button className="danger" disabled={busy} onClick={() => execute(async () => { const result = await invoke<string>("trash", { database, fileId: file.id }); await refresh(); return result; })}>移入回收站</button>}</div></div>)}
      </article>)}
    </section>
    <footer>{busy ? "正在处理，请勿关闭程序..." : message}</footer>
  </main>;
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

createRoot(document.getElementById("root")!).render(<App />);
