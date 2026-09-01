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

## Owned files, exclusions, gates, growth drivers, forks, and verdict

To be completed with the implementation checkpoint. Forbidden receipt-retention, archive rebaselining/compaction, `validate_namespace` internals, `page_name_index.rs`, and app-layer files remain out of scope.
