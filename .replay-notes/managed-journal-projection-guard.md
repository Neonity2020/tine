# Journal-committed one-page graph projection guard

Branch: `perf/managed-journal-projection`

Starting revision: `897b9a54eb544fb343823a44391fbb1e807912d2`

## Outcome

`Graph` now exposes one internal existing-page boundary that keeps the current
managed-text writer authority, retained graph-text identity authority, and
exact path lock across strict validation, a caller-supplied durable journal
append, and the existing guarded graph publication machinery. The journal
append proof is generic `A`; `model.rs` neither names nor encodes the parallel
worker's record/proof type.

It also exposes one callback-free restart boundary. After process memory is
lost, the later journal codec/runtime owner supplies the authenticated opaque
proof plus exact recorded path, prior base, target, base revision, and target
revision. The graph reacquires runtime-only file/parent identity authority and
either publishes from the exact base, reproves an already exact target, or
preserves a divergent external winner as committed-pending.

The boundary supports one existing, pinned Markdown or Org path only. It does
not route create, delete, rename, title/ownership change, or multi-page work.

## Exact API

```rust
pub(crate) fn Graph::commit_existing_page_with_journal<A>(
    &self,
    page: &PageDto,
    base_rev: &str,
    expected_base: &[u8],
    exact_target: &[u8],
    append: impl FnOnce() -> io::Result<A>,
) -> io::Result<JournalPageProjectionOutcome<A>>;

pub(crate) fn Graph::retry_committed_journal_page_projection<A>(
    &self,
    pending: CommittedPendingJournalPageProjection<A>,
) -> JournalPageProjectionOutcome<A>;

pub(crate) fn Graph::recover_committed_journal_page_projection<A>(
    &self,
    append_proof: A,
    relative_path: &str,
    base_revision: &str,
    expected_base: &[u8],
    exact_target: &[u8],
    revision: &str,
) -> JournalPageProjectionOutcome<A>;

pub(crate) enum JournalPageProjectionOutcome<A> {
    Durable(DurableJournalPageProjection<A>),
    CommittedPending(CommittedPendingJournalPageProjection<A>),
}
```

`DurableJournalPageProjection<A>` carries `A` unchanged plus
`JournalPageProjectionTarget { relative_path, target, revision }`.
`CommittedPendingJournalPageProjection<A>` carries `A` unchanged, the exact
path/base/file identity/parent identity/target/revision needed for recovery,
and the last `io::Error`. Its public(crate) accessors expose the proof, exact
relative path, exact target bytes, and failure without exposing constructors.
For an in-process retry it retains the stronger pre-append runtime identity
plan. For restart recovery it retains only the authenticated record material;
each retry reacquires compatible current file and parent identities rather than
treating a divergent file as an authorized base.

The private typestate is:

```text
VerifiedJournalPageProjection --append: io::Result<A>-->
JournalCommittedPageProjection<A> --publish-->
Durable<A> | CommittedPending<A>

authenticated durable record after restart --rebind exact runtime authority-->
Durable<A> | CommittedPending<A>
```

The verified state has no graph mutation method. The committed state is the
only state with `publish`. The retry API accepts no closure, so it cannot append
a second record. The restart API likewise accepts no closure and never enters
the pre-append typestate.

## Lock and commit boundary

Lock order remains:

```text
ManagedTextWritePermit
  -> resource-shared GraphTextIdentityMutationGuard
    -> exact page lock
      -> cache
        -> disk revisions
```

Those first three authorities stay live continuously through:

1. pinned existing-path and scope resolution;
2. exact current base bytes and editor revision;
3. retained exact file owner/resource identity;
4. retained parent-chain and final-parent resource identities;
5. portable case/NFC and physical alias checks;
6. unique warm effective semantic ownership at the exact path;
7. strict Markdown/Org DTO serialization equality with `exact_target`;
8. a final exact base/file/parent reread;
9. the `append` callback; and
10. publication, durability proof, retained-index/cache publication, and
    watcher-marker cleanup.

After restart, the boundary reacquires the same first three authorities before
examining the exact path. It rejects inconsistent UTF-8/revision material and
excluded/private paths. At the exact base or target, the current file resource
must match the retained admission record and the retained parent chain must
match before the existing publication/durability machinery runs. A file whose
bytes are neither exact base nor exact target remains untouched and is returned
committed-pending with the original proof and exact target.

Validation failure and append failure return the original `io::Error`; graph
bytes are not touched. The append callback's successful return is the commit
boundary. Every later error becomes `CommittedPending` and retains the opaque
proof and exact target. A retry either republishes from the exact original base
and file/parent identities, completes barriers/cache for an already exact
target, or remains committed-pending behind a divergent external winner.

## Publication reuse

The committed state calls the existing identity-bound editor replacement:

- file-synced staged recovery;
- no-replace retirement of the live name;
- exact displaced bytes and file-resource validation;
- no-replace target publication;
- required parent-chain directory syncs;
- final file sync and exact reread;
- retained graph-text identity update;
- loaded file identity and disk revision/cache publication; and
- existing self-write marker lifecycle for watcher echo suppression.

`write_page` and the new boundary share the extracted
`serialize_page_dto_for_path`, so Markdown/Org corruption firewalls and direct
round-trip behavior remain singular.

## Focused evidence

`RUST_MIN_STACK=134217728 cargo test -p tine-core --lib journal_projection -- --test-threads=1`
passed 11 tests. The matrix proves:

- Markdown and Org validate before one append and finish at the exact target;
- stale revision, changed target resource, portable collision, hardlink alias,
  semantic collision, and replaced parent append zero times and change zero
  operation bytes;
- an append error is returned unchanged and changes zero bytes;
- retire, publish, file-sync, directory-sync, and cache-publication cuts return
  committed-pending with exact proof/path/target;
- every cut retries to the exact target without another append;
- an external precommit winner causes zero append, while a post-append external
  winner is preserved and the journal operation remains committed-pending;
- configured nested Markdown and Org paths work and private paths are refused;
- a successful synthetic append-before-publication cut can discard every
  in-memory pending value, reopen the graph, reconstruct only opaque durable
  proof plus exact path/base/target/revisions, and publish nested nonstandard
  Markdown and Org targets without another append;
- restart recovery performs exactly one graph mutation from the exact base,
  performs zero graph mutations when the exact target is already visible, and
  reproves required file/directory/cache state in both cases;
- a divergent restart-time external winner is preserved across recovery and
  retry while the proof and exact target remain committed-pending;
- restart recovery refuses private graph paths;
- eight warm sequential commits perform zero complete graph-text inventories,
  visit zero inventory entries, and perform zero SQLite/archive/receipt/catalog/
  application-load work; and
- the restart journeys perform zero graph inventory and forbidden foreground
  work after startup authority is primed; and
- a source-structure proof keeps append before publish and keeps both retry and
  restart recovery callback-free.

The named invalidated projection tests all passed individually:

- `projection_exact_proof_binds_path_bytes_digest_and_exact_preconditions`
- `projection_exact_never_clobbers_pre_publish_or_proves_post_publish_changes`
- `projection_boundary_race_is_rejected_before_displacement`
- `projection_sync_failure_requires_recovery_and_stale_write_remains_conflict`
- `projection_retry_resumes_after_synced_partial_parent_chain`
- `projection_exact_updates_warm_cache_once_and_suppresses_watcher_echo`

The two guarded-save identity-race tests passed. The `handoff_` filter passed 29
tests. Repository verification passed:

- `cargo fmt --all`
- `cargo check -p tine --all-targets` (0 errors)
- `git diff --check`

An additional broad `model::` run produced 323 passed, 2 failed, and 1 ignored.
The two failures are existing watcher/effective-identity tests in untouched
logic (`cold_watcher_failures_publish_generation_bound_identity_evidence_until_repair`
and `warm_install_and_watcher_failures_replace_effective_identity_evidence`):
both expect `AlreadyExists` but receive `InvalidData`, and the first reproduces
alone. They are outside this one-page journal guard and were not changed.

## Slow-path operations and next call site

The trusted-local coordinator must retain the existing slow path for:

- new, missing, unpinned, guide, excluded, or private paths;
- cold/incomplete semantic ownership evidence;
- page title/kind/ownership changes and renames;
- deletes and creates; and
- every multi-page/source/destination/referrer plan.

The next coordinator call site is the eligible existing-page branch after it
has prepared the exact editor base, strict rendered target, and durable journal
record input. It should call `commit_existing_page_with_journal`, perform the
real local-journal append inside `append`, retain `CommittedPending` without
redrafting, and call `retry_committed_journal_page_projection` for recovery.
At startup it should authenticate/decode its durable record, then call
`recover_committed_journal_page_projection` with the opaque proof and exact
recorded path/base/target/revisions; no `PageDto`, append callback, redrafting,
or internal identity digest is required. The runtime/coordinator owner must
match `Durable` versus `CommittedPending`; `Graph::save_page` must not be called
after a successful append.
