# Harvest A2 — provider journal retirement · packet receipt

Preserved as `RECEIPT-a2.md` by the cumulative Wave 1 integration merge.

Branch `batch/harvest-a2`, worktree
`/aux/koutecky/logseq/tine-agent-worktrees/batch-harvest-a2`.
Base: `3f3a7afe49c488ba5444c4882fff727d7c0099c0`.
Dossier: `specs/campaigns/2026-09-invariant-sweep/A2-provider-journal-dossier.md`.
Spec: `SPEC-A-lifetime-cost.md` §A2. Invariants in play: I-10, I-2, I-8.

Write set actually touched: `crates/tine-core/src/oplog/wire.rs` (+ its tests),
`docs/storage-sync-contract.md`, and — **one deliberate extension, flagged for
the manager** — two pinned integers in
`crates/tine-core/src/projection_producer_census.rs`. That file is a test-only
guard (`mod projection_producer_census` in `lib.rs`) whose failure message is
"update the census before accepting a primitive delta"; this packet's change to
`wire.rs` is exactly the delta it exists to catch, and leaving it red would
have been a new red name. It is not one of the files the dossier reserves for
another lane (`hot_engine.rs`, `projection_store.rs`, `page_name_index.rs`,
`model.rs`, `sync_runtime.rs`, frontend). No production code outside `wire.rs`
was touched. `git status` shows exactly those three files plus this receipt.

---

## 1. The defect, restated from the code

`ProviderRetryJournal` keeps one `completed/` record per provider filesystem
operation it finishes. Records are named by a content-derived operation id — a
hash over the operation, its binding, its provenance, the paths, and the source
length and digest — so the directory has **no chronology**. Two conditional
recyclers existed (`recycle_completed_put_for_absent_destination`,
`recycle_completed_remove_for_reappeared_source`), each firing only at the
entry point of one exact operation and only in one narrow provider state.
Nothing retired the ordinary cases, so the store grew with the *lifetime* of the
device, and at `MAX_PROVIDER_JOURNAL_COMPLETED` (16,384)
`validate_completed_usage` refused — every subsequent publish, rename and
remove failed with `ProviderJournalLimit`. That is a persistent state blocking
ordinary work, reachable by ordinary long use, with no terminal action (I-10).

Production producers of completed records, all unbounded in the lifetime sense:
`publish_object_exact` (one per distinct object), `publish_manifest` (one per
accepted batch), `publish_frontier_head` (one per head generation),
`publish_descriptor`, `retire_frontier_head` (one Remove per superseded head),
`remove_identical_generated_conflict`. `run_provider_rename_with` is
`#[cfg(test)]` — there is no production rename producer today, which the
compiler enforces.

## 2. Record-type census and the retirement predicate

The predicate asks the operation type one question about **current provider
state**. It never looks at time, arrival order, or a count — a window over
hash-named records could replay or suppress the wrong operation after namespace
loss or reappearance.

| Record type | Producers | Provider-state question | Verdict | Why an exact repeat still reaches the same outcome without the record |
| --- | --- | --- | --- | --- |
| `Put` | `publish`, `publish_exact` (objects, manifests, descriptor, frontier heads, clean-baseline chunks) | Is the destination `tree:from_path` present as a no-follow regular file? | present → `Reflected`; absent → `Moot`. **Always retirable** | Present: the repeat compares destination bytes and either settles or refuses `ProviderConflictingBytes` — the same answer the retained record's `validate_put_destination` gave. Absent: the repeat republishes, which is what `recycle_completed_put_for_absent_destination` already arranged. |
| `Rename` | `run_provider_rename_with` (test-only today) | Is the retired source `tree:from_path` back? | back → `Moot`; gone → `Reflected`. **Always retirable** | Both directions settle from this device's own retirement diagnostic `removed/retired-<operation id>`: its name is derived from the retired bytes, so it authenticates this exact operation with no journal record. The destination must still hold those bytes too. |
| `Remove` | `retire_frontier_head` (`SettleIfAbsent`), `remove_identical_generated_conflict` (`RequirePresent`) | Is the removed source `tree:from_path` back? | back → `Moot`; gone → `Reflected`. **Always retirable** | Back: the repeat settles from the same retirement diagnostic, or performs an ordinary authorized removal. Gone: a `SettleIfAbsent` caller settles; a `RequirePresent` caller gets `UnknownProviderPath` — see §5. |

A record whose `operation_id` also appears in `records/` is never retired: a
crash can leave the same authenticated Cleanup record in both directories, and
the pending copy is what the ordinary retry validator reads. Both existing
recyclers make the same check; the sweep reuses it.

The question is asked with `provider_path_is_present_regular_file`: resolve the
delivered parent, `open_provider_file_nofollow`, and validate it is a regular
file. **Presence only — the bytes are never read**, so compaction costs the
number of records, not the size of the graph. It treats an open error
(a disk error, a permission change, a namespace a file-sync tool has not
delivered) as absent. That is deliberate and bounded: compaction only discards
*evidence*, it never performs or un-performs a provider operation, so the worst
consequence of a misclassification is a redundant republish of identical bytes
(`Put` read as moot) or an `UnknownProviderPath` on a repeat whose retirement
diagnostic is equally unreadable (`Rename`/`Remove` read as reflected). Neither
loses or rewrites provider data.

## 3. At-cap behaviour — the I-10 terminal action

`ProviderRetryJournal::complete` now counts `completed/` from names alone
before it reserves room. At or above
`PROVIDER_JOURNAL_COMPLETED_COMPACTION_TRIGGER` it runs
`reconcile_completed_against_provider`, which re-observes provider state and
retires every retirable record, then proceeds. Reaching the trigger is an
instruction to re-observe the provider, never a reason to fail the user's next
operation. Because every record class is retirable in every provider state
(§2), the structural bound `MAX_PROVIDER_JOURNAL_COMPLETED` cannot be reached
by ordinary work and no path refuses on journal capacity.

**No generation/commit-pointer machinery was needed, and the spec's hard
constraint on multi-file atomicity therefore does not bind here.** Each
completed record is *independently* retirable and its retirement is idempotent:
a crash part-way through a sweep leaves a prefix retired and the rest
untouched, which is a state the next sweep reaches again by itself. There is no
mixed generation to publish. Each individual removal is `remove_file` +
directory `fsync`, i.e. the audited protocol for a durable name removal (I-2);
no new write path was added.

**Trigger tightened from the spec's provisional 1,024 to 64** — the spec
permits tightening freely. Reason, measured: `ProviderRetryJournal::load`
decodes and HMAC-authenticates *every* completed record on *every* operation,
so the completed count is also the ordinary path's per-operation cost. At the
old structural bound that was thousands of verifications per publish. The
20,000-operation gate ran 266 s at trigger 64; at trigger 1,024 the same gate
was still running after 11 minutes of CPU and was killed. Nothing needs a wider
window, because provider state — not this store — is what makes an exact repeat
settle once a record is retired.

## 4. Production change beyond retirement: one publication, one answer

`SharedProviderTransport::publish` was a second, subtly different answer to
"are these exact bytes at this exact name?": it always refused a destination
that already existed, so manifest, descriptor and frontier-head publications
depended on a retained completed record to make an exact repeat settle.
`publish` now delegates to `publish_exact`, the sibling that already compared
destination bytes and settled on equality (the blessed exemplar for objects and
clean-baseline chunks). `reject_provider_temporary_path` moved into
`publish_exact` so the unified path keeps the temp-path rejection `publish` had
via `put_complete` (`wire.rs:505`). Putting it at `publish_exact`'s first line
also closes a small pre-existing hole: `publish_exact`'s identical-destination
branch returned `Ok(())` before ever reaching `put_complete`, so an exact
publication naming a path under the provider's temporary namespace could
settle without the rejection ever running.

Two narrow settle paths were added to `run_provider_rename_with` and
`run_provider_remove_with`, both bound to
`operation_settled_by_retirement_diagnostic`. They are **not** "the destination
exists, so succeed": the diagnostic's name is the operation id recomputed from
the retired bytes, so it proves *this* device performed *this* operation. They
exist because retiring the completed record must not turn an exact repeat into
a conflict against the destination or diagnostic the operation wrote itself —
which is exactly what happened in the first run of
`retired_completed_remove_settles_for_the_policy_that_tolerates_absence`
(`UnsafeProviderJournal`), before they were added.

## 5. One narrowing, surfaced for the manager (I-8)

Retiring a completed `Remove` whose source is absent changes one answer: a
caller whose missing-source policy is `RequirePresent` re-requesting that exact
removal now gets `UnknownProviderPath` instead of the retained record's silent
`Ok`. This is the same state-derived answer that policy already gives for any
absent source; no *new* refusal shape was invented. The one production
`RequirePresent` caller is `remove_identical_generated_conflict`, and
`sync_runtime::process_clean_provider_path` reads the conflict path first
(`crates/tine-core/src/sync_runtime.rs:21673`, `read_exact(path)`) and returns
`Ok(())` when it is absent, so it cannot reach the removal call at
`:21694` in that state — but that fact lives in `sync_runtime.rs`, outside
this packet's write set, and is therefore recorded as a read argument, not
pinned by a test here.
A refusal-table row with its in-scope scenario (sync-service delivery, honest
concurrent instance, or this device's own earlier completed removal) is in
`docs/storage-sync-contract.md` §3.1, and
`retired_completed_remove_settles_for_the_policy_that_tolerates_absence`
asserts both policies' answers so the behaviour cannot drift silently.

## 6. Contract delta (`docs/storage-sync-contract.md`)

* New **§2.10c-i "The provider retry journal's completed store is bounded by
  provider state"**: the retention bound, the per-record-type predicate table,
  the no-generation-pointer argument, the one-publication-one-answer note, the
  proof test names, and a closing note naming the neighbouring bound this
  section does NOT cover (§8 of this receipt).
* §2.1 layout table: the device-private provider journal row now states the
  `completed/` retention bound; the `{inbox,outbox}/removed/` row now states
  that it is also the evidence an exact repeat settles from, and names its cap.
* §3.1 retryable-refusal table: one new row for the `RequirePresent` removal of
  an already-absent path (§5).

No other contract surface moved: no layout entry was added or removed, no
schema identifier changed, no wire payload changed, and no new write path was
introduced (compaction is `remove_file` + directory `fsync`, the audited
protocol for a durable name removal).

## 7. Evidence

### Gate numbers actually measured

`oplog::wire::tests::provider_journal_completed_records_retire_against_live_provider_state`,
one process, one provider tree, one device-private journal:

| Quantity | Value |
| --- | --- |
| Completed provider operations driven | **20,000** (`publish_object_exact`, all distinct) |
| Publications that failed on journal capacity | **0** (each `unwrap_or_else` names the index) |
| Structural bound `MAX_PROVIDER_JOURNAL_COMPLETED` | 16,384 — passed 20,000 times over |
| Compaction trigger | 64 |
| Peak `completed/` count (sampled every 997 operations) | **64** |
| Final `completed/` count | **32** |
| Spec's provisional steady-state ceiling | 1,024 — tightened to 64, see §3 |

The test then drops and reopens the transport (a reopen revalidates the whole
authenticated journal graph, so a compacted store has to survive it), and
exercises both retirement reasons on live state: a republish whose destination
is still there, and one whose destination a provider lost.

### Fail-before / necessity evidence

Both probes were applied to the candidate tree, run, and reverted; the file was
restored from a byte copy and `grep -c PROBE` returns 0 in the committed tree.

**Probe A — the mechanism.** Disabled the compaction call in
`ProviderRetryJournal::complete` (`if false && …`) and set
`MAX_PROVIDER_JOURNAL_COMPLETED` to 128 so the pre-fix stuck state is reached
in seconds rather than after 16,384 operations. The gate test failed exactly as
the defect predicts:

```
publication 128 must never fail on journal capacity: provider retry journal exceeded explicit bound
```

That is the I-10 stuck state: an ordinary publication refused on journal
capacity, with no terminal action available to the user.

**Probe B — retirement must not blindly delete.** Kept compaction and removed
the two state-derived settles instead: made
`operation_settled_by_retirement_diagnostic` return `Ok(false)` unconditionally
and made `publish_exact`'s identical-destination branch refuse
(`if true { return Err(ProviderConflictingBytes) }`). All four
idempotency/crash tests went red:

```
retired_completed_remove_settles_for_the_policy_that_tolerates_absence      FAILED
a_crash_across_completed_record_retirement_reopens_and_still_settles        FAILED
retired_completed_rename_settles_only_on_its_own_retirement_evidence       FAILED
retired_completed_provider_records_still_settle_exact_repeat_operations    FAILED
test result: FAILED. 0 passed; 4 failed
```

with, for instance,
`Err value: ProviderConflictingBytes("manifests/f92ddcb5-….manifest")` for the
manifest repeat and
`Err value: UnsafeProviderJournal("869d4269…")` for the redelivered-source
removal repeat. So the tests do fail if retirement deletes without provider
state answering in the record's place; they are not vacuous.

The necessity assertions *inside* the tests carry the other half: a rename or
remove repeat with a different event id or a different source path still gets
`UnknownProviderPath` even though the destination exists, so the settle is
bound to this device's evidence for that exact operation.

### Baseline by names

Baseline (base `3f3a7afe`, before any edit), full `cargo test -p tine-core --lib`:

```
test result: FAILED. 1723 passed; 72 failed; 42 ignored; 0 measured; 0 filtered out
```

72 names in `../baseline-a2-names.txt`. No `oplog::wire::*` name is in that set,
so wire.rs's own tests are a clean oracle for this packet.

First candidate run: `1725 passed; 75 failed`, three new names, all real and all
fixed rather than excused:

| New red name | Cause | Resolution |
| --- | --- | --- |
| `oplog::wire::tests::provider_publication_revalidates_every_race_boundary` | Its case (1) asserted the old asymmetry — that the plain manifest path refuses *any* pre-existing occupant, including a byte-identical one | Rewrote that assertion to the unified contract: a DIFFERENT occupant is still refused and left untouched; a byte-identical occupant settles without rewriting it and mints no journal record. The safety property (never silently overwrite different bytes) is asserted, not weakened. |
| `projection_producer_census::g_a_mutation_primitive_counts_are_pinned_per_file` | `wire.rs` `cap.remove_file` 8 → 9: the sweep's `self.completed.remove_file(&name)` | Pinned count updated 8 → 9. The guard did its job: it demanded the census be updated in the same change. |
| `projection_producer_census::g_b_choke_helper_caller_counts_are_pinned` | `put_complete` callers 2 → 1: `publish` no longer calls it directly, it delegates to `publish_exact` | Pinned count updated 2 → 1. This is the census recording that a producer family lost a caller — the one-question-one-answer collapse in §4. |

Reported by the test itself (`--nocapture`):

```
harvest A2 gate: 20000 completed provider operations, peak completed/ 64,
final completed/ 32, trigger 64, structural bound 16384
```

Final candidate run, same command, same extraction, same comparison:

```
test result: FAILED. 1728 passed; 72 failed; 42 ignored; 0 measured; 0 filtered out; finished in 324.92s
comm -13 baseline-a2-names.txt candidate2-a2-names.txt   ->   (empty)   # zero new red
comm -23 baseline-a2-names.txt candidate2-a2-names.txt   ->   (empty)   # nothing silently "fixed"
```

**Zero new red names**, and the same 72 pre-existing names, so nothing in the
baseline was masked either. Five more tests pass than at baseline
(1723 → 1728), which is the five new tests in this packet.

### Commands run

```
# baseline, before any edit
source scripts/env.sh && cargo test -p tine-core --lib          # 72 failed

# focused, after each change
cargo test -p tine-core --lib -- --exact \
  oplog::wire::tests::provider_journal_completed_records_retire_against_live_provider_state
cargo test -p tine-core --lib -- --exact <the four idempotency/crash tests> \
  oplog::wire::tests::provider_publication_revalidates_every_race_boundary \
  projection_producer_census::g_a_mutation_primitive_counts_are_pinned_per_file \
  projection_producer_census::g_b_choke_helper_caller_counts_are_pinned   # 7 passed

# fail-before probes (applied, run, reverted from a byte copy)
#   A: compaction call disabled + MAX_PROVIDER_JOURNAL_COMPLETED = 128
#   B: operation_settled_by_retirement_diagnostic -> Ok(false),
#      publish_exact identical-destination branch -> refuse

cargo fmt --all                                                  # from the repo root, once
cargo test -p tine-core --lib -- --nocapture                     # final candidate suite
```

The box carried a release build and two other lanes throughout, so every number
above is a count or a behaviour, never a timing.

## 8. Neighbouring finding, NOT addressed (for the manager)

`{inbox,outbox}/removed/` is capped at `MAX_PROVIDER_RESIDUE_ENTRIES` (512) by
`ensure_provider_diagnostic_capacity`, which **refuses with
`ProviderRescanLimit`** beyond it, and nothing ever retires those diagnostics.
`retire_frontier_head` writes one per superseded frontier head, so this is the
same lifetime-growth-to-a-hard-refusal shape as A2's, one namespace over, in
the provider tree rather than the journal: after 512 head retirements the
*removal itself* refuses. A2 does not fix it, does not make it worse (no new
diagnostic is written by anything this packet added), and does not hide it — it
is stated in the new contract section so the next reader does not mistake A2's
guarantee for covering it.

It was deliberately left out of scope for two reasons. It is a different
mechanism with a different owner — provider residue in the shared tree, versus
device-private journal evidence — so its predicate has to reason about other
devices, not just this one. And this packet **raises the stakes on those
diagnostics**: `removed/retired-<operation id>` is now the evidence an exact
repeat of a retired rename or remove settles from, so a future pruner cannot
treat them as pure diagnostics any more. It needs its own packet and its own
proof; if a manager wants the 512 refusal gone, that constraint should be in
the dossier.
