<p align="center">
  <img src="docs/logo.svg" alt="Tine" height="84">
</p>

<p align="center">
  <b>A fast, local, Logseq-compatible outliner.</b><br>
  Reads and writes the <i>same</i> Markdown graph as Logseq — swap between the two on the same files.
</p>

<p align="center">
  <a href="README.md">English</a> | <a href="README.zh-CN.md">简体中文</a><br>
  <sub>The English README is authoritative if the versions differ.</sub>
</p>

<p align="center">
  <img src="https://img.shields.io/badge/Tauri-2-24C8DB?logo=tauri&logoColor=white" alt="Tauri 2">
  <img src="https://img.shields.io/badge/SolidJS-1.9-2C4F7C?logo=solid&logoColor=white" alt="SolidJS">
  <img src="https://img.shields.io/badge/Rust-2021-000000?logo=rust&logoColor=white" alt="Rust">
  <img src="https://img.shields.io/badge/platform-Linux%20%7C%20macOS%20%7C%20Windows%20%7C%20Android%20%7C%20iOS-555" alt="Platforms">
  <img src="https://img.shields.io/badge/license-AGPL--3.0-blue" alt="AGPL-3.0">
</p>

<p align="center">
  <img src="docs/img/hero.png" alt="Tine — journals view" width="820">
</p>

<p align="center">
  <b>▶ <a href="https://tine.page/guide/">Try it live in your browser</a></b> — the Guide, published with Tine's own live export: search, tabs and split view all work.<br>
  <a href="https://tine.page/">Website</a> · <a href="https://tine.page/compare.html">Tine vs Logseq</a> · <a href="https://github.com/martinkoutecky/tine/discussions">Discussions</a> · <a href="#support">♥ Support</a>
</p>

---

## What is Tine?

Tine is an outliner for desktop and mobile, built to look and feel like [Logseq](https://logseq.com) while being
much faster. It operates directly on the standard Logseq graph layout —
`journals/`, `pages/`, `assets/`, and `logseq/config.edn` — so you can point it at the graph you
already use and keep editing in either app (one at a time). Files are written back in
Logseq-compatible Markdown, so there's **no import/export step and no lock-in**.

**Why build it?** Logseq's UI is Electron + DataScript with heavy re-rendering, and it gets
sluggish on large graphs. Tine is a ground-up rewrite: a small native shell (Tauri/WebKitGTK), a
pure-Rust core for parsing and indexing, and a fine-grained reactive frontend (SolidJS) that never
diffs a virtual DOM. The editor keeps the live block tree in the frontend, so keystrokes never
round-trip to Rust, and whole-graph reads (search, backlinks, queries) hit an in-memory cache
instead of re-parsing.

> **Status:** a usable daily driver on Linux, macOS, Windows and Android, with an iOS beta.
> Not yet 1.0 — see [Roadmap](#roadmap).

---

## Install

Grab a prebuilt installer from the **[Releases](https://github.com/martinkoutecky/tine/releases)**
page. macOS builds are signed and notarized; Windows builds aren't code-signed yet, so Windows may
warn the first time — here's how to get past it:

- **Linux** — the **AppImage** runs on any distro with no install: `chmod +x Tine_*.AppImage`, then
  run it. Or use the **`.deb`** (Debian/Ubuntu) or **`.rpm`** (Fedora/openSUSE).
- **macOS** — open the universal **`.dmg`** and drag Tine into Applications.
- **Windows** — run the **`.exe`** installer; if SmartScreen appears, click **More info → Run
  anyway**. Prefer no installer? Grab the portable **`Tine_*_x64-portable.zip`**, unzip, and run
  `Tine.exe` — it needs the WebView2 runtime, which is preinstalled on Windows 10/11.
- **Android** — install from **[F-Droid](https://f-droid.org/packages/page.tine.app/)**, or sideload the **`.apk`** from the Releases
  page.
- **iPhone / iPad** — join the public beta on **[TestFlight](https://testflight.apple.com/join/rpGGpTVW)** (install Apple's TestFlight
  app, then open the link). There is no App Store listing yet.

(Want to hack on Tine instead? See [docs/DEVELOPING.md](docs/DEVELOPING.md). Something wrong? See
[docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md).)

### Command line

The desktop binary also supports terminal workflows. Run `tine --help` for the full reference.

```sh
tine --version
tine open ~/notes
tine capture
tine doctor ~/notes
tine export static ~/notes
tine export live ~/notes --all-pages --output public-site --name "My notes"
```

Exports select pages using the graph's publication settings. Output directories are relative to
the graph, and existing output is retained unless you explicitly pass `--replace`. Linux `.deb`
and `.rpm` packages install the generated `man tine` page.

---

## Highlights

What Tine adds on top of Logseq — the things that started as *"I wish Logseq did this."*

- **⚡ Native speed** — a pure-Rust core and a tiny native runtime instead of Electron; typing
  stays in the frontend, whole-graph reads hit an in-memory index.
- **🪟 Split view and tabs** — panes with their own tabs and history, `Alt+click` to open beside,
  middle-click for a background tab, pins, drag between panes, browser-style back/forward.
- **🔎 Search that becomes a query** — open every Ctrl+K result in a persistent tab, refine it,
  view it as a list/table/board, and name it to make it a query page. The visual query editor and
  the query text stay in sync.
- **▦ Sheets** — grids, typed field tables, boards, formula columns, filters, aggregates and CSV
  import over plain bullets.
- **🤝 Concord** — sync-conflict copies (Syncthing, Dropbox) and git conflict markers reviewed in
  place, block by block, with a suggestion for each difference; conflicts are never silently
  overwritten, and launch snapshots plus delete-to-trash back everything up.
- **🌐 Live export** — publish pages as a static site that still searches, tabs and splits like
  Tine ([the Guide](https://tine.page/guide/) is one); `tine export live` from the command line.
- **⚡ Global quick-capture**, **🎯 focus mode + dim**, **🔁 carry unfinished tasks forward**, an
  **📖 in-app Guide**, and a first-run **👋 demo graph** that opens in Logseq too.

<p align="center">
  <img src="docs/img/quick-capture.png" alt="Global quick-capture mini-window" width="32%">
  <img src="docs/img/focus-dim.png" alt="Focus mode with inactive blocks dimmed" width="32%">
  <img src="docs/img/tabs.png" alt="Built-in tabs" width="32%">
</p>

<p align="center">
  <img src="docs/img/sheets.png" alt="Sheets grid, field table, and task board" width="640">
</p>

<p align="center">
  <img src="docs/img/waveform.png" alt="Audio waveform overlay player with skip and speed controls" width="640"><br>
  <sub>Paste audio, then <b>⤢ Expand</b> to a waveform scrubber (±5s / ±15s skip, speed, time read-out) — no Logseq-core equivalent.</sub>
</p>

---

## Features

A quick map of what's in the box — the **[full feature list lives in docs/FEATURES.md](docs/FEATURES.md)**,
and the **[Guide](https://tine.page/guide/)** shows the rendered-content side of it in your browser.

| Area | Highlights |
|------|-----------|
| **Outliner** | Click-to-edit with exact caret landing, Logseq keyboard semantics, zoom, drag-reorder, multi-block select; in-block lists & checklists; callouts; a live `/calc` block. |
| **Media** | Paste/import images, video & audio; configurable asset names; drag-resize images *and* video; an audio waveform overlay player; image lightbox; orphaned-media cleanup. |
| **Links, refs & queries** | `[[page]]` · `#tag` · `((block ref))` · `{{embed}}` with autocomplete; live linked/unlinked references; per-block ref counts; one query engine behind friendly filters, a visual builder, raw DSL and search/list/table/board views; a scoped Datalog path. |
| **Tasks, journals & dates** | Task workflows + priorities, scheduled/deadline with a date picker, recurring tasks, carry-forward, a multi-day journal feed, agenda, and a calendar. |
| **PDF** | Zoomable virtualized viewer, in-PDF find, text + area (image) highlights stored Logseq-compatibly, each a bullet you can annotate. |
| **Search & nav** | `Ctrl+K` switcher (titles + full text) with persistent result tabs, bounded match evidence and query-page promotion; command palette, in-app Guide, namespace tree, tabs, split view, back/forward, focus mode, global quick-capture, page icons. |
| **Your files** | Safe to run alongside Logseq mobile over Syncthing — Concord conflict review, format-preserving atomic saves, transactional rename, org-mode (byte-faithful or read-only), snapshots + trash. |
| **Customize & export** | Remappable shortcuts with `?` help, built-in theme gallery + custom CSS, multi-language spell check, static and live HTML export, copy/export as Markdown, **export a page to PDF** (desktop). |

→ **[Every feature, with the details, in docs/FEATURES.md.](docs/FEATURES.md)**<br>
→ **[Tine vs Logseq, feature by feature](https://tine.page/compare.html)** — an honest, dated
comparison: parity, scoped, not-yet, and what Tine deliberately doesn't do.

---

## Community

Everything happens on GitHub. Questions, ideas, screenshots and show-and-tell go to
**[GitHub Discussions](https://github.com/martinkoutecky/tine/discussions)**; concrete bugs and feature requests go to
[issues](https://github.com/martinkoutecky/tine/issues).

## Contributing

Tine is a single-maintainer project with an unusual contribution model: **the most valuable thing
you can send is testing and bug reports**, and code changes come in as **proposals/specs the
maintainer implements** rather than merged patches (docs/typo PRs excepted). The why, and how to
file a good report, are in **[CONTRIBUTING.md](CONTRIBUTING.md)**. Building from source:
**[docs/DEVELOPING.md](docs/DEVELOPING.md)**.

## Roadmap

Tine is at 0.6: Sheets, split view, the query engine, Concord, live export, mobile and an
experimental [capability-limited plugin API](https://tine.page/plugins.html) have shipped. Next: 0.7
rounds off the query engine and live export, then 0.8 brings localization; a graph view is under evaluation. **Out of scope by design:** whiteboards, flashcards, Logseq/Obsidian plugin
compatibility, and built-in sync. The working backlog — next, deferred and WONTFIX — is
[`docs/BACKLOG.md`](docs/BACKLOG.md).

## Support

Tine is free, open source, and built in the cracks of a busy life. If it's made your notes a little
faster or your day a little smoother, and you'd like to say thanks with a coffee — that genuinely
makes me happy, and I'm grateful. Anything donated goes first toward Tine's running costs — the
domain, and things like app-store registration fees down the line.

One honest thing, so there are no crossed wires: **donating won't move Tine up my list, and it won't
buy a feature.** I work on Tine when I can and on what I find worth doing — a tip changes none of
that. Think of it as a thank-you for what already exists, not a down payment on what's next. No
expectations, no obligations, either way. 🌱

[![Ko-fi](https://img.shields.io/badge/Ko--fi-support-FF5E5B?logo=kofi&logoColor=white)](https://ko-fi.com/martinkoutecky)
· [GitHub Sponsors](https://github.com/sponsors/martinkoutecky)

## Unofficial Tine resources

- [Tana to Tine](https://github.com/mikob/tine/tree/converters) — convert Tana workspaces to Tine.

## Acknowledgements

Tine is an independent reimplementation, not a fork — the codebase is original Rust + SolidJS and
contains no Logseq source. It does target Logseq's on-disk format and adapts parts of Logseq's
outliner CSS (variables and bullet/indent rules), so it is a derivative work for licensing purposes
and is released under the same license.

[Logseq](https://github.com/logseq/logseq) is © its authors, licensed AGPL-3.0. Tine is **not
affiliated with or endorsed by Logseq.** Thanks to the Logseq project for the format and the design
it pioneered.

## License

[GNU AGPL-3.0-only](LICENSE).

Copyright (C) 2026 Martin Koutecký.

This program is free software: you can redistribute it and/or modify it under the terms of the GNU
Affero General Public License as published by the Free Software Foundation, version 3. It is
distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the implied
warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the [LICENSE](LICENSE) for
details.

---

<sub>Built ground-up as a faster, file-compatible alternative to Logseq. Not affiliated with Logseq.</sub>
