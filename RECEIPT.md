# Harvest packet A1 — steps 1+2 receipt (instrument, attribute, inventory)

Branch `batch/harvest-a1`, worktree
`/aux/koutecky/logseq/tine-agent-worktrees/batch-harvest-a1`.
Invariants in play: **I-14** (no lifetime-proportional waited-path cost),
**I-2** (audited write path), **I-10** (no stuck states).

Scope, per the lane dossier: measurement and inventory only. No retention,
compaction, or pruning design or implementation is in this packet.

---

## 0. Headline

**Receipts do NOT carry the managed crash-reopen curve.** The receipt store
contributes **0 evidence names, 0 content reads and 0 full-catalog passes** on
the reproduced curve, and its stage costs 0.1–0.2 ms flat at every N.

The curve is carried, at 89.6% of the whole clean recovery at N=800, by
**`committed_tail_replay`** — `HotEngine::replay_clean_committed_tail`
(`crates/tine-core/src/oplog/hot_engine.rs:6880`, called from
`crates/tine-core/src/sync_runtime.rs:6844`), which replays **every accepted
batch ever committed** onto the sequence-zero immutable baseline on every clean
managed open. The counter proves it directly: `committed_tail_replayed` equals
`accepted_batches` exactly, at every checkpoint.

A distant second, also lifetime-proportional, is
**`object_store_repair_and_validation`** — `ObjectStore::validate_namespace`
(`oplog/object_store.rs:1227`, called from `sync_runtime.rs:6828`), which reads
and re-digests every object and manifest in the archive: 9.2 → 38.6 → 76.0 ms
(8.2×), 5.5% share at N=800.

Per SPEC-A §A1.1: since receipts do not carry the curve, the A1 retention work
keeps its cardinality-growth justification but **loses its performance
urgency** — reporting to the manager before proceeding, as the spec directs.

---

## 1. Baseline by names

```
source scripts/env.sh
cargo nextest run -p tine-core --profile ci --no-fail-fast
```

- Before: `2006 tests run: 1937 passed, 69 failed, 43 skipped`.
  69 failing names saved to
  `/aux/koutecky/logseq/tine-agent-worktrees/baseline-names.txt`
  (raw log `../baseline-raw.txt`).
- After (same command, instrumented tree): see §6. **Zero new red names.**

---

## 2. Instrumentation landed

Permanent, production-shipped, content-free. It **measures and never branches**:
every counter is written after the work it describes, and no open-path decision
reads any of them. This follows the existing `*_stats` house pattern rather than
adding a debug-only channel.

| What | Where |
| --- | --- |
| `SyncRuntimeCleanOpenCounters` — 21 content-free work counters for one clean managed cold open | `crates/tine-core/src/sync_runtime.rs` (type), emitted once as `SyncRuntimeOpenProgress::CleanOpenCounters` |
| Two new stage boundaries splitting the old `retained_journals_drain` blob: `own_endpoint_retirement_scan`, `absence_decision_map_open` | `SyncRuntimeCleanOpenStage`, `sync_runtime.rs` |
| `CleanOpenStageTrace::report_counters` — one `TINE_DEBUG=1` line, same shape as the existing per-stage line | `sync_runtime.rs` |
| `ReceiverAbsenceSummaryOpenStats` promoted from `#[cfg(test)]` to permanent, plus the generic `validated_catalog()` fallback branch now records its own attribution (it previously recorded nothing) | `oplog/hot_engine.rs:7720`, `oplog/hot_engine.rs:7562` |
| `LocalCompletionIndex::open_stats` promoted from `#[cfg(test)]`; new `completed_entry_count()` | `oplog/local_completion_index.rs` |
| `SweepManager::chain_count()` | `oplog/absence_sweep.rs` |
| Tauri diagnostic sink for the new progress variant | `src-tauri/src/sync_runtime.rs` |
| Contract: the clean-open boundary list gains the two new stages and the counter record (same-commit living-contract rule) | `docs/storage-sync-contract.md` §"Invariants" item 3 |
| Guard: the existing clean cold-open stage test now asserts the counters are emitted exactly once and are non-empty (instrumentation that is never emitted attributes nothing) | `crates/tine-core/src/sync_runtime_tests.rs`, `public_cold_open_prefers_clean_marker_without_discovering_legacy_enrollment` |

`hot_engine.rs` was touched only in the open path (`open_absence_decision_map`
and its accessors). The conflict-scan region (~19144+) and doc comments
elsewhere were not touched.

## 2a. Reproduction

- Benchmark: `managed_open_stage_attribution_manual_benchmark`
  (`crates/tine-core/src/sync_runtime_tests.rs`, `#[ignore]`, release-only).
  Fixture `ActivationFixture::scaled(..., 17)` = exactly 20 pages, so **only
  accepted history grows**. It walks the checkpoints in ONE process
  invocation, crash-reopening (drop with one undrained frame) at each, and
  prints `attribution`, `attribution_stage`, `attribution_counters` lines.
  `TINE_MANAGED_OPEN_ATTRIBUTION_CHECKPOINTS` (default `50,400,800`).
- Driver: `scripts/harvest-a1-open-attribution.mjs` — runs it 3×, reports
  medians, per-stage share of clean recovery, growth factors, and min/max.

```
source scripts/env.sh
node scripts/harvest-a1-open-attribution.mjs --runs 3 --checkpoints 50,400,800
```

---

## 3. Attribution (median of 3 runs, one shared box)

The reproduction matches the debt audit almost exactly (audit: 128 → 663 →
1467 ms; here 134 → 681 → 1381 ms).

| N accepted batches | reopen ms | clean recovery ms | growth vs N=50 |
| --- | --- | --- | --- |
| 50 | 134.2 | 133.6 | 1.00× |
| 400 | 681.2 | 680.5 | 5.08× |
| 800 | 1380.6 | 1379.8 | 10.29× |

### Per stage

| stage | N=50 ms | N=400 ms | N=800 ms | share@50 | share@400 | share@800 | growth 50→800 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| **committed_tail_replay** | **73.4** | **578.2** | **1235.9** | **55.0%** | **85.0%** | **89.6%** | **16.83×** |
| object_store_repair_and_validation | 9.2 | 38.6 | 76.0 | 6.9% | 5.7% | 5.5% | 8.23× |
| terminal_projection_repair | 22.9 | 25.4 | 29.1 | 17.1% | 3.7% | 2.1% | 1.27× |
| retained_journals_drain | 9.8 | 8.8 | 17.4 | 7.3% | 1.3% | 1.3% | 1.78× |
| completion_flush | 3.6 | 6.3 | 9.0 | 2.7% | 0.9% | 0.7% | 2.50× |
| engine_indexes_and_sweeps_open | 5.0 | 6.2 | 4.3 | 3.8% | 0.9% | 0.3% | 0.85× |
| projection_open | 3.7 | 3.5 | 3.7 | 2.8% | 0.5% | 0.3% | 1.01× |
| retained_journals_open | 2.5 | 1.6 | 2.8 | 1.9% | 0.2% | 0.2% | 1.10× |
| own_endpoint_retirement_scan | 1.2 | 3.3 | 1.4 | 0.9% | 0.5% | 0.1% | 1.15× |
| graph_open | 1.3 | 1.3 | 1.3 | 1.0% | 0.2% | 0.1% | 0.95× |
| authenticated_baseline_open | 0.5 | 0.5 | 0.6 | 0.4% | 0.1% | 0.0% | 1.02× |
| endpoint_and_receipt_open | 0.3 | 0.3 | 0.3 | 0.2% | 0.0% | 0.0% | 0.96× |
| **absence_decision_map_open** | **0.1** | **0.2** | **0.2** | **0.1%** | **0.0%** | **0.0%** | **1.25×** |
| receipt_claim_precheck | 0.1 | 0.1 | 0.1 | 0.1% | 0.0% | 0.0% | 0.94× |

### Counters (last run)

| counter | N=50 | N=400 | N=800 | reading |
| --- | --- | --- | --- | --- |
| accepted_batches | 50 | 400 | 800 | the lifetime axis |
| **committed_tail_replayed** | **50** | **400** | **800** | **every accepted batch replayed on every open** |
| archive_inspected_manifests | 99 | 449 | 849 | ≈ N + 49, linear in lifetime |
| archive_inspected_objects | 383 | 1783 | 3383 | ≈ 4N + 183, linear in lifetime |
| archive_manifest_reads | 6 | 44 | 86 | linear |
| archive_object_reads | 3 | 22 | 43 | linear |
| archive_directory_enumerations | 3 | 3 | 3 | constant |
| **receipt_evidence_names** | **0** | **0** | **0** | **receipts contribute nothing** |
| **receipt_content_reads** | **0** | **0** | **0** | — |
| **receipt_full_catalog_passes** | **0** | **0** | **0** | the expensive fallback never fires |
| summary_content_reads | 1 | 1 | 1 | constant |
| summary_rebuilt | false | false | false | the horizon always matched |
| summary_delta_completions / _intents | 0 / 0 | 0 / 0 | 0 / 0 | — |
| local_completion_names | 50 | 400 | 545 | sublinear: compaction cycles |
| local_completion_content_reads | 50 | 144 | 31 | falls after a compaction |
| local_completion_entries | 50 | 162 | 49 | pruned, not lifetime |
| retired_own_intent_probes | 51 | 163 | 50 | tracks entries, not lifetime |
| retired_own_receipt_artifacts | 0 | 0 | 0 | no own-endpoint receipt residue |
| sweep_chains | 0 | 0 | 0 | — |

### Noise

Per-stage min/max across the three runs (the box was running other builds):

```
committed_tail_replay:              N=50:67.1-124.2  N=400:556.6-764.1  N=800:1217.5-2351.5
object_store_repair_and_validation: N=50:7.0-9.4     N=400:34.0-44.6    N=800:75.2-114.1
absence_decision_map_open:          N=50:0.1-0.2     N=400:0.2-0.5      N=800:0.1-0.2
terminal_projection_repair:         N=50:21.8-37.7   N=400:24.5-45.5    N=800:27.9-51.0
```

The signal survives the noise with room to spare: the *worst* N=50
`committed_tail_replay` (124.2 ms) is still 4.5× below the *best* N=800
(1217.5 ms), and the ordering never inverts in any individual run. The
receipt-side conclusion needs no noise argument at all — it rests on counters
that are exactly zero, not on timings.

### Causal story

The clean managed runtime is reconstructed from an immutable
sequence-zero `lazy_genesis` baseline plus committed operation manifests.
`replay_clean_committed_tail` refuses to run at all unless the engine is at
acceptance sequence zero (`hot_engine.rs:6884-6892`), so the *only* replay base
that exists is the original activation baseline. It then calls
`store.committed_manifests()` and `inspect_batch` for **each** manifest,
computes dependency heads per batch, and stages them in a Kahn-order fixed
point. Nothing ever re-baselines, checkpoints, or compacts that history, so the
work of an open is proportional to every edit the user has ever made — the
literal shape I-14 names. The measured per-batch cost is roughly flat
(1.47 / 1.45 / 1.54 ms per batch at N=50/400/800), i.e. the curve is linear in
lifetime rather than quadratic; the audit-4 P1 fix already removed the
quadratic term (the memoized `dependency_heads` comment at
`hot_engine.rs:6923-6931` records it). `validate_namespace` adds a second,
smaller linear term by re-reading and re-digesting the whole immutable archive.
Receipts stay out of it because this workload is single-device own-endpoint
work: no foreign receiver executes, so no receipt intent/completion rows exist
to scan, and the `receiver-absence-summary-v1` horizon matches trivially.

### What this does NOT say

- It does not say receipt cardinality growth is fake. It says the growth is
  **not observable on this workload's open path**, because this fixture mints
  no foreign receipts. A multi-device 0.7 workload that actually receives
  would populate `intents/` and `completions/`, and nothing in the code
  retires them (§5).
- It does not measure the exceptional `validated_catalog()` path
  (`receipt_full_catalog_passes` stayed 0). The instrumentation will count it
  when it fires; a workload that fires it is not part of steps 1+2.

---

## 4. Producer / consumer inventory

Receipt-store state lives under `receipts/{projection-receipts.claim,
projection-receipts.init, bases, intents, completions, attempts, forensics,
.pending-cleanup}` plus the own-endpoint chain
`archive/operations/sweeps/local-completion-index-v1/` and the derived
`archive/operations/sweeps/receiver-absence-summary-v1/`.

### Producers

| # | Producer | Site | Writes | Grows with |
| --- | --- | --- | --- | --- |
| P1 | Foreign receiver executor | `oplog/projection.rs:1638` `execute_receiver_local_projection_under_handoff` → `receipts.publish_intent` (`oplog/projection.rs:1761`) | `bases/<digest>`, `intents/<id>.intent`, creates `attempts/<id>/` + `forensics/<id>/` namespaces | one row + two directories per received intent, forever |
| P2 | Same, completion half | `oplog/projection.rs:1832` → `ProjectionReceiptStore::publish_completion` (`oplog/projection_store.rs:2091`) | `completions/<id>.completion`, retires the mutation-authority file | one row per completed received intent, forever |
| P3 | Foreign receiver crash recovery | `oplog/projection.rs:1884` `recover_receiver_incomplete_projection_under_handoff` → `reserve_fallback_attempt` (`projection_store.rs:1779`), `begin_mutation` (`:1918`), `reconstruct_completion` (`:2775`) | `attempts/<id>/*`, mutation authority + lease, `completions/<id>.completion` | per recovery attempt |
| P4 | Foreign receipt cleanup queue | `projection_store.rs:2318` `pending_projection_cleanup_bounded`, `:2442` `retire_pending_projection_cleanup` | `.pending-cleanup/round-{0,1}` markers + round-robin state | bounded (two rounds, per-pass cap) |
| P5 | Own-endpoint projection work | `oplog/projection.rs:2789` `execute_manifested_projection_work_located` and `:3163` `write_projection_exact_with_handoff` → `HotEngine::stage_local_projection_completion` (`hot_engine.rs:7754`) | `LocalCompletionIndex` buffer, flushed to `local-completion-index-v1/*.delta` | **bounded**: compacts at `max(256, 2×pages)` deltas and prunes superseded objects (`local_completion_index.rs:355`) |
| P6 | Own-endpoint flush | `oplog/local_active.rs:717`, `sync_runtime.rs:6639/6652/12536` `flush_local_projection_completions` | `.delta` / `.compaction` objects | as P5 |
| P7 | Receiver absence summary installer | `oplog/receiver_absence_summary.rs:95` `open` (installs), `record_completion` / `record_intent` | generation-named summary objects under `receiver-absence-summary-v1/` | **bounded**: chain-versioned, disposable, filename horizon |

### Consumers

| # | Consumer | Site | Question it answers from the data | Cost shape |
| --- | --- | --- | --- | --- |
| C1 | Absence-decision map build | `hot_engine.rs:7562` `open_absence_decision_map`, from `sync_runtime.rs:6967` | "for `(page_id, path)`, what is the frontier-maximal completion across the receiver and own halves?" | names-only readdir of `intents/` + `completions/` via `absence_summary_evidence_names` (`projection_store.rs:2633`); delta-reads only names the summary lacks. **Measured 0.1–0.2 ms flat.** |
| C2 | Summary rebuild (exceptional) | `receiver_absence_summary.rs:180` → `validated_catalog()` (`projection_store.rs:2481`) | same as C1, when the summary is absent/torn/ahead of the directories | **O(lifetime) with a large constant** — decodes every intent, every completion, and every distinct base. Did not fire in this run. |
| C3 | Generic-engine fallback | `hot_engine.rs:7577` (no archive capability) → `validated_catalog()` | same as C1 for engines without an archive store | same as C2; production managed opens do not take it |
| C4 | Receiver create/delete authorization | `oplog/projection.rs:1755` `engine.receiver_absence_decision` | "may this receiver create this file, or must it defer as `DeferredAbsence`?" | in-memory map lookup |
| C5 | Retained incomplete-intent recovery | `oplog/projection.rs:1731` `engine.incomplete_receiver_projection_intents` | "which durable intents for this page have no completion?" | in-memory map lookup; the map's incomplete set came from C1/C2 |
| C6 | Restored-generation deferral | `oplog/import.rs:3629` `restored_generation_requires_absence_deferral` | "does a restored generation for this path have to defer?" | in-memory map lookup |
| C7 | Own-endpoint residue reporting | `sync_runtime.rs:6952` `receipts.retired_own_endpoint_artifacts` (`projection_store.rs:3056`) | "which receipt artifacts for own-endpoint intents are still on disk (inert)?" | names-only `symlink_metadata` ×6 per retired own intent id + one `.pending-cleanup` prefix scan. Bounded by the compacted local index, **not** by lifetime (`retired_own_intent_probes` 51/163/50). |
| C8 | Incomplete-intent base reload | `hot_engine.rs:12632` `receipts.load_retained_base` (inside `capture_author_transaction`, `hot_engine.rs:12582`) (`projection_store.rs:1733`) | "what exact base bytes did this intent record?" | per-intent |
| C9 | Foreign recovery entry point | `oplog/projection.rs:3219` `recover_incomplete_projections` → `store.incomplete_intents()` (`projection_store.rs:2468`) → `validated_catalog()` | "which durable intents have no completion, store-wide?" | **O(lifetime), large constant.** See flag F3 — no production caller. |
| C10 | Foreign cleanup pass | `oplog/projection.rs:3346` `pending_projection_cleanup_bounded` | "which cleanup markers are due this round?" | bounded per pass |

---

## 5. Contract cross-check (`docs/storage-sync-contract.md`)

Confirmed accurate:

- L287 — `receiver-absence-summary-v1` is a disposable acceleration; retained
  receipt records are truth and rebuild it. Matches
  `receiver_absence_summary.rs:95-200`.
- L1552-1567 — one names-only readdir of each of the two evidence namespaces;
  equal horizon reads no content; extra names delta-read; missing/torn/invalid
  triggers exactly one full validated-catalog rebuild. Matches the code and
  the counters (`summary_content_reads = 1`, `receipt_content_reads = 0`,
  `receipt_full_catalog_passes = 0`).
- L289 — retired own-endpoint rows are "inert, reported, and not deleted".
  Matches C7; `retired_own_receipt_artifacts = 0` in this run only because no
  own-endpoint rows were ever written by this workload.

**Flags (surfaced, not edited — the contract was not modified by this packet):**

- **F1 — the contract has no statement of the receipt store's retention
  bound.** L289 names the producers and consumers but never says what retires
  a `bases/`, `intents/`, `completions/`, `attempts/<id>/` or
  `forensics/<id>/` entry. Source confirms nothing does: the only
  `remove_file` calls in `projection_store.rs` are temp-name cleanup
  (`:4337`, `:4768`) and exact mutation-authority retirement
  (`:4713`). Foreign-receiver receipt cardinality is therefore monotone in
  receiving lifetime, exactly as SPEC-A states.
- **F2 — a lifetime-reachable refusal with no refusal-table row.**
  `validated_catalog()` fails closed with `EvidenceTooLarge` at
  `MAX_PROJECTION_CATALOG_ROWS = 2_000_000` (`projection_store.rs:111`) and
  `MAX_PROJECTION_CATALOG_DIRECTORY_ENTRIES = 4_000_000` (`:112`). Given F1
  those caps are reachable by ordinary long receiving use, and the contract's
  refusal table carries no row for them. Recording it as an I-10/I-8
  observation for the manager; no code change in this packet.
- **F3 — a contract-implied consumer with no production caller.**
  L289 lists "foreign recovery/readiness checks" as a consumer of the receipt
  namespaces. The function that implements that read —
  `recover_incomplete_projections` (`oplog/projection.rs:3219`, `pub`, and the
  only in-tree caller of `incomplete_intents()`) — has **no non-test caller
  anywhere in the worktree**: the only other references are the `pub use` in
  `oplog/mod.rs:147` and a `use` inside
  `oplog/operational_coordinator.rs:1574`'s test module. I am reporting the
  fact, not classifying it (I-11 candidate; a "reachable only from tests"
  finding in duplication-audit probe 6 terms).
- **F4 — the contract's own §3.2c/§4.7 open-order narrative does not mention
  that the whole accepted history is replayed on every clean open.** The
  lifetime-proportional stage that dominates managed reopen is not described
  as such anywhere in the contract's cost discussion. Flagged; A1 step 3+ will
  need to state a bound here or hand the finding to a different packet, since
  the mechanism is the operation archive, not the receipt store.

---

## 6. Gates

| Gate | Command | Result |
| --- | --- | --- |
| Baseline by names (before) | `cargo nextest run -p tine-core --profile ci --no-fail-fast` | 69 failed / 2006, names in `../baseline-names.txt` |
| Same, after instrumentation | idem | see `../after2-raw.txt`; **zero new red names** |
| Same, on the exact final tree | idem | `Summary [130.987s] 2006 tests run: 1937 passed, 69 failed, 44 skipped`; `../final-names.txt` is **byte-identical** to `../baseline-names.txt` (69 = 69, empty diff both directions) |
| Release build incl. tests | `cargo build -p tine-core --release --lib --tests` | clean |
| Tauri crate compiles with the new progress variant | `npm run build` then `cargo check -p tine --features custom-protocol` | clean |
| Formatting | `cargo fmt --all` from repo root, then `cargo fmt --all -- --check` | clean |
| Reproduction | `node scripts/harvest-a1-open-attribution.mjs --runs 3 --checkpoints 50,400,800` | §3 |

**Necessity/honesty note on the guard test.** The first version of the new
assertion in `public_cold_open_prefers_clean_marker_without_discovering_legacy_enrollment`
claimed `accepted_batches > 0`; it failed, because that fixture activates and
reopens with no accepted history at all (`accepted_batches: 0`). That was my
assertion being wrong, not the instrumentation. The guard now asserts
`archive_directory_enumerations > 0` — the counters describe work this open
actually did, rather than a default value — which is the architectural fact
worth enforcing. Reported here rather than quietly corrected.

---

## 7. Handoff to the manager (A1 steps 3+4)

1. **Receipts are not the perf problem.** SPEC-A §A1.1's own escape clause
   applies: retention work for receipt cardinality remains valid on I-14
   cardinality and I-10 grounds (F1, F2), but it should not be scheduled as
   the fix for the 128→663→1467 ms curve, because it is not.
2. **The curve's owner is committed-tail replay**, i.e. the absence of any
   re-baselining/checkpoint of the operation archive. That is outside A1's
   stated subject (receipt retention) and outside A2 (provider journal) and A3
   (conflict scans). It needs a decision: widen A1, or open a fourth packet.
   Per the proportionality gate I am not designing it here.
3. `object_store_repair_and_validation` is a second, smaller lifetime term
   from the same source (whole-archive re-validation), and would likely be
   addressed by the same re-baselining decision.
4. The instrumentation is permanent and will now attribute any future open
   regression without re-deriving this lane.
