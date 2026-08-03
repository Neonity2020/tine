# Managed local journal derivative drain

Date: 2026-08-03

Branch/base: `perf/managed-local-journal-drain` at `c2c78685`

## Outcome

`crates/tine-core/src/oplog/local_journal_drain.rs` adds a production-shaped,
restartable derivative drain for one authenticated
`ManagedLocalJournalPayloadKind::RecordV1`. The record and its already-exact
graph target are authoritative on entry. The drain never drafts an operation,
allocates identity, appends a journal frame, or writes graph text.

The production facade accepts a promoted `PromotedRuntimeSession`, `Graph`,
`ProjectionReceiptStore`, complete journal frame, current per-device
checkpoint, optional live continuation hint, and a runtime-owned
`ManagedLocalDerivativePublisher`. A testable lower seam accepts the exact
promoted parts plus `LocalRuntimeAdmission`; the admission is re-proved before
every authority-changing boundary.

Authorship/provider publication remains owned by the runtime. The trait hands
that owner exact device, sequence, batch, manifest, and accepted-frontier
authority. Its contract requires exact pre-existing state to return `Complete`
and divergent authenticated winners to return `Conflict` without overwrite.
This avoids copying the private runtime publication logic into the oplog.

## Exact stage machine

1. **Authenticate**: canonical journal decode reconstructs the exact
   `PreparedBatch`; checkpoint device/workspace/lineage/sequence, engine
   workspace/lineage, hot committed prefix, endpoint/device, receipt store,
   graph resource, source endpoint, and exact current path/target bytes are
   checked. Duplicate, gap, corrupt, cross-binding, stale/unreplayed hot
   prefix, and divergent graph target fail before any checkpoint advance.
2. **Archive publication**: point-inspect the named batch. A ready batch is
   compared byte-for-byte with the record's canonical manifest and object
   envelopes. Otherwise `ObjectStore::publish_prepared` publishes exact objects
   before the manifest. Exact winners are success; immutable divergence is a
   typed conflict; transient partial publication returns `Pending`.
3. **Engine acceptance**: point-prove accepted membership or call
   `stage_archive_batch_bounded` with eight work units. Missing causal
   dependencies return `Blocked`; incomplete bounded fanout returns `Pending`;
   rejection/divergence returns `Conflict`/`RecoveryRequired`.
4. **Tail and SQLite**: reconstruct the exact `AcceptedBatchEvent`, idempotently
   `try_enqueue`, build `RebuildSource`, and `drain_ready` one accepted batch.
   The SQLite frontier must equal the engine's authenticated accepted authority
   before progress continues.
5. **Projection adoption**: derive the point-addressed work id from the exact
   manifested intent, load that single accepted work row, and call the existing
   manifested projection executor. Because graph bytes already equal the
   journal target, its existing recovery path proves/syncs that target and
   publishes intent/completion/work-index state without replacing the file.
6. **Authorship receipt**: the runtime publisher idempotently adopts/publishes
   exact local-authorship state.
7. **Provider publication**: the same runtime owner idempotently resumes exact
   provider pending/publication/head machinery.
8. **Checkpoint**: only after all prior stages are complete, derive a canonical
   next checkpoint committing prior checkpoint, device, journal sequence,
   payload digest, batch id, and accepted-frontier digest. The completion
   exposes the inclusive reclaimable sequence, but it becomes reclaim authority
   only after the lifecycle owner atomically persists that checkpoint.

Outcomes are bounded and typed: `Complete`, `Pending` with an exact-record
continuation, `Blocked`, `Conflict`, or `RecoveryRequired`. Continuations carry
only device/sequence/payload/batch identity and a stage hint; every retry
reconstructs its position from journal/archive/history/SQLite/receipt/provider
state, so losing process memory cannot lose recovery.

## Idempotency and restart proof

- The journal frame is decoded canonically on every entry; there is no API that
  can draft or append another semantic batch.
- Archive reuse compares the exact canonical manifest and ordered object
  envelopes, not just batch identity.
- Accepted history, tail admission, SQLite application, receipt publication,
  work-index completion, and the runtime publication adapter all use their
  existing exact idempotent boundaries.
- Projection re-entry now admits an exact `Completed` work row through the same
  accepted-batch witness verifier. Blocked, superseded, absent, reserved, or
  divergent rows remain refusals.
- The checkpoint remains unchanged through all fourteen injected before/after
  cuts. Retrying without the returned continuation converges to one archive
  batch, one accepted semantic event, one SQLite application, one exact graph
  target/inode, one projection completion, and one authorship/provider state.
- Twelve consecutive records advance one sequence at a time. Checkpoints are
  encoded, discarded, and canonically decoded at representative boundaries;
  the final semantic engine/SQLite/receipt/provider state equals the established
  pipeline fixture.

## Reused old-pipeline seams

- `decode_managed_local_record` and the exact `PreparedBatch` object closure.
- `ObjectStore::{inspect_batch,publish_prepared}` immutable collision and
  objects-before-manifest contract.
- `ShardedHotEngine::{accepted_batch_is_active,stage_archive_batch_bounded}`.
- `AcceptedBatchEvent`, `TailOverlay::try_enqueue`, `RebuildSource`, and
  `TailOverlay::drain_ready`.
- `ProjectionWorkIndex::{get,status}` point lookups and
  `execute_manifested_projection_work` exact-target recovery/adoption.
- The runtime's existing authorship/provider owner through the narrow
  `ManagedLocalDerivativePublisher` adapter.

No archive manifest inventory, graph inventory, receipt directory scan, or
accepted-prefix replay is introduced per record.

## Semantic and failure evidence

The shared managed-record `OverlayFixture` supplies real finalized batches and
synthetic Markdown/Org graphs. New assertions cover:

- nested/nonstandard Markdown and Org paths, exact graph bytes and inode, and
  semantic engine/SQLite/projection/provider convergence;
- exact pre-existing archive/receipt/provider adoption;
- preserved divergent archive and provider winners;
- twelve ordered records with lost live continuations;
- failures before/after archive publication, engine acceptance, tail admission,
  SQLite commit, projection adoption, authorship, and provider publication;
- gap, duplicate, and mismatched graph target before checkpoint advancement;
- canonical checkpoint round-trip and gap-free monotonic advancement.

The prerequisite managed-record suite separately covers corrupt payload,
wrong device/workspace/lineage, stale base, torn tail, and zero foreground
derivative counters before this drain is invoked.

Focused commands passed:

- `cargo test -p tine-core --lib managed_local_drain -- --test-threads=1`:
  5 passed, 1 manual benchmark ignored.
- `cargo test -p tine-core --lib hot_overlay_tests -- --test-threads=1`:
  13 passed, 2 ignored at the time of the run.
- `cargo test -p tine-storage --lib local_journal -- --test-threads=1`:
  13 passed.
- immutable exact-publication retry/concurrent-winner filters: pass.
- `cargo test -p tine-core --lib journal_projection -- --test-threads=1`:
  11 passed.
- exact-source adoption, manifested-projection fault cleanup, SQLite budget
  continuation, local-authorship recovery, and provider crash-recovery filters:
  pass.

## Release probe

Command:

```text
RUST_MIN_STACK=134217728 \
TINE_MANAGED_LOCAL_DRAIN_BENCH_PAGES=100,10000 \
TINE_MANAGED_LOCAL_DRAIN_BENCH_EDITS=3 \
cargo test --release -p tine-core --lib \
  managed_local_drain_manual_release_benchmark -- --ignored --nocapture --test-threads=1
```

The measured interval begins after synthetic foreground journal replay, exact
graph publication, graph authority priming, and fixture setup. It includes the
normal archive, accepted-history, tail/SQLite, projection-receipt, authorship,
provider, and checkpoint derivative cost.

The final release probe passed and reported:

- 100 pages: p50 `23.117149 ms`, samples
  `[20.766766, 23.117149, 23.911072]`.
- 10,000 pages: p50 `142.813181 ms`, samples
  `[133.080830, 142.813181, 235.542334]`.

Structural work is identical at 100 and 10,000 unrelated pages. For each size,
three records carried four archive objects each; engine bounded stage work was
`[7, 1, 1]` (cold then warm). Every record performed one exact graph point read,
one accepted event, one SQLite batch, one projection-work point read, and one
authorship/provider attempt. No graph/archive manifest inventory or prefix
replay counter is present.

The structural bound passes, but the wall-clock result is not flat: the
10,000-page median is 6.18x the 100-page median. The probe excludes fixture
construction, foreground journal replay, and graph-target recovery from the
measured interval, so this cannot be attributed to those setup steps. This
branch does not introduce graph/archive inventory, but the normal reused
archive/accepted-history/SQLite/receipt persistence path still has a
size-sensitive constant or persistent-index cost. That timing discrepancy is
an explicit remaining performance limitation; the runtime integration should
not claim graph-size-independent latency from this evidence even though the
drain's counted record/point/batch work is constant.

## Runtime integration call sites and limitations

The assignment deliberately forbids editing `sync_runtime.rs` and `oplog/mod.rs`.
The manager must add the production module declaration and wire these precise
sites:

1. **Open** — `RuntimeActor::open` at `sync_runtime.rs:5753`, after promoted
   runtime recovery (`reopen_promoted_local_runtime_existing_projection` /
   unsafe takeover) establishes the accepted hot base and before
   `from_proven_resources` starts exact external-feed discovery. Replay journal
   frames into the hot overlay, recover any committed graph target, then resume
   this drain from the persisted checkpoint before watcher import.
2. **Idle/ordinary turn** — `RuntimeActor::prepare_editor_turn` at
   `sync_runtime.rs:7127` and the actor idle/tick branch in
   `run_actor_loop` at `sync_runtime.rs:5196`. Drain at most one bounded
   continuation slice per turn; foreground save completion must not await it.
3. **Shutdown** — `RuntimeActor::clean_shutdown` at `sync_runtime.rs:10337`,
   before provider shutdown drain/quiesce and `quiesce_and_mark_safe`. Refuse
   Safe while a graph-pending journal record or an uncheckpointed completed
   derivative prefix remains; persist the returned checkpoint before allowing
   journal-prefix reclamation.

The runtime adapter must call its existing `record_local_authorship_receipt`
and `record_provider_publication` machinery and map exact/pending/blocked/
conflict states to `ManagedLocalPublicationState`. No journal collapse or frame
deletion is implemented here. The returned checkpoint/reclaim proof is the
handoff to that later lifecycle lane.

`lib.rs` contains a test-only path declaration so this file-disjoint branch can
compile and test the new module without editing the manager-owned `oplog/mod.rs`;
the manager may remove that test declaration when adding the production module.
