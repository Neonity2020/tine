# Managed local record and replay bridge

Date: 2026-08-03

Branch/base: `perf/managed-local-record` from `897b9a54`

## Result

The local-journal and committed-hot-overlay prototypes now meet at one
canonical production record. `ManagedLocalJournalPayloadKind::RecordV1` names
the complete managed-local record; it is not an archive `ObjectKind` and in
particular is not mislabeled as `ObjectKind::CrdtUpdate`.

One canonical postcard payload contains only:

- record schema version and exact journal sequence;
- the canonical finalized `OperationBatch` manifest bytes;
- every canonical `OperationObject` envelope in manifest order.

That closed object set is the exact existing `PreparedBatch`. Its semantic
effect and Loro update objects replay the hot overlay without redrafting. Its
manifested projection intent identifies the exact page, endpoint, path,
precondition, deterministic target bytes/annotations, and post-frontier. The
referenced annotated base object supplies the exact existing path bytes and
identity annotations, so later graph recovery does not consult SQLite.

Workspace and lineage are authenticated from the reconstructed manifest.
Device is bound twice: by the physical journal frame/append receipt and by the
manifest author device. The record sequence must equal both the frame sequence
and the engine prefix position.

## Typed API contract

- `ShardedHotEngine::prepare_managed_local_record(&PreparedBatch, sequence)`
  accepts only an already-finalized `BatchOrigin::LocalMutation` batch with
  exactly one existing-page Present-to-Present projection and its exact
  annotated base. It rejects partial drafts, title/path/kind changes, moves,
  rename/referrer work, create/delete, multi-page effects, and Logseq identity
  mutation.
- `append_managed_local_record(&mut
  LocalJournalSegment<ManagedLocalJournalPayloadKind>, &PreparedManagedLocalRecord)`
  checks segment device and sequence before writing. The returned
  `LocalJournalAppend` binds the actual device, sequence, payload digest, frame
  bytes, and exactly one `sync_data` durability barrier.
- `ShardedHotEngine::apply_appended_managed_local_record` accepts only that
  exact append receipt, revalidates current CRDT base/effect and deterministic
  projection rendering in temporary candidates, then advances visible state.
- `decode_managed_local_record` canonically reconstructs the exact
  `PreparedBatch` and resolves a typed `ManagedLocalProjection` containing the
  manifested intent and its annotated existing base.
- `ShardedHotEngine::replay_managed_local_record` applies a recovered frame
  through the same fail-before-mutation validation and prefix transition.
- `managed_local_prefix_state` reports next sequence, pending record count, and
  the O(1) rolling prefix commitment. `managed_local_work` reports touched-page
  and imported-update work. `collapse_managed_local_prefix` remains an
  all-or-nothing accepted-prefix compaction boundary.

The older latency prototype now uses the explicitly separate
`FastCommitPrototypePayloadKind`; its incomplete raw semantic/update frames
cannot share or impersonate the managed-local record discriminator.

## Recovery and refusal behavior

Preparation changes no visible state. Live apply and replay first decode the
complete canonical batch, prove manifest/object closure, workspace, lineage,
device and sequence bindings, require the current dependency frontier and Loro
base, differentially validate the declared semantic effect, render the exact
projection target from the carried annotated base, and validate page/shard and
block-identity invariants. Overlay documents, heads, claims, prefix entry,
sequence and commitment change only after every check succeeds.

A fresh accepted hot engine with its touched-page base established replays a
complete prefix to the same current page semantics as uninterrupted overlay
execution and as the engine that accepted the exact batches. A complete
corrupt, wrong-device/workspace/lineage, stale-base, gap, or duplicate record
leaves visible state unchanged. The existing `LocalJournalSegment::open`
recovery truncates a torn final frame and exposes only the complete prefix.

Append/apply and replay perform no synchronous SQLite drain, archive object
read, projection receipt load, graph-wide catalog decode, or application page
load. Record encode/decode, candidate validation, rendering, and overlay
application visit only the carried batch objects and touched page/documents.

## Semantic and differential proofs

The shared synthetic builder now produces real finalized local
`PreparedBatch` chains by using the existing authoring, graph capture,
projection-intent/base, archive-acceptance, and projection machinery. The
managed-local proofs reuse those exact batches rather than the prototype's
partial `prepared_core` shortcut.

The tests cover:

- Markdown and Org prepare -> append -> apply against the engine that accepted
  the exact same finalized batch and against rebuilt SQLite page/block rows;
- twelve consecutive finalized records replayed into a fresh engine versus
  uninterrupted overlay execution and the final accepted engine;
- byte-identical canonical manifest and every canonical object envelope after
  decode/reconstruction;
- exact projection intent, path, target, endpoint, annotated base bytes and
  annotations after encode/decode;
- wrong device, workspace, lineage and sequence binding; gap, duplicate,
  complete payload corruption and stale base, all before visible mutation;
- a physically torn second frame recovering and replaying exactly the first
  complete record;
- one append frame and one data barrier, with wrong-device append refused
  before any frame is written;
- complete accepted-prefix collapse with monotonic journal sequence;
- equal structural touched work and zero forbidden synchronous work at 100 and
  10,000 synthetic pages.

No digest snapshot assertions were added. Assertions compare canonical bytes,
typed projection material, semantic page state, recovery prefix, and structural
work.

## Verification

All commands ran from the repository root and were prefixed with `rtk`.

- `cargo fmt --all`: pass.
- `cargo test -p tine-storage --lib local_journal -- --test-threads=1`: 13
  passed.
- `cargo test -p tine-core --lib hot_overlay_tests -- --test-threads=1`: 8
  passed, 1 manual release benchmark ignored.
- `cargo test -p tine-core --lib fast_commit -- --test-threads=1`: 7 passed, 1
  manual release benchmark ignored.
- `cargo test -p tine-core --lib 'oplog::hot_engine::validation_tests::' --
  --test-threads=1`: 120 passed, 3 existing manual measurements ignored.
- `cargo check -p tine --all-targets`: pass with 46 existing warnings.
- `git diff --check`: pass.

Final short release probe:

```text
RUST_MIN_STACK=134217728 \
TINE_MANAGED_LOCAL_RECORD_BENCH_PAGES=100,10000 \
TINE_MANAGED_LOCAL_RECORD_BENCH_EDITS=4 \
TINE_MANAGED_LOCAL_RECORD_BENCH_WARMUPS=1 \
cargo test --release -p tine-core --lib \
  managed_local_record_manual_release_benchmark -- --ignored --nocapture
```

Result: 1 passed. The measured interval is canonical record preparation,
record decode/replay, hot-overlay application, and exact touched-page
materialization. Archive/receipt/graph setup used to build finalized batches is
outside the interval.

- 100 pages: raw `0.849286`, `0.873401`, `0.916762` ms; p50 `0.873401` ms.
- 10,000 pages: raw `1.055790`, `1.094822`, `1.112996` ms; p50 `1.094822` ms.
- Delta: `0.221421` ms; ratio: `1.2535x`.
- Both sizes applied four records, imported four touched documents, performed
  four direct page materializations, and observed zero forbidden synchronous
  work.

## Deliberate limitations

- The production fast case is one existing Markdown/Org page with content,
  block, membership or non-title preamble changes. Title/path/kind/rename,
  create/delete, multiple pages/referrers, external reconciliation and remote
  frames remain on the established slow path.
- The accepted touched-page base must be established in the hot engine before
  replay. This bridge performs no archive reads itself and does not define the
  broader startup base-loading order.
- No Tauri save command or runtime route was changed. No graph projection
  writer, journal projection authority, async archive/SQLite/receipt/provider
  expander, checkpoint, garbage collection, or shutdown predicate was added.
- Complete-prefix collapse is retained; partial compaction remains future
  expander/checkpoint work.
- This is unpublished prototype state. There is intentionally no compatibility
  decoder for the former `ObjectKind::CrdtUpdate` overlay envelope.

## Precise next runtime and expander call sites

1. Split the trusted-local prepare/finalize phase from
   `OperationalCoordinator::execute_local_inner` so
   `RuntimeActor::save_editor_page` / `execute_editor_transaction` can receive
   the fully finalized one-page `PreparedBatch` before
   `publish_and_drain`. Remote and external calls continue through the existing
   coordinator unchanged.
2. At that cut, read `engine.managed_local_prefix_state().next_sequence`, call
   `prepare_managed_local_record`, and call `append_managed_local_record` on the
   enrolled device segment. Only after the receipt returns call
   `apply_appended_managed_local_record`.
3. The runtime-owned graph-publication step uses
   `prepared.record().projection().intent()` plus
   `precondition_base()` to complete the exact target under the existing
   guarded writer ordering. This lane deliberately does not introduce a second
   writer.
4. During `RuntimeActor::open`, after the accepted hot base is established but
   before exact external-feed startup, open
   `LocalJournalSegment<ManagedLocalJournalPayloadKind>`, stream its complete
   frames in order, and call `replay_managed_local_record` for each. Finish any
   journal-committed graph target before importing watcher observations.
5. The later expander calls `decode_managed_local_record`, publishes
   `record.prepared_batch()` through the existing object/manifest archive
   pipeline, stages/accepts/drains it, and uses `record.projection()` to adopt
   the already-exact graph target into existing projection receipt/work-index
   accounting without rewriting it.
6. After exact archive/engine/SQLite/projection/provider catch-up and a durable
   checkpoint, pass the authenticated accepted frontier to
   `collapse_managed_local_prefix`; checkpoint/segment reclamation remains
   owned by that later lane.
