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

## Authorized fork resolution

Martin authorized the dossier §6 exception exactly as proposed and directed A5 to resume from `066d5734d25fe5353d41dc6d873276a01a836e20`. The implementation adds only a `pub(crate)` current-format section encoder/decoder in `page_name_index.rs`, composing its existing canonical postcard helpers. Fields remain private; there is no new codec, migration, dual reader, or sibling file. Populated round-trip and malformed/trailing-byte rejection tests pass.

## Implementation checkpoint and numeric fork

The branch now contains the complete disposable two-slot checkpoint protocol, shared sealed-index accepted roster, coherent actor capture, exact restore, names-only authoritative comparisons, tail-only replay, direct genesis predicate, coalesced background publication, lag instrumentation/escalation, path counters, equivalence oracles, and the enumerated publication/damage matrix. Focused checkpoint suite: 25 passed, 2 ignored. `cargo check -p tine-core --lib` and `git diff --check` pass.

The final quiet release measurement used the binding command and medians of three after eliminating construction garbage, per-row proof repetition, duplicate payload allocations, and duplicate namespace enumeration:

| metric | N=50 | N=400 | N=800 | N=800 / N=50 | gate |
| --- | ---: | ---: | ---: | ---: | ---: |
| A5-owned sum (`clean_checkpoint_open` + `clean_checkpoint_tail_discovery` + `committed_tail_replay`) | 4.3 ms | 17.7 ms | 34.2 ms | 7.95x | 1.25x |
| whole reopen | 60.9 ms | 119.7 ms | 180.9 ms | 2.97x | 2.507x |
| checkpoint payload | 185,185 B | 992,594 B | 1,994,339 B | 10.77x | reported |
| capture work | 349 | 2,099 | 4,099 | 11.74x | not superlinear versus 16x history growth |

The residual A5 stages are the required linear decode/restore of the exact accepted sequence, statuses/evidence, causal rows, fingerprints, required-object roster, and semantic runtime state. §A5 explicitly requires those lifetime-proportional components in v1 and forbids a parallel roster; the implementation has no remaining redundant I/O or superseded sealed nodes to remove. Therefore the primary 1.25x gate and the authorized secondary 2.507x gate both fail on the required empirical shape.

Required authorization to continue: manager sign-off must either (1) accept these measured gates for the empirical A5 design while retaining archive rebaselining as the terminal bound, or (2) replace the v1 shape with a strict O(tail) design and authorize the resulting scope expansion (durable acceptance sequencing/incremental state outside the current packet). No code change can honestly claim the current 1.25x gate without changing one of those contract decisions.

## Verdict

**CHECKPOINTED AT NUMERIC FORK — not integrated.** Correctness, crash, authority, and focused compile/test gates are green; the binding performance gates above are red and require manager authorization before final formatting, full baseline-by-name suite comparison, npm bench, anonymized-graph acceptance, catalog status promotion, or packet verdict. Receipt-retention, archive rebaselining/compaction, validation checks, and app-layer files were not touched.
