# Fast trusted-local commit: retained guarded identity makes the spine flat

Branch `perf/managed-fast-commit-prototype`, requested starting point
`1a8a7e6f`.

## Outcome

The post-v0.6.5 ordinary-save regression is removed without weakening the
portable-path or physical-resource collision invariant. A warm guarded save now
performs zero complete graph-text inventories and visits zero graph-text
entries. On ext4, the complete permanent release matrix passes every gate for
both Markdown and Org:

| format | pages | p50 ms | p95 ms | 10k/100 p50 | inventories/edit | entries/edit |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Markdown | 100 | 3.230 | 4.078 | | 0.000 | 0.0 |
| Markdown | 1,000 | 3.172 | 3.812 | | 0.000 | 0.0 |
| Markdown | 10,000 | 3.173 | 3.749 | **0.982x** | 0.000 | 0.0 |
| Org | 100 | 3.115 | 3.582 | | 0.000 | 0.0 |
| Org | 1,000 | 3.209 | 3.812 | | 0.000 | 0.0 |
| Org | 10,000 | 2.901 | 3.660 | **0.931x** | 0.000 | 0.0 |

All p50 values are below the 5 ms target, all p95 values are below 20 ms, and
the 10,000-page p50 is no slower than the 100-page p50. The previous
receipt was approximately 446 ms p50 at 10,000 pages, four inventories per
edit, and 39,999 visited entries.

## Retained identity and invalidation contract

`Graph` now retains a complete guarded graph-text identity generation built by
the existing bounded, two-sided retained-capability capture. The generation
contains:

- every exact regular-file path and its physical resource/link identity;
- portable case/NFC collision groups for every eligible Markdown/Org path;
- semantic page/journal ownership, reusing the warm cache on the first lazy
  build and parsing current bytes on a rebuild after uncertainty;
- exact directory-resource bindings and tombstones needed by remove/restore
  transitions.

The owner of the observation/write race is the resource-shared managed-text
write gate. Its reentrant graph-text identity-mutation authority covers strict
validation, the filesystem transition, and retained-index publication. Every
`Graph` reopened through the same resource shares this serialization, including
supported moved roots and symlink aliases already accepted by the managed
writer gate.

That gate also owns a monotonic resource mutation epoch. Every exact Tine
transition, exact external observation, and uncertain/broad external
invalidation advances it while holding the same reentrant authority. Each
per-`Graph` retained index records the exact resource epoch it represents and
is reusable only while the two values match. A mismatch rebuilds that Graph's
own configured index once from current bytes before validation; the index itself
remains per-Graph so differently opened scope/configuration is never conflated.

Tine-owned create, replace, move, rename, remove, migration, restore, and sync
entrypoints take that authority. Staging/recovery names are introduced only
after the retained baseline exists, so temp/retire/publish/restore states never
become duplicate owners. Successful filesystem transitions publish exact
path deltas into persistent reverse maps. If an index publication fails after a
durable filesystem transition, the edit remains committed and its normal cache
publication completes; the resource epoch has already advanced, so neither the
owning Graph nor any sibling Graph can authorize a later write from stale
evidence. A successful exact delta publishes the final advanced epoch with the
updated owning index, preserving its next scan-free warm save.

The legacy Tauri watcher now retains the exact graph root already bound in each
`GraphSlot` and subscribes recursively once at that root. It does not reopen the
graph or infer ownership from the process cwd, and it no longer needs redundant
nested page, journal, or managed-sync subscriptions. The existing page/journal
cache reconciliation remains scoped to its configured directories; the wider
root subscription is the guarded-identity observation boundary.

The platform callback records graph-root observations before the existing 200
ms cache-coalescing delay:

- supported Markdown/Org callbacks anywhere under that exact root are re-read
  with two-sided resource/bytes proof and applied as content-derived exact
  deltas, including nested nonstandard locations;
- create, delete, and rename final states update the generation without a full
  walk;
- `logseq/config.edn`, eligible directory/root mutation, backend rescan,
  watcher error/overflow, poll cycle, unsupported event, hardlink/alias, unsafe
  shape, unreadable path, failed delta, or otherwise uncertain generation
  invalidates it;
- ownership is tested against each exact bound root, so another open graph's
  callback is ignored; fixed/configured exclusions are classified by the core
  scope policy, keeping `.tine-sync`, assets, Logseq recovery/storage, and
  hidden excluded subtrees out of retained graph-text ownership;
- exact non-text or excluded callbacks are harmless, while ambiguous rename or
  directory events conservatively invalidate rather than guessing;
- `Graph::sync_file_checked` is also an exact observation boundary for direct
  callers, and broad public cache invalidation invalidates guarded identity;
- after invalidation, the next guarded write performs exactly one complete
  current rebuild under the same mutation authority before validation. A real
  collision is refused; otherwise the write proceeds and later warm writes use
  exact deltas again.

Strict editor/name-only writes consult the retained generation for portable,
physical-resource, and content-derived semantic ownership. The sparse durable
projection validator keeps its existing exact-path authority and bounded direct
probes; it was deliberately not coupled to this ordinary-save cache.

The contract continues to preserve configured nested/nonstandard page and
journal roots, graph-wide Markdown and Org discovery, case and NFC equivalence,
hardlink/same-resource refusal, managed symlink behavior, retained-root moves,
and the existing no-replace recovery state machine.

## Permanent proofs

The structural fast-commit proof now performs one lazy complete build, then
measures repeated warm saves at 4 and 400 pages. Both sizes perform identical
work: one exact retained delta per save, zero inventory scans, zero entries
visited, and no SQLite/archive/receipt/catalog/page-reload work. It also asserts
that each graph performs exactly one complete build.

New model proofs cover:

- exact external create, delete, and rename without complete rebuild;
- hardlink creation poisoning the exact delta, followed by one rebuild and a
  real collision refusal;
- watcher overflow/uncertainty causing exactly one later rebuild;
- case and NFC portable collisions both on an exact retained delta and after
  invalidation/rebuild;
- content-derived semantic collision refusal at the callback boundary, before
  deferred cache reconciliation, and again after rebuild;
- an injected retained-index publication failure: the durable edit and cache
  agree, a fresh reopen sees the committed semantic/file state, and the next
  write rebuilds once.

Four independent-two-Graph resource-epoch proofs additionally cover:

- both instances first build warm indexes at one shared epoch, then an exact
  portable collision, hardlink alias, or content-derived semantic collision
  observed through Graph A makes Graph B rebuild once and refuse the real
  collision;
- an ordinary non-colliding save through A makes B rebuild once, after which
  repeated B saves perform zero inventories and visit zero entries until the
  next resource transition;
- uncertain watcher intake through A advances the resource epoch and makes B
  rebuild exactly once;
- an injected post-filesystem index-publication failure through A leaves the
  committed bytes visible to a fresh reopen and prevents B from matching the
  prior epoch before its one safe rebuild.

Focused watcher proofs additionally cover Markdown and Org create, delete,
rename, and content-derived collision intake in nested nonstandard directories;
configuration, graph-root/directory, rescan, and notify-error uncertainty; exact
cross-root isolation; excluded/private ownership; the bound-root subscription;
and the existing configured-root incremental and sparse-runtime routing lanes.

Existing differential and failure-injection coverage remains green, including
Markdown/Org twins, same-resource aliases, nested layouts, unsafe symlink and
special-file refusals, root replacement/move behavior, and failures at retire,
publish, restore, rollback, and projection recovery boundaries.

## Verification

Commands were run with `RUST_MIN_STACK=134217728` where applicable.

- `cargo fmt --all`: pass.
- `cargo check -p tine --all-targets`: pass.
- `cargo test -p tine watcher::tests -- --test-threads=1`: **32 passed**.
- `cargo test -p tine-core --lib resource_epoch_ -- --test-threads=1`: **4
  passed**.
- `cargo test -p tine-core --lib fast_commit -- --test-threads=1`: **7 passed,
  1 ignored** (the ignored case is the manual release benchmark).
- `cargo test -p tine-core --lib -- model:: --test-threads=1`: **313 passed,
  0 failed, 1 ignored**.
- `cargo test -p tine-core --lib fast_commit -- --test-threads=1`: **7 passed,
  0 failed, 1 ignored**.
- focused guarded identity proofs: **3 passed**.
- focused two-instance resource-epoch proofs: **4 passed**.
- permanent release benchmark, ext4 only, defaults of 100 timed edits after 10
  warmups, both formats and 100/1,000/10,000 pages: **all latency, scale, and
  zero-work gates pass**.

After the resource-epoch correction, a short ext4 release probe used 2 warmups
and 10 timed edits at 100 and 10,000 pages in both formats. Markdown measured
3.222/3.239 ms p50 (1.005x) and at most 4.067 ms p95; Org measured 3.123/3.173
ms p50 (1.016x) and at most 4.743 ms p95. Every configuration performed zero
inventories and visited zero graph-text entries. The stored raw full-matrix
samples were not rewritten for this short probe.

The exact correction probe command was:

```text
RUST_MIN_STACK=134217728 TINE_FAST_COMMIT_BENCH_SURFACES=ext4 \
  TINE_FAST_COMMIT_BENCH_PAGES=100,10000 \
  TINE_FAST_COMMIT_BENCH_EDITS=10 TINE_FAST_COMMIT_BENCH_WARMUPS=2 \
  cargo test --release -p tine-core --lib \
  fast_local_commit_latency_manual_release_benchmark -- --ignored --nocapture
```

The exact benchmark command was:

```text
RUST_MIN_STACK=134217728 TINE_FAST_COMMIT_BENCH_SURFACES=ext4 \
  cargo test --release -p tine-core --lib \
  fast_local_commit_latency_manual_release_benchmark -- --ignored --nocapture
```

Raw samples are in `managed-fast-commit-prototype-samples.txt` beside this note.

## Workspace limitation

The managed execution sandbox exposed the worktree as writable but the linked
Git worktree index under `/aux/koutecky/logseq/backups/logseq-claude/.git/` as
read-only. The required meaningful commits and final clean branch therefore
could not be produced from this session: `rtk git add ...` failed creating
`index.lock` with `Read-only file system`. Source, tests, and receipts are
present in the worktree; no push, integration, deployment, tag, or release was
attempted.
