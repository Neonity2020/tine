# 0021. PDF export: reuse the HTML render + the webview's own print engine

- **Status:** Accepted
- **Date:** 2026-07-05

## Context

Martin wants to export a page to PDF — a feature he used in Logseq (OG) via a
community plugin. OG itself ships **no** native PDF export (its `export.cljs`
offers only HTML / Markdown / OPML / PNG); the plugins that provided it did one of
two things: render the page HTML and call `window.print()` with a print
stylesheet, or rasterize the DOM with `html2canvas` → `jsPDF`.

Forces at play:

- **No new dependencies.** Tine deliberately keeps its weight down (it dropped the
  colour-emoji font to save 8 MB). A JS PDF writer (`jsPDF` + `html2canvas`, image-only,
  hundreds of KB) or a sidecar HTML→PDF engine (weasyprint / wkhtmltopdf /
  headless-chrome — external binaries to ship and sandbox) both fail this bar.
- **We already render the graph to HTML.** `publish.rs` renders any page to styled,
  self-contained HTML from the *same* lsdoc parser the app uses. That render is the
  expensive part, and it is done.
- **The editor virtualizes blocks.** Only the on-screen blocks of a long page are in
  the live DOM (ADR 0008, block virtualization), so `window.print()` on the *live app*
  would silently drop most of a long page. The export render is complete.
- **The webview can already print.** WebKitGTK / WebView2 / WKWebView each carry a
  browser-grade layout + pagination engine reachable through `window.print()` — real
  text, selectable, vector, paginated. `window.print()` is a native capability (unlike
  `window.confirm/alert`, which are wry no-ops — see the webkitgtk-confirm note), so it
  is available without a plugin.

The genuine alternatives were: (a) the JS raster route (rejected — bloat + poor
fidelity); (b) a sidecar engine (rejected — packaging weight, and it's the opposite
of the weak-machine target we optimize for); (c) a silent Rust
`webkit2gtk::PrintOperation` to a file (nicer UX, but Linux-only without
per-platform work, plus hidden-window load-timing complexity); (d) the print-dialog
MVP that reuses (b)'s render and the webview's engine.

## Decision

We will export a page to PDF by **rendering it to a self-contained HTML document in
`tine-core` and printing that document through the webview's own print engine**, via
the OS print dialog ("Save as PDF").

Concretely:

- `publish::page_print_html(graph, name)` renders one page to a **standalone** HTML
  document: the same block render as the site export, but with the stylesheet + a
  print stylesheet **inlined**, **no sidebar / search / app scripts**, and **image
  assets inlined as `data:` URIs** (a new `Ctx.inline_assets` flag; a dependency-free
  base64 encoder). It reuses the existing render path — it is not a second renderer.
- The frontend fetches that HTML and prints it in a **hidden same-origin `<iframe>`**
  (`iframe.srcdoc` → `contentWindow.print()`), so it prints the complete export
  document, not the virtualized live page and not the app chrome. Entry points: a
  page-title context-menu item **Export to PDF…** and an **Export current page to
  PDF…** command.

## Consequences

- **Easier:** zero new runtime dependencies; the PDF's content is exactly the HTML
  export's (one render path to maintain, already tested); all platforms work through
  the same `window.print()` path; the weakest webview (WebKitGTK) is a first-class
  target, not an afterthought.
- **Harder / costs we accept:**
  - The MVP is **dialog-driven** — the user picks "Save as PDF" (or a printer). A
    silent, no-dialog, choose-your-filename export (a Rust `export_pdf` command +
    `PrintOperation` file output) is a deferred follow-up (Linux-easy; Windows
    `PrintToPdfAsync` / mac `createPDF` are the per-platform cost).
  - **Whole-page only** for now; block-subtree export is a deferred follow-up.
  - Math / code typeset from **CDN** inside the print frame (KaTeX / highlight.js),
    same as a published page — offline prints raw TeX / plain code.
  - The print *document* render is screenshot-verified (`scripts/shot-printdoc.mjs`),
    but the interactive GTK print dialog can't be asserted headlessly — the
    end-to-end "dialog → good PDF" smoke test on real WebKitGTK is a tracked
    follow-up (see the backlog).
