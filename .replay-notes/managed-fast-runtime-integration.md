# Managed fast runtime integration

## Foreground path

Eligible existing-page Markdown and Org saves now follow this actor-owned path:

1. Load the accepted page from the hot engine and the exact parser-owned page
   shell from its pinned graph path.
2. Prepare one trusted local operation against the current accepted plus
   journal-overlay state.
3. Append one authenticated frame to the private per-workspace/per-device local
   journal.
4. Publish the exact guarded Markdown/Org target.
5. Apply the committed record to the hot overlay.
6. Construct the application/editor reply directly from the committed post-page
   and exact target bytes.

The reply does not wait for archive publication, accepted-history expansion,
SQLite/tail adoption, projection receipts, local authorship, provider
publication, or application reload. New/delete/rename/title/path/kind/identity,
multi-page, external, remote, and declined operations retain the established
coordinator path.

## Recovery and drain

The enrolled journal lives under the private application runtime root, outside
the graph projection. Actor open authenticates and replays its complete prefix,
finishes any exact graph publication without another append, restores the hot
overlay, and queues uncheckpointed derivatives before opening the external
feed.

Idle/tick work advances one restartable derivative through archive, accepted
engine history, SQLite/tail, projection adoption, authorship, provider
publication, and an immutable checkpoint. Later local records for the same path
are authenticated as superseding projections, so an older derivative does not
overwrite newer graph bytes. Clean shutdown refuses while committed foreground
recovery or an uncheckpointed derivative remains.

Exact watcher echoes are admitted as no-ops. Divergent external bytes remain
external work and prevent a falsely clean handoff.

## Evidence

- `cargo check -p tine --all-targets`: passed.
- `cargo test -p tine-core managed_local_ --lib`: 10 passed, 2 ignored manual
  benchmarks.
- `cargo test -p tine-core application_gateway_ --lib`: 5 passed.
- `cargo test -p tine-core 'oplog::trusted_local_commit::tests' --lib`: 7 passed,
  1 ignored manual benchmark.
- `cargo fmt --all` and `git diff --check`: passed.

The runtime branch predates the retained-author foreground performance change,
so final release timings must be measured only after integration onto that
newer branch. That change independently measured warm coordinator p50 at 1.080
ms for 100 pages and 1.383 ms for 10,000 pages.

## Known limit

The first implementation retains the append-only journal and one immutable
checkpoint per drained sequence. This is correct and restartable, but journal
and checkpoint compaction is required before long-lived production use so cold
open does not become proportional to lifetime edit count. Compaction must retain
the latest authenticated checkpoint and any uncheckpointed suffix atomically;
it is not part of foreground save latency.
