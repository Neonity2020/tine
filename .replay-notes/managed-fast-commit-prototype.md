# Fast trusted-local commit: the spine passes, the audited replacement does not

Branch `perf/managed-fast-commit-prototype`, base `89e5d8f6`.

**Outcome: the 10,000-page kill gate FAILS, and the cause is measured exactly
and is outside the commit spine.** The prototype's own work — stale/base
validation, one canonical journal frame, one durability barrier, and the direct
return of the post-edit state — costs **0.65 ms p50 at 10,000 pages on ext4**,
which is 15× inside the hard budget and 7× inside the target. The complete
ordinary edit costs **445.8 ms**, because the existing audited guarded
Markdown/Org replacement that the prototype is contractually required to reuse
re-derives the whole graph's text inventory **four times per save**, visiting
39,999 entries.

That inventory is not in `v0.6.5`. `v0.6.5`'s `Graph::save_page` reads the
target file once and writes it; the graph-wide collision validation arrived
later on the sync lineage (`71d1aa3b fix(sync): isolate duplicate graph text
collisions`). So the ordinary-edit path is currently ~35× slower than `v0.6.5`
at 1,000 pages **on the Direct Files path itself**, independently of anything
managed.

| Commit | Subject |
| --- | --- |
| `292b59d9` | `feat(storage): add the local journal frame and per-device segment` |
| `fd14152b` | `feat(core): add the fast trusted-local commit prototype and its proofs` |
| `879eb712` | `test(core): add the permanent fast-commit latency release benchmark` |
| `ebe89f3d` | `perf(core): attribute the fast commit's cost to a named step and scan` |
| `f61f1e73` | `style: apply cargo fmt --all` |

Diff `89e5d8f6..f61f1e73`: 9 files, +3,026 / −0. New: `tine-storage`'s
`local_journal.rs` (1,300 lines) and `tine-core`'s `fast_commit.rs` (1,712).
Every existing file gained only one-line additions — six instrumentation calls
and three module/export registrations, nothing removed or changed. No schema,
DDL, wire, version constant, durable format, archive object, encoding, index or
public query surface changed, and no existing behaviour changed.

## What was built

### 1. The journal frame and segment (`tine-storage/src/local_journal.rs`)

`LocalJournalFrame<K>` is versioned, canonical, length-prefixed and checksummed,
laid out exactly like the existing `OperationObject` envelope:

```
magic "TINEJRN1" (8) | header_len u32 BE (4) | payload_len u64 BE (8)
| header (postcard) | payload | SHA-256 over everything preceding (32)
```

The typed header carries `frame_schema_version`, `device_id`, `sequence`,
`payload_kind` and an independent `payload_digest`, so a decoded frame proves its
payload both physically (whole-frame checksum) and by content identity. Decode
re-encodes the header and refuses any non-canonical spelling. Both digests reuse
`ContentDigest`; nothing here hand-rolls SHA-256.

The payload is opaque bytes and `K` is a domain-supplied type, so `tine-storage`
gains no domain knowledge and `tine-core` supplies its existing `ObjectKind`
vocabulary and its existing encodings.

`LocalJournalSegment<K>` is the append-only per-device store:

- **one append = one write + exactly one `fdatasync`.** The directory entry is
  made durable once, at creation, so a steady-state commit never pays a directory
  barrier. Measured, not asserted: `directory_syncs=1` and
  `durability_syncs_per_commit=1.000` in every benchmark configuration.
- **an append that fails mid-write poisons the segment** rather than guessing its
  own cursor; recovery is a reopen and rescan.
- **recovery adopts the longest prefix of complete canonical frames** and
  truncates a torn tail.
- **a duplicate open is refused** by an exclusive advisory lock held for the
  segment's whole life.

The torn-tail rule is deliberate and is the one interesting design decision.
Appends are ordered and each is durable before the caller proceeds, so an
interrupted process can only have torn the *final* frame. Therefore a decode
failure whose declared extent lies wholly inside the file is **not** a torn tail —
the committed bytes that follow prove that region was written completely once —
and it is refused as `CorruptSegment` instead of being silently discarded. A
device mismatch or a sequence gap is likewise corruption, not a tail.

### 2. The commit spine (`tine-core/src/fast_commit.rs`)

`FastLocalCommitter::commit` performs exactly the four contract steps and
nothing else:

1. stale/base validation against the device's own retained trusted-local base;
2. `intent.journal_payload()` → one `segment.append()` → one durability barrier;
3. `Graph::save_page(&page, Some(base_rev))` — the existing audited guarded
   replacement, unchanged;
4. the caller's already-computed post-edit page returned with its new revision
   attached. Nothing is re-read.

`FastCommitIntent` is `SemanticEffect(&SemanticEffect)` or `CrdtUpdate(&[u8])`,
carried in the canonical semantic-effect encoding and the engine's own exported
update bytes respectively. No second parser, no second digest.
`recover_commit_intent` decodes a recovered frame back to the same typed value.

### 3. Structural counters at the real boundaries

`ForbiddenCommitWork` is incremented at the five *real* sites, not in this
module's own bookkeeping, so "zero" is a statement about reachable code:

| field | site |
| --- | --- |
| `sqlite_drains` | `TailOverlay::drain_ready` (`oplog/sqlite.rs`) |
| `archive_object_reads` | the accepted-object read (`oplog/object_store.rs`) |
| `projection_receipt_loads` | `load_completed_receipt` (`oplog/projection_store.rs`) |
| `graph_wide_catalog_decodes` | the exact catalog decode (`oplog/hot_engine.rs`) |
| `application_page_loads` | `Graph::load_page` (`model.rs`) |

`GraphWideCommitWork` counts whole-graph text inventories and the entries they
visit, at `Graph::graph_text_inventory`. `FastCommitTimings` splits every commit
into its four contract steps and rides back on the outcome.

All three are **permanent and always on**. Four clock reads and a few
thread-local increments are nothing against a millisecond-scale durable
operation, and every prior lane in this series had to record "the
instrumentation is not committed" as a limitation. This one does not.

## Release receipt

`RUST_MIN_STACK=134217728 cargo test --release -p tine-core --lib
fast_local_commit_latency_manual_release_benchmark -- --ignored --nocapture`,
default configuration: 100/1,000/10,000 pages, 10 blocks per page, Markdown and
Org, **100 timed edits after 10 warmups**, one page edited. Machine idle apart
from this run. `ext4` is `/dev/nvme0n1p2` (the repository's own filesystem);
`overlay` is `/tmp`, a volatile overlay, and is a diagnostic only.

Every raw sample is in `managed-fast-commit-prototype-samples.txt` beside this
note, and the benchmark reprints them on every run.

| surface | format | pages | n | p50 | p95 | min | max | mean |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| ext4 | markdown | 100 | 100 | 6.696 | 7.292 | 6.315 | 11.825 | 6.807 |
| ext4 | markdown | 1,000 | 100 | 42.567 | 49.331 | 41.361 | 56.355 | 43.878 |
| ext4 | markdown | 10,000 | 100 | **445.827** | 455.265 | 439.899 | 468.010 | 446.777 |
| ext4 | org | 100 | 100 | 6.685 | 7.161 | 6.361 | 7.965 | 6.735 |
| ext4 | org | 1,000 | 100 | 42.980 | 48.203 | 42.415 | 53.312 | 43.535 |
| ext4 | org | 10,000 | 100 | **446.364** | 456.172 | 439.173 | 466.534 | 447.086 |
| overlay | markdown | 100 | 100 | 4.793 | 4.985 | 4.639 | 5.109 | 4.781 |
| overlay | markdown | 1,000 | 100 | 42.513 | 43.807 | 42.159 | 44.749 | 42.754 |
| overlay | markdown | 10,000 | 100 | 484.602 | 494.876 | 473.992 | 509.933 | 485.184 |
| overlay | org | 100 | 100 | 4.832 | 5.005 | 4.678 | 5.065 | 4.824 |
| overlay | org | 1,000 | 100 | 43.205 | 47.293 | 42.468 | 52.428 | 43.842 |
| overlay | org | 10,000 | 100 | 476.197 | 486.517 | 470.450 | 490.642 | 476.782 |

All figures in milliseconds. Markdown and Org are indistinguishable at every
size, which is expected: the cost is not in the serializer.

### Per-step attribution

| surface | pages | validation p50 | **journal p50** | replacement p50 | inventories/commit | entries visited/commit |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ext4 | 100 | 0.0001 | **0.4448** | 6.2368 | 4.000 | 399 |
| ext4 | 1,000 | 0.0001 | **0.5380** | 41.9902 | 4.000 | 3,999 |
| ext4 | 10,000 | 0.0001 | **0.6541** | 445.0662 | 4.000 | 39,999 |
| overlay | 100 | 0.0001 | 0.0063 | 4.7869 | 4.000 | 399 |
| overlay | 1,000 | 0.0001 | 0.0072 | 42.5014 | 4.000 | 3,999 |
| overlay | 10,000 | 0.0001 | 0.0124 | 484.5817 | 4.000 | 39,999 |

Markdown shown; Org is within noise of it at every row.

Read this plainly:

- **The spine is flat and cheap.** On ext4 the journal step is 0.44 → 0.65 ms
  across a 100× graph-size increase, and stale/base validation is 100 ns. On the
  overlay the same step is 6–12 µs, which isolates the durability barrier: one
  `fdatasync` on this NVMe costs about 0.43–0.64 ms, and that is essentially the
  whole spine.
- **The replacement is linear in total graph pages** at ~0.0443 ms per page, and
  it is the whole of the miss. Subtracting it leaves a commit that meets the
  target at every size.
- **The overlay is not faster at scale.** At 10,000 pages the overlay is *slower*
  than ext4 (484.6 vs 445.8 ms). Whatever dominates is CPU and `readdir`/`statx`
  work, not `fsync`. This is the diagnostic's whole value here: it removes
  durability from the list of suspects.

### Exact durability operations

Per ordinary commit, at every size and on both surfaces and formats:

| operation | count |
| --- | ---: |
| journal frames appended | 1 |
| journal `fdatasync` barriers | 1 |
| journal directory barriers | 0 (1 per segment lifetime, at creation) |
| journal bytes | 540–548 |
| SQLite drains | 0 |
| archive object reads | 0 |
| projection receipt loads | 0 |
| graph-wide catalog decodes | 0 |
| application page reloads | 0 |
| whole-graph text inventories | **4** |
| graph text entries visited | **4 × pages − 1** |

## Gate decision

| gate | 100 | 1,000 | 10,000 |
| --- | --- | --- | --- |
| hard p50 ≤ 10 ms (ext4) | **PASS** (6.70 / 6.69) | **FAIL** (42.6 / 43.0) | **FAIL** (445.8 / 446.4) |
| target p50 ≤ 5 ms (ext4) | MISS | MISS | MISS |
| p95 ≤ 20 ms (ext4) | **PASS** (7.29 / 7.16) | **FAIL** (49.3 / 48.2) | **FAIL** (455.3 / 456.2) |
| forbidden structural work zero | **PASS** | **PASS** | **PASS** |

**10,000-page kill gate: FAIL.** p50 at 10,000 pages is **66.6×** the 100-page
p50 for Markdown and **66.8×** for Org on ext4 (101× and 99× on the overlay),
against a ceiling of 2×.

Against the spine alone the same gate would read: 0.654 ms vs 0.445 ms =
**1.47×** for Markdown and **1.46×** for Org — inside the 2× ceiling, inside the
5 ms target, and inside the 10 ms hard bound. The prototype's own design is not
what fails.

The benchmark enforces the gate, so it currently fails by design. It is
`#[ignore]`d, so it never blocks a suite; it is the standing receipt that will
turn green when the cause below is removed.

## The cause, and why it is not this lane's to fix

`Graph::save_page` reaches `validate_current_graph_text_collision_strict`
repeatedly across one publication: in `validate_graph_text_target`
(`model.rs:6023`), before the retire rename (`:6304`), after the retire
(`:6326`), and after the publish (`:6335`), with further calls on the
temp-write (`:6242`) and restore (`:6353`) paths that an ordinary save does not
take. The counter measures **four** per ordinary save. Each call runs
`graph_text_inventory` (`model.rs:4846`), a whole-graph `readdir` + per-entry
`symlink_metadata` walk, to prove two graph-wide invariants:

- no other graph text path shares the target's portable case/NFC identity;
- no other graph text path aliases the same physical resource.

Both are real invariants and both are deliberately read from the *current*
filesystem rather than from any cache, because a non-cooperating external writer
(Logseq, Syncthing) may have created a colliding sibling since the cache was
built. That is exactly what `71d1aa3b fix(sync): isolate duplicate graph text
collisions` was for.

`v0.6.5`'s `save_page` (`git show v0.6.5:crates/tine-core/src/model.rs:5902`) has
none of it: one `fs::read_to_string` of the target, a `base_rev` compare, and
`write_page`. That matches the 1.06–1.28 ms at 1,000 pages recorded in
`direct-v065-managed-performance-comparison.md`, and it means the 42.6 ms
measured here is a **post-`v0.6.5` regression on the ordinary Direct Files edit
path**, not a managed-storage cost at all.

Making it cheap requires deciding *how the audited writer proves graph-wide
uniqueness without re-deriving it per keystroke* — a retained, invalidated
inventory, an index, or a narrower invariant. Every option trades a safety
property against latency, and one of them (`71d1aa3b`) was chosen deliberately.
That is a product/architecture and safety decision, not a narrow cut, so this
lane measured it, pinned it with a standing test, and stopped.

**Decision required from the manager.** Three shapes, in the order I would try
them:

1. **Collapse the repeats.** Four whole-graph inventories per publication is at
   minimum three more than the invariant needs; the target's identity is already
   re-proved by `expected_identity` at the publication boundary. Cutting to one
   is a ~4× win with no invariant weakened, and is the cheapest thing to try.
2. **Retain and invalidate the inventory.** One derivation per external-change
   generation instead of one per save. `Graph` already has this exact pattern
   (`cache_gen` + `effective_identity_index`). This is the real fix and the one
   that makes the gate pass; it needs a decision about what invalidates it.
3. **Narrow the invariant to the target's portable neighbourhood.** Cheapest at
   scale, but it changes what the writer promises, so it needs the same decision
   `71d1aa3b` made, made again.

Item 1 alone would put 1,000 pages at roughly 11 ms and 10,000 at roughly
112 ms — still failing. Only item 2 or 3 reaches the contract.

## Correctness and crash proof

`cargo test -p tine-core --lib fast_commit` — 7 passed, 1 ignored (the
benchmark).

| Proof | What it establishes |
| --- | --- |
| `a_fast_commit_journals_one_frame_and_replaces_exactly_one_file` | Markdown and Org: exactly one graph file changes, none created or removed, the returned revision equals `content_rev` of the file's actual bytes, one durability barrier, and the frame decodes to the exact `SemanticEffect` that was committed |
| `a_fast_commit_performs_no_sqlite_archive_receipt_catalog_or_page_reload_work` | all five structural counters unchanged across five commits in both formats — and then a real `Graph::load_page` is observed by the same counters, so the zero is not vacuous |
| `the_projected_page_reparses_and_a_fresh_reopen_sees_the_last_committed_edit` | the projected Markdown/Org reparses to the intended semantic page, and a brand-new `Graph` sees the last commit |
| `a_stale_or_untracked_base_is_refused_before_anything_durable_happens` | no frame journalled, no barrier, no graph file changed |
| `a_crdt_update_intent_round_trips_through_the_journal` | an engine-exported Loro update returns byte-identical and typed as `CrdtUpdate` |
| `a_torn_final_append_is_recovered_without_losing_earlier_commits` | the fourth append torn at **every** byte boundary: three earlier commits and their typed payloads survive each time, and the published graph text is untouched by recovery |
| `the_spine_costs_the_same_at_every_graph_size_and_the_audited_replacement_does_not` | at 4 and 400 pages the journal spine's stats are byte-identical, while the inventory work is proportional to the graph — the standing regression test for the finding above |

`cargo test -p tine-storage --lib` — 143 passed, 1 ignored (pre-existing). The 13
journal tests cover:

- decode exactly reproduces the encoded typed payload for empty, small and 9,973-byte
  payloads, and re-encoding a decoded frame is byte-identical;
- **every single-byte corruption** of a frame is refused, at every byte position;
- **every truncation** of a frame is refused, at every length;
- an unknown frame schema version, and a header whose payload digest disagrees
  with its payload, are both refused;
- one append performs exactly one data barrier and one directory barrier per
  segment lifetime;
- a completed append survives a restart, with the payloads intact and in order;
- **a partial tail at every byte boundary** keeps every prior complete frame, is
  truncated exactly once, and the recovered segment is immediately appendable at
  the right sequence — and that new frame survives its own reopen;
- a fully-sized but damaged final frame is a torn tail; damage to a frame that
  committed bytes follow is `CorruptSegment`;
- a foreign device id and a sequence gap are refused;
- a duplicate open is refused while the first is live, and succeeds after it drops;
- an unsafe segment name is refused before any filesystem work.

Every one of these asserts semantics or differentials. None snapshots an internal
digest.

## Commands and results

| Command | Result |
| --- | --- |
| `cargo test -p tine-storage --lib` | 143 passed, 0 failed, 1 ignored |
| `cargo test -p tine-core --lib fast_commit` | 7 passed, 0 failed, 1 ignored |
| `cargo test -p tine-core --lib -- model::` | 310 passed, 0 failed, 1 ignored |
| `cargo test --release -p tine-core --lib -- --test-threads=4 oplog::sqlite oplog::projection oplog::local_active oplog::import` | 369 passed, 1 failed (pre-existing, below), 6 ignored |
| `cargo check --workspace --all-targets` | clean |
| `cargo fmt --all` then `cargo fmt --all -- --check` | clean |
| `git diff --check` | clean |

`RUST_MIN_STACK=134217728` is set for the `tine-core` runs, as in the preceding
notes.

The benchmark receipt:

```
RUST_MIN_STACK=134217728 cargo test --release -p tine-core --lib \
  fast_local_commit_latency_manual_release_benchmark -- --ignored --nocapture
```

Overrides: `TINE_FAST_COMMIT_BENCH_PAGES`, `_BLOCKS_PER_PAGE`, `_EDITS`,
`_WARMUPS`, `_FORMATS`, `_SURFACES`, `_EXT4_ROOT`, `_OVERLAY_ROOT`.

## Preserved invariants

- Nothing in the shipping save route changed. `Graph::save_page` is called, not
  modified; the prototype is a narrowly scoped internal API and no UI or runtime
  path reaches it.
- The six additions to existing files are counter calls and one module
  registration. No control flow, no authority, no encoding, no format.
- The journal is a new, separate, versioned artifact under its own versioned
  directory. It shares no bytes and no directory with the archive, the scratch
  store, SQLite, or the projection.
- The audited replacement's guarantees are untouched: the same base-revision
  proof, the same retained-identity proof, the same collision validation, the
  same publication chain.
- Journal recovery never touches graph text. The torn-tail proof asserts this
  explicitly at every byte boundary.

## Limitations and follow-ups

1. **The kill gate fails and the cause is outside the write set.** Needs the
   manager decision above. *Blocker for the ordinary-edit latency contract.*
2. **The ordinary Direct Files save is a ~35× regression against `v0.6.5` at
   1,000 pages** (42.6 ms vs 1.06–1.28 ms), independent of managed storage. This
   is a live user-facing regression on the current shipping backend, not only a
   prototype concern. *Recommend routing this ahead of further managed work.*
3. **The structural counters are thread-scoped**, matching the oplog's existing
   instrumentation. That is exact for the prototype, which is entirely
   synchronous on its caller's thread, but a future integration that moves the
   commit onto an actor thread must read them on that thread.
4. **The prototype's stale/base check is device-local.** It refuses a base the
   device did not itself commit; the audited replacement still re-proves the same
   revision against the file, so an external change is caught there. A runtime
   integration will need to decide how an external-change notification retires a
   retained base.
5. **The journal is written but not yet replayed into anything.** Recovery reads
   frames back and decodes them to typed intents; deciding what a recovered
   frame *does* to the archive and the CRDT engine is the next lane's design.
6. **One graph text file per page, flat under `pages/`.** A deeply nested graph
   would change the inventory walk's constant but not its shape, since the walk
   is graph-wide.
7. **Pre-existing failure**:
   `oplog::import::tests::detached_bootstrap_conflicting_abandoned_content_address_fails_closed`
   fails with all six of this lane's instrumentation lines removed from the
   working tree, so it is not caused by this work. It is the same failure
   recorded in `managed-save-hot-draft-opus.md` and
   `managed-save-sqlite-drain-opus.md`.
8. **`oplog::projection_store` tests are flaky under full test parallelism** on a
   12-core machine (`MutationAuthorityPending`, and two crash-corpus cases).
   They pass when that module runs alone and when the broad selection runs with
   `--test-threads=4`. Different tests failed on each run, which is the signature
   of a shared-resource race in the harness rather than a defect in this work.
   *Follow-up.*
