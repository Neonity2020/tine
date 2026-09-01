# Harvest B1 — authority census + semantic-operation adapters (receipt)

Worktree `/aux/koutecky/logseq/tine-agent-worktrees/batch-harvest-b1`,
branch `batch/harvest-b1`, base `3f3a7afe49c488ba5444c4882fff727d7c0099c0`.

Invariants in play: **I-6** (storage authority is selected in one place, then
flows as a value), **I-20** (a late result cannot land on the wrong state),
**I-12** (one question, one implementation).

**Verdict: complete and behaviour-preserving.** Two commits:

| SHA | What |
| --- | --- |
| `47032864` | `harvest-B1: add the storage-authority front door (src/storageDispatch.ts)` |
| `0556bc19` | `harvest-B1: migrate the five semantic-operation call sites onto the front door` |

Not pushed, per the dossier.

---

## 1. Census — every `applicationPageAdmission` consumer in `src/`

Method: `grep -rn "applicationPageAdmission" src/` over all `.ts`/`.tsx`, plus a
read of every hit's enclosing function to classify it. No aliased read of the
snapshot field exists outside these sites (the snapshot is only destructured in
`managedStorageRuntime.ts` itself; every consumer names the field).

Classes per the dossier: **(a)** semantic-operation dispatch → migrate,
**(b)** legitimate value consumption / `binding_generation` re-check in an async
continuation → keep and list, **(c)** other → describe.

### Class (a) — migrated

| # | Site (pre-migration line) | Function | What it did | Disposition |
| - | --- | --- | --- | --- |
| a1 | `src/store.ts:5935` | `moveBlock` | read the admission, branched managed / unavailable-toast / Direct fall-through for a single-root cross-page move | now `dispatchCrossPageMove({sourcePages:[oldPage], destinationPage:newPage, roots:[id]}, {managed, direct, unavailable})`; the Direct fall-through is expressed as `handled === false` |
| a2 | `src/store.ts:6099` | `moveBlocksRelative` | same branch again, for the N-root relative move, with the Managed **single-source refusal** asymmetry | `dispatchCrossPageMove(...)`; the unchanged in-memory move + Direct tail was extracted into a local `applyRelativeMove()` closure so the same code serves both the same-page early return and the Direct arm |
| a3 | `src/store.ts:6910` | `moveBlockFeedNow` | same branch a third time, over `prepareManagedCrossPageMoveIntent` / `runManagedCrossPageMove` | `dispatchCrossPageMove<"within"\|"crossed"\|"none">(...)` |
| a4 | `src/store.ts:6988` | `moveSelectionItems` | same branch a fourth time, over `enqueueManagedCrossPageMove` | `dispatchCrossPageMove<void>(...)` |
| a5 | `src/filedrop.ts` (`insertDroppedFiles`) | `insertDroppedFiles` | read the admission to choose the managed bulk-insertion choreography vs `insertDroppedFilesDirect` | `dispatchDroppedFileInsertion({afterId, paths}, {managed, direct, unavailable})`; `filedrop.ts` now has **zero** authority readers |
| a6 | `src/carry.ts` (`report`) | `report` → `persist` | **no branch at all** — ran the Direct choreography under every admission, including a managed binding. `INVARIANTS.md` names this the I-6 specimen. | `dispatchCarryPersist({destinationPage, sourcePages}, {direct})`. Behaviour verbatim (Direct on every route); the route is now *recorded* and the gap is asserted by tests so B2 must delete them deliberately. `persist()` itself — destination-first ordering and its comment — is byte-identical. |

`carry.ts` is the site the dossier's known-site list did not contain; it is the
one the census was for.

### Class (b) — legitimate, kept (all in `src/store.ts`, post-migration lines)

| # | Line | Function | Why it is not a dispatch |
| - | --- | --- | --- |
| b1 | 2007 | `createPageMutationPlan` | **stamps** the admission into the plan as a value, at plan-build time. This is I-6 working as intended: decide once, carry the value. |
| b2 | 2154 | `pageMutationPlanCurrent` | **I-20** re-check: compares the plan's stamped admission against the live one before the plan is allowed to land. |
| b3 | 2659 | `preflightManagedBulkInsertion` | captures the admission (and its `application_*` limits) for a bulk insertion about to be issued. |
| b4 | 2710 | `consumeManagedBulkInsertionAdmission` | **I-20** re-check of that captured admission at consumption time. |
| b5 | 4036 | `captureBulkRouteFence` | captures the route fence for a bulk operation. |
| b6 | 4051 | `bulkRouteFenceCurrent` | **I-20** re-check of the fence. |
| b7 | 6402 | `managedMoveAdmission()` | the managed arm's own writability accessor, called *inside* the managed choreography **after** the route has already been chosen. Not a second derivation of the route; it is how the managed arm reads its own limits. |

Every class-(b) site is either a capture or the re-check of a capture — the
capture/re-check pair *is* I-20, and removing them would be the defect.

### Class (c) — other

| # | Site | What it is |
| - | --- | --- |
| c1 | `src/App.tsx:773` | `sweepScopeAuthority` memo: a reactive **scope key** for the absence-sweep subscription. It decides whether to *listen*, not where a write goes. |
| c2 | `src/mock.ts:945,955,957` | the browser mock backend **constructs** the native envelope. A producer of the value, like `managedStorageRuntime`'s own source — not a consumer at all. |

### Owners (excluded from the ratchet rather than allowlisted)

`src/managedStorageRuntime.ts` (11 occurrences) publishes the snapshot — binds
it, validates the envelope, revokes writability. `src/storageDispatch.ts` is the
one place a semantic operation's authority branch may live.

---

## 2. Adapter module surface — `src/storageDispatch.ts` (293 lines, new)

```ts
export type ManagedWritableAdmission = Extract<ApplicationPageAdmission, { authority: "managed_writable" }>;
export type StorageRoute = "managed" | "direct" | "unavailable";
export type SemanticStorageOperation = "cross-page-move" | "dropped-file-insertion" | "carry-persist";

export interface StorageRouteDecision  { route: StorageRoute; admission: ApplicationPageAdmission | null }
export interface StorageDispatchCounters { managed: number; direct: number; unavailable: number }
export interface StorageDispatchRecord  { operation: SemanticStorageOperation; route: StorageRoute; request: unknown }

export function selectStorageRoute(operation, request?): StorageRouteDecision;   // the ONE authority read
export function storageDispatchCounters(operation): Readonly<StorageDispatchCounters>;
export function resetStorageDispatchCounters(): void;
export function lastStorageDispatch(operation): StorageDispatchRecord | null;

export const CROSS_PAGE_MOVE_UNAVAILABLE_TOAST: string;                          // the one refusal message
export async function dispatchCrossPageMove<T>(request: CrossPageMoveRequest, arms: CrossPageMoveArms<T>): Promise<T>;
export async function dispatchDroppedFileInsertion<T>(request: DroppedFileInsertionRequest, arms: DroppedFileInsertionArms<T>): Promise<T>;
export async function dispatchCarryPersist<T>(request: CarryPersistRequest, arms: CarryPersistArms<T>): Promise<T>;
```

Request shapes are stated in domain terms — `CrossPageMoveRequest {sourcePages,
destinationPage, roots}`, `DroppedFileInsertionRequest {afterId, paths}`,
`CarryPersistRequest {destinationPage, sourcePages}` — because they are the seed
of the storage API in the spec's north star. They are load-bearing today, not
decorative: `lastStorageDispatch` records the stated intent and the guards assert
it, so a call site that routes correctly but hands over the wrong pages fails.

The route partition is exhaustive over the union's three variants plus `null`:
`unavailable` = (`null` ∨ `managed_unavailable`), `managed` = `managed_writable`,
`direct` = `direct`.

**Shape note — why "arms" and not "move the bodies in".** The dossier says the
dispatcher should call the existing arm implementations *moved, not rewritten*.
The Direct arms close over module-private `store.ts` internals (`setDoc`,
`produce`, `pushUndo`, `doc`, the crossMove helpers). Physically relocating them
into `storageDispatch.ts` would either export a dozen store internals or create
an import cycle (`store → storageDispatch → store`), and would be a large code
motion — i.e. exactly the non-behaviour-preserving change B1 forbids. So the arms
stay where their closures are and the **decision** moves: `storageDispatch.ts`
imports only `managedStorageRuntime`, `./ui`, and types. What is now
single-sourced is the branch, the admission read, the instrumentation, and the
shared refusal toast. Pulling the arm bodies across is B2/B3 work, on top of
these signatures.

---

## 3. Guards, and their fail-before evidence

Necessity gate: the three guard files were run against the **pre-migration**
tree (front door present, call sites unmigrated). Raw output:
`/aux/koutecky/logseq/tine-agent-worktrees/harvest-b1-necessity-pre.txt`.

```
 Test Files  2 failed | 1 passed (3)
      Tests  18 failed | 22 passed (40)
```

| Guard file | Tests | Fail-before |
| --- | --- | --- |
| `src/storageDispatchRoutes.test.ts` | 20 | **16 failed.** Every counter/instrumentation assertion was zero — the real `moveBlock` / `moveBlocksRelative` / `moveBlockFeedNow` / `moveSelectionItems` / `insertDroppedFiles` / carry paths did not reach the dispatcher at all. The two `"does not dispatch at all for a same-page …"` tests correctly **passed** before and after (they assert the dispatcher is *not* called). |
| `src/storageAuthorityRatchet.test.ts` | 2 | **2 failed.** `New authority reader in filedrop.ts` (unmigrated), and `Refusal text duplicated outside the front door … ['storageDispatch.ts','store.ts']` (four hand-written copies of the message still in `store.ts`). |
| `src/storageDispatch.test.ts` | 20 | passed pre-migration **by construction** — it exercises the front door directly, and the front door is this packet's own new module. Its necessity is structural, not temporal: before B1 there was nothing for it to test. Recorded honestly rather than manufactured. |

What each guard holds:

1. **Route-level capability** — `src/storageDispatch.test.ts`. A table of all
   four published admission states × expected route; each front door is invoked
   with an `unreachable(arm)` helper in every arm but the expected one, which
   throws an I-6-citing error if reached. So "under a managed binding the Direct
   arm is unreachable" is proved by *execution*, not by a name-grep. Also covers:
   the shared refusal toast fires on `unavailable` only; `lastStorageDispatch`
   records the stated intent; the route is decided once, before any arm runs (a
   rebind mid-arm cannot re-route).
2. **Real-path capability + instrumentation** — `src/storageDispatchRoutes.test.ts`.
   Drives the actual store/filedrop/carry entry points on a loaded document.
   `watchDirectPersistence()` spies `backend().savePage` and the store mutation
   observer; under a managed or unavailable binding it asserts
   `{saves: 0, dirtyMarks: 0}` with an I-6 message. This is the spec's
   instrumentation gate: `storageDispatchCounters` must show the managed route in
   the positive cases and the refusal route in the negative ones, so "it compiles"
   cannot pass for "it dispatches".
3. **Stale generation (I-20)** — same file. `const pending = moveBlock(...)`, then
   the binding is replaced with a *later* `binding_generation` while the operation
   is in flight, then `await pending`. Asserts the managed actor was never called,
   Direct persistence was never reached, and the counters still read
   `{managed: 1, direct: 0, unavailable: 0}` — the operation refused rather than
   landing on the wrong backend.
4. **Source-scan ratchet** — `src/storageAuthorityRatchet.test.ts` (house pattern:
   the `hmac::verify` source-count guard). The CENSUS table above lives in this
   file, in code, with each entry's class and reason; the scan fails on any new
   reader or a changed count. Its `RULE` message states I-6, names
   `src/storageDispatch.ts` with `dispatchCrossPageMove` /
   `dispatchDroppedFileInsertion` as the exemplar to imitate, cites the four
   drifted forks (audit UI-3) as the reason, and tells the reader how to add a
   genuine class-(b) read to the census. Its second test pins the refusal message
   to exactly one file.

The ratchet is deliberately the *cheap* half of a pair: a grep cannot prove
reachability (aliasing evades it), so it exists to make a new authority read
visible in review, while guards 1–3 carry the reachability claim.

---

## 4. Baseline-by-names delta — **zero new red names**

Baseline captured before any edit, at `3f3a7afe`, saved to
`../baseline-names.txt`:

```
npx tsc --noEmit                          -> EXIT 0, zero errors
npm test (node pool,   vitest.config.ts)  -> EXIT 0   196 files / 3278 tests passed
npm test (render pool, vitest.render...)  -> EXIT 0   179 files / 1462 tests passed
FAILING TEST NAMES AT BASELINE: (none)
```

After the migration, on the candidate tree:

```
npx tsc --noEmit                          -> TSC_EXIT=0
npm test (node pool)                      -> 199 files / 3320 tests passed
npm test (render pool)                    -> 179 files / 1462 tests passed
TEST_EXIT=0
```

Delta: **+3 test files, +42 tests, 0 failures.** The +3/+42 are exactly this
packet's three guard files (20 + 2 + 20). The failing-name set is empty before
and after, so zero new red names.

---

## 5. Commands run

| Command | Result |
| --- | --- |
| `source scripts/env.sh; npx tsc --noEmit` (baseline) | exit 0 |
| `source scripts/env.sh; npm test` (baseline) | exit 0 — 3278 + 1462 passed |
| guards vs. pre-migration tree | **2 files failed, 18/40 tests failed** (necessity gate — evidence file above) |
| `npx vitest run src/storageDispatch.test.ts src/storageAuthorityRatchet.test.ts src/storageDispatchRoutes.test.ts` (post) | 3 files / 42 tests passed |
| `npx tsc --noEmit` (post) | exit 0 |
| `npm test` (post) | exit 0 — 3320 + 1462 passed |

No E2E was run or edited (`scripts/e2e-*.mjs` untouched), per the dossier: B1 is
frontend logic, and the move journeys stay green at integration.

---

## 6. Anything not behaviour-preserving

One item, recorded for completeness; it is a scheduling detail, not a semantic
change.

**Microtask ordering at the front door.** `dispatchCrossPageMove` and its
siblings are `async`, so the *resolution* of a dispatched call is one or two
microtask ticks later than the equivalent inline branch was. The arm bodies
themselves still begin executing synchronously — the dispatcher body contains no
`await` before invoking an arm, and the admission is read on the synchronous
path. Same-page operations are entirely unaffected (the dispatcher is not called
at all: `moveBlocksRelative` returns `applyRelativeMove()` directly, which
`storageDispatchRoutes.test.ts` asserts). Nothing in these paths observes the
number of microtasks between the mutation and the caller's continuation; the full
suite, including the move/undo/persistence tests, is green.

Everything else is arm-for-arm verbatim, and deliberately so:

- Managed's **single-source refusal** in `moveBlocksRelative` is preserved,
  including its position relative to the plan rebuild after
  `prepareCrossPageSources` — the Managed arm computes `movedRaw` *before* the
  refusal check, the Direct arm computes it *after* the plan rebuild, matching
  the original order. (`movedRaw`/`affectedPages` computation is pure, so
  hoisting it into a closure changes nothing observable.)
- Direct's preflush → in-memory mutation → **destination-first** save, then the
  N sources, is untouched; in `carry.ts` `persist()` is byte-identical.
- carry's **missing managed arm** is preserved: `dispatchCarryPersist` records the
  route and still runs the Direct arm under every admission. It is asserted in
  both guard files precisely so B2 has to delete those assertions deliberately
  instead of rediscovering the gap.
- The four copies of the refusal toast collapsed into one constant with identical
  text and identical `"error"` severity; the ratchet pins it to one file.

---

## 7. Contract delta

**Where the dispatcher front door is documented: the module header comment of
`src/storageDispatch.ts`** — the prompt-channel deliverable for this packet
(PREVENTION.md's *prompt* disposition; the *shape* disposition is the arms-record
signature that makes a second branch awkward to write, and the *guard*
disposition is the three tests).

The header states, in order: the I-6 rule in full ("authority is decided once by
the native slot, published as a value; a semantic storage operation states its
intent here and hands over its arms"); the concrete history that motivates it
(the four cross-page-move forks of audit UI-3, plus `carry.ts` which had no branch
at all); what remains legitimate elsewhere (value capture and the I-20 re-check,
with a pointer to the census); the **three guard tests by name**, each with the
claim it holds, framed as "if you are about to add a mode branch to a call site,
these are the tests that will stop you, and this file is the exemplar to imitate";
and finally that B1 is behaviour-preserving and which asymmetries are recorded
rather than fixed.

No existing contract doc under `docs/` covers frontend storage dispatch —
`docs/storage-sync-contract.md` describes the `tine-storage` layout and state
machines, not the frontend's authority routing — and B1 adds no layout, state
machine, schema version, or native public surface. So there is no same-commit
contract-doc update to make; the module header is the contract for this surface,
and it is enforced by `storageAuthorityRatchet.test.ts` rather than left as prose.
When B2/B3 give the front door real request/response types, that is the point at
which this surface earns an entry under `docs/contracts/`.
