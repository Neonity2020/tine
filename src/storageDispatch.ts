// The storage-authority front door for SEMANTIC storage operations.
//
// **The rule (I-6 — storage authority is selected in one place, then flows as a
// value).** The decision "which authority governs this graph" is made once, by
// the native slot, and published as `applicationPageAdmission`. A semantic
// storage operation — a cross-page move, a dropped-file insertion, a carry —
// must NOT re-implement the branch-and-dispatch choreography at its call site.
// It states its intent here and hands over the arms; THIS module reads the
// admission snapshot exactly once per operation, maps it to an exhaustive
// route, and invokes exactly one arm. Call sites never see a second authority
// derivation, and the arms cannot drift apart the way four hand-written copies
// of the same branch did (audit UI-3: `moveBlock`, `moveBlocksRelative`,
// `moveBlockFeedNow`, `moveSelectionItems` — plus `carry.ts`, which had no
// branch at all).
//
// **What is still legitimate elsewhere.** Reading the admission *value* to
// stamp it into a plan/fence, and re-checking `binding_generation` in an async
// continuation before landing a result (I-20), are correct — those are not
// authority decisions. The census in
// `specs/campaigns/2026-09-invariant-sweep/` (packet B1 receipt) lists every
// such reader; `src/storageAuthorityRatchet.test.ts` pins that list and fails
// on a new one.
//
// **Guards that hold this shape** — if you are about to add a mode branch to a
// call site, these are the tests that will stop you, and this file is the
// exemplar to imitate instead:
//   - `src/storageDispatch.test.ts`          — route-level capability: under a
//                                              managed admission the Direct arm
//                                              is unreachable; every admission
//                                              runs exactly one arm.
//   - `src/storageDispatchRoutes.test.ts`    — the real store/filedrop/carry
//                                              paths go through this module
//                                              (dispatch counters) and a managed
//                                              binding never reaches Direct
//                                              persistence (`savePage`/dirty).
//   - `src/storageAuthorityRatchet.test.ts`  — source-scan ratchet over
//                                              `applicationPageAdmission`
//                                              readers.
//
// **B1 is behaviour-preserving.** Every arm below is the arm that ran before,
// verbatim; the asymmetries (Direct's N-source choreography, Managed's
// single-source refusal, carry's missing managed arm) are recorded here and
// changed by B2/B3, not by this module.

import { managedStorageRuntime } from "./managedStorageRuntime";
import { pushToast } from "./ui";
import type { ApplicationPageAdmission } from "./types";

/** The one admission shape that admits a native managed write. */
export type ManagedWritableAdmission = Extract<
  ApplicationPageAdmission,
  { authority: "managed_writable" }
>;

/**
 * The exhaustive authority route for a semantic storage operation.
 *
 * `unavailable` covers BOTH "no admission published yet" and
 * `managed_unavailable` — the two states in which a managed graph has no writer
 * and Direct persistence must not be reached. This partition is exhaustive over
 * `ApplicationPageAdmission`'s three variants plus `null`; do not add a fourth
 * route without adding the corresponding admission variant.
 */
export type StorageRoute = "managed" | "direct" | "unavailable";

/** The semantic operations that dispatch through this module. */
export type SemanticStorageOperation =
  | "cross-page-move"
  | "dropped-file-insertion"
  | "carry-persist";

export interface StorageRouteDecision {
  readonly route: StorageRoute;
  /** The admission the route was decided from, for the arm that needs its limits. */
  readonly admission: ApplicationPageAdmission | null;
}

export interface StorageDispatchCounters {
  managed: number;
  direct: number;
  unavailable: number;
}

/** The intent a semantic operation stated, plus the route it was given. */
export interface StorageDispatchRecord {
  readonly operation: SemanticStorageOperation;
  readonly route: StorageRoute;
  readonly request: unknown;
}

const OPERATIONS: readonly SemanticStorageOperation[] = [
  "cross-page-move",
  "dropped-file-insertion",
  "carry-persist",
];

function emptyCounters(): StorageDispatchCounters {
  return { managed: 0, direct: 0, unavailable: 0 };
}

const counters = new Map<SemanticStorageOperation, StorageDispatchCounters>(
  OPERATIONS.map((operation) => [operation, emptyCounters()]),
);

const lastDispatch = new Map<SemanticStorageOperation, StorageDispatchRecord>();

/**
 * Instrumentation for the packet's acceptance gate: a test asserts the
 * dispatcher route was actually exercised in the positive cases and the
 * refusal route in the negative ones, so "it compiles" cannot pass for
 * "it dispatches".
 */
export function storageDispatchCounters(
  operation: SemanticStorageOperation,
): Readonly<StorageDispatchCounters> {
  return { ...counters.get(operation)! };
}

export function resetStorageDispatchCounters(): void {
  for (const operation of OPERATIONS) counters.set(operation, emptyCounters());
  lastDispatch.clear();
}

/**
 * The most recent dispatch of `operation`: which route it took and the intent
 * the call site stated. The guard tests assert BOTH — a call site that routes
 * correctly but hands over the wrong pages/roots is still a defect, and this is
 * what makes the request object load-bearing rather than decorative.
 */
export function lastStorageDispatch(
  operation: SemanticStorageOperation,
): StorageDispatchRecord | null {
  return lastDispatch.get(operation) ?? null;
}

/**
 * The ONE read of `applicationPageAdmission` made for the purpose of deciding
 * where a semantic storage operation runs. Every other read in `src/` is either
 * a value capture or an I-20 staleness re-check, and is listed in the census.
 */
function readApplicationPageAdmission(): ApplicationPageAdmission | null {
  return managedStorageRuntime.snapshot().applicationPageAdmission;
}

/**
 * Decide, once, which arm of a semantic storage operation runs. Exported for
 * the guard tests and for an operation whose arms cannot be expressed as
 * callbacks; ordinary call sites use one of the `dispatch*` front doors below
 * so the arm invocation itself is single-sourced too.
 */
export function selectStorageRoute(
  operation: SemanticStorageOperation,
  request: unknown = null,
): StorageRouteDecision {
  const admission = readApplicationPageAdmission();
  const route: StorageRoute = !admission || admission.authority === "managed_unavailable"
    ? "unavailable"
    : admission.authority === "managed_writable"
      ? "managed"
      : "direct";
  counters.get(operation)![route] += 1;
  lastDispatch.set(operation, { operation, route, request });
  return { route, admission };
}

// ---------------------------------------------------------------------------
// Cross-page move
// ---------------------------------------------------------------------------

/**
 * The one refusal message for a cross-page move with no writer. It was written
 * out identically at all four move call sites; it lives here now so the arms
 * cannot drift.
 */
export const CROSS_PAGE_MOVE_UNAVAILABLE_TOAST =
  "Can't move between pages while managed storage is changing state.";

/**
 * What the caller wants to happen, stated in domain terms. The dispatcher does
 * not act on it today beyond routing — it is the seed of the storage API's
 * cross-page-move request (spec B north star), so the next operation added here
 * states its intent the same way rather than smuggling it through a closure.
 */
export interface CrossPageMoveRequest {
  /** Every page losing blocks. Direct's choreography saves these LAST. */
  readonly sourcePages: readonly string[];
  /** The page gaining the blocks. Direct's choreography saves this FIRST. */
  readonly destinationPage: string;
  /** The subtree roots being moved, in document order. */
  readonly roots: readonly string[];
}

export interface CrossPageMoveArms<T> {
  /** Native managed request. Receives the admission the route was decided from. */
  readonly managed: (admission: ManagedWritableAdmission) => T | Promise<T>;
  /** Direct Files frontend choreography (preflush sources → mutate → dest-first save). */
  readonly direct: () => T | Promise<T>;
  /** No writer for this binding. The shared refusal toast has already been raised. */
  readonly unavailable: () => T | Promise<T>;
}

/**
 * Route one cross-page move. The admission is read ONCE, here; the arm that
 * does not match is never invoked, which is what
 * `src/storageDispatch.test.ts` proves at the route level.
 */
export async function dispatchCrossPageMove<T>(
  request: CrossPageMoveRequest,
  arms: CrossPageMoveArms<T>,
): Promise<T> {
  const decision = selectStorageRoute("cross-page-move", request);
  if (decision.route === "unavailable") {
    pushToast(CROSS_PAGE_MOVE_UNAVAILABLE_TOAST, "error");
    return arms.unavailable();
  }
  if (decision.route === "managed") {
    return arms.managed(decision.admission as ManagedWritableAdmission);
  }
  return arms.direct();
}

// ---------------------------------------------------------------------------
// Dropped-file insertion
// ---------------------------------------------------------------------------

export interface DroppedFileInsertionRequest {
  /** The block the dropped files are inserted after. */
  readonly afterId: string;
  /** Native-resolved absolute paths, in drop order. */
  readonly paths: readonly string[];
}

export interface DroppedFileInsertionArms<T> {
  readonly managed: (admission: ManagedWritableAdmission) => T | Promise<T>;
  readonly direct: () => T | Promise<T>;
  /**
   * No writer for this binding. Unlike a cross-page move this operation has no
   * shared refusal text — it reports through the managed bulk-insertion
   * preflight — so the arm owns its own message.
   */
  readonly unavailable: () => T | Promise<T>;
}

export async function dispatchDroppedFileInsertion<T>(
  request: DroppedFileInsertionRequest,
  arms: DroppedFileInsertionArms<T>,
): Promise<T> {
  const decision = selectStorageRoute("dropped-file-insertion", request);
  if (decision.route === "unavailable") return arms.unavailable();
  if (decision.route === "managed") {
    return arms.managed(decision.admission as ManagedWritableAdmission);
  }
  return arms.direct();
}

// ---------------------------------------------------------------------------
// Carry persistence
// ---------------------------------------------------------------------------

export interface CarryPersistRequest {
  /** Today's journal — the ADDITION side, saved first. */
  readonly destinationPage: string;
  /** The source days losing tasks — the REMOVAL side, saved only after today lands. */
  readonly sourcePages: readonly string[];
}

export interface CarryPersistArms<T> {
  /** Direct Files choreography: destination first, then the N sources. */
  readonly direct: () => T | Promise<T>;
}

/**
 * **KNOWN GAP, recorded not fixed (packet B2).** Carry is an N-source
 * cross-page move, but it has never had a managed arm: `carry.ts` ran the
 * Direct choreography under every admission, including a managed binding —
 * `INVARIANTS.md` names `src/carry.ts` as the I-6 specimen precisely because it
 * mentions neither "managed" nor "authority" and every audit that searched
 * outward from Managed Storage missed it.
 *
 * B1 is behaviour-preserving, so this front door still runs the Direct arm on
 * every route. What it adds is visibility: the decision is now made at ONE site
 * that says out loud which route it took, `storageDispatchCounters` records it,
 * and `src/storageDispatchRoutes.test.ts` asserts this exact asymmetry — so B2
 * has to delete that assertion deliberately when it gives carry its managed
 * arm, instead of the gap staying invisible for another campaign.
 */
export async function dispatchCarryPersist<T>(
  request: CarryPersistRequest,
  arms: CarryPersistArms<T>,
): Promise<T> {
  selectStorageRoute("carry-persist", request);
  return arms.direct();
}
