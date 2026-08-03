# Managed hot overlay receipt

Date: 2026-08-03

Branch/base: `perf/managed-hot-overlay` from `1a8a7e6f`

## Result

`ShardedHotEngine` now owns a rebuildable committed-local overlay above its
accepted archive base. One existing page can be authored, admitted after one
durable local-journal append, returned directly, read directly, and used as the
base of the next edit without SQLite, archive drain/read, projection receipt,
graph-wide catalog decode, application page reload, or cold engine reload.

The overlay does not create another CRDT or durable authority. It stores Loro
documents produced by importing the exact `ObjectKind::CrdtUpdate` objects that
the existing authoring engine exported. The canonical local journal remains
the pre-archive durability authority; accepted archive state remains the
compacted base.

Direct page materialization also closes the prior total-catalog dependency.
The rebuildable authenticated current-path row is schema version 3 and now
contains the exact effective `LogicalPageName` alongside path, kind, immutable
home, and name digest. A store-backed direct read therefore authenticates one
point row and loads only the page membership/home shards. It reports
`catalog_documents_loaded == 0`.

## Device-receipt correction

`LocalJournalAppend` now exposes the `device_id` copied from the owning
`LocalJournalSegment`, which is the same identity encoded in the durable frame.
`apply_durable_hot_overlay` no longer accepts a caller-supplied device and uses
only that receipt-bound identity when validating the payload manifest. This
removes the live/replay split in which a caller could append an A-authored
payload to a B-owned segment but claim device A during live admission. The
change adds no scan, archive, SQLite, projection-receipt, provider, or
projection work.

## Exact API

- `prepare_hot_overlay_commit(&PreparedBatch, PageId, u64) -> Result<PreparedHotOverlayCommit, HotOverlayError>` prepares a journal envelope from an already-finalized local batch without changing visible state.
- `prepare_hot_overlay_draft(AuthorTransactionDraft, u64) -> Result<PreparedHotOverlayCommit, HotOverlayError>` consumes the existing engine draft and prepares the same envelope. It refuses stale generations and any effect not confined to one existing page.
- `PreparedHotOverlayCommit::{batch_id, sequence, journal_payload, payload_kind, post_page}` exposes the exact append bytes and speculative semantic response. `payload_kind` is `ObjectKind::CrdtUpdate`.
- `apply_durable_hot_overlay(&LocalJournalAppend, &PreparedHotOverlayCommit) -> Result<HotOverlayApplyOutcome, HotOverlayError>` requires the append sequence, payload digest, receipt-bound device identity, and exactly one data-durability sync to cover those bytes, then revalidates and commits the overlay candidate. The caller cannot substitute a device identity: `LocalJournalAppend::device_id` is copied from the segment owner whose identity was encoded in the durable frame.
- `replay_hot_overlay_frame(&LocalJournalFrame<ObjectKind>) -> Result<HotOverlayApplyOutcome, HotOverlayError>` runs recovered canonical frames through the same validator and transition.
- `materialize_current_page_at_path(&ManagedPath) -> Result<Option<MaterializedPage>, EngineError>` resolves and materializes accepted-plus-overlay state by authenticated point lookups and touched shards.
- `collapse_hot_overlay(&AcceptedFrontierRoot) -> Result<usize, HotOverlayError>` removes the complete overlay only when the supplied authenticated root is the engine's exact accepted root, every pending batch is accepted, and every overlaid decoded document version exactly equals accepted state.
- `hot_overlay_len`, `hot_overlay_next_sequence`, and `hot_overlay_work` expose prefix position and structural work. `HotOverlayWork` counts page materializations, commits, imported documents, and imported raw-update bytes.

The public types above are re-exported from `oplog`. The only test integration
declaration is the single line `mod hot_overlay_tests;` in
`hot_engine_integration_tests.rs`; all new proofs live in its new child module.

## Journal binding and transition

The canonical postcard `HotOverlayJournalPayloadV1` binds:

- schema version, workspace, lineage, journal sequence, and affected page;
- the canonical operation manifest;
- the exact canonical semantic-effect object;
- the complete set of canonical CRDT update objects required by that manifest;
- the exact pre-frontier and the resulting post-frontier.

Preparation performs no visible mutation. Admission decodes canonical bytes,
checks the frame/append and manifest bindings, proves the complete descriptor
set, requires `LocalMutation`, validates current direct heads and Loro causal
vectors, imports every update into temporary document clones, differentially
checks the declared semantic effect against before/after snapshots, validates
immutable shard/block ownership, reconstructs the resulting frontier, and
materializes the one post-save page. Only after all checks succeed does it
replace overlay documents/heads/claims, append the prefix entry, advance the
journal sequence, and update the rolling O(1) prefix commitment.

The author-generation root includes that rolling commitment. The next draft
therefore sees the committed overlay documents and heads, extends the last
local causal dot, and invalidates any draft prepared before the preceding
commit. Duplicate recovered sequence 0 after the prefix has advanced is
explicitly refused as `OutOfOrder`, so it has no second effect.

Collapse is deliberately all-or-nothing. A frontier mismatch, a missing or
non-accepted pending batch, or any accepted/overlay document-version mismatch
returns `AcceptedFrontierMismatch` before clearing anything. Journal sequence
numbering remains monotonic after collapse.

## Semantic and failure proofs

The synthetic-only tests in `hot_overlay_tests.rs` prove:

- Markdown and Org direct results equal both the established accepted-engine materialization and rebuilt SQLite rows for page ID, immutable home, name, path, kind, preamble, block ID/home/parent/order/content, and sparse Logseq UUID/origin.
- 24 consecutive local edits compose immediately with no archive/SQLite/receipt/provider/application reload or accepted catch-up. Every exact response equals the following direct read.
- Replaying 12 recovered frames into a fresh identical accepted engine produces the same semantic Org page as uninterrupted execution; duplicate replay is refused without an effect.
- Stale base, out-of-order sequence, corrupt bytes, wrong workspace, wrong device binding, and accepted-frontier mismatch leave visible hot state unchanged.
- Appending an A-authored payload to a B-owned segment returns a B-bound durable receipt. Live admission and replay of the recovered B frame refuse with the same manifest/journal binding error, and both leave the visible page and overlay length unchanged.
- Publishing and accepting the exact prepared batch permits collapse with no semantic change, and a later edit continues at the next journal sequence.
- Around durable-overlay admission plus direct materialization, `ForbiddenCommitWork` remains zero for all five real boundaries: SQLite drains, archive object reads, projection receipt loads, graph-wide catalog decodes, and application page loads. `HotOverlayWork` is one imported document, one commit, and one page materialization per content edit, independent of graph size.

No digest snapshots were added; assertions compare semantic state and work.

## Verification

- Fail-before: `rtk cargo test -p tine-core --lib committed_hot_overlay_boundary_starts_empty` failed before the boundary existed because `hot_overlay_work` and `hot_overlay_len` were absent.
- Final formatting: one `rtk cargo fmt --all` pass completed successfully.
- Device-receipt correction formatting: `rtk cargo fmt --all` completed successfully.
- `rtk cargo test -p tine-storage --lib local_journal`: 13 passed.
- `rtk cargo test -p tine-core --lib hot_overlay_tests`: 7 passed, 1 manual benchmark ignored.
- `rtk cargo check -p tine --all-targets`: passed with 46 existing warnings.
- `rtk cargo test -p tine-core --lib 'oplog::hot_engine::validation_tests::'`: 120 passed, 3 pre-existing manual measurements ignored.
- `rtk cargo test --release -p tine-core --lib committed_hot_overlay_manual_release_benchmark -- --ignored --nocapture`: 1 passed.
- `rtk git diff --check`: clean.

The test build continues to report 15 existing unrelated warnings; this lane
does not modify their owning files.

## Benchmark receipt

The ignored release test `committed_hot_overlay_manual_release_benchmark` uses
synthetic Markdown accepted bases of 100 and 10,000 pages. It performs five
warmups and records 50 samples per size. The timed interval is recovered-frame
admission plus exact current-page materialization; journal fsync and graph
projection are excluded. Raw samples are recorded separately in
`.replay-notes/managed-hot-overlay-samples.txt`.

Final release measurements:

- 100 pages: p50 `1.156566 ms`.
- 10,000 pages: p50 `1.333875 ms`.
- Delta: `0.177309 ms`; ratio: `1.153306`.
- Both p50 values are below `5 ms`; exact `HotOverlayWork` equality and the benchmark's bounded timing assertion passed.

## Deliberate limits

- This boundary admits changes to exactly one existing page. Page creation/deletion, title/path/kind changes, rename/referrer planning, and cross-page effects remain on the authenticated slow path.
- Existing sparse Logseq identity is retained and materialized. Logseq identity mutation stays slow because it requires accepted claim-index publication.
- Collapse currently removes only the complete pending prefix; partial-prefix compaction is not exposed.
- This lane does not route Tauri production calls, install startup replay, or schedule the asynchronous archive/SQLite drain. It supplies the bounded engine boundary for that next wiring.
- The existing draft/finalization surfaces are reused. This work does not optimize the synchronous coordinator or claim that its broader projection planning is part of the measured interval.
- The authenticated current-path row is rebuildable scratch authority tied to the exact accepted frontier. Schema version 2 rows are intentionally rejected and reconstructed, not migrated in place.

## Next runtime integration call sites

1. In `RuntimeActor::save_editor_page`, after authenticated changed-page planning has produced an ordinary one-page `AuthorTransactionDraft`, call `prepare_hot_overlay_draft` with the local journal's next sequence.
2. Append `journal_payload` as its reported `payload_kind`; only after the append returns its durability receipt call `apply_durable_hot_overlay`.
3. Return the `Applied` page directly as the exact post-save response, and replace the ordinary current-page SQLite/application lookup with `materialize_current_page_at_path`.
4. During activation, replay recovered canonical local frames in sequence before serving reads or saves.
5. Let the background worker publish/stage the same prepared objects, perform existing graph/receipt/SQLite work, obtain the resulting authenticated accepted root, and call `collapse_hot_overlay` only after exact catch-up.

No production Tauri route was changed, and the separately owned `model.rs`,
`fast_commit.rs`, and `watcher.rs` were not edited.

## Managed-workspace handoff blocker

The requested commits and clean worktree could not be produced because this
session has read-only access to the worktree's external Git administrative
directory. The attempted `git add` failed with:

`fatal: Unable to create '/aux/koutecky/logseq/backups/logseq-claude/.git/worktrees/perf-managed-hot-overlay/index.lock': Read-only file system`

All source, proof, format, benchmark, and receipt work is present in the working
tree. A manager with write access to that Git directory must create the
implementation/proof/receipt commits.
