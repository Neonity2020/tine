# Developing Tine

How to build, run and test Tine from source. For using Tine, start with the
[Guide](https://tine.page/guide/) and the [README](../README.md).

## Built with

| Layer | Tech | Notes |
|------|------|-------|
| Shell | [Tauri 2](https://tauri.app) (Rust) | OS webview (WebKitGTK on Linux) — tiny runtime vs Electron |
| Frontend | [SolidJS](https://solidjs.com) + TypeScript + [Vite](https://vitejs.dev) | fine-grained reactivity, no virtual-DOM churn |
| Core | `crates/tine-core` (pure Rust) | parse/serialize, model, indexing, queries, refs, dates, PDF/EDN, HTML publish |
| Rendering | [pdf.js](https://mozilla.github.io/pdf.js/), [KaTeX](https://katex.org), highlight.js | PDF, math, code |

The Rust core is GUI-free and unit-tested in isolation; the Tauri layer is a thin set of IPC
commands over it. The frontend owns the live editing tree (normalized store) and pushes debounced,
format-preserving saves; whole-graph reads hit an in-memory page cache (`RwLock<Arc<Graph>>` — read
commands clone the Arc and release the lock immediately) keyed by a graph generation counter.

The bigger architectural choices — Tauri/WebKitGTK over Electron, the pure-Rust core, in-browser
WASM parsing, the data-safety invariants — are written up as short decision records in
[`docs/adr/`](adr/).

## Project layout

```
crates/tine-core/    Rust core: parse/serialize, model, config, dates, refs, query, pdf, edn, publish
src-tauri/           Tauri app — IPC commands + windows (main + quick-capture)
src/                 SolidJS frontend (components, store, render pipeline, keybindings)
scripts/             env.sh (toolchain paths), screenshot generators
docs/                Logo, images, FEATURES.md, ADRs, feature notes
samples/             Demo graph used by tests/screenshots
```

## Build & run

```bash
source scripts/env.sh        # toolchain env (CARGO_HOME/RUSTUP_HOME, lib paths)
npm install                  # first time

# Build the release binary (NOT plain `cargo build` — that produces a dev-mode
# binary that can't connect to the bundled frontend):
npx tauri build --no-bundle

# Run it against your graph:
TINE_GRAPH=/path/to/your/graph ./target/release/tine
```

- Point `TINE_GRAPH` at the same `journals/` + `pages/` + `logseq/config.edn` tree you use with
  Logseq. **Run one app at a time** on a given graph.

## Develop

```bash
npm run dev                  # frontend only, in a browser, against an in-memory mock backend
npm run app                  # full Tauri dev window  (alias for: tauri dev)
node scripts/screenshot.mjs  # regenerate screenshots from the mock backend
```

## Testing

```bash
source scripts/env.sh
cargo test -p tine-core      # Rust: parse/serialize round-trip, model, queries, search cache
npm test                     # Frontend: Vitest (editor ops, outline, autocomplete, markers, …)
```

Round-trip parsing is validated against a real Logseq graph (0 structural diffs beyond accepted
canonicalization); `tine-check` is a privacy-safe profiler that proves byte-faithful serialization
without reading note content.
