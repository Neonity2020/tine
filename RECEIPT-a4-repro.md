# Harvest packet A4 — repro receipt (the 4,096 run-local page-name index cap)

Preserved as `RECEIPT-a4-repro.md` by the cumulative Wave 1 integration merge.

Branch `batch/harvest-a4`, worktree
`/aux/koutecky/logseq/tine-agent-worktrees/batch-harvest-a4`, based on
`batch/harvest-a1`'s head (`a3ee0342`).
Invariants in play: **I-10** (no stuck states), **I-14** (no
lifetime-proportional waited-path cost), **I-8** (every refusal names an
in-scope scenario).

Scope, per the lane dossier: **empirical reproduction only.** Zero production
edits — every change is test code plus this receipt. No fix, no retirement
design.

---

## 0. Headline

**A4 should jump the queue, but not as "the page-name cap".** The same defect
shape, in a sibling index, is a reproducible *permanent loss of the managed
store from ordinary editing*, proven at the real application boundary:

```
a4_blocks final_blocks=4500 refusal=Some("drain never settled: ... managed_local_pending: 1 ...")
a4_blocks reopen_ms=127.8 reopen_status=OpenRefused { detail:
  "clean managed runtime open failed: immutable archive error:
   manifest-committed clean operation 5d4857e1-7ff2-4034-89f5-67c4862f615b
   did not validate as accepted: Rejected { error: InvalidTransaction(
   \"run-local block-claim index reached its fixed capacity\") }" }
```

Nine ordinary `save_application_page` calls (500 blocks each) took the graph
past **4,096 lifetime-distinct blocks**. The ninth save returned `Saved` to the
application, its managed-local frame then never drained (`pending: 1`, forever),
and **the store never opens again**. The Markdown/Org tree on disk is intact —
this is not content corruption — but the managed store is dead, the last save is
stranded in an undrained journal, and no user action recovers it.

Four findings, in the order the manager needs them:

1. **A4 is not one cap; it is a family of four.** All `4_096`, all run-local,
   all rebuilt from accepted history at every open, none with any removal path:
   `MAX_EPHEMERAL_PAGE_NAME_RECORDS` (`page_name_index.rs:27`),
   `MAX_EPHEMERAL_PORTABLE_PATHS`, `MAX_EPHEMERAL_BLOCK_CLAIMS`,
   `MAX_EPHEMERAL_LOGSEQ_CLAIMS` (`hot_engine.rs:106-108`). Retiring only the
   page-name cap changes **nothing a user can observe**: creating a page
   consumes a page-name record and a portable-path record in lockstep, and the
   portable-path check runs first, so it is the one that reports.

2. **The budget is post-activation lifetime work only; graph size is
   irrelevant.** A synthetic **5,000-page** graph activates `Active` in
   **2.06 s**, imports all 5,000 pages, still accepts a new page afterwards,
   and reopens `Active` in **135 ms**. Bootstrap import goes through the
   lazy-genesis checkpoint, which authors no `SemanticOperation` and therefore
   populates none of the four indexes.

3. **The `BootstrapImport` exemption the dossier points at is dead in
   production.** Every production publication path refuses that origin, so the
   authored-path/replay-path asymmetry the dossier expected is unreachable.

4. **The mechanism that turns a cap refusal into an unopenable store** is the
   publish-before-accept order of the managed-local drain, combined with
   `replay_clean_committed_tail`'s (correct) refusal to tolerate a committed
   manifest that does not accept. Any cap whose refusal can fire *after*
   publication is a permanent-loss bug; the block-claim cap is one today, and
   the page-name cap's own pre-check is blind to the journal-durable overlay
   (§6), so it is a latent second one.

---

## 1. The manager's read, verified claim by claim

| Claim in the dossier | Verdict |
| --- | --- |
| `MAX_EPHEMERAL_PAGE_NAME_RECORDS = 4_096` at `oplog/page_name_index.rs:27` | **Correct.** |
| It caps `EphemeralPageNameOwnershipStateV1.records`, a run-local `BTreeMap` held by `ShardedHotEngine` (`hot_engine.rs:6320`) | **Correct.** |
| `commit()` only inserts (`page_name_index.rs:451-511`); no removal path exists | **Correct.** `commit` inserts records and extends `exact_names`; it removes only *stale exact-name blobs* for a key it is overwriting, never a record. |
| Accepted-history replay at open refills it via `prepare_page_name_updates` | **Correct**, and the entry point is `replay_clean_committed_tail` (`hot_engine.rs:6860`), reached from `sync_runtime.rs:6844` on every open. |
| The count grows with every lifetime-distinct page name created or renamed | **Correct, with exact semantics in §4.** A rename costs *two* records over its lifetime (released old key + new key); returning to an already-used name is free. |
| At the cap: `MalformedBatch("run-local page-name index reached its fixed capacity")` | **Text is wrong.** The literal is `"no-store page-name test index reached its fixed capacity"` (`page_name_index.rs:973`). The `"run-local …"` phrasing belongs to the *Logseq-claim* and *block-claim* siblings. |
| "no-store … test index" implies a test-only structure | **No — this is a copy-drifted comment, and it mistrained the read.** `prepare_page_name_transition_core` has exactly **one** `PageNameTransitionAccess` implementation (`EphemeralPageNameTransitionAccess`). The "no-store test index" *is* the only page-name index Tine has, on the production accept path. Pinned by `a4_run_local_identity_caps_are_a_family_of_four_at_the_same_value`. |
| Reopen replays back over the cap, so the refusal looks permanent (I-10) | **Correct in effect, wrong in mechanism.** Replay refills to *exactly the same* occupancy, so open succeeds and the refusal returns unchanged. Accepted history can never *exceed* the cap by this route, because occupancy is a monotone function of a set of keys and is order-independent. |
| The authored path exempts `BatchOrigin::BootstrapImport` (`hot_engine.rs:13871`) | **Correct as source, dead in production** — see §3. |
| Whether the REPLAY path has an equivalent exemption is unverified | **Verified: it has none.** `validate_and_apply` calls `prepare_page_name_updates` whenever `declared_effect.pages()` is non-empty, regardless of origin (`hot_engine.rs:18606-18628`). |

---

## 2. Q1 — the incremental wedge

**Test:** `oplog::hot_engine_integration_tests::a4_repro_lifetime_distinct_page_names_wedge_at_the_cap`
(`#[ignore]`, ~15 s debug). Seeds 4,096 distinct page names through 16
ordinary accepted batches on a real `ObjectStore`.

Observed at the cap:

| Operation | Result |
| --- | --- |
| Create a 4,097th page | `Err(InvalidTransaction("no-store **portable-path** test index reached its fixed capacity"))` — refused at **draft**, before a batch exists |
| Rename an existing page to a new **name** keeping its existing **path** | `Err(InvalidTransaction("no-store **page-name** test index reached its fixed capacity"))` — this is the A4 cap, isolated |
| **Delete** a page | `Err(InvalidTransaction("no-store portable-path test index reached its fixed capacity"))` |
| Rename a page **back to a name it already held** | `Err(InvalidTransaction(…))` |
| Create a block on an existing page | **Accepted** |

Two things that matter here:

- **The user's only intuitive remedy does not work.** Deleting pages frees
  nothing (§4) — and worse, deleting is *itself* refused, because the
  portable-path check adds the batch's whole `changed` set to the occupancy
  rather than only its novel keys
  (`hot_engine.rs:20194-20203`: `len() + local_overlay + changed.len() > CAP`).
  At exactly 4,096 records **no page-level operation of any kind is possible**.
- The workspace is **not** marked blocked and there is no fatal evidence; the
  refusal presents as an ordinary failing save.

**I-8:** none of the four refusals names an in-scope failure scenario. They
defend a fixed-size in-memory map, not a crash, torn write, sync delivery, or
malformed input. Per the refusal rule they are availability bugs, not hardening.

---

## 3. Q3 — import reachability (hypothesis FALSIFIED)

**Test:** `sync_runtime::tests::a4_repro_import_of_a_graph_larger_than_every_run_local_cap`
(`#[ignore]`, release). Fixture is **synthetic** — 5,000 generated
`A4-Import-N.md` pages under `notes/a4/`; `~/research/logseq-anonymized` was
not read and nothing from it appears anywhere in this lane.

```
a4_import pages=5000 activation_ms=2055.7 status=Active
a4_import imported_pages=5000
a4_import post_import_save=Ok(Saved { … page: "A4 Import Post Page" … })
a4_import reopen_ms=135.2 reopen_status=Active
```

So: a graph far larger than every cap imports cleanly, keeps working, and
reopens. **No import wedge exists**, because the bootstrap import never enters
the semantic engine — it builds a deterministic CRDT checkpoint directly
(`lazy_genesis`; pinned by the existing
`lazy_genesis_checkpoint_builder_is_terminal_state_not_import_history`).

The dossier's supporting premise is also dead:
`BatchOrigin::BootstrapImport` cannot be produced or published by any
production path. `draft_author_transaction_with_observation` refuses it
(`hot_engine.rs:11545`), and `ObjectStore::stage_manifest_bytes`,
`publish_prepared` and `publish_turn_covered_prepared` each return
`StoreError::BootstrapBatchRequiresDirectPublication`
(`object_store.rs:562/587/609/640`). Only the `#[cfg(test)]`
`publish_prepared_fixture` accepts one.

**Consequence for the campaign:** A4 is *not* "a user with a large graph is
stuck after the first restart". It is "a user who does enough editing is stuck
forever", which is §5.

---

## 4. Q4 — count semantics (exact)

**Test:** `oplog::hot_engine::validation_tests::a4_page_name_records_count_distinct_names_and_are_never_released`
— fast, in the ordinary suite, asserting `record_count()` after each step.

| Action | Δ records |
| --- | --- |
| Create a page with a new name | **+1** |
| Create a page whose canonical key already exists (e.g. a deleted page's name) | **0** |
| `EditPagePath` (path only, name unchanged) | **0** |
| Rename to a brand-new name | **+1** (the released old key is retained *and* the new key is acquired, so one page renamed N times costs N+1 records) |
| Rename back to a name the graph has used before | **0** |
| Case-only rename (`Gamma` → `GAMMA`) | **0** — same canonical key |
| `DeletePage` | **0 — deletion frees nothing** |

So the budget is **lifetime-distinct canonical page names**, monotone, with no
release. The sibling `exact_names` map (holding the pre-canonicalization
spelling) has no capacity check of its own; `commit` prunes only the stale
blobs of a key it is overwriting, so it tracks `records` and grows with it.

---

## 5. Q2 — reopen behaviour, and the permanent-loss path

### 5a. At the cap, reopen succeeds and the wedge is permanent

**Test:** `oplog::hot_engine_integration_tests::a4_repro_reopen_replays_back_to_the_cap_and_the_refusal_is_permanent`
(`#[ignore]`).

```
a4_reopen replayed_batches=16 manifests=16
```

Every committed manifest replays `Accepted` into a fresh sequence-zero engine
— open is fine — and the isolating same-path rename is refused again with the
identical page-name message. Restarting is not a remedy (**I-10**). A
page-creating batch legitimately authored by a *peer* whose own index has room
is `Rejected` by the full receiver with the same cap error, so the cap is also
a 0.7 sync hazard: an arriving batch that is valid everywhere else cannot be
admitted here, and there is no state in which it ever becomes admissible.

### 5b. Over the cap, the store never opens again — proven end to end

**Test:** `sync_runtime::tests::a4_repro_managed_block_budget_and_what_it_does_to_the_next_open`
(`#[ignore]`, release; 24 s). Ordinary `save_application_page` calls, 500
blocks each, on a `nested_unicode` fixture:

```
a4_blocks page=0..7  blocks=500..4000  drained=true  pending=0
a4_blocks page=8     blocks=4500       drained=false pending=1
a4_blocks reopen_ms=127.8 reopen_status=OpenRefused { detail:
  "clean managed runtime open failed: immutable archive error:
   manifest-committed clean operation … did not validate as accepted:
   Rejected { error: InvalidTransaction(
   \"run-local block-claim index reached its fixed capacity\") }" }
```

The causal chain, all in production source:

1. `MAX_EPHEMERAL_BLOCK_CLAIMS` counts every lifetime-distinct block ever
   created (`None -> Some` block deltas) and is checked **only on the
   acceptance path** (`hot_engine.rs:21208`) — unlike the page-name and
   portable-path caps it has *no* authoring-time pre-check, so the save is
   drafted and journalled happily.
2. The managed-local drain publishes the manifest first
   (`archive.publish_turn_covered_prepared`, `local_journal_drain.rs:735`) and
   only then asks the engine to accept it
   (`accept_clean_prepared_below_managed_local_overlay`,
   `local_journal_drain.rs:785`). Publication is immutable; the rejection at
   step 3 cannot unpublish it.
3. Acceptance returns `Rejected`; the drain returns `conflict(EngineAcceptance…)`
   and the frame stays pending forever. **The application saw `Saved`.**
4. At the next open, `replay_clean_committed_tail` requires every committed
   manifest to accept and turns anything else into
   `EngineError::Archive("manifest-committed clean operation … did not
   validate as accepted")` (`hot_engine.rs:6988`), which `sync_runtime.rs:6844`
   propagates as `SyncRuntimeOpenStatus::OpenRefused`.

Engine-level companion: `a4_repro_block_claim_cap_binds_first_and_refuses_only_at_acceptance`
pins that exactly **4,096** lifetime blocks are accepted, that the draft never
refuses, and that `instrumentation().block_claim_hot_entries == 4096`.

**This is A4's defect shape — a run-local, never-released, fixed-capacity index
rebuilt from lifetime history — with the worst possible consequence.** It is
the reason to promote the packet.

---

## 6. The same shape is latent in A4's own cap

`CommittedLocalOverlay` (`hot_engine.rs:5907-5926`) carries `block_claims` and
`portable_paths` for journal-durable-but-unaccepted saves — and **no page
names**. The portable-path draft check adds
`self.local_overlay.portable_paths.len()`; the page-name draft check
(`page_name_index.rs:966-975`) consults only `state.records`. So K
page-creating saves that are drafted and journalled before any of them is
accepted all pass the page-name pre-check against the same stale count, and
the (K+1)-th acceptance can cross the cap *after* publication — the §5b
sequence, with A4's own constant.

Not reproduced in this lane (it needs ~4,096 names plus a stalled drain, which
the lane's budget did not allow). Recorded as a source-level finding for the
fix phase, not as a proven bug.

---

## 7. Q5 — near-cap cost

**Measurement:** `oplog::hot_engine_integration_tests::a4_measure_committed_tail_replay_cost_by_lifetime_page_names`
(`#[ignore]`). Seeds N lifetime-distinct page names, then replays the entire
committed tail into a fresh sequence-zero engine — exactly what
`replay_clean_committed_tail` does on the waited open path — and reports wall
time and occupancy. Release build, busy box (timings indicative, the shape is
the evidence):

```
a4_replay names=512  batches=2  manifests=2  seed_ms=93.9   replay_ms=37.2   replay_ms_per_name=0.073
a4_replay names=1024 batches=4  manifests=4  seed_ms=180.2  replay_ms=95.1   replay_ms_per_name=0.093
a4_replay names=2048 batches=8  manifests=8  seed_ms=452.0  replay_ms=269.2  replay_ms_per_name=0.131
a4_replay names=4096 batches=16 manifests=16 seed_ms=1315.0 replay_ms=892.5  replay_ms_per_name=0.218
```

So the answer to the dossier's question is **yes, measurably, and worse than
linearly**: the committed-tail replay costs ~0.9 s at the cap in a release
build, and the marginal cost per name **triples** between 512 and 4,096
(0.073 → 0.218 ms/name), i.e. roughly `n^1.6`. Every open pays it, on the
waited path, forever — this is the `committed_tail_replay` stage A1
attributed, and the page-name/portable-path/block-claim refills are inside it.

For contrast (Q3's run, same instrumentation layer): a **5,000-page** graph
with an empty accepted tail reopens `Active` in **135.2 ms**. Graph size costs
nothing; lifetime accepted history costs everything. That is **I-14** stated as
a measurement.

### What did NOT complete, and why (recorded, not glossed)

`a4_repro_managed_page_creation_wedges_at_the_run_local_budget` — the
application-boundary version of Q1 — was started in release with
`TINE_A4_MAX_PAGES=4400` and **stopped after 52 minutes having created 677
pages** (~13 pages/minute and visibly slowing). It was killed rather than left
to run for hours; the lane's Q1/Q2 answers therefore rest on the engine-level
proof in §2 and §5a, plus the application-boundary proof for the sibling cap in
§5b. The test is committed and re-runnable (`TINE_A4_MAX_PAGES` is honoured), so
the fix phase can rerun it with a smaller budget or on a faster box.

That abort is itself a datum worth the manager's attention: **creating pages
one at a time through `save_application_page` degrades sharply with graph
size** — 677 sequential new-page saves took 52 minutes in a release build. This
is outside A4's scope and is not attributed here (it may be catalog admission,
projection, or the harness driving a feed per save), but it is a real
I-14-shaped observation on an ordinary user path and should be looked at by
whoever owns the save path.

---

## 8. What A4's fix phase has to answer (input to A5, not a design)

Recorded because the repro constrains the fix, not because this lane designs it:

- The fix must cover **all four** caps. Retiring only `MAX_EPHEMERAL_PAGE_NAME_RECORDS`
  leaves the user-visible wedge exactly where it is (the portable-path check
  reports first) and leaves the unopenable-store path (block claims) untouched.
- **A page-name record cannot simply be dropped when the page is deleted.** The
  released old key is what makes a rename idempotent under replay and what a
  peer's late batch is validated against; that retention is the *point* of the
  structure, so the fix is a retention/serialization question, not a
  free-on-delete question. That is the A5 checkpoint-format dependency.
- Any cap that can refuse **after** the manifest is published is a permanent
  data-loss bug regardless of its value. Either the check moves ahead of
  publication (and sees the journal-durable overlay), or `replay_clean_committed_tail`
  must be able to open past it. Today neither is true for block claims.
- **I-14**: all four indexes are rebuilt from full accepted history on the
  waited open path, so their cost is proportional to lifetime work, not to
  graph size. §7 measures what that currently costs.

---

## 9. Test inventory

| Test | File | Gate | Answers |
| --- | --- | --- | --- |
| `a4_page_name_records_count_distinct_names_and_are_never_released` | `oplog/hot_engine.rs` (`validation_tests`) | ordinary suite, fast | Q4 |
| `a4_run_local_identity_caps_are_a_family_of_four_at_the_same_value` | `oplog/hot_engine_integration_tests.rs` | ordinary suite, fast | source guard: the four constants, the four refusal literals, and the single `PageNameTransitionAccess` impl |
| `a4_repro_lifetime_distinct_page_names_wedge_at_the_cap` | `oplog/hot_engine_integration_tests.rs` | `#[ignore]` | Q1 (engine) |
| `a4_repro_reopen_replays_back_to_the_cap_and_the_refusal_is_permanent` | `oplog/hot_engine_integration_tests.rs` | `#[ignore]` | Q2 |
| `a4_repro_block_claim_cap_binds_first_and_refuses_only_at_acceptance` | `oplog/hot_engine_integration_tests.rs` | `#[ignore]` | the accept-only sibling |
| `a4_measure_committed_tail_replay_cost_by_lifetime_page_names` | `oplog/hot_engine_integration_tests.rs` | `#[ignore]` | Q5 |
| `a4_repro_managed_page_creation_wedges_at_the_run_local_budget` | `sync_runtime_tests.rs` | `#[ignore]`, release | Q1/Q2/Q5 (application boundary) |
| `a4_repro_import_of_a_graph_larger_than_every_run_local_cap` | `sync_runtime_tests.rs` | `#[ignore]`, release | Q3 |
| `a4_repro_managed_block_budget_and_what_it_does_to_the_next_open` | `sync_runtime_tests.rs` | `#[ignore]`, release | the permanent-loss proof (§5b) |

Every `#[ignore]` test that would otherwise be red is written as a
**characterization test asserting current (defective) behaviour**, commented as
the A4 repro pinning a defect. When the fix lands they flip to asserting the
fixed behaviour. Zero new red in the ordinary suite.

---

## 10. Baseline by names — zero new red

Baseline: `../baseline-a4-names.txt` (copied verbatim from A1's
`../final-names.txt`, **69** names; the file carries a `tine-core ` prefix per
line, stripped before comparison).

Final full run, `cargo test -p tine-core` at the lane head:
`1723 passed; 74 failed; 49 ignored`.

Set difference: **0 baseline names disappeared**, and 5 names appeared that are
not in the baseline. All five were run again **serially** (`--exact
--test-threads=1`) and all five **passed**:

| Test | Serial rerun |
| --- | --- |
| `sync_runtime::tests::managed_one_block_save_stays_within_two_parser_passes` | ok (0.20 s) — known load-sensitive flake |
| `sync_runtime::tests::over_limit_restore_rediffs_after_interference_and_resumes_from_durable_cursor` | ok (32.04 s) — known load-sensitive flake |
| `sync_runtime::tests::projection_recovery_equivalence_oracle_real_store_subset` | ok (43.66 s) — known load-sensitive flake |
| `sync_runtime::tests::final_projection_then_split_provider_task_chain_heals_intermediate_conflict` | ok (1.60 s) |
| `sync_runtime::tests::managed_search_reports_building_then_returns_backend_results` | ok (12.44 s) |

The full run was executed while the release page-budget probe was saturating a
core, which explains the two additional load-sensitive names beyond the three
the dossier already listed. **Net: zero new red.**

`cargo fmt --all` was run once from the repository root; it reformatted only
the three A4 test additions (no semantic change).

---

## 11. Commands run

```bash
source scripts/env.sh

# fast, ordinary suite
cargo test -p tine-core --lib a4_page_name_records_count_distinct_names_and_are_never_released
cargo test -p tine-core --lib a4_run_local_identity_caps_are_a_family_of_four_at_the_same_value

# engine-level repros (debug, ignored)
cargo test -p tine-core --lib a4_repro_lifetime_distinct_page_names_wedge_at_the_cap -- --ignored --nocapture
cargo test -p tine-core --lib a4_repro_reopen_replays_back_to_the_cap_and_the_refusal_is_permanent -- --ignored --nocapture
cargo test -p tine-core --lib a4_repro_block_claim_cap_binds_first_and_refuses_only_at_acceptance -- --ignored --nocapture

# release test binary for the application-boundary probes
cargo build -p tine-core --release --lib --tests            # 4m52s
cargo test -p tine-core --release --lib a4_repro_import_of_a_graph_larger_than_every_run_local_cap -- --ignored --nocapture
cargo test -p tine-core --release --lib a4_repro_managed_block_budget_and_what_it_does_to_the_next_open -- --ignored --nocapture
cargo test -p tine-core --release --lib a4_measure_committed_tail_replay_cost -- --ignored --nocapture
cargo test -p tine-core --release --lib a4_repro_managed_page_creation -- --ignored --nocapture   # ABORTED at 52 min / 677 pages

# closing gates
cargo fmt --all
cargo test -p tine-core                                     # baseline-by-names comparison
# plus the five serial reruns in §10
```

Write set honoured: three test files
(`crates/tine-core/src/oplog/hot_engine.rs` test module,
`crates/tine-core/src/oplog/hot_engine_integration_tests.rs`,
`crates/tine-core/src/sync_runtime_tests.rs`) and this receipt. **Zero
production edits** — `wire.rs`, `model.rs`, `src-tauri/**` and the frontend were
not touched. Nothing was pushed. `~/research/brain` was never read; the Q3
import fixture is generated in-test and quotes no corpus content.
