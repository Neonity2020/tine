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

`cargo test -p tine-core -- --list` on the exact base enumerated 2,041 tests (1,992 ordinary, 43 ignored, 6 doc-tests) with sorted-name SHA-256 `fd63531732250e14a7b5d5a44abc9ccfd6fa19e24a715a9a8a53009d8d603ee4`.

`cargo test -p tine-core` on the exact base completed green: 1,992 passed, 0 failed, 43 ignored, plus 6 green doc-tests.

## Owned implementation

- `crates/tine-core/src/oplog/checkpoint.rs`: v1 checkpoint payload, canonical validation, atomic current/predecessor generation publication, crash cuts, fallback/open selection, checkpoint capture, and checkpoint-first reopen.
- `crates/tine-core/src/oplog/checkpoint_generation.rs`: the authorized `pub(crate)` codec seam, strictly composing `AcceptedBatchEvidence::encode_canonical` and `decode_canonical`; no new codec or public surface.
- `crates/tine-core/src/oplog/mod.rs`: module registration and crate-private checkpoint test/support exports.
- `crates/tine-core/src/oplog/hot_engine.rs`: crate-private accepted-evidence codec visibility needed by the seam, checkpoint restoration/capture, replay pruning, fingerprint validation, cleanup, and force-flush integration.
- `crates/tine-core/src/oplog/import.rs`: checkpoint-aware open routed through the existing clean-activation reopen path, with full replay still the fallback.
- `crates/tine-core/src/sync_runtime.rs`: runtime-open state threads the checkpoint-backed engine and open telemetry without a second reopen route.
- `crates/tine-core/src/oplog/oplog_bench.rs`: binding A5 scale measurement and checkpoint payload/work receipts.
- `crates/tine-core/src/measure.rs`: A5 stage names and checkpoint payload/work counters.
- `crates/tine-core/src/projection_producer_census.rs`: source guard for checkpoint access through existing clean-open and public-boundary guard.
- `crates/tine-core/src/oplog/operational_coordinator.rs`: removal of the final independent clean-runtime open, so checkpoint ownership remains single-path.
- `crates/tine-core/src/lib.rs`: no public export; test-only benchmark harness wiring.
- Contract/catalog/changelog and root evidence files.

## Correctness and crash evidence

- Focused checkpoint tests: 22 passed.
- Storage/open integration tests: 8 passed.
- Checkpoint tests cover no-tail equality, dependency-staged tail replay, zero-tail replay, corrupt payload and corrupt current-generation fallback, interrupted publication at every directory-name transition, predecessor fallback, archive rebaselining invalidation, sequence/watermark binding, fingerprint tamper rejection, roster enforcement, status/evidence restoration, duplicate causal rows, replay-plan mismatches, and scale workload validation.
- The accepted-evidence seam is a section of the existing canonical codec; its source guard and public-boundary test prove no parallel encoding and no public exposure.
- Authoritative archive damage remains an archive error; checkpoint damage remains disposable and never becomes a refusal surface.

## Numeric gate evidence

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
