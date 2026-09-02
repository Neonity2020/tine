# Harvest F receipt — consumption-point hostile content

## Contract and base

- Authority: `F-hostile-content-dossier.md`, I-22/I-11, and settled decision
  D-14.
- Pinned base: `e19514f98cf1044391f3a3d2f09d2579275353ee` (`batch/harvest-docs`).
- Contract delta: `docs/contracts/content-consumption-boundaries.md` records
  the single outbound-link front door, the recursion/size cap table, and the
  unauthenticated HTML-paste provenance rule. A source guard checks the
  doc/code cap table.

## Owned surface and exclusions

The packet changes only the F write set: shared outbound-link rendering and
its Macro/inline consumers, the mobile explicit-scheme interceptor, formula,
query-builder and PeekPopup bounds, HTML-paste escaping, Rust query bounds,
the authorized one-line model constant visibility change, tests, contract,
catalogs, and changelog.

It does not edit `Block.tsx`, `ImproveTab.tsx`, `backend.ts`, Tauri native
surface, or `sync_runtime.rs`. The static `ImproveTab` issues URL is the sole
enumerated raw-href exception. The anonymized-graph gate is EXEMPT: this packet
changes no storage or real-graph parse path.

## Necessity

`necessity-f.txt` records the pre-fix focused failures. The covered bypasses
are Macro native routing, mobile fail-closed handling, formula depth,
query-parser depth, QueryBuilder render depth, PeekPopup traversal depth, Rust
backlink-filter depth, HTML-paste escaping, and the source-shape guards. All
deep fixtures were built iteratively; no process-aborting overflow was used.

The advanced-query byte ceiling required no new edit at this base:
`advanced_pred` already checks `query_source_within_limit` at the single choke
point, including the `page_affects_advanced_query` path.

## Resolved forks

- Outbound links: extracted and reused one `ExternalLink`; no second sanitizer,
  scheme allowlist, or codec was created.
- Mobile schemes: relative/hash navigation remains local; explicit
  file/http/https/mailto routes through native opening; every other explicit
  scheme is prevented and blocked as the safe default.
- Formula cap: 128, high enough for ordinary formulas and low enough to bound
  recursive evaluation independently of the 1024-name cycle set.
- Query parser/render and PeekPopup caps: 64, matching the established UI-side
  hostile-structure limits.
- Rust backlink cap: reuses `MAX_MANAGED_BLOCK_DEPTH = 128`; `model.rs` changes
  only its visibility to `pub(crate)`.
- HTML paste: general HTML always retains Turndown escaping. The accepted known
  presentation cost is `a [b] c` becoming `a \\[b\\] c`; a future provenance
  heuristic is a follow-up candidate, not part of F.
- D-14: every fix composes existing boundaries; native capability is not
  widened.

## Verification

Pre-edit baselines by name:

- `npm test -- --run`: green.
- `npx tsc --noEmit`: green.
- `cargo test -p tine-core`: 1767 passed, 70 failed, 52 ignored; exact 70-name
  inherited floor recorded in `baseline-f-cargo-test.txt`.

Post-edit:

- Focused frontend tests for all F boundaries: green.
- `npm test -- --run`: green.
- `npx tsc --noEmit`: green.
- `cargo test -p tine-core`: 1768 passed, 70 failed, 52 ignored. Exact failure
  set comparison: 70 baseline, 70 final, zero missing, zero added.
- `npm run build`: green.
- `npm run bench`: completed; different-machine deltas were advisory only
  (`bigLoad -29.9%`, `scrollBig +152.2%`).
- `node scripts/check-regression-catalog.mjs`: green (387 UI entries, 252
  issues, both inventories).
- `npm run check:regressions`: inherited red only at
  `crates/tine-core/src/sync_runtime_tests.rs:27934`; that file is bytewise
  unchanged from the pinned base.
- `cargo fmt --all`: run once after the Rust edits.
- `git diff --check`: green.

## Verdict

PASS. Harvest F closes the reachable consumption-point bypass class within its
write set, pins the shared shapes against recurrence, and introduces no new
native authority.
