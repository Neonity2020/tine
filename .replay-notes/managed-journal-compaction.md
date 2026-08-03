# Managed journal/checkpoint compaction

## Durable order and state machine

Compaction remains entirely in the actor's deferred managed-local derivative
lane. Foreground preparation, append, projection, overlay publication, response
construction, timings, and counters are unchanged.

For each record, the existing drain first proves archive/accepted-engine/SQLite,
exact graph projection, authorship, and provider derivatives. It then publishes
the immutable `checkpoint-N.bin`. Only after that publication does the runtime
advance its in-memory checkpoint and remove the queue front. When the queue
becomes empty it collapses the overlay against the exact current accepted
frontier. Rotation is considered only after this collapse and only when the
physical segment has retained at least 64 frames or 8 MiB (tests use 4 frames
to exercise more generations cheaply).

One rotation is:

1. Recheck that the journal cursor equals checkpoint `N`, the hot overlay is
   empty at logical cursor `N`, and the checkpoint's accepted batch remains
   active in the current accepted engine.
2. Create/open an empty immutable-name `device-D-generation-N.segment` with
   `LocalJournalSegment::open_from_sequence(..., N)`. Creation makes the
   directory entry durable.
3. Publish immutable `device-D-generation-N.anchor`. The canonical anchor binds
   its schema, generation, workspace, lineage, device, complete drain
   checkpoint (sequence, prefix commitment, accepted-frontier digest), and last
   accepted batch ID.
4. Switch the actor's append owner to the already-open generation segment.
5. Run retryable cleanup.

The queue must be empty, so no suffix copy is needed. A generation segment
created without an anchor is incomplete and is never authoritative. Once an
anchor exists, the old append owner is never used again.

## Recovery choice

Open enumerates only anchors for the enrolled device and selects the greatest
generation. No anchor means the legacy fixed-name base-zero segment and its
contiguous checkpoint set are used unchanged. An anchorless higher segment is
ignored. The selected anchor is decoded canonically and rebound to workspace,
lineage, device, generation, checkpoint, and accepted batch before its segment
is opened from the anchored sequence.

The authoritative anchored segment must exist and authenticate at that base.
A malformed anchor or missing/corrupt authoritative segment fails closed with
the generation filename in the error; recovery does not silently fall back to
older history. Checkpoints later than the anchor are accepted only through the
physical segment cursor. Startup authenticates the bounded checkpointed frame
prefix and proves its last batch active, then seeds an empty overlay directly at
that logical sequence. Only the uncheckpointed physical suffix is restored.
For repeated edits of one path, restart repairs only the latest exact projection
while the normal drain authenticates earlier records against that successor.

Legacy startup performs the same accepted-batch proof before seeding its
checkpointed cursor. Its bytes remain untouched until a new segment and anchor
are durable; the first deferred rotation then upgrades automatically.

## Cleanup bound

Cleanup retains the newest two complete anchors and their segments, plus the
latest checkpoint file. It removes an obsolete anchor before its segment and
syncs that anchor retirement; legacy and anchorless/incomplete device segments
are removable only after a complete current generation exists. Checkpoint and
remaining segment removals receive a final directory sync. Failure leaves
`cleanup_pending` set so an idle deferred tick retries.

Normal file count is at most two anchors, two segments, and one checkpoint.
Replay is below the 64-frame threshold in production. Disk retention is bounded
by two generations, each no larger than the 8 MiB rotation threshold plus one
maximum journal frame, rather than lifetime edit count.

## Focused proofs

- Existing 14-edit Markdown/Org/nested-path managed save, drain, safe reopen,
  semantic DTO, revision, and exact graph-target assertions.
- 16 sequential Unicode-path saves across four test generations, bounded file
  count/bytes, zero active replay after drain, five-frame pending suffix,
  unsafe reopen, next logical append, and exact semantic/revision equality.
- Real actor restart cuts after checkpoint publication, generation-segment
  creation, anchor publication, in-memory switch, and cleanup removal. Each
  restart drains without loss and appends the next sequence.
- Anchorless higher generation selection and corrupt authoritative anchor
  fail-closed behavior.
- Existing foreground source-boundary test continues to exclude drain,
  archive/database, authorship, provider, and other derivative work.

Exact command results are recorded in the final handoff after the last
verification run.

## Known limits

- Rotation deliberately waits for an empty queue; it does not copy a live
  suffix. This keeps the crash state machine small and means a continuously
  non-draining queue cannot compact until derivative work catches up.
- A legacy installation pays its historical scan once before automatic first
  rotation. Subsequent generations have bounded replay.
- Persistent filesystem deletion errors can temporarily retain extra fallback
  files. The runtime reports `compaction_cleanup`/recovery-blocked state and
  retries; it never deletes the authoritative current generation to force the
  bound.
