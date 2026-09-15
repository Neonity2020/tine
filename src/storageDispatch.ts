// The storage-authority front door for SEMANTIC storage operations.
//
// **The rule (I-6 — storage authority is selected in one place, then flows as a
// value).** The decision "which authority governs this graph" is made once, by
// the native slot, and published as `applicationPageAdmission` for the current
// graph binding. A semantic storage operation — a cross-page move, a
// dropped-file insertion, a bulk insertion, a carry — must NOT re-implement the
// branch-and-dispatch choreography at its call site. It states its intent here
// and hands over the arms; THIS module reads the admission snapshot exactly
// once per operation, maps it to an exhaustive route, and invokes exactly one
// arm. Call sites never see a second authority derivation, and the arms cannot
// drift apart the way four hand-written copies of the same branch did (audit
// UI-3: `moveBlock`, `moveBlocksRelative`, `moveBlockFeedNow`,
// `moveSelectionItems` — plus `carry.ts`, which had no branch at all).
//
// Direct Files is the only storage authority. The routes that remain are
// "direct" (a graph is bound and admitted) and "unavailable" (no binding has
// published an admission yet — the graph is still opening or switching).
//
// **What is still legitimate elsewhere.** Reading the admission *value* to
// stamp it into a plan/fence, and re-checking `binding_generation` in an async
// continuation before landing a result (I-20), are correct — those are not
// authority decisions. `src/storageAuthorityRatchet.test.ts` pins the list of
// such readers and fails on a new one.
//
// **Guards that hold this shape** — if you are about to add a mode branch to a
// call site, these are the tests that will stop you, and this file is the
// exemplar to imitate instead:
//   - `src/storageDispatch.test.ts`          — route-level capability: with no
//                                              admission the Direct arm is
//                                              unreachable; every admission
//                                              runs exactly one arm.
//   - `src/storageDispatchRoutes.test.ts`    — the real store/filedrop/carry
//                                              paths go through this module
//                                              (dispatch counters).
//   - `src/storageAuthorityRatchet.test.ts`  — exact source-scan ratchet over
//                                              every binding snapshot reader.

import { graphBindingRuntime } from "./graphBindingRuntime";
import { pushToast } from "./ui";
import type { ApplicationPageAdmission } from "./types";

/**
 * The exhaustive authority route for a semantic storage operation.
 *
 * `unavailable` means "no admission published yet" — the state in which the
 * graph has no writer and Direct persistence must not be reached.
 */
export type StorageRoute = "direct" | "unavailable";

/** The semantic operations that dispatch through this module. */
export type SemanticStorageOperation =
  | "cross-page-move"
  | "dropped-file-insertion"
  | "bulk-insertion"
  | "carry";

export interface StorageRouteDecision {
  readonly route: StorageRoute;
  /** The admission the route was decided from. */
  readonly admission: ApplicationPageAdmission | null;
}

export interface StorageDispatchCounters {
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
  "bulk-insertion",
  "carry",
];

function emptyCounters(): StorageDispatchCounters {
  return { direct: 0, unavailable: 0 };
}

const counters = new Map<SemanticStorageOperation, StorageDispatchCounters>(
  OPERATIONS.map((operation) => [operation, emptyCounters()]),
);

const lastDispatch = new Map<SemanticStorageOperation, StorageDispatchRecord>();

/**
 * Instrumentation for the guard tests: a test asserts the dispatcher route was
 * actually exercised in the positive cases and the refusal route in the
 * negative ones, so "it compiles" cannot pass for "it dispatches".
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
 * where a semantic storage operation runs. Every other authority-bearing read
 * in `src/` is a value capture or an I-20 staleness re-check.
 */
function readApplicationPageAdmission(): ApplicationPageAdmission | null {
  return graphBindingRuntime.snapshot().applicationPageAdmission;
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
  const route: StorageRoute = admission ? "direct" : "unavailable";
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
  "Can't move between pages while the graph is changing.";

/**
 * What the caller wants to happen, stated in domain terms. The dispatcher does
 * not act on it today beyond routing — it is the seed of a storage API's
 * cross-page-move request, so the next operation added here states its intent
 * the same way rather than smuggling it through a closure.
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
  readonly direct: () => T | Promise<T>;
  /**
   * No writer for this binding. Unlike a cross-page move this operation has no
   * shared refusal text, so the arm owns its own message.
   */
  readonly unavailable: () => T | Promise<T>;
}

export async function dispatchDroppedFileInsertion<T>(
  request: DroppedFileInsertionRequest,
  arms: DroppedFileInsertionArms<T>,
): Promise<T> {
  const decision = selectStorageRoute("dropped-file-insertion", request);
  if (decision.route === "unavailable") return arms.unavailable();
  return arms.direct();
}

// ---------------------------------------------------------------------------
// Bulk insertion
// ---------------------------------------------------------------------------

export interface BulkInsertionRequest {
  /** The block receiving the inserted outline, or null for an empty page. */
  readonly targetId: string | null;
  /** The page receiving an insertion when it does not yet have an anchor. */
  readonly targetPageName?: string;
}

export interface BulkInsertionArms<T> {
  /** Direct Files has no page limits. */
  readonly direct: () => T;
  /** No writer is currently available; the caller owns the existing toast. */
  readonly unavailable: () => T;
}

/**
 * Route one synchronous bulk-insertion admission.
 *
 * This front door deliberately stays synchronous: clipboard Cut grants and
 * editor completions must be admitted before any continuation or mutation can
 * run.
 */
export function dispatchBulkInsertion<T>(
  request: BulkInsertionRequest,
  arms: BulkInsertionArms<T>,
): T {
  const decision = selectStorageRoute("bulk-insertion", request);
  if (decision.route === "unavailable") return arms.unavailable();
  return arms.direct();
}

// ---------------------------------------------------------------------------
// Carry
// ---------------------------------------------------------------------------

export interface CarryRequest {
  /** Today's journal — the ADDITION side, saved first. */
  readonly destinationPage: string;
  /** The source days losing tasks — the REMOVAL side, saved only after today lands. */
  readonly sourcePages: readonly string[];
}

export interface CarryArms<T> {
  /** Direct Files: the in-memory carry plus its destination-first choreography. */
  readonly direct: () => T | Promise<T>;
  /** No writer for this binding. The shared refusal toast has already been raised. */
  readonly unavailable: () => T | Promise<T>;
}

/**
 * Route one carry.
 *
 * The decision is taken BEFORE the in-memory carry runs, not at persistence
 * time: refusing after `carryUnfinished` had already moved blocks would leave
 * the editor holding a mutation storage never accepted.
 */
export async function dispatchCarry<T>(
  request: CarryRequest,
  arms: CarryArms<T>,
): Promise<T> {
  const decision = selectStorageRoute("carry", request);
  if (decision.route === "unavailable") {
    pushToast(CROSS_PAGE_MOVE_UNAVAILABLE_TOAST, "error");
    return arms.unavailable();
  }
  return arms.direct();
}
