# FileLens

FileLens 是一个本地优先的重复文件整理工具：多目录扫描、BLAKE3 完整内容哈希确认精确重复、感知指纹给出相似候选、可恢复的应用回收站，所有处理仅在人工确认后执行。桌面端基于 Tauri 2 + React 19，核心逻辑是独立的 Rust 库（含 CLI）。

## 功能

- **扫描与索引**：增量扫描（按大小+修改时间跳过未变文件）、并行哈希（多线程管线）、进度百分比/速度/预计剩余时间、可取消、失效目录自动跳过、外部删除的文件在下次扫描标记缺席。启动时可用已保存目录自动增量扫描（设置页可关）。
- **检测能力**：精确重复（BLAKE3 + 大小，可自动处理）；相似照片（dHash ≤4/64，仅人工查看）；相似文档（TXT/MD/CSV/JSON/XML/HTML 的 SimHash ≤8/64）；音频/视频指纹未接入，界面如实标注。
- **审核与处理**：重复组按大小降序分页；点击缩略图应用内放大预览（EXIF 方向、磁盘缓存）或调系统程序打开；每行显示文件名/路径/修改时间/大小；"标记删除 → 选择回收站或直接永久删除"两步语义；超 1 GB 文件提示回收站会占用双倍空间；保护路径规则禁止误删。
- **回收站**：移入前后哈希校验、恢复防覆盖、单个永久删除、一键清空、按保留天数自动过期（默认 30 天，0 关闭，扫描结束时自动清理）。
- **操作历史**：每次移入/恢复/删除全部记录，按时间倒序分页展示。
- **项目自动管理**：首次启动自动创建 SQLite 数据库与回收站目录，零配置；可自定义位置，重启自动打开上次项目。

## 安全模型

扫描只读取文件。任何删除动作前都要求：文件属于精确重复组、已被人工标记、未被保护规则命中、当前内容与索引哈希一致。移入回收站执行后再次校验哈希；恢复时若原位置已有文件则拒绝执行。

## 构建与安装

桌面端（在仓库根目录）：

```bash
npm install
npm run tauri build            # Linux: deb 等
npm run tauri build -- --bundles nsis --runner cargo-xwin \
  --target x86_64-pc-windows-msvc   # 从 Linux 交叉编译 Windows NSIS 安装包
```

注意：必须通过 `npm run tauri build` 构建——直接 `cargo build` 产出的二进制不包含前端资产。交叉编译 Windows 包需要 `nsis lld llvm`、`rustup target add x86_64-pc-windows-msvc` 和 `cargo install cargo-xwin`。

核心库 CLI（可选）：

```bash
cargo build --release    # 产物为 filelens 命令：init/scan/groups/approve/trash/restore/status
cargo test               # 单元测试覆盖扫描→删除→恢复全链路
```

## 数据位置

| 内容 | Linux | Windows |
|---|---|---|
| 数据库/回收站 | `~/.local/share/local.filelens.desktop/` | `%APPDATA%\local.filelens.desktop\` |
| 缩略图缓存 | `~/.cache/local.filelens.desktop/thumbnails/` | `%LOCALAPPDATA%\local.filelens.desktop\thumbnails\` |

数据库结构自 v1 起保持兼容，升级版本可直接使用已有索引。
