# 0066. Remove Managed Storage; Direct Files is the only storage mode

- **Status:** Accepted
- **Date:** 2026-09-15
- **Supersedes:** [0049](0049-oplog-first-sparse-storage.md),
  [0053](0053-enrollment-checkpoint-integrity.md),
  [0054](0054-lazy-genesis-managed-activation.md),
  [0060](0060-qualified-generations-and-indefinite-cold-history.md),
  [0061](0061-unwire-per-block-managed-layout-forward.md),
  [0062](0062-real-deletion-and-restore-by-reconstruction.md),
  [0063](0063-checkpoint-floor-policy-and-recovery-input-journal.md),
  [0064](0064-generation-root-extension-and-hot-retirement.md)
- **Amends:** [0055](0055-native-storage-mode-supervisor.md)

## Context

Managed Storage was Tine's experimental operation-backed storage mode: a CRDT
operation log (Loro) held the graph's truth, Markdown/Org became a derived
projection, and a shared provider directory carried history between devices.
It lived behind a **Testing only** opt-in and never enabled itself. The
blank-slate rule meant no released user depended on its private formats.

Its cost was not confined to the opt-in. Managed Storage accounted for a large
share of the Rust core and native shell, most of the storage contract, a large
fraction of the regression catalog and native E2E suites, a dedicated Android
CI job, and a vendored CRDT library. Its admission, shutdown, and binding
machinery sat on paths that every Direct Files user runs: the Direct Files
regression audit of 2026-08-06 traced a class of Direct save-path regressions
to Managed Storage capture machinery running on the ordinary save path. Each
Managed Storage milestone also added new mechanism faster than it retired old
mechanism.

Meanwhile the product value Tine has shipped in the same period — Concord
conflict resolution, the query engine, live export — lives entirely on Direct
Files. The 0.7 story no longer needs sync to be complete.

## Decision

We will remove Managed Storage from the codebase entirely. Direct Files is the
only storage authority. The implementation stays in git history and is not
kept as dormant code, feature-flagged code, or a compatibility reader.

- The storage-mode supervisor (ADR 0055) remains as the one owner of graph
  lookup and Direct Files opening. Its managed phases, the return-to-Direct
  transition, and per-window mode selection are gone.
- Concord (ADRs 0056 and 0057) is unaffected: its base ledger was always a
  Direct Files cache.
- Any future sync or end-to-end-encrypted subgraph sharing will be specified
  from scratch. That specification must set explicit budgets for code size,
  performance, and disk space, state the code invariants it must keep, and
  reuse existing code where possible. It does not inherit the superseded ADRs'
  formats or state machines.

## Consequences

- A graph that was in Managed Storage opens as an ordinary Direct Files graph.
  Its Markdown/Org projection is the graph. Tine no longer reads or writes the
  `.tine-sync/` directory or the private app-data stores; it leaves them on
  disk untouched.
- The storage contract (`docs/storage-sync-contract.md`) describes Direct
  Files only. Refusal-table rows, regression-catalog rows, E2E journeys, and CI
  jobs that existed only for Managed Storage are deleted, not quarantined.
- Direct Files code paths lose the admission, lease, retirement, and shutdown
  arms that existed to hand the graph between two authorities. Several
  single-variant shapes remain for now (for example a storage route with only
  `direct` and `unavailable`); collapsing them is follow-up consolidation.
- Device-to-device sync stays delegated to the user's own tool (Syncthing,
  Dropbox, git) over the Markdown/Org files, which Tine already coexists with
  through conflict-copy detection and Concord.
- Reintroducing sync is a new project with its own ADR, not a revert.
