# FileLens

FileLens is a local-first duplicate-file finder for personal media archives. This repository currently contains the safe MVP core: multi-root scanning, full BLAKE3 exact-duplicate detection, review-state tracking, and a recoverable local application recycle bin.

The repository includes a Tauri + React desktop interface in `src-tauri/` and `src/`. It currently exposes the exact-duplicate MVP workflow: project setup, scan roots, index statistics, exact duplicate review, explicit approval, and recycle-bin movement.

## Build

```bash
cargo build --release

# Build the desktop frontend.
node .tools/npm/bin/npm-cli.js install
node .tools/npm/bin/npm-cli.js run build
```

The desktop command bridge uses the workspace Rust CLI during development. Before distributing an installer, the next packaging task is to bundle the CLI as a Tauri sidecar (or extract the core into a shared Rust library), so installed applications do not require Cargo.

## Quick start

```bash
# Create a project database and a local recycle-bin directory.
filelens init --database /data/filelens.db --trash /data/FileLens-Recycle

# Scan one or more local archive roots. The database persists the index.
filelens scan --database /data/filelens.db --root /data/photos --root /data/downloads

# List exact duplicate groups. Every item starts unreviewed.
filelens groups --database /data/filelens.db

# Mark an item approved, then move it to the application recycle bin.
filelens approve --database /data/filelens.db --file-id 42
filelens trash --database /data/filelens.db --file-id 42

# Restore a moved item to its original location.
filelens restore --database /data/filelens.db --operation-id 3
```

`scan` skips FileLens recycle-bin folders, common system/cache folders, and files that change while being hashed. It never deletes files. `trash` refuses to operate unless the item belongs to an exact-duplicate group, was explicitly approved, is not protected, and still matches its indexed BLAKE3 hash.

## Current MVP scope

- Exact byte-for-byte duplicate detection for all readable file types.
- Incremental rescans based on path, size, and modification time.
- Protected paths via `--protect <substring>` during scanning.
- Cross-volume recycle operations copy, verify BLAKE3, then remove the source.
- Restore conflict protection and persistent operation audit records.

Image/video/audio/document similarity, thumbnail generation, GUI review, and SMB write support are planned stages documented in `DESIGN.md`.
