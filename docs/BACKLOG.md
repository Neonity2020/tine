# Tine + lsdoc — Backlog & Triage

The single place where deferred work lives, so it stops living in Martin's head.
Derived from a full sweep of memory files, ADRs, `~/research/tine-notes/`, and in-code
markers (2026-07-01). The README's *Roadmap & non-goals* is the public summary; this is
the working detail.

**Categories are deliberate — they are not the same thing:**
- **In flight** — being built right now.
- **P1 / P2** — will do; P1 next, P2 when P1 clears.
- **Deferred** — genuinely might do later, but no slot yet. *Deferred is NOT WONTFIX.*
- **WONTFIX** — decided we are not doing it, with a reason. (By-design non-goals + a few
  subsystem calls.)

Anything moved between categories should be edited here in the same chunk of work.

---

## In flight (mainline now)

| Item | Where |
|---|---|
| **lsdoc single-pass rebuild** — replace the optimistic scanner with a real two-phase lexer (container-prefix walk → hiccup index → inline delimiter stack). Acceptance = the op-count gate (`cargo test --test complexity`). | lsdoc; [[lsdoc-single-pass-audit]] |
| **lsdoc raw-HTML unification (D10/D11/D12)** — one source-faithful port of mldoc's `Raw_html.parse`, routed from block + inline; de-dups 4 look-alikes. Plan written, approved, awaiting codex dispatch. | `lsdoc/subagent-tasks/raw-html-unification-plan.md` |

---

## P1 — do next (high value, bounded scope)

| Item | Notes |
|---|---|
| **Table column-alignment render** | AST already carries `Table.aligns` (gated). Tine render (`AstList`) + HTML export (`render_block`) don't consume it yet → emit `data-align`. Small, pure OG-parity gap. |
| **lsdoc divergences D13/D14/D15** | D13: md link-label doesn't reparse entities/latex (`[\alpha](u)`, `[$x$](u)`). D14: timestamp order-permissive (`<… +1d 12:00>`). D15: md drawer name rejects punctuation (`:LOG@BOOK:`). All verified vs oracle; batch into the divergence loop. OG-parity. |
| **User-defined `:macros` text substitution** | Highest *user-facing* payoff in the macro cluster: honor the graph's `:macros` config map at render time. Medium effort. |
| **Easy embed macros** — twitter, vimeo, bilibili, `img` | Quick OG-parity wins, self-contained — good for fragmented sessions. |
| **Tauri updater: verify signing password secret** | Setup landed (commit `1dbd1d7`) but never tested end-to-end; verify the password secret matches the key so auto-update is trustworthy. |

---

## P2 — backlog (real, lower urgency)

| Item | Notes |
|---|---|
| **Block virtualization (windowing)** | Perf prime directive, but **measurement-gated**: first drive a ~5k-block page and measure; if it hurts, do windowing. Subsumes the "optional AST parse cache-prime to kill the fallback→AST flash" item. |
| **Plugin CSS-variable alias shim** | Martin: "put it on the backlog, I want to get back to it." Realistic slice = alias OG `--ls-*` vars so the Awesome-Styler theme family "mostly works". NOT full `@logseq/libs` compat (that's WONTFIX). ~1–2 days. |
| **Custom overlay scrollbar** | Cosmetic, auto-hide on idle, draggable. ~1 hr. No usability impact. |
| **Datalog query coverage expansion** | Scoped subset works today (EDN front-end on the Pred engine; unsupported clauses are flagged, not silently dropped). Expand clauses on demand — which clauses matter isn't known until a real query needs one. |
| **Graph view** | Martin can live without it; also a README "planned" item. Defer until asked. |
| **lsdoc M11 cleanup** | After the single-pass rebuild lands: delete both v1 inline scanners (`inline.rs` Scanner + `org.rs` OrgScanner) + the cache zoo they justified; fold keeper tests into `perf.rs`; rewrite `DESIGN-lsdoc-v2.md`. Pure cleanup. |
| **lsdoc P2 unification opportunities** | Analysis-only, behavior-preserving (list / display-math / quote-helper / bracket-scan dedup; inline-ctx boolean-bags). Needs Martin's approval before applying. |
| **Lower-tier data-safety** | Syntax-aware rename (skip code fences/URLs; narrows the inherent TOCTOU window vs a non-cooperating external editor — A1 already made it transactional w/ rollback); CRLF handling (#23, low trigger on an LF graph); restore-config-dirs; backup-name ms-collision. |
| **Org-page checkbox toggle edge cases** | Positional line-match toggle implemented (best-effort R12); may have edge cases on org pages. |
| **Interactive graph view in HTML export** | Richer export shipped (sidebar + search); no interactive graph yet. |
| **Screenshot harness upkeep** | Standing hygiene: `docs/img/` don't regenerate on build; refresh per `docs/SCREENSHOTS.md` when a feature changes. |

### P2 / low — performance lower-tier
All explicitly excluded from the launch plan; do only if a measurement demands it.

- **PERF #24** — RCU `Arc` cache snapshot (per-entry `Arc<Vec<Arc<…>>>`, not naive `Arc<Vec>`); removes save-vs-scan lock contention. Touches the data-safety-critical cache core → its own tested pass.
- **PERF C1** — per-block projection cache (#51). Risk-gated.
- **PERF C2** — scoped query invalidation (#52). Risk-gated.
- **Autosize-on-type** — skip on WebKitGTK.
- **Undo** — byId-scan index.
- **Startup** — backup/warm timing.

---

## Deferred — genuinely later, no slot yet (NOT WONTFIX)

| Item | Notes |
|---|---|
| **macOS notarization** | Mac build is unsigned → "unidentified developer" wall (right-click→Open works around it). Not spending $99/yr on an Apple Dev ID right now; Martin is exploring signing via a friend's ID, or revisiting later. Signing secrets (`APPLE_*`) are already stubbed in `release.yml`. Document the workaround in the README meanwhile. |
| **Verso/Servo engine swap** | The long-term answer to the WebKitGTK scroll gap. Servo's web-compat for a dense editor (contenteditable, complex CSS, PDF.js, KaTeX) isn't ready. Revisit ~early 2027. |
| **TreeSheets nested-grid ("breadth") concepts** | Post-public exploration; spec drafted at `docs/breadth-grid-spec.md`. Real work is the modal cell-select keyboard model, not the geometry. |
| **lsdoc M7 — explicit `lex_lines` line-lexer** | Would be dead code after the M8/M9 block rewrite already hit O(n); a large lateral rewire for stylistic uniformity, zero perf/correctness gain. Only if a focused clarity pass is wanted. |
| **lsdoc consumer-recursion → iterative project/serialize** | Deep Block tree's recursive drop/project/serialize is bounded by ~6k stack frames (strictly better than mldoc's ~1000, adversarial-only). Making it iterative is the only thing that removes the ceiling; explainer owed. |
| **Hiccup → HTML transform** | Render Clojure hiccup `[:tag …]` as HTML instead of literal text. Absent from every real graph — upgrade only if one appears. |
| **YouTube-timestamp seeking (full)** | OG-faithful seeking needs the YouTube IFrame Player API (Tine has a bare iframe). The degraded "render the timestamp as a clickable link" version is a cheap P2 alternative; full seeking is deferred. |

---

## WONTFIX — not doing (with reason)

### By-design non-goals (already public in README)
| Item | Reason |
|---|---|
| **Whiteboards** | Separate application domain; Tine is a fast local-first outliner. |
| **Flashcards / SRS** | Needs a dedicated spaced-repetition review engine; out of scope for an outliner. |
| **Full plugin system (`@logseq/libs`)** | Months of work (a datascript engine + OG's React render model); not worth it solo. Tine coexists with Logseq instead. |
| **Built-in git** | Delegate to the user's sync tool (Syncthing, etc.). |
| **Native mobile app** | Coexist with the Logseq mobile app over your own sync, not replace it. |

### Recommended WONTFIX (Martin's call — flag if you disagree)
These inherit the non-goals above; recorded here so they stop resurfacing as "someday".
| Item | Reason |
|---|---|
| **`function` macro (SCI/Clojure evaluator)** | Needs a Small-Clojure-Interpreter + plumbing the surrounding `{{query}}` result set into the macro. Huge, rarely used. |
| **`cloze` / `cards` macros** | Only meaningful inside the SRS review loop → same reason as flashcards. (Render-only "always visible / click-to-reveal" is a trivial fallback if ever wanted.) |
| **Zotero connector macros** | Niche; needs a Zotero data-dir + item-metadata + storage-path resolution Tine has none of. |
| **CEF / Chromium engine swap** | Evaluated and ruled out: ~150–250 MB multi-process Chromium per OS, a JS-compat shim, packaging + macOS notarization. Stay on WebKitGTK; Verso is the live long-term bet (Deferred, above). |

---

## Closed by verification (2026-07-01)

| Item | Finding |
|---|---|
| **DS #21 — page/journal composite key** | **Already addressed.** The store is a `Vec<(PageEntry, Arc<Document>)>` keyed by full entries (kind + path), lookups go through `load_named(name, kind)` → `find_entry(name, kind)`, and the `disk_revs` side table uses a composite `rev_key(kind, name)` whose doc comment says "scoped by kind so a page and a journal of the same title never collide." The name-only collision the item described does not exist in the current code — the memory was stale. Residual (not data-loss): a bare `[[Title]]` matching both a page and a same-titled journal is a link-*resolution* nuance; file separately only if it actually bites. |
