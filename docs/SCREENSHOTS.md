# Screenshots — keeping them current

The README's images live in `docs/img/`. They are **not** auto-generated on
build, so they drift silently when the UI changes. **When you ship a feature or
change an existing feature's design, check the table below and regenerate any
shot it touches** (and update the README prose in the same commit).

## Verify UI work visually before handing it off (project process)

The same harness is how Claude **self-verifies any visual/UI change** — not just
README shots. For a new or changed UI feature: build, drive the relevant state in
the mock, screenshot it, look at the image, and iterate until it matches the
intended (and OG) look *before* asking Martin to test. Martin has OK'd the extra
token cost — it saves his time, and he shouldn't be the one eyeballing something
that can be screenshotted here. (No Xvfb needed; it's headless Chromium against
the real frontend. Xvfb + the real WebKitGTK app is reserved for WebKitGTK-only
rendering quirks — fonts/emoji — not layout.)

## How they're made

All shots are **headless Chromium renders of the built frontend against the mock
backend** (`src/mock.ts` + `src/fixtures/kitchen-sink.md`) — *not* the real
WebKitGTK app. So they reflect the real layout/CSS and current components, but
with fake data and Chromium font rendering. No display/Xvfb is needed.

```bash
source scripts/env.sh        # sets PLAYWRIGHT_BROWSERS_PATH + the nspr/nss libs
npm run build                # refresh dist/ so the harness serves current code
node scripts/shot-readme.mjs # → screenshots/rm-tabs.png, rm-focus-dim.png, rm-quick-capture.png
node scripts/shot-capture.mjs# → screenshots/rm-quick-capture.png (better: slash menu + window frame)
node scripts/screenshot.mjs  # → screenshots/journals-light.png, pdf-notes-light.png, … (the review set)
node scripts/shot-improve.mjs# → screenshots/improve-{empty,report,findings}.png (Help improve Tine diff panel; uses __tineDiffFixture)
node scripts/shot-query-sheet.mjs # → screenshots/query-{sentence,sheet,sheet-narrow}.png (the query builder's two states, wide and phone-width)
node scripts/shot-query-vocabulary.mjs # → screenshots/query-vocabulary{,-search,-novel,-narrow,-narrow-chosen,-phone,-phone-chosen}.png + query-{text-pane-invalid,advanced-pane,crossing-notice}.png
```

Scripts write to the gitignored `screenshots/` dir; the README set is then
**curated by hand** — copy the chosen file to its `docs/img/` name (below).

## README image inventory

| `docs/img/` file     | Shows                                              | Generator → source file                          | Regenerate when… |
|----------------------|----------------------------------------------------|--------------------------------------------------|------------------|
| `hero.png`           | Journals feed (blocks, tasks, code, table, sidebar)| `screenshot.mjs` → `journals-light.png`          | journal/outline rendering, block markers, **sidebar sections (e.g. Namespaces)** change |
| `tabs.png`           | Built-in tabs (3 tabs, one pinned)                 | `shot-tabs-pdf.mjs` → `feat-tabs.png`            | tab bar, pinning, or topbar layout changes |
| `focus-dim.png`      | Focus mode + dim-inactive-blocks                   | `shot-readme.mjs` → `rm-focus-dim.png`           | focus/dim behavior or chrome changes |
| `dim.png`            | Dim-inactive-blocks (one block spotlit)            | `shot-features.mjs` → `feat-dim.png`             | dim behavior changes |
| `carry.png`          | Carry-unfinished-tasks buttons on a journal        | `shot-features.mjs` → `feat-carry.png` (clipped) | carry UI/buttons change |
| `query.png`          | The query sheet open over a query block            | `shot-query-sheet.mjs` → `query-sheet.png`       | the resting sentence, the anchor line, or the sheet's rows change |
| _(probe only)_       | The one vocabulary picker on a 250-key graph, narrowed to a rare key, and the two honest "use this key by name" rows | `shot-query-vocabulary.mjs` → `query-vocabulary{,-search,-novel}.png` | the picker's sections, row metadata, row heights or windowing change (verification probe, not a curated README image) |
| _(probe only)_       | The same picker at 560px and at a phone's 390px: a property row and its count brought into the window by the bottom sheet's own scroll, then chosen | `shot-query-vocabulary.mjs` → `query-vocabulary-{narrow,narrow-chosen,phone,phone-chosen}.png` | the sheet's narrow docking, the picker's viewport height, or the scroll path to the graph's own keys change |
| _(probe only)_       | The live query text pane: the engine's diagnostics, the ⟨advanced⟩ route into it, and the §7.5 crossing notice hosted inside it | `shot-query-vocabulary.mjs` → `query-{text-pane-invalid,advanced-pane,crossing-notice}.png` | the pane's diagnostics, the stale-row greying, or where the crossing notice is hosted change |
| `sheets.png`         | Sheets grid/table/board composite                  | `shot-sheets.mjs` → `shot-sheets.png`            | sheet schema table, formula columns/filter chip, tag board, grid/table/board rendering, or controls change |
| _(probe only)_       | Grid hover-`+` edge affordances + board Group-by toolbar | `shot-chunk2.mjs` → `/tmp/shot-chunk2-{grid,board}.png` | grid edge-grow affordances or board group-by picker change (verification probe, not a curated README image) |
| `quick-capture.png`  | Quick-capture mini-window with slash menu open     | `shot-capture.mjs` → `rm-quick-capture.png`      | capture window, slash menu, or editor-parity changes |
| `pdf.png`            | PDF pane + text highlight + area (image) highlight | `shot-tabs-pdf.mjs` → `feat-pdf.png`             | PDF viewer, highlight rendering (text/area), or pane layout changes |
| `settings.png`       | Settings modal (shortcuts shown)                   | `shot-settings.mjs` → `settings.png`             | Settings modal gains/loses controls (**watch mode, first-day-of-week**, themes, snapshots) |
| `calc.png`           | Live `/calc` block (inputs, results, a variable)   | `shot-stills.mjs` → `feat-calc.png`              | calc rendering (line numbers, result column) changes |
| `callouts.png`       | Colored note / warning / tip callouts              | `shot-stills.mjs` → `feat-callouts.png`          | callout colors/title/body styling changes |
| `waveform.png`       | Audio waveform overlay player (decoded waveform)   | `shot-media.mjs` → `audio-overlay.png`           | audio overlay / waveform rendering changes (shot synthesizes a real WAV so the waveform draws) |

## Known limitations / honest caveats

- **The browser mock cannot run a query.** Execution goes through the Rust
  engine now, so under `vite preview` every `{{query}}` answers zero results —
  `query.png` therefore shows the builder, not a populated result list, and the
  count pill beside the sentence reads `0`. `shot-query-sheet.mjs` installs a
  canned PARSE through the mock-only `__tineMockQueryFixture` seam so the
  sentence and the rows are real renderings of a real query IR; it deliberately
  does not fake RESULTS, because inventing rows for a README image would be
  showing something the product did not compute. The real end-to-end behaviour
  is proven by `scripts/e2e-query-sheet.mjs` against the built binary.

- **`query-text-pane-invalid.png` shows a Show me the real engine does not
  offer here.** The canned diagnostic carries a span, so the button is drawn;
  the real parser's `unknown_ident` carries none, so against a live graph that
  same typo is named and its repair suggested but not selectable. The picture is
  evidence for the pane's layout, not for that affordance on this diagnostic —
  what IS proven natively, in `scripts/e2e-query-vocabulary.mjs`, is that a
  diagnostic the engine does span ("the anchor goes first") gets a **Show me**
  that selects exactly the engine's range. See RECEIPT-p4.md for the parser-side
  follow-up.

- **The vocabulary picker's numbers come from the same seam.**
  `shot-query-vocabulary.mjs` installs a canned REGISTRY — 250 property keys
  with counts, observed types and top values — because the mock has no graph to
  observe. What the pictures are evidence for is therefore layout and
  windowing: that a 250-key list mounts a viewport's worth rather than the
  graph, that a rare key is reachable, and that a built-in row shows no count.
  That the counts are the GRAPH's is proven against the real engine by
  `scripts/e2e-query-vocabulary.mjs`, whose fixture graph is written so that
  `status` is on exactly eleven blocks.

- **A tall popover opens downward, like every other sheet menu.** The picker is
  the tallest of them, so on a query block low in the window it can reach the
  bottom edge; the shot script scrolls the block up first. Placement is the
  sheet menus' shared behaviour and is not changed here — see the follow-up in
  `RECEIPT-p4.md`.

- **Narrow screens are DRIVEN, not photographed.** The first version of the
  narrow pass took one 560px picture in which most of the list lay under the
  window edge and called the placement pre-existing. It now walks the keyboard
  to the first property row on the unfiltered list, scrolls the bottom sheet the
  way a person would (a wheel over the sheet — the list's own viewport is a
  scroller and would eat the delta), and refuses to pass unless that row, its
  count, and the whole list viewport are inside the window, nothing is cut off
  sideways, and pressing Enter opens the row's value editor. It runs at 560px
  and at a phone's 390px. The one layout change it forced is in
  `src/styles/app.css`: under 600px the vocabulary viewport is
  `min(272px, 32vh)` rather than the desktop's 320px, because at 320px the box
  still ended ~20px below the window after the sheet had been scrolled to its
  end.

- **Quick-capture is a frameless OS window.** The mock can only render its web
  content (a bare editor on white), so `shot-capture.mjs` adds the drop-shadow +
  rounded corners the window manager draws and opens the slash menu, so it reads
  as a floating window doing real work. For the *most authentic* shot — the real
  `tine --capture` window floating over another app — capture it on a real Linux
  desktop (build the release binary, bind/run `tine --capture`, screenshot the
  window). Swap that into `docs/img/quick-capture.png` if you want the real thing.
- Mock data is generic ("kitchen-sink"). If a feature needs specific content to
  be visible (e.g. a `/calc` result, a callout, a datalog query), add it to
  `src/fixtures/kitchen-sink.md` or a dedicated shot script.

## Currently unillustrated (candidates for new README shots)

These shipped without a screenshot — add one if the site would benefit: the
sidebar namespace tree, advanced (datalog) query results. (Tabs, dim, carry,
queries+builder, text+area PDF highlights, `/calc` blocks, and callouts are now
illustrated — calc + callouts via `scripts/shot-stills.mjs`.)
