# Harvest B3 implementation receipt

## Contract

Harvest packet B3: replace WebView `localStorage` live-conflict retention with a named, graph-keyed, app-private native capsule protocol; prove atomic publication/retirement with crash cuts; add Managed Storage capture parity without persisting replacement authority; prove restart recovery and both resolution actions on both backends.

Pinned base: `2dcd01341e6797f1223d35bc2d1fdd7a4f98f910`.

## Owned files touched

- `tests/regressions/non-ui.json`: added the required pre-fix Harvest B3 regression row with status `fixing`.
- `baseline-b3-npm-test.txt`, `baseline-b3-tsc.txt`, `baseline-b3-cargo-test.txt`: pre-edit baselines.
- `RECEIPT.md`: this blocker receipt.

No production file was edited.

## Exclusions honored

- Did not read, search, index, diff, or use either forbidden brain corpus path.
- Did not use or quote the anonymized test corpus.
- Did not edit `src-tauri/src/settings.rs` or any file outside the assigned worktree.
- Did not commit or run a Git-mutating command.

## Fail-before evidence

- Manager-verified source defect accepted from the dossier: Direct Files retains the live draft in WebView `localStorage`, and the Managed conflict arm has no restart capsule.
- The regression catalog entry was created before any attempted fix.
- The required pre-fix E2E necessity run was not reached because the write-set blocker prevents a contract-correct implementation.

## Baselines

- `source scripts/env.sh && npx tsc --noEmit`: green; no failure names.
- `source scripts/env.sh && cargo test -p tine`: green; 342 passed, 0 failed, 2 ignored.
- `source scripts/env.sh && npm test`: infrastructure red before test collection: `EROFS` while Vite tried to create `node_modules/.vite-temp/vitest.config.ts.timestamp-*.mjs`.

## Blocker

The dossier requires native IPC restore to populate the conflict queue before a user can act. The existing synchronous call is `restoreLiveSaveConflicts(meta.root)` at `src/graph.ts:214`; `src/graph.ts` is absent from the dossier's exhaustive write set. A native load is asynchronous. Without changing that call to await the load (or adding an equivalent graph-open gate there), `loadGraphPath` can report the graph interactive and clear `graphTransitioning` before restoration finishes. Eventual restore is unsafe because the affected page can be edited or resolved against an absent capsule during that window.

Durable retirement has the same ownership mismatch: successful live-conflict resolution calls synchronous `clearConflict(pageName)` at `src/components/ConflictResolution.tsx:436`, while native atomic retirement must be awaited before the resolution is acknowledged. That component is also outside the exhaustive write set.

Required write-set decision from the manager: authorize `src/graph.ts` for awaited restore ordering and `src/components/ConflictResolution.tsx` for awaited durable retirement, or name an alternative authoritative gating/acknowledgment design within the current write set.

## Argued cuts

- No localStorage migration machinery is needed beyond deleting the legacy key on first use, as specified. This cut was not implemented because production work stopped at the blocker.

## Verdict

BLOCKED before production implementation by the exhaustive write set. Expanding it by the two required-neighbor files above is necessary to satisfy, rather than weaken, the B3 ordering and durability acceptance gates.
