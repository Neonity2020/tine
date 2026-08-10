# New docs progress

## Completed prerequisites

- **Phase A — `124f6a5b`**: completed editorial calibration for `Tine Guide` and `Features/Sheets`; the index now uses the Start/Workflows/Feature reference/showcase lenses and Sheets begins with ordinary bullets.
- **Phase 0 — `0a1dca82`, `80a898fc`**: completed Guide-link hardening; accidental targets are rejected while registered pages and deliberate stubs are checked by focused regression proof.

## Phase B — candidate: Workflows/Structure repeated information

Added one canonical workflow page, registered it in the shared manifest, linked it from the Workflows lens, and added a semantic onboarding proof for its registration, copy behavior, canonical links, executable tracker/query, and observable result. It remains a manager-review candidate, not a declaration that J05 is complete.

### Behavioral claim sources

- `crates/tine-core/src/templates/sheets.md:7` — “rows are child bullets, columns are their properties, cards are child bullets or blocks a query finds.” The workflow begins with ordinary tracker children and then changes only their view.
- `crates/tine-core/src/templates/sheets.md:8` — “Show children as → Grid or Table” and “`/Query`, then run `/Table` or `/Board`.” The table/board path names current visual controls before stored syntax.
- `crates/tine-core/src/templates/sheets.md:58` — “without copying the source blocks.” The query-backed board is explicitly presented as another view of the matching tracker bullets.
- `docs/FEATURES.md:179` — “Search, list, table, and board are presentations of one result membership.” The workflow separates selecting matching blocks from choosing a result presentation.
- `docs/FEATURES.md:180` — “friendly search-text surface” and “interactive visual query builder” compile with raw DSL to “the same query plan.” Friendly search and the visual builder precede the raw query explanation.
- `docs/adr/0030-query-view-unification.md:23` — “The query block owns membership through the query DSL and builder.” The query instructions describe membership separately from view.
- `docs/adr/0030-query-view-unification.md:24` — “`tine.view::` owns presentation.” The stored-syntax explanation identifies the board/table properties as presentation only.
- `docs/adr/0042-one-query-plan-many-frontends.md:52` — “Friendly search remains the primary authoring surface; raw DSL is optional.” The raw query text appears only after the visual path.
- `src/components/QuickSwitcher.tsx:585` — “Open search tab.” The workflow names the shipped transition from the quick switcher into the persistent result workspace before using workspace-only controls.
- `src/components/QueryWorkspace.tsx:733` — “Filters / Advanced”; `src/components/QueryWorkspace.tsx:744` — Search/List/Table/Board presentations. The workflow uses the shipped workspace labels.
- `src/components/QueryWorkspace.tsx:521` — “Edit as visual query”; `src/components/QueryBuilder.tsx:599` — “➕ Add filter”; `src/components/QueryBuilder.tsx:620` — “Property.” The workflow names the current visual narrowing controls.
- `crates/tine-core/src/templates/formulas.md:19` — “a read-only column appears.” The optional formula step links the canonical Sheets and Formulas pages instead of reproducing their walkthroughs.

## Phase C — candidates: J09 files/backups map (Reference/Files, external edits, and backups)

### Behavioral claim sources

- `README.md:36-38` — “It operates directly on the standard Logseq graph layout — `journals/`, `pages/`, `assets/`, and `logseq/config.edn`”. The page map names exactly these folders.
- `docs/FEATURES.md:532-538` — “Tine scans `pages/` (and `journals/`) **recursively**… keyed by its **file name**… edits save back to the file in place.” The sub-folder claim.
- `src/components/Settings.tsx:3449-3455` — “Deleting a block never deletes its media (a safety net), so unused files can accumulate.” The assets and orphan-scan claims.
- `docs/FEATURES.md:625-628` — “saved to your graph's `logseq/config.edn` as `:ui/show-brackets?`, matching Logseq's default (on)”; `docs/FEATURES.md:516-519` reads journal formats from config; `docs/FEATURES.md:167-168` supports user `:macros` from `config.edn`. The shared-configuration claim.
- `src/persistence.ts:590` — `SAVE_DEBOUNCE_MS = 400`; `docs/FEATURES.md:511-512` — “skip byte-identical rewrites”. The automatic-save claims (“about half a second after a pause”).
- `docs/FEATURES.md:509-512,547-548` — “Saves preserve each file's exact formatting (tabs vs spaces, comments, compact EDN)”; “`atomic_write` + fsync”. The atomic, format-preserving save claims.
- `docs/FEATURES.md:513-515` — “Page rename is transactional… re-checking each file just before writing and rolling back on conflict.” The rename claim.
- `docs/FEATURES.md:539-545` — “A `.org` file is rewritten only when Tine can reproduce it **byte-for-byte** — anything it can't round-trip loads **read-only**”. The Org guard claim.
- `src/components/Settings.tsx:3232-3256` — “Watch for external edits” with “Live (inotify)… no idle CPU wakeups” vs “Poll (3s)… filesystems where inotify is unreliable… Saved per device.” The watcher claims; `src/store.ts:331` — “external change (content differs) still reloads + invalidates” backs in-place updates for unedited pages.
- `src/components/ConflictBar.tsx:45-56` — ““{name}” changed on disk… Your unsaved changes weren't written”, buttons “Use disk version” / “Keep mine (overwrite)”; header comment states conflicted pages are “skipped by every future save batch until resolved”. The banner behavior claim.
- `docs/adr/0039-filesystem-scope-boundary.md:20-21` — “The window registry rejects equal, ancestor, and descendant graph roots owned by different windows.” The two-windows claim.
- `src/components/Settings.tsx:2589-2596` — “Tine snapshots your graph's Markdown/Org files to a local folder each time it opens (outside the graph, so Syncthing never sees it). A safety net against a bad write — independent of OG Logseq's own backups.”; keep default signal 12 at `Settings.tsx:2505`; “Oldest snapshots beyond this are pruned.” The snapshot claims.
- `src/components/Settings.tsx:2553-2558` — restore confirm: “This overwrites journals/ and pages/ with the {n} file(s) in that backup. Your current state is snapshotted first, so this is reversible.” The restore outcome.
- `src/components/ContextMenu.tsx:947` — “The file moves to the graph's .tine-trash folder.” The delete-to-trash claim; `src/components/Settings.tsx:3410-3414,3438` — empty asset trash “cannot be undone. Page, journal, and conflict recovery files in logseq/.tine-trash will be kept.”
- `docs/FEATURES.md:520-522` — “Duplicate journal days lets you **Open** / **Merge** / **Rename** / **Trash** each”; `docs/FEATURES.md:523-531` — “`*.sync-conflict-*` (or `(conflicted copy)`)… out of your page list… **Review & merge** shows a **block-by-block diff**… per-block **keep-current / keep-copy / keep-both**… **Discard copy** trashes it… never auto-merged”. The conflict-copy claims.
- `crates/tine-core/src/templates/managed-sync.md:4,7` — “**Testing only.** Tine-managed storage is experimental. Your normal mode is **Direct files**” and “Do not enable it merely because you already synchronize your graph folders.” The single managed-sync pointer.
- `src/components/Settings.tsx:2103-2112` — Settings → Graph → “Export graph to HTML”; `docs/FEATURES.md:656` — “One-click **static HTML export** (`public:: true` pages)”; `docs/FEATURES.md:668` — “right-click a page title → **Export to PDF…**”; `docs/FEATURES.md:679` — “**Copy/export as** Markdown… with a *Rendered* mode”. The export claims.

## Phase C — candidates: J02 open-an-existing-graph path (Start/Bring an existing graph)

### Behavioral claim sources

- `src/components/Welcome.tsx:67-69` — “Open an existing graph / Point Tine at a Logseq graph folder you already have.” The Welcome-screen step.
- `src/components/Sidebar.tsx:353-356,441-464` — “Tine lists its known graphs here alongside open/create actions”, with menu items “Open graph…” and “New graph…”. The later switching steps. NOTE: `docs/FEATURES.md:552` still says “(No saved recent-graphs list yet — you pick the folder each time.)” — stale relative to the known-graphs menu; the template claim follows current code. Recorded, not repaired (outside the allowed files).
- `src/components/Settings.tsx:2093-2097` — “Open another graph…” on the Graph tab; `README.md:199-200,208-209` — `TINE_GRAPH=/path/to/your/graph` and “Run one app at a time on a given graph.” The command-line and one-app-at-a-time claims.
- `docs/FEATURES.md:516-519` — “reads `:journal/file-name-format` and `:journal/page-title-format` and recognizes/creates journal files in your format”. The journal-honoring claim (`Settings.tsx:154-161` lists the shipped formats).
- `docs/FEATURES.md:511-512,532-538,543-545` — “skip byte-identical rewrites”, recursive folder scan, Org “byte-for-byte … read-only”. Cross-cited evidence shared with the Files page.
- `docs/FEATURES.md:222-227` — “config is a few harmless `tine.*` properties… Logseq renders the same file as an ordinary nested outline”. The view-property coexistence claim, linked to [[Features/Sheets]].
- `docs/FEATURES.md:506-509` — “A filesystem watcher… reconciles changes synced in from other devices”. The “changes appear automatically” claim; banner behavior cross-cited from `src/components/ConflictBar.tsx:45-56`.
- `crates/tine-core/src/templates/managed-sync.md:4` — “Your normal mode is **Direct files**… **Testing only.**” The single managed-sync pointer wording.

## Phase C — candidates: J09 recovery actions (Reference/Troubleshooting and recovery)

### Behavioral claim sources

- `src/components/ConflictBar.tsx:6-10,45-56` — “A save is refused (not clobbered) when the file changed on disk under us… skipped by every future save batch until resolved”, buttons “Use disk version” / “Keep mine (overwrite)”. The banner scenario and outcomes.
- `src/components/ContextMenu.tsx:939-947` — delete confirm: “The file moves to the graph's .tine-trash folder.”; `docs/FEATURES.md:547` — “page delete moves to a recoverable **trash**”. The trash-restore scenario; the file-manager restore path is the only restore surface (no in-app trash browser exists — verified by reading `AssetsTab` at `src/components/Settings.tsx:3330-3480`, which lists asset trash only).
- `src/components/Settings.tsx:2553-2574` — restore confirm “Your current state is snapshotted first, so this is reversible.” and `Settings.tsx:2589-2596` — snapshots happen “each time it opens… outside the graph”. The snapshot-restore scenario (snapshot scope: `journals/` and `pages/` per the same confirm).
- `src/components/Settings.tsx:2867-2900` — “Syncthing and Dropbox leave a `*.sync-conflict-*` copy… Review & merge shows a block-by-block diff… Discard copy trashes it (recoverable)”; fallback “The page this shadows no longer exists — discard the copy, or restore it in Logseq.” The merge scenario and the shadowless-copy case.
- `docs/FEATURES.md:520-522` — duplicate days “keeps **both**… **Open** / **Merge** / **Rename** / **Trash**”; `src/components/Settings.tsx:2799-2810` — “usually left over from changing the date format… Open reaches a file directly (it's editable and saves back to itself); Merge folds a stray into the canonical day; Rename turns it into a normal page; Trash removes the redundant one (recoverable).” The duplicate-day scenario.
- `README.md:218-231` — “run it with debug logging on… `TINE_DEBUG=1 tine` or `tine --debug`… defaults to `/tmp/tine-debug.log` (override with `TINE_DEBUG_LOG=/path`)… The log records no note content — only startup diagnostics.” The startup-recovery scenario.
- `README.md:210-213` — “On the rare GPU/compositor combo where WebKitGTK's DMABUF renderer aborts (the window fails to appear, or you see `EGL_BAD_PARAMETER`…), set `TINE_GPU=0` to fall back to software rendering”. The window-never-appears step.
- `docs/FEATURES.md:629-635` — “runs Tine's parser (lsdoc) against Logseq's own parser… Every divergence snippet is **anonymized**… and **re-verified to still reproduce the divergence**… nothing is uploaded.” The diagnostics/report scenario.
