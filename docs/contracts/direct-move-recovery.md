# Direct cross-page move recovery — living contract

**Status:** current. This describes what the code IS, not what was decided; it is
updated in the same commit as any change to the record layout, the durable step
order, the state machine, or the schema identifier. The load-bearing values below
(`QUARANTINE_RETENTION`, the schema identifier, the durable step names) are
asserted by `the_contract_document_states_the_load_bearing_values` in
`crates/tine-core/src/direct_move_recovery_tests.rs`, so drift fails CI instead
of accumulating.

Invariants in play: **I-3** (one user intent is one operation to storage),
**I-2** (the process may die between any two lines and the graph survives),
**I-4** (what lands on disk stays Logseq-readable), **I-8** (a storage refusal
names the in-scope failure it defends against).

Implementation: `crates/tine-core/src/direct_move_recovery.rs`,
`Graph::prepare_direct_cross_page_move` (`crates/tine-core/src/model.rs`),
`begin_direct_cross_page_move` / `finish_direct_cross_page_move` and the pre-open
call in `src-tauri/src/graph.rs`, and `withDirectMoveRecord` in `src/store.ts`.

---

## 1. What this exists for

A Direct Files cross-page move writes **N + 1 files**: the destination, which
GAINS the blocks, and the N sources, which lose them. Every shape produces it —
`moveBlock`, `moveBlocksRelative`, `moveBlockFeedNow`, `moveSelectionItems`, and
carry, which gathers N journal days into today.

The frontend already saves the destination first, so a removal never lands
before its addition. That bounds the damage but does not remove it: a crash
between two of those writes leaves the blocks present in the destination AND
still present in a source, silently, with nothing on disk saying a move was in
flight.

Two or more `rename()` calls cannot be made atomic. The contract is therefore
**not atomicity but convergence**:

> After a crash at any point in a Direct cross-page move, the next open either
> completes the move or rolls it back. It never leaves a half-move, and it never
> silently overwrites a write that came from outside Tine.

## 2. Where the record lives

`<app-data>/direct-move-recovery/<graph-root-id>/`, where `<graph-root-id>` is
the same identifier `backups/` and `concord-ledger/` use
(`src-tauri/src/backup.rs::root_backup_id`).

App-private and graph-keyed, deliberately **outside the graph tree**. The graph
directory is Logseq-shared surface and a sync transport carries it; a device's
in-flight move record is device-private state and must not travel
(`docs/storage-sync-contract.md`). `Graph` is never handed this root — it is
passed per call by the layer that owns it — because recovery must run before a
`Graph` exists at all (§3).

```
<root>/records/<move-id>.json      the pending records
<root>/blobs/<sha256>              content-addressed page images
<root>/quarantine/<move-id>.json   records recovery must not act on
```

The record is **schema 1**, and that is the only schema any production code
reads or writes: there are no dual readers, no legacy decoders and no in-place
migration. A record carrying anything else is unrecognized private state — it is
preserved in `quarantine/`, never acted on, and never allowed to block the open.

For the destination and each source the record binds:

| Field | Why it is there |
| --- | --- |
| `relative_path` | the PHYSICAL file the editor is pinned to, never a name to re-resolve — a duplicate-day stray and its canonical twin are different participants |
| `page_name`, `page_kind` | page identity, for diagnostics and for the conflict surface a quarantine hands back |
| `base_revision` | the revision the editor held (the `content_rev` of the preimage) |
| `preimage` | the exact bytes before the move — `Absent` when the file did not exist |
| `postimage` | the exact bytes the save is about to publish |

Both images are content-addressed blobs (`Absent` is a real third state).
**Every blob is written and fsynced, and its directory barriered, BEFORE the
record that names it is published.** A crash before the record's own rename
leaves orphan blobs, which the bounded sweep reclaims — the harmless direction.
A record can therefore never name bytes that are not on stable storage.

Recovery must be able to complete OR roll back **from the record alone**: it
never parses a page, never opens a `Graph`, and never consults the cache.

Every write in this store — record, blob, quarantine entry — goes through
`model::atomic_write`, the same named audited protocol the graph's own page
writes use: temp file → fsync → atomic rename → directory barrier. Retirement
unlinks and then barriers the directory, because an un-barriered unlink can be
undone by a crash and would resurrect a record whose participants have moved on.

## 3. The durable steps, and when recovery runs

The durable steps of one move, in order — this list is
`direct_move_durable_steps` in code, the sequence `src/store.ts` emits (pinned by
`src/directMoveOrder.test.ts`), and the sequence the crash matrix cuts between:

1. **commit the record** (blobs first, then the record itself)
2. **write the destination** — the ordinary audited `Graph::save_page`
3. **write each source**, in record order — likewise
4. **retire the record**, once every participant is durably terminal

Recovery is composed and completed at the **Tauri pre-open boundary**
(`prepare_direct_files_open`), before `Graph::open_checked` parses anything, so
no reader ever observes a half-move. It is the layer that owns the app-private
root; `Graph` receives no such root, which is exactly why recovery cannot live
inside it.

One deliberate window: `persistCrossPage` marks the destination dirty
*synchronously*, before the record round-trip returns, so that `flushAll` (graph
switch, window close) sees the page as unsaved from the instant memory changes.
The debounce may therefore publish the destination before the record exists. That
window converges: the record then observes an already-terminal destination and
recovery carries the move forward, which is the safe direction. Proved by
`record_composed_after_the_destination_landed_still_completes_forward`.

## 4. The state machine, and the one refusal

Each participant is classified by comparing its **current on-disk bytes** with
the two recorded images:

| Current bytes | State |
| --- | --- |
| == postimage | `Completed` (checked first, so a no-op save is terminal in both directions) |
| == preimage | `Pending` |
| neither | `Diverged` |

Then, for the record as a whole:

| Observation | Action |
| --- | --- |
| any participant `Diverged` | **quarantine**, write nothing |
| all `Completed` | retire; no graph bytes written |
| all `Pending` | retire; no graph bytes written |
| destination `Completed`, some `Pending` | **complete**: publish each pending postimage |
| destination `Pending`, some `Completed` | **roll back**: restore each completed preimage |
| read/write error | leave the record; the next open tries again |

The forward/backward choice is not a heuristic. The destination is the ADDITION
side and is written first, so a durable destination means the blocks exist and
the removals can safely follow; a pending destination means they do not, and a
removal without its addition is the only state that loses the user's blocks.

**The byte comparison replaces nothing — it is strictly stronger than the guard
it stands in for.** The base-revision guard asks "is this file still the revision
the editor loaded". This asks "is this file still exactly one of the two byte
strings this move accounted for". Anything else means somebody outside this move
wrote the file, and Markdown/Org is authoritative.

### Refusal table (I-8)

| Site | Refusal | In-scope scenario it defends against |
| --- | --- | --- |
| `recover_one`, participant `Diverged` | quarantine: complete/roll back is refused, no graph byte is written, both versions are preserved (the file on disk, the recorded images in the store) and the ordinary conflict machinery surfaces the divergence on the next save | **external-editor race**, **sync-service delivery**, **honest concurrent instance** — another writer published this file while the move was in flight, so neither terminal state of the move is the user's intent |
| `DirectMoveRecord::validate`, participant path not contained | the record is quarantined, unread | **crash/power loss, torn write, disk error** leaving app-private state malformed — a path with `..` would otherwise let recovery write outside the user's graph. This is NOT a defence against an attacker with arbitrary write access to the account (2026-08-07 trust boundary); it is corruption containment |
| `RecoveryStore::read_blob`, digest mismatch | the record is left in place and reported as failed | **torn write, disk error** — an image that does not hash to its own name cannot be published as a terminal state |
| `pending()`, undecodable or wrong-schema record | quarantined, never applied, open proceeds | unrecognized private state (I-7): preserved as backup, the graph is rebuilt from the files |

**No path here refuses a move.** Composition failure — an unavailable app-data
home, an unreadable file, or the serializer's own corruption firewall — is
reported and the move proceeds *unbracketed*, exactly as convergent as it was
before this contract existed. Refusing to move a page because device-private
state is unavailable would be an availability bug with no in-scope threat behind
it, and the rule is to prefer recovery over refusal where a rebuild is possible.

`finish_direct_cross_page_move` retires a record ONLY when every participant is
already terminal. It never writes a graph byte: a participant that is not
terminal means a live save is still in flight or conflicted, and that decision
belongs to the user's conflict UI. Whatever it leaves behind, the next open
converges.

## 5. Bounded cleanup

`RecoveryStore::sweep` runs after every recovery pass:

- `QUARANTINE_RETENTION = 32` quarantined records are kept, oldest dropped
  first. A quarantine entry is diagnostic — the user's bytes are on disk and
  untouched — so an unbounded pile would be device-private garbage, not safety.
- every blob no live record (pending or quarantined) names is reclaimed.

## 6. What is byte-exact, and how the Logseq oracle is discharged

Recovery publishes the postimage the record carries, and that postimage was
produced by `serialize_page_dto_for_path` — **the same serializer the ordinary
Direct save uses**, called with the same `existing` bytes. So the completed
recovery state is byte-identical to what an uncrashed move writes, and the
rolled-back state is byte-identical to the pre-move file.

That is what discharges the I-4 oracle requirement by reduction rather than by a
second oracle run: this path never introduces a byte string the ordinary Direct
save would not have written, so there is nothing here for the Logseq oracle to
disagree with that the save path has not already been gated on.
`both_terminal_states_are_byte_exact_for_every_format_shape` asserts exactly
this over Markdown and Org, LF and CRLF, property blocks, heading shapes and a
non-participant bystander file; `duplicate_looking_identities_bind_the_physical_file`
covers the twin-identity shape.

## 7. Managed storage

None of this applies to Managed Storage, whose cross-page move is one native
request with its own recovery. Carry has no managed arm at all — the native move
accepts one source page — and since B2 it **refuses** under a managed binding
instead of running the Direct choreography underneath it
(`dispatchCarry` in `src/storageDispatch.ts`). Lifting the managed multi-source
limit is an open product question, not an implementation gap.
