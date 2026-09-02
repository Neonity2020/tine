# Harvest H receipt — correctness hygiene

## Contract and base

- Authority: `H-correctness-hygiene-dossier.md`, I-7/I-11/I-1, and settled
  decisions D-1 and D-14.
- Pinned base: `e19514f98cf1044391f3a3d2f09d2579275353ee`
  (`batch/harvest-docs`).
- Contract delta: `docs/storage-sync-contract.md` now identifies lazy-genesis
  schema 5 and projection receipt claim `TINEPR7`/version 7, with schema 4 and
  `TINEPR6` classified as prior containing formats. The capsule and forensic
  record layouts are private implementation formats and add no new
  cross-subsystem contract surface.

## Owned surface and exclusions

The packet changes the lazy-genesis and projection-store formats, their test
siblings, the `fast_commit` header/census guard, theme persistence and its new
concurrency test, the storage contract, non-UI catalog, and changelog. The
small `sync_runtime_tests.rs` edit is test-only: it advances the real-open
prior-claim fixture from `TINEPR5` to `TINEPR6` and the fresh-current assertion
from `TINEPR6` to `TINEPR7`.

It does not edit the read-only production `sync_runtime.rs`, the D-owned
`Settings.tsx`/UI catalog, or any B3/B5p/F-owned production file. The
anonymized-graph gate is EXEMPT: H changes private containing-format dispatch
and theme metadata, not graph parsing or user-graph semantics.

## Item outcomes and necessity

- 1a: removed the lazy capsule V4 fallback and bumped the containing
  lazy-genesis manifest from schema 4 to 5. Current bytes round-trip; prior
  capsule bytes and schema-4 containing manifests reject.
- 1e: collapsed local forensic decoding to schema 2 only, bumped the receipt
  store claim to `TINEPR7`/7, and moved `TINEPR6` into the recognized-prior
  refusal family. Current state reopens; schema-1 forensic bytes and the prior
  containing claim reject.
- D-1 binding: `necessity-h.txt` records that the same old containing states
  opened as current before the changes. The final real core-open fixture takes
  a `TINEPR6` claim through the outer incompatible-state status, while the
  existing Tauri graph-open tests prove that this status archives the private
  state, publishes Direct Files reconstruction intent, and automatically
  rebuilds current Managed authority. The common outer route also owns an
  incompatible schema-4 lazy-genesis manifest; H does not duplicate or edit
  that A5-owned production route.
- 1f: preserved unchanged. Backup schema 2 is a user-facing restore format,
  outside D-1's unreleased Managed-private-format exception; deleting it would
  require separate compatibility authority.
- 3a: already satisfied at the pinned base. The import header describes the
  preparation/commit split and the existing activation-publication guard is
  green, so H does not duplicate it.
- 3g: corrected the header to distinguish the unwired `FastLocalCommitter`
  from its production-wired counters. The census guard proves zero external
  committer uses and exactly eight production `note_*` calls.
- Theme RMW: one module-wide persistence chain serializes the shared
  `theme.packages.v1` read-modify-write. Each mutation recomputes inside the
  chain and publishes the same committed array to storage and signal. The
  deterministic overlap test fails on the former bare-await implementation
  and passes now without losing either theme.

## Verification

Pre-edit baselines by name:

- `npm test -- --run`: green.
- `npx tsc --noEmit`: green.
- `cargo test -p tine`: 353 passed, 2 ignored.
- `cargo test -p tine-core`: 1767 passed, 70 failed, 52 ignored; the exact
  inherited 70-name floor is recorded in `baseline-h.txt`.

Post-edit:

- All focused lazy-genesis, projection-store, real-open, import-header,
  fast-commit-census, and theme-overlap tests: green.
- `npm test -- --run`: green.
- `npx tsc --noEmit`: green.
- `cargo test -p tine`: 353 passed, 2 ignored.
- `cargo test -p tine-core`: 1769 passed, 70 failed, 52 ignored. Exact failure
  set comparison: 70 baseline, 70 final, zero missing, zero added.
- `npm run build`: green.
- `npm run bench`: completed; different-machine timing was advisory
  (`bigLoad -35.7%`, `scrollBig +145.2%`).
- `node scripts/check-regression-catalog.mjs`: green (385 UI entries, 252
  issues, both inventories).
- `npm run check:regressions`: inherited retired-managed-v1 source-guard red
  only at `crates/tine-core/src/sync_runtime_tests.rs:27934`; H's diff in that
  file is confined to the claim fixtures near lines 268 and 407, and the
  flagged legacy fixture is unchanged from the pinned base.
- `cargo fmt --all`: run once after the Rust edits.
- `git diff --check`: green.

## Verdict

PASS. Harvest H removes both Managed-private dual decoders together with their
containing-format acceptance, preserves the user-facing backup compatibility
fork, makes the fast-commit documentation enforceable, and closes the shared
theme persistence race without adding a codec or authority path.
