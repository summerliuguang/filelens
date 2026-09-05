# FileLens — 本地优先的重复文件查找器（桌面应用）

Tauri 2 + React 19 + TypeScript(strict) + Rust。纯本地：扫描/哈希/删除均在用户机器上完成，
应用回收站 + 操作留痕是核心安全模型。改代码前建议先读 `DESIGN.md`（产品设计）与 `README.md`（功能与构建）。

## 目录与分层边界

- 根 `src/main.rs` — **核心库**（扫描管线、BLAKE3 哈希、相似检测、回收站、安全链）+ CLI + 全部单元测试。不依赖 tauri。
- `src/lib.rs` — 核心库导出清单；新增公开函数后要同步在这里 re-export。
- `src-tauri/src/main.rs` — **薄命令层**：`#[tauri::command]`、serde 结构体、缩略图缓存。业务逻辑不写在这里，放进核心库（可测试）。
- `src/main.tsx` — 整个前端（单文件，~2000 行）；`src/styles.css` 全部样式。
- `IMPROVEMENT.md` — 本地改进清单（P0–P3），经 `.git/info/exclude` 排除、**不入库**。
- 测试只写核心库（`src/main.rs` 的 `mod tests`），命令层不设测试。

## 构建与验证命令

```bash
npm run tauri build     # 桌面产物（deb/rpm/AppImage/NSIS）——唯一有效的构建方式
npm run tauri dev       # 开发调试
cargo test --lib        # 核心库测试（改 Rust 后必跑）
npx tsc --noEmit        # 前端类型（改 TS 后必跑）
```

**铁律**：裸 `cargo build` 产出的二进制不含前端资产（webview 白屏，编译无任何报错）。
出包后验证：`strings src-tauri/target/release/filelens-desktop | grep -o "assets/index-.*\.js"`，无输出 = 坏包。

## 跨层字段命名（最容易踩的坑）

- Rust 结构体字段按 **snake_case** 原样序列化给前端；TS 类型必须逐字段镜像真实命名
  （`trash_retention_days` 而非 `trashRetentionDays`），错位 = 静默读到 undefined。
- invoke 参数相反：JS 传 **camelCase**（`protectRules`），自动映射到 Rust snake_case 形参——这是唯一的自动转换。
- 版本号四处同步：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`、根 `Cargo.toml`。

## 文件操作安全链（不许旁路）

删除/移动必须走完整链：查索引 → 拒绝受保护路径 → 重哈希比对内容 → 原子操作（rename 失败走
copy+校验+删源）→ `operations` 表留痕 → 置 `present=0`。相似候选走 `trash_paths`/`delete_paths`，
精确重复走 `trash`/`delete_direct`——新增任何删除类命令都必须复用这条链，历史上 `delete_paths`
绕过校验曾是安全后门。UI 侧破坏性操作统一用 `ConfirmDialog`（带取消 + Escape）。

## SQLite 约定

- 每个连接打开即设 `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;`（核心库与命令层都要）。
- `ensure_initialized` 校验 `schema_version==1` 并幂等补建索引（`CREATE INDEX IF NOT EXISTS`）。
  **没有迁移框架**：改表结构前必须先设计迁移路径，否则老库直接打不开。
- 长任务模式：`Mutex<Option<ScanTask>>` 任务槽 + `AtomicBool` 协作取消 + 轮询 `scan_state`。

## 约定与环境

- UI 文案中文；核心库错误消息用英文短语（前端 `friendlyError` 正则映射成中文）；命令层批量失败消息直接中文。
- 提交信息英文、`type: summary` 格式；提交身份 `summerliuguang <192430597+summerliuguang@users.noreply.github.com>`（仓库级已配置）。
- WSL2 环境：`*:Zone.Identifier` 是垃圾文件（已 gitignore，进 `.git` 会弄坏 refs，用
  `find .git -name "*Zone.Identifier*" -delete` 修复）；启动日志的 EGL/MESA/GBM 警告是良性的；
  本地装 deb 必须绝对路径。
- Windows 交叉编译用 `npx tauri build --runner cargo-xwin --target x86_64-pc-windows-msvc`，
  NSIS 插件 dll 卡住时手动放 `~/.cache/tauri/NSIS/Plugins/x86_64/`。
