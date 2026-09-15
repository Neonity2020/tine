# 0065. A published query export ships the read-only app over a baked snapshot

- **Status:** Accepted
- **Date:** 2026-09-14

## Context

"Publish a query" (Stage 1) writes the whole owner pages of a query's results as
a static HTML site under `published-queries/<name>/`. Martin's rich-export idea
is that the same folder should open as the real Tine app — the query's rows,
page navigation, Linked References, block previews — in an ordinary browser,
read-only, with nothing installed.

Three routes were on the table for where the answers come from:

- **R-A — the engine in the browser.** Compile `tine-core` to wasm and run the
  query engine over the exported pages client-side. The spike showed this
  cannot be built with the pinned dependencies: `libsqlite3-sys 0.35` supports
  only `wasm32-wasi*`, not the browser target, and the engine's answers are
  SQL-only by invariant.
- **R-B — a second, browser-side engine.** sqlite-wasm plus the query
  executor extracted from `tine-core` into a separate crate built for the
  browser. Two builds of one engine still drift in what surrounds them (the
  capture, the recency axis, the registry); the project already retired a
  "walk arm" once so that every query answer is one engine's answer.
- **R-C — bake.** Run the one native engine at export time over the closed
  sub-graph of exported pages, record every query's `parseQuery` answer and
  `queryRun` result, and ship the frontend build beside a `snapshot.json`. The
  frontend gets a third `Backend` that answers from that file.

The frontend already had the seam: `backend()` picks the Tauri backend or a
browser mock, and the app boots in a plain browser today.

## Decision

We will ship **R-C**. A query export's `app/` is the ordinary frontend build
(`vite base: "./"`, one build for Tauri and the export) plus `snapshot.json`
(schema 1): the exported pages as `PageDto`s (all `read_only`), page entries,
backlinks, block-ref counts, aliases, icons, and one record per query executed
over the export. The export's own home page (the export name, or
`<name> (export)` on a name collision) hosts the exported query and links each
exported page.

`src/publishedBackend.ts` answers the `Backend` interface from the snapshot.
`parseQuery` and `queryRun` are lookups keyed exactly as `Macro.tsx` asks —
dialect, `tine.*` host properties and text; stable JSON of the IR, the current
page and the view the run was baked under (two `(task TODO)` twins on one page
that differ only in `tine.sample::` are two records) — and a miss is a typed
`query-unavailable` / `published_export_static`, never a browser-side run.
The engine runs each query under the view its result kind resolves to, the
way the app does (`anchored_view`), and the export's home block carries the
host block's `tine.*` properties so the home opens under the same display
(an advanced home has none to carry; it runs under its own settings). The
engine's view is written densely and the app's resolved sparsely; the two are
compared under one normalizing key, pinned from both sides. The Quick Switcher's
navigation search is a substring match over the snapshot's page names and block
text, as the browser mock's is; every other search lane is refused. Every other method is
classified answered / constant / refused, and the guard test
(`publishedBackend.guard.test.ts`) fails on an unclassified method.

The engine runs the exported queries the way the app does: `<% current page %>`
is substituted, the query runs in the page's context, `#+BEGIN_QUERY` keeps its
table view, nested result rows are not recorded, and the home run excludes its
host block. The executor gained a scope for this (`publish::Ctx.scope`), used
only by query exports.

Hosted over HTTP the root `index.html` redirects to `app/`; `file://` or
`?static` keeps the static site. Twemoji, theme thumbnails and the capture
window are not shipped; the export uses the browser's emoji face.

## Consequences

- One engine, one answer: everything a reader sees was computed natively at
  export time. No query semantics exist in TypeScript.
- The decision is reversible at exactly two methods. A future browser-side
  engine (R-A) replaces `parseQuery` and `queryRun` in `publishedBackend.ts`;
  nothing else in the app changes.
- Exports are static by design: view changes and new queries are refused with
  a typed reason, and the Guide says so; local table-column sorting re-orders
  baked rows in the browser and changes nothing. A reader who wants more needs
  the app. Settings, favorites, the graph switcher and the plugin registry are
  not offered in an export. A sampled query says "sample of N" beside its
  count, since the builder sentence that says so in the app is not offered.
- The frontend bundle is embedded in the binary already; an export copies it
  (~8 MB of assets) beside the pages. No second build, no second embed.
- `Backend` grows a third implementation that must be kept classified; the
  guard test makes that a compile-time-shaped obligation rather than a review
  note.

**Unit cost:** per export, one `snapshot.json` (the exported pages' DTOs plus
one record per query; 3.4 KB for the three-page E2E fixture with one query,
716 KB for 86 pages and 4 queries on the anonymized real-graph corpus) and one
copy of the frontend bundle (~8 MB); nothing per edit, nothing persisted in the
graph, nothing read back. Export of the 86-page selection took 0.3 s and the
published app booted in 0.25 s over local HTTP. Measured on the
`e2e-publish-query` fixture and the anonymized corpus, 2026-09-14.
