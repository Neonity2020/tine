# Harvest A5 implementation receipt

## Contract and pinned base

A5 implements the disposable clean-open checkpoint from SPEC-A r4: authoritative operation history remains the sole truth; a valid checkpoint restores observable engine state and replays only the dependency-staged archive tail; every checkpoint failure recovers by predecessor generation or sequence-zero full replay, except authoritative archive damage remains surfaced through the existing archive-damage path.

Pinned cumulative Wave-1 base (authorized as satisfying the code-availability precondition without claiming master integration): `e19514f98cf1044391f3a3d2f09d2579275353ee`.

## Launch preconditions

- A1 steps 1+2 and its benchmark driver are present on the pinned base.
- A4 cap removal is present on the pinned base.
- No A3 worktree/lane was running when A5 started.
- Coordinator background check allowed work. Master and the active release candidate were not touched.

## Pre-edit baseline

`cargo test -p tine-core`: 1765 passed, 72 failed, 52 ignored. The exact failure-name floor is in `baseline-a5-cargo-test.txt`; final judgment is zero new names in either direction, with load-sensitive names rerun exact and single-threaded.

## Pre-A5 calibration (recorded before implementation)

Command: `node scripts/harvest-a1-open-attribution.mjs --runs 3 --checkpoints 50,800`, release profile, medians of 3, quiet machine.

| metric | N=50 | N=800 | ratio |
| --- | ---: | ---: | ---: |
| whole reopen | 125.6 ms | 1393.4 ms | 11.10x |
| committed_tail_replay | 67.0 ms | 1253.5 ms | 18.70x |
| non-A5 residual | 58.6 ms | 139.9 ms | 2.387x |

Binding secondary whole-open constant: `max(1.45, 2.387 × 1.05) = 2.507x`. The primary A5-owned stage bound remains 1.25x and is not recalibrated.

## Fail-before evidence

The catalog row preceded the implementation. On the pinned base, `cargo test -p tine-core --lib checkpoint_open_counter_distinguishes_checkpoint_from_full_replay -- --nocapture` failed exactly at `oplog::checkpoint_generation::tests::checkpoint_open_counter_distinguishes_checkpoint_from_full_replay`: no checkpoint-open path or path-discriminating counter existed. An earlier attempt that filled the filesystem while linking all integration binaries is excluded from evidence; after reclaiming only regenerable Wave-2 Cargo targets, the `--lib` run reached the intended assertion.

## Fork checkpoint

Implementation stopped before product edits under the campaign's never-guess write-set rule. The checkpoint must restore `ShardedHotEngine::ephemeral_page_names` exactly because it influences later admission, conflict, and query decisions. Its concrete type, `EphemeralPageNameOwnershipStateV1`, deliberately keeps both canonical ownership records and exact-name blobs private to `oplog/page_name_index.rs` and exposes no canonical encode/decode or lossless enumeration seam. The A5 write set simultaneously marks `page_name_index.rs` forbidden. Reconstructing this state from current page names would discard acquisition/release provenance; replaying accepted page-name effects would retain the dominant lifetime semantic-replay term A5 exists to remove; an unsafe/raw-memory image would create an unreviewed on-disk format. None is acceptable.

Recommended default: authorize one narrow A5-owned exception in `oplog/page_name_index.rs` adding a current-format, canonical, validated checkpoint encode/decode seam for `EphemeralPageNameOwnershipStateV1` only. Keep fields private, add no migration/dual reader, and cover round-trip plus malformed/trailing-byte rejection. This is the smallest change that preserves the existing page-name authority boundary and lets the checkpoint remain disposable rather than semantic authority.

Fork record: `A5/write-set escape — exact ephemeral page-name checkpoint requires a canonical seam in forbidden page_name_index.rs; recommend the narrow current-format encode/decode exception above.`

## Verdict

**CHECKPOINTED — not integrated.** Branch retains the pre-fix catalog row, calibration, baseline-by-names, and failing necessity test at the exact authorized base. No partial checkpoint protocol or product edit remains. Receipt-retention, archive rebaselining/compaction, `validate_namespace` internals, and app-layer files were not touched. H2 must skip its A5-dependent items and continue the rest, per the Wave-2 sequencing rule.
