# Managed trusted-local coordinator

Date: 2026-08-03

## Scope

This lane adds the production core coordinator for one eligible existing-page
local content edit. It deliberately does not route the public runtime save
command and does not expand the journal commit into archive, tail, SQLite,
receipt, authorship, or provider state.

Production ownership is concentrated in
`crates/tine-core/src/oplog/trusted_local_commit.rs`. The only shared slow-path
change is the extraction of the existing authorize/draft/capture/finalize work
in `operational_coordinator.rs`; the old local pipeline calls that seam and then
continues through its unchanged publication/drain path. Remote and external
operations remain on `OperationalCoordinator`.

## Exact foreground call sequence

The core call sequence is:

1. `PromotedLocalRuntime::admit_promoted_mutation` supplies the established
   promoted admission/session identity.
2. `OperationalCoordinator::prepare_trusted_local` runs the established local
   authorization, engine draft, capture, and finalize logic and returns the
   sealed `PreparedLocalMutation`. It performs no archive publication, tail
   admission/drain, SQLite mutation, or receipt/provider publication.
3. `TrustedLocalCommitCoordinator::commit` rejects an unpinned, non-editable,
   non-Markdown/Org page before append.
4. `PreparedLocalMutation::into_trusted_batch` releases the old graph-wide
   handoff and yields the already-finalized canonical `PreparedBatch`.
5. `ShardedHotEngine::prepare_managed_local_record` applies the managed-record
   eligibility rules and constructs `PreparedManagedLocalRecord`. Unsupported
   title/path/kind/identity, create/delete/rename/referrer, multi-page, or other
   non-content operations return `Declined` before append.
6. The coordinator verifies the parser-owned page identity, exact path, exact
   base bytes/revision, Present-to-Present target, and prepared post-page.
7. `Graph::commit_existing_page_with_journal` acquires the exact path guard and
   rechecks the base. Its callback calls `append_managed_local_record` exactly
   once. The successful `LocalJournalAppend` receipt is the commit boundary.
8. The graph publishes and durably syncs the exact prepared target. It returns
   either `DurableJournalPageProjection` or the receipt-bound
   `CommittedPendingJournalPageProjection`; a precommit race or append error
   changes no graph or overlay state.
9. `ShardedHotEngine::apply_appended_managed_local_record` applies the exact
   prepared record with the exact append receipt. No semantic redraft occurs.
10. The coordinator returns the prepared `MaterializedPage` and the exact graph
    target revision directly. It performs no SQLite/application reload.

`CommittedLocalOverlayEntry` now retains the exact managed-local projection and
journal sequence. The established engine capture/finalize path may use that
authenticated unexpanded projection as the prior for the next local edit. This
is what allows a second and subsequent save to compose before derivative work
runs. The graph cache refresh uses the existing parser-owned `parse_exact_page`
path so explicit/nonstandard page identity is retained across those saves.

## Outcome and recovery contract

- `Declined { reason }` crosses no append boundary. The later runtime lane must
  pass the original transaction to the established slow path.
- `Committed` contains non-forgeable private state for the batch/record
  identity, exact append proof, durable graph target, direct post page, and
  revision. Its constructors are not exposed.
- `CommittedPendingProjection` means the append is committed and the exact
  record is already visible in hot state, but graph publication needs retry.
  `retry_pending_projection` accepts only the retained pending value and a
  graph; it accepts no journal callback or semantic transaction, so it cannot
  append or redraft.
- `CommittedRecoveryRequired` means append already committed but overlay
  application failed. It retains the exact prepared record, append proof,
  durable-or-pending graph state, and error. `retry_committed_recovery` performs
  only the atomic overlay transition and never appends or redrafts.
- `restart_projection_input` derives callback-free retry input from an already
  authenticated decoded `ManagedLocalRecord`.
  `recover_projection_after_restart` calls the graph restart API without an
  append callback. Startup journal iteration and overlay replay remain for the
  runtime lane.

## Semantic and failure evidence

The coordinator tests reuse the managed hot-overlay fixture and the established
operation builders. Assertions are semantic or differential rather than checks
against newly hard-coded digests.

- Nested/nonstandard Markdown (`.markdown`) and Org paths each produce one
  journal frame, one exact graph synchronization, the exact target, current hot
  state, and a direct post response semantically matching the eventually
  accepted old pipeline.
- Twelve consecutive edits compose before derivative work; each next edit is
  based on the revision returned by the previous commit.
- Stale revision, an external precommit winner, and append-device mismatch
  produce zero frames, zero overlay applications, and no graph clobber.
- An injected post-append graph failure returns committed-pending, applies hot
  state once, retries/restarts without another frame, and reaches the exact
  target.
- An injected post-append/pre-overlay failure returns committed recovery,
  applies the retained record once to fresh hot state, and never adds a frame.
- Path/title rename, kind, create, delete, Logseq-identity, multi-page, and
  unpinned operations decline before append with zero graph/overlay change.
- A source-level structural test rejects references from the foreground module
  to the old coordinator terminal, archive staging/publication, tail
  reserve/enqueue/drain, SQLite, projection receipts, authorship/provider
  publication, reload, and settlement APIs.
- Commit-return instrumentation is zero for SQLite drains, archive object reads,
  projection receipt loads, graph-wide catalog decodes, application loads, and
  graph inventory work. Foreground source structure covers the remaining
  archive/tail/provider operations that have no shared runtime counter.

## Boundedness probe

The ignored release probe constructs synthetic 100-page and 10,000-page graphs
with the shared release fixture. Fixture construction and finalized-operation
preparation are outside the measured interval. The measured interval is
`TrustedLocalCommitCoordinator::commit` only.

| Pages | Commit interval | Hot work `(commits_applied, documents_imported)` | Graph-wide work | Derivative counters |
| ---: | ---: | ---: | ---: | ---: |
| 100 | 2,644 us | identical to 10,000-page run | 0 | 0 |
| 10,000 | 126,061 us | identical to 100-page run | 0 | 0 |

Command test time was 17.59 s after the optimized build; the optimized rebuild
took 4m37s. The probe passed.

## Verification

Commands and results:

- `cargo test -p tine-core --lib trusted_local_commit -- --test-threads=1`:
  7 passed, 1 ignored; 8.48 s after expanding the non-content decline matrix.
- `cargo test -p tine-core --lib hot_overlay_tests -- --test-threads=1`:
  8 passed, 1 ignored; 10.17 s.
- `cargo test -p tine-core --lib journal_projection -- --test-threads=1`:
  11 passed.
- The exact admitted-local semantic mutation coordinator test passed.
- `cargo test -p tine-core --lib 'oplog::operational_coordinator::tests::local_'`:
  5 passed.
- `cargo test -p tine-storage --lib local_journal`: 13 passed.
- `RUST_MIN_STACK=134217728 cargo test -p tine-core --lib
  'oplog::local_active::tests::' -- --test-threads=1`: 93 passed; 342.90 s.
  The larger stack is required by an existing large authorization test; the
  first default-stack run overflowed, and the complete rerun passed.
- `RUST_MIN_STACK=134217728 cargo test --release -p tine-core --lib
  trusted_local_commit_manual_release_boundedness_probe -- --ignored --nocapture
  --test-threads=1`: 1 passed; 17.59 s test time.
- `cargo fmt --all`: passed.
- `cargo check -p tine --all-targets`: passed with the repository's existing
  warnings (0 errors, 46 warnings across 406 crates).
- `git diff --check`: passed.

All fixtures are synthetic Markdown/Org data. No private graph was accessed.

## Precise later `sync_runtime.rs` integration sites

These are line numbers in the unchanged file in this worktree:

### Route

- Public handle entry points: `SyncRuntimeHandle::save_editor_page` at 2666 and
  `SyncRuntimeHandle::save_application_page` at 2724.
- Actor application route: `RuntimeActor::save_application_page` at 6600 calls
  the actor editor route at 6692.
- Eligible existing-page routing belongs in `RuntimeActor::save_editor_page` at
  6907, immediately after transaction construction and the call into
  `execute_editor_transaction` at 7124.
- The cut itself is `RuntimeActor::execute_editor_transaction` at 7213, where
  the current synchronous call to `OperationalCoordinator::execute_local` is at
  7251. The runtime lane should admit once, call
  `OperationalCoordinator::prepare_trusted_local`, then call the new coordinator
  for eligible operations; `Declined` must call the existing slow route.
- The fast committed result must bypass the current inline authorship call at
  7261, provider call at 7274, SQLite editor reload at 7290, application reload
  at 6696, and application settlement/reload path at 6718-6722. It should build
  the editor/application response from the direct prepared post page/revision.

### Open/restart

- `RuntimeActor::open` starts at 5753.
- Callback-free journal decode, ordered hot-overlay replay, and pending exact
  graph recovery must run after the promoted runtime/engine has reopened and
  before `ExactExternalFeedState::open` at 5988. Only after replay/projection is
  complete may the external feed begin its startup scan.

### Drain and retained recovery

- `prepare_editor_turn` at 7127 is the current foreground readiness gate; it
  must not synchronously drain the new derivative queue before allowing the
  next trusted-local edit.
- The old generic local transaction route starts at
  `execute_local_transaction` 10064 and still calls the slow coordinator at
  10102. It remains unchanged for operations not routed by the eligible editor
  cut.
- Existing retained slow publication retry is
  `advance_local_mutation_once` at 10112 and `retain_local_state` at 10198, with
  inline authorship/provider work at 10206/10213. The later journal expansion
  lane should schedule its own owned pending-journal drain adjacent to this
  actor idle/advance machinery, not represent a committed journal record as a
  slow `PendingLocalMutation` and not re-enter `execute_local`.

## Limitations left intentionally for later lanes

- No public Tauri/runtime command routes to this core yet.
- No runtime startup journal scan/replay loop or Safe-shutdown coverage proof is
  installed; only callback-free per-record recovery input/API is present.
- No archive/engine-history/tail/SQLite/receipt/authorship/provider expansion or
  background queue is implemented.
- Rename/title/identity/create/delete/referrer/multi-page operations remain slow.
- The direct core response is a `MaterializedPage` plus exact graph revision;
  the runtime lane still must adapt it to the existing editor/application DTOs
  without a SQLite or application reload.
