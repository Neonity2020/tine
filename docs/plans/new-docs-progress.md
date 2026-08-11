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

- `crates/tine-core/src/graph_text_scope.rs` — the shared policy admits eligible `.md`, `.markdown`, and `.org` files graph-wide while excluding hidden/internal trees, assets, publish output, and provider conflict copies. This is the page-discovery and snapshot boundary.
- `crates/tine-core/src/model.rs` — existing graph text keeps its exact relative path; configured pages/journals roots classify and create new files rather than limiting discovery.
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
- `src/components/Settings.tsx` — restore confirms that files return to their original locations and that current state is snapshotted first. `src-tauri/src/backup.rs` schema 3 preserves graph-relative paths while schema 2 remains restorable.
- `crates/tine-core/src/model.rs` — typed trash uses `logseq/.tine-trash/pages`, `/journals`, `/assets`, and `/conflicts`, with timestamp-prefixed leaf names. `src/components/Settings.tsx` keeps non-asset recovery entries when emptying asset trash.
- `docs/FEATURES.md:520-522` — “Duplicate journal days lets you **Open** / **Merge** / **Rename** / **Trash** each”; `docs/FEATURES.md:523-531` — “`*.sync-conflict-*` (or `(conflicted copy)`)… out of your page list… **Review & merge** shows a **block-by-block diff**… per-block **keep-current / keep-copy / keep-both**… **Discard copy** trashes it… never auto-merged”. The conflict-copy claims.
- `crates/tine-core/src/templates/managed-sync.md:4,7` — “**Testing only.** Tine-managed storage is experimental. Your normal mode is **Direct files**” and “Do not enable it merely because you already synchronize your graph folders.” The single managed-sync pointer.
- `src/components/Settings.tsx:2103-2112` — Settings → Graph → “Export graph to HTML”; `docs/FEATURES.md:656` — “One-click **static HTML export** (`public:: true` pages)”; `docs/FEATURES.md:668` — “right-click a page title → **Export to PDF…**”; `docs/FEATURES.md:679` — “**Copy/export as** Markdown… with a *Rendered* mode”. The export claims.

## Phase C — candidates: J02 open-an-existing-graph path (Start/Bring an existing graph)

### Behavioral claim sources

- `src/components/Welcome.tsx:67-69` — “Open an existing graph / Point Tine at a Logseq graph folder you already have.” The Welcome-screen step.
- `src/components/Sidebar.tsx:353-356,441-464` — “Tine lists its known graphs here alongside open/create actions”, with menu items “Open graph…” and “New graph…”. `docs/FEATURES.md` now matches the shipped known-graphs menu.
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
- `src/components/Settings.tsx` — restore confirms “Your current state is snapshotted first, so this is reversible”; `src-tauri/src/backup.rs` captures eligible graph text at exact paths plus config and asset sidecars, and restores schema-2 snapshots through the legacy configured-root path.
- `src/components/Settings.tsx:2867-2900` — “Syncthing and Dropbox leave a `*.sync-conflict-*` copy… Review & merge shows a block-by-block diff… Discard copy trashes it (recoverable)”; fallback “The page this shadows no longer exists — discard the copy, or restore it in Logseq.” The merge scenario and the shadowless-copy case.
- `docs/FEATURES.md:520-522` — duplicate days “keeps **both**… **Open** / **Merge** / **Rename** / **Trash**”; `src/components/Settings.tsx:2799-2810` — “usually left over from changing the date format… Open reaches a file directly (it's editable and saves back to itself); Merge folds a stray into the canonical day; Rename turns it into a normal page; Trash removes the redundant one (recoverable).” The duplicate-day scenario.
- `src-tauri/src/debug.rs` — debug output defaults to `std::env::temp_dir()/tine-debug.log`, with `TINE_DEBUG_LOG` override. The Guide names the platform temporary folder rather than assuming Linux `/tmp`.
- `README.md:210-231` — the WebKitGTK GPU fallback is Linux-specific; `TINE_DEBUG=1` / `--debug` and the no-note-content diagnostic contract remain the startup recovery path.
- `docs/FEATURES.md:629-635` — “runs Tine's parser (lsdoc) against Logseq's own parser… Every divergence snippet is **anonymized**… and **re-verified to still reproduce the divergence**… nothing is uploaded.” The diagnostics/report scenario.

## Phase C — candidates: J03 day planning (Workflows/Capture and plan your day)

### Behavioral claim sources

- `src/components/Sidebar.tsx:147` — sidebar nav label “Journals”; `docs/FEATURES.md:408-409` — “Multi-day **journal feed** (one continuous editable list); today's journal created lazily on first edit”. The open-today-and-type path and “no file until you type”.
- `crates/tine-core/src/templates/quick-capture.md:13-16` — “**Leave the title empty** → the text is appended to **today's journal**… It's the real editor in there”; `src/store.ts:2013` — “Append a quick-capture”. The quick-capture pointer.
- `src/editor/marker.ts:1-5` — “cycle-marker-state: TODO -> DOING -> DONE -> (none); LATER -> NOW -> DONE -> (none); … Bound to mod+enter”; `src/keybindings.ts:385` — `{ id: "editor/cycle-todo", binding: "mod+enter" }`; `src/store.ts:3234-3247` — `cycleSelectionTasks` cycles every selected block; `docs/FEATURES.md:382-383` — “cycle with Mod+Enter (including every selected task at once)”. The marker-cycling actions.
- `src/components/Block.tsx:830-836` — marker chip `onClick` → `cycleBlockMarker`; `Block.tsx:906-915` — checkbox `toggleBlockCheckbox` (open → DONE, DONE → open marker); `src/markers.ts:44-53` — checkbox states (DONE checked, open unchecked, CANCELED/none no box); `docs/FEATURES.md:391-396` — same prose. The click paths.
- `src/editor/autocomplete.ts:422-424` — `{ label: "Priority A", action: "priority-a", key: "A" }` etc.; `src/editor/format.ts:223-224` — “Set (or replace) the `[#X]` priority … placed after any task marker”. The priority step.
- `src/components/Block.tsx:2045-2051` — slash `scheduled`/`deadline` “open the calendar popup anchored under the editor”; `Block.tsx:851-874` — date chips, title “Scheduled — click to change”, `openDatePicker`; `src/components/DatePicker.tsx:74` — “Optional clock time (`HH:mm`), like OG's ‘Add time’”. The schedule steps and outcomes (chip).
- `docs/FEATURES.md:397-405` — date picker, optional clock time `(<2026-07-07 Tue 14:30>)`, repeaters `+1w`/`.+1w`/`++1w`, “Re-picking the date keeps an existing time (and repeater)”, type-anywhere planning lines normalized on exit, fenced/inline-code planning lines stay literal. Cross-check for the stored-syntax claims.
- `src/editor/repeat.ts:42-44` — “`.+` repeats from the completion date (today); `+`/`++` from the stored date. `++` is catch-up”; `repeat.ts:68-86` — completing a repeater advances dates “and resets the marker to the workflow's open state”. The repeater summary.
- `src/components/Page.tsx:462-473` — “Agenda sits at the bottom of today's (the first) day, like OG.” `<QueryMacro body={agendaQuery()} title="Scheduled & Deadline" hideWhenEmpty>`; `src/ui.ts:469-514` — default window 7/7, window “tested against the scheduled/deadline date itself — NOT the journal day”, “Finished tasks are excluded … matches OG's `:block/marker "NIL"` default”; `Settings.tsx:1902-1928` — “Agenda window … days back · days ahead”. The agenda section.
- `src/components/Page.tsx:954-996` — carry buttons `Carry unfinished tasks → today` / `Carry from previous day` / `Carry last {N} days` under journal titles; `src/carry.ts:71` — toast `Carried ${n} items to today` / “No unfinished tasks to carry”; `ContextMenu.tsx:888` — right-click “Carry unfinished tasks → today”; `src/keybindings.ts:353-356` — command-palette presets 7/30/365/N; `src/ui.ts:432-456` — defaults (buttons shown, keep-context on, header off); `Settings.tsx:1822-1852` — the carry settings and hints. The carry-over section.
- `src/editor/queryBuilder.ts:17,187-189` — `(task TODO DOING NOW LATER)` DSL clause (list of markers). The executable-query example.
- `crates/tine-core/src/templates/showcase.md:57-73` — the Feature showcase page already ships open-task examples — why the workflow page's query outcome names other pages' open tasks too.

### Reference page (Reference/Journals, tasks, and scheduling) sources

- `src/journal.ts:1-5` — frontend title format mirrors the backend, “defaults to Logseq's ‘MMM do, yyyy’”, fed by `GraphMeta.journal_page_title_format`; `journal.ts:90-93` — `[[journal title]]` links route “to the journal page rather than opened as an empty regular page”. The journal-routing claim.
- `src/components/Settings.tsx:1784-1794` — “How journal dates are displayed and how new `[[date]]` titles are written. Display-only — your journal *file names* are untouched and existing journals keep working. Saved to `:journal/page-title-format`”; `src/types.ts:593` — `journal_file_name_format` “`:journal/file-name-format` (default `yyyy_MM_dd`)”. Date-format scope.
- `src/components/Settings.tsx:1524-1532` — “Template inserted into a new day's journal. Saved to `:default-templates {:journals …}`… ‘(none)’ → blank days (the default). Make a template via a block's right-click menu.” New-journal template.
- `src/markers.ts:13-25` — the full marker set (TODO DOING NOW LATER WAITING WAIT STARTED IN-PROGRESS DONE CANCELED CANCELLED), “stored” as the first word (markers.ts:37-42 `MARKER_RE`).
- `src/components/Settings.tsx:1855-1874` — “Task workflow … Saved to `:preferred-workflow` in `config.edn`, so it travels with the graph”, buttons “TODO / DOING” vs “NOW / LATER”; `src/ui.ts:106-107` — `createSignal<"now" | "todo">("now")`. Workflow default.
- `src/editor/autocomplete.ts:412-426` — slash commands for every marker plus “Priority A/B/C”, “Scheduled”, “Deadline”; `src/components/Block.tsx:2045-2051` — the date commands “open the calendar popup anchored under the editor”; `Block.tsx:851-874` — date chips with title “Scheduled — click to change” / “Deadline — click to change”.
- `src/editor/planning.ts:1-10,63-64` — “type anywhere, normalize on exit… first line → SCHEDULED → DEADLINE → properties”, “Canonical order: SCHEDULED before DEADLINE”, inline-code/fenced `SCHEDULED:` never moved. Stored-syntax claims.
- `src/components/DatePicker.tsx:13` — “First day of week from the Tine display pref”; `Settings.tsx:1796-1804` — “Starting column of the calendar and the scheduled/deadline date pickers. Saved to `:start-of-week`”. The picker-startweek claim.
- `src/components/Macro.tsx:437-439` — `hideWhenEmpty && !ADVANCED_RE && total() === 0` hides the block (user-authored queries instead show “No results”, Macro.tsx:114-116). The agenda “disappears entirely” claim.
- `src/ui.ts:509-514` — `agendaQuery()` returns `query (and (or (between scheduled -7d +7d) (between deadline -7d +7d)) (not (task DONE CANCELED CANCELLED)))` at default 7/7 — the stored-query quotation on the page.
- `src/components/Settings.tsx:1877-1888` — “Time tracking … Marker transitions write OG-compatible `:LOGBOOK:` CLOCK rows. Saved to `:feature/enable-timetracking?`; seconds mode follows `:logbook/settings`”; `src/ui.ts:116-122` — both default true. Logbook claims.
- `src/sheet/fields.ts:319-334`, `src/components/SheetTable.tsx:1379` — sheet state/priority/deadline cells route through the same marker/date machinery (“The same chips and pickers appear wherever the block does”).
- `docs/SETTINGS-INVENTORY.md:30-40` — Journals-tab rows exist for every control named on the page (Journal date format, First day of week, carry-over trio, Task workflow, Time tracking, New-journal template, Agenda window).

### Discrepancies / deliberate scope

- `docs/FEATURES.md:552` (recent-graphs staleness) still pending from the J02 slice; not touched.
- No per-platform quick-capture claim beyond the desktop shortcut flow already on [[Features/Quick capture]] (its page leads with Linux DE setup); Android has no global-capture story here, so none is asserted.
- HEAD's `reference-troubleshooting-and-recovery.html` (and two sibling pages) were stale against the template corrections in `360f251d` (“Correct Guide file and recovery claims”); the J03 regen refreshed them — flagged here so the manager knows those tracked-HTML changes are build output, not new wording by this worker.
- `website/demo/search-index.js` and `website/demo/feature-showcase.html` are untracked build artifacts on this branch (untouched by the commits; noted for the manager in case tracking them is intended).
