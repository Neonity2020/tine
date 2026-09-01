# Harvest D receipt — stale async landing

## Contract and pinned base

Harvest D implements I-20 for the dossier's full frontend sweep: graph-scoped
async work captures `graphBinding()` plus the graph root before its first await,
rechecks at every landing boundary, and never treats the render epoch as graph
identity. User actions fail visibly; background work retires silently and
queues replacement work where required.

Pinned cumulative Wave-1 base:
`e19514f98cf1044391f3a3d2f09d2579275353ee`.

## Owned surface

- `src/landAsync.ts` and its tests/guard
- QuickSwitcher resource/create landing
- pinned tab/PDF close confirmation
- all eight Block asset initiators and their required editor/graph token
- plugin invocation ownership
- reload-on-focus graph retirement and replacement scheduling
- Settings plugin/backup busy ownership and the mobile media-editor gate
- `docs/contracts/frontend-staleness.md`, the Harvest D catalog rows, and
  CHANGELOG

No Rust, backend, persistence, ui, graph, or native source was changed.

## Baseline and necessity

The prior executor committed the implementation without preserving baseline
artifacts. Before sealing the packet, both required commands were reconstructed
in a detached worktree at the exact parent: `npm test -- --run` passed in full,
and `npx tsc --noEmit` reported no errors. See
`baseline-d-npm-test.txt` and `baseline-d-tsc.txt`.

Necessity was independently reconstructed by applying the D helper and D tests,
but none of the D production-site changes, to that exact parent. The unit and
render invocations both failed at the intended assertions: stale QuickSwitcher
navigation, tab loss after confirmation, wrong-editor asset insertion,
render-epoch plugin invalidation, missing replacement focus refresh, and a
prematurely re-enabled backup button. The source guard also caught the old
graph-epoch comparison and missing switcher binding. See `necessity-d.txt`.

## Gates

- `npm test -- --run`: PASS in full after adding graph identity to the existing
  modified-click render harness.
- `npx tsc --noEmit`: PASS.
- `npm run build`: PASS.
- `node scripts/check-regression-catalog.mjs`: PASS, 391 entries and 252 GitHub
  issues.
- `npm run check:regressions`: inherited red floor only. Its catalog and index
  checks pass, then the retired-managed-v1 guard flags
  `crates/tine-core/src/sync_runtime_tests.rs:27934`. That Rust file is bytewise
  unchanged from the pinned parent, and the flagged inert-v1 fixture is already
  present there; D neither owns nor alters it.
- `git diff --check`: PASS at seal time.

## Forks resolved

- Graph identity is the existing `graphBindingRev`, not a new counter and not
  `graphEpoch`.
- Block asset scope is captured at each initiator, not after latency at the
  sink; the sink argument is required.
- A graph change during focus refresh queues exactly one non-overlapping
  replacement after the stale tail settles.
- QuickSwitcher render tests now mount the graph-scoped component with the
  production-equivalent graph owner instead of a null graph.

## Verdict

**IMPLEMENTED AND VERIFIED WITH ONE INHERITED OUT-OF-SCOPE GUARD FAILURE.** All
D-owned tests, full frontend tests, typecheck, build, catalog validation, source
guard, contract doc, and catalog rows are green. The only composed regression
command failure is unchanged at the exact parent and outside D's write set.
