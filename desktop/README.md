# CAVS Desktop

The official desktop UI for CAVS — a **local build/update lab for large files**.
Tauri (Rust backend) + React (frontend). The backend calls the same CAVS Rust
core (`cavs-sdk-core`) used by the CLI and SDKs; no CAVS logic is duplicated in
TypeScript.

## Highlights

- **Multi-language** — Spanish and English, switchable at any time. Every
  section ships contextual help in the selected language.
- **Sidebar navigation** — grouped, top-aligned and scrollable. Selecting a
  section loads its content on the right.
- **Per-section history** — each section opens on a table of everything you have
  run there. Rows support: view result (click / info), open output folder, and
  delete (which also removes the generated files on disk).
- **Create flows** — every operational section has a **Create** button that opens
  a modal: a **wizard** (Next/Back, result at the end) for guided flows, or a
  **compare** modal (two drag-and-drop zones) for old→new comparisons, or a
  simple form.
- **Header settings** — a Settings modal for language and light/dark theme.
- **Projects first** — the first screen manages local projects (name, output
  folder, engine, optional icon; name + folder required). Opening a project
  loads a dashboard where every section is scoped to that project.
- **SQLite persistence** — projects, history and settings live in
  `~/.cavs-desktop/cavs-desktop.db`. Generated files are stored inside each
  project's own output folder, per section and operation:
  `<project output folder>/{section}/{operation_id}/`.
- **Local test server, external-tool detection, CLI transparency, reports,
  recommendations and more** — see the feature spec.

## Develop

```bash
cd desktop
npm install
npm run tauri dev      # launches the app (needs the Tauri CLI toolchain)
```

Frontend-only iteration:

```bash
npm run dev            # Vite dev server on :1420
npm run build          # tsc typecheck + vite production build
```

Backend typecheck:

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```

## Package

`src-tauri/icons/` ships a PNG icon. To generate full platform icon sets before
a release build:

```bash
npm run tauri icon src-tauri/icons/icon.png
npm run tauri build
```

## Layout

```
desktop/
  src/                 React frontend
    api/               typed Tauri command wrappers + event types
    app/               App shell, global store, section registry
    components/        Header, Sidebar, modals, charts, result view…
    i18n/              en/es dictionaries + per-section help
    pages/             generic SectionPage + custom pages
  src-tauri/           Rust backend
    src/commands.rs    Tauri command surface (history, run, server, tools)
    src/db.rs          SQLite schema + queries
    src/storage.rs     app-data / artifact layout
    src/server.rs      local dev HTTP server
```
