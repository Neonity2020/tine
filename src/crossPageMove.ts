// The ONE front door for a Direct Files cross-page move.
//
// **What a cross-page move is.** Blocks leave N source pages and arrive on one
// destination page, so it writes N+1 files. It is not atomic and cannot be made
// atomic (two `rename()` calls never are), so it is made CONVERGENT instead:
// every participant is named in a durable record before the first write, and the
// next open completes the move or rolls it back. See
// `docs/contracts/direct-move-recovery.md`.
//
// **Why one door (I-3, I-12).** Six shapes produce this write pattern —
// `moveBlocksRelative` (drag), `moveBlockFeedNow` and `moveSelectionItems` (the
// journal-feed day boundary), `carryUnfinished`, the test-only `moveBlock`, and
// undo/redo of any of them. Each used to arrange admission, pre-flush,
// revalidation and persistence for itself, and the copies drifted: the K6a
// census (2026-09-15) found five distinct defects that are all the SAME defect
// seen from five call sites.
//
//   - **H1** carry never held its sources behind the destination, so a
//     conflicted destination plus any later edit to a source day wrote the
//     carried tasks out of the only file that still had them.
//   - **H2** undo/redo restored both pages and saved them in PARALLEL, with no
//     barrier and no record — so undoing a move could write the losing page
//     while the gaining page's save was refused, leaving the blocks in neither
//     file.
//   - **H3** `moveSelectionItems` captured its plan BEFORE an await and applied
//     it after, so two fast key repeats could each append the same roots and
//     leave a block in the target day twice (duplicate content, duplicate `id::`).
//   - **H4** a failed pre-flush toasted at three call sites and was silent at two.
//   - **H5** no call site re-checked the graph binding across its await.
//
// Only `moveBlocksRelative` had the re-plan-and-compare that closes H3/H5, and
// only the non-carry paths had the barrier that closes H1. This module
// generalizes the good half of each and deletes the rest, so a sixth shape
// cannot reintroduce a sixth variant.
//
// **The shape of the door.** A caller states its intent and hands over two
// closures; it never arranges persistence:
//
//   1. admission is read ONCE, by `dispatchCrossPageMove` (I-6);
//   2. every source is pre-flushed while it still holds the blocks;
//   3. the binding is re-checked and `plan()` is RE-RUN and compared across the
//      await — a plan computed before an await is a statement about a document
//      that may no longer exist (H3, H5);
//   4. `apply(plan)` runs, still owned by the editor: `pushUndo`, one `produce`,
//      selection and caret;
//   5. the door then runs the barrier and destination-first persistence inside
//      the record bracket.
//
// **Guards that hold this shape** — if you are about to hand-roll a cross-page
// move, these are the tests that will stop you, and this file is the exemplar:
//   - `src/directMoveOrder.test.ts`       — the four durable steps in contract
//                                           order, for every shape including
//                                           undo/redo and carry.
//   - `src/persistenceMoveBarrier.test.ts`— the source barrier itself.
//   - `src/storageDispatchRoutes.test.ts` — every real path routes through the
//                                           dispatcher (dispatch counters).

import { backend } from "./backend";
import { doc, markDirty } from "./store/doc";
import { projectPageDto } from "./store/mutationPlans";
import {
  flushPage,
  graphBinding,
  holdSourcesForDest,
  isDirty,
  isSaving,
} from "./persistence";
import { dispatchCarry, dispatchCrossPageMove } from "./storageDispatch";
import { pushToast } from "./ui";
import type { PageDto } from "./types";

/**
 * The ONE message for a cross-page move that cannot start because a source page
 * has changes that could not be flushed (an unresolved conflict).
 *
 * It was written out at three call sites and omitted entirely at two more, so
 * the same failure was loud for a drag and silent for a keyboard move (H4). The
 * page name is deliberately NOT interpolated: an N-source carry has no single
 * name to blame, and the user's next action — resolve the banner — is the same
 * either way.
 */
export const CROSS_PAGE_MOVE_BLOCKED_TOAST =
  "Couldn't move — a source page has unsaved changes that need resolving first.";

/** The pages a cross-page move touches, in storage terms. */
export interface CrossPageMoveIntent {
  /** Every page LOSING blocks. Saved LAST, and held until the destination lands. */
  readonly sourcePages: readonly string[];
  /** The page GAINING the blocks. Saved FIRST. */
  readonly destinationPage: string;
  /** The subtree roots being moved, in document order. */
  readonly roots: readonly string[];
}

/**
 * What the door did.
 *
 * Two results, because the callers genuinely want different ones: a drag must
 * not await disk I/O, so it reads `applied` and returns; carry must know whether
 * the move is durable before it reloads the journals feed (a reload re-reads the
 * files and would drop an unsaved move from memory), so it awaits `landed`.
 *
 * `landed` never rejects.
 */
export interface CrossPageMoveOutcome {
  /** The move was admitted, still valid after the await, and applied to memory. */
  readonly applied: boolean;
  /** Resolves once every participant is durably terminal and the record retired. */
  readonly landed: Promise<boolean>;
}

const NOT_APPLIED: CrossPageMoveOutcome = { applied: false, landed: Promise.resolve(false) };

/** The cross-page sources of an intent: duplicates and the destination removed. */
function crossSourcesOf(intent: CrossPageMoveIntent): string[] {
  return [...new Set(intent.sourcePages)].filter((name) => name !== intent.destinationPage);
}

function sameStrings(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((value, index) => value === b[index]);
}

/**
 * Is the re-run plan still the SAME move?
 *
 * A concurrent reparent, a second key repeat, or an external reload can change
 * the document while the pre-flush is awaiting real IPC. Continuing with the
 * pre-await plan is what duplicated roots in H3. A changed plan is a safe abort,
 * not permission to mutate a source we may no longer have flushed.
 */
function sameIntent(a: CrossPageMoveIntent, b: CrossPageMoveIntent): boolean {
  return a.destinationPage === b.destinationPage
    && sameStrings(a.roots, b.roots)
    && sameStrings(crossSourcesOf(a), crossSourcesOf(b));
}

export interface CrossPageMoveRequest<P> {
  /**
   * Compute the move from the CURRENT document, or null if it is not possible.
   *
   * Called twice: once to state the intent, and again after the pre-flush await.
   * It must be pure with respect to the document — it may read, never mutate.
   */
  readonly plan: () => P | null;
  /** The pages and roots this plan touches. */
  readonly intent: (plan: P) => CrossPageMoveIntent;
  /**
   * Apply the move to memory: `pushUndo`, one `produce`, selection and caret.
   * Runs only after the plan has been revalidated. Return false to abort.
   */
  readonly apply: (plan: P) => boolean;
  /**
   * Optional extra equality, for a plan carrying more than the intent shows.
   * `moveBlocksRelative` uses it to compare each root's own source page, which a
   * de-duplicated source LIST cannot distinguish.
   */
  readonly unchanged?: (before: P, after: P) => boolean;
  /**
   * Wording for the pre-flush refusal, when the operation is not called "move".
   *
   * The ONE error policy (H4) is that this refusal is always raised — the defect
   * was that two of five call sites failed silently, not that the copy varied.
   * Carry names itself here so a user who pressed "Carry" is not told a move
   * failed; everything else takes the default.
   */
  readonly blockedToast?: string;
  /**
   * Which semantic operation this is, for the authority dispatcher (I-6).
   *
   * Carry is the same STORAGE shape — N sources, one destination, N+1 files —
   * but a different user intent, and `storageDispatch.ts` routes and counts the
   * two separately. Dispatching a carry as a move would silently retire the
   * `carry` route its guard tests assert. Defaults to `cross-page-move`.
   */
  readonly operation?: "cross-page-move" | "carry";
}

/**
 * Route one cross-page move.
 *
 * The caller owns WHAT moves; this owns that it is admitted once, revalidated
 * across the await, persisted destination-first behind the source barrier, and
 * recoverable after a crash at any point.
 */
export async function requestCrossPageMove<P>(
  request: CrossPageMoveRequest<P>,
): Promise<CrossPageMoveOutcome> {
  const first = request.plan();
  if (first === null) return NOT_APPLIED;
  const intent = request.intent(first);
  const sources = crossSourcesOf(intent);
  // A degenerate "cross-page" move is one ordinary save, already atomic. Callers
  // route their same-page arm themselves; reaching here with no source is a
  // no-op rather than an empty record.
  if (!sources.length) return NOT_APPLIED;

  const arms = {
    // The dispatcher already raised the shared refusal toast.
    unavailable: () => NOT_APPLIED,
    direct: async (): Promise<CrossPageMoveOutcome> => {
      // Capture the binding BEFORE the await and re-check it after (H5). The
      // continuation of a move belongs to the graph it started in; a graph
      // switch mid-flight must abandon it, not apply it to same-named journal
      // days of the new graph.
      const binding = graphBinding();
      if (!(await prepareCrossPageSources(sources))) {
        pushToast(request.blockedToast ?? CROSS_PAGE_MOVE_BLOCKED_TOAST, "error");
        return NOT_APPLIED;
      }
      if (binding !== graphBinding()) return NOT_APPLIED;

      // Re-plan and compare (H3). See `sameIntent`.
      const again = request.plan();
      if (again === null) return NOT_APPLIED;
      if (!sameIntent(request.intent(again), intent)) return NOT_APPLIED;
      if (request.unchanged && !request.unchanged(first, again)) return NOT_APPLIED;

      if (!request.apply(again)) return NOT_APPLIED;
      return { applied: true, landed: persistCrossPage(intent.destinationPage, sources) };
    },
  };

  // Same storage shape, two user intents: `storageDispatch.ts` routes and counts
  // them separately, and a carry's request is the carry request — not a move's,
  // which would carry a meaningless `roots`.
  return request.operation === "carry"
    ? dispatchCarry<CrossPageMoveOutcome>(
        { destinationPage: intent.destinationPage, sourcePages: sources },
        arms,
      )
    : dispatchCrossPageMove<CrossPageMoveOutcome>(
        { sourcePages: sources, destinationPage: intent.destinationPage, roots: intent.roots },
        arms,
      );
}

// ---------------------------------------------------------------------------
// Persistence: the barrier, the destination-first order, and the record bracket
// ---------------------------------------------------------------------------

/**
 * Before a cross-page move mutates memory, durably flush every SOURCE page while
 * it still contains the blocks.
 *
 * In-scope scenario (I-8): a save that was ALREADY pending or in flight for a
 * source — from an earlier, unrelated edit — can fire right after the in-memory
 * removal and write the post-removal state to disk before the destination is
 * saved. That is a removal-only, data-losing state that destination-first
 * ordering alone cannot prevent, because that save was scheduled before the move
 * existed. Returns false if any source cannot be flushed (an unresolved
 * conflict); the caller MUST abort. Clean sources flush as instant no-ops.
 */
export async function prepareCrossPageSources(sources: readonly string[]): Promise<boolean> {
  for (const s of new Set(sources)) {
    if ((isDirty(s) || isSaving(s)) && !(await flushPage(s))) return false;
  }
  return true;
}

/**
 * Persist a cross-page move so the ADDITION side (`dest`) lands on disk BEFORE
 * any REMOVAL side (`sources`).
 *
 * If dest fails to save (an external conflict, a disk error), the sources are
 * NOT written, so disk is never left with the blocks removed from their source
 * but never written to their destination — the data-losing state. The barrier
 * (`holdSourcesForDest`, audit C#1) extends that guarantee to saves this move
 * did not initiate: an unrelated edit to a source during the dest-write window
 * cannot write its post-removal state either.
 *
 * Resolves to whether every participant landed durably. Never rejects.
 */
export function persistCrossPage(dest: string, sources: readonly string[]): Promise<boolean> {
  const held = [...new Set(sources)].filter((name) => name !== dest);
  holdSourcesForDest(dest, held);
  // Marked dirty SYNCHRONOUSLY, before the record round-trip: `flushAll` (graph
  // switch, window close) must see this page as unsaved from the instant the
  // move mutates memory. The debounce can therefore publish the destination
  // before the record exists — a window that converges anyway, because the
  // record then observes an already-terminal destination and recovery carries
  // the move FORWARD, which is the safe direction (contract §3, and
  // `record_composed_after_the_destination_landed_still_completes_forward`).
  // Do NOT "fix" this by awaiting `begin`.
  markDirty(dest);
  return trackDirectMove(withDirectMoveRecord(dest, held, async () => {
    if (!(await flushPage(dest))) return false;
    // `releaseSourcesFor(dest)` has already re-dirtied and rescheduled the held
    // sources; flushing them here only awaits that work (a clean page is an
    // instant no-op) so the record is retired on a durably terminal graph.
    const results = await Promise.all(held.map((name) => flushPage(name)));
    return results.every(Boolean);
  }));
}

/**
 * Open the durable recovery record for a Direct cross-page move, run the
 * choreography inside it, and retire the record once every participant is
 * durably terminal.
 *
 * **Why this exists (I-3, I-2).** Ordering keeps the damage one-sided — the
 * addition always lands before any removal — but the process can die between two
 * of those writes, and the graph is then left with the blocks in the destination
 * AND still in a source, with nothing on disk saying so. The record makes the
 * move CONVERGENT: composed before the first write, it lets the next open
 * complete the move or roll it back.
 *
 * The durable-step order it emits — record, destination, each source, retire —
 * is the order `crate::direct_move_recovery::direct_move_durable_steps` names
 * and the crash matrix cuts between; `src/directMoveOrder.test.ts` pins it.
 *
 * A record is never required: `null` (a degenerate move, a firewalled DTO, an
 * unavailable app-private root, or a binding the native side refuses) simply
 * leaves the move as convergent as it was before the record existed. Refusing to
 * move a page because device-private state is unavailable would be an
 * availability bug, not hardening (G2).
 */
export async function withDirectMoveRecord(
  destinationPage: string,
  sourcePages: readonly string[],
  choreography: () => Promise<boolean>,
): Promise<boolean> {
  const moveId = await openDirectMoveRecord(destinationPage, sourcePages);
  const landed = await choreography();
  if (moveId && landed) {
    try {
      await backend().finishDirectCrossPageMove(moveId);
    } catch {
      // A record we could not retire is not a failure: the next open sees every
      // participant already terminal and retires it without writing anything.
    }
  }
  return landed;
}

/** The page DTO the record names a participant by. */
function pageDto(pageName: string): PageDto | null {
  return projectPageDto(doc.pages.find((page) => page.name === pageName), doc.byId, true);
}

async function openDirectMoveRecord(
  destinationPage: string,
  sourcePages: readonly string[],
): Promise<string | null> {
  const sources = [...new Set(sourcePages)].filter((name) => name !== destinationPage);
  if (!sources.length) return null; // same-page/degenerate: one ordinary save, already safe
  const destination = pageDto(destinationPage);
  if (!destination) return null;
  const sourceDtos: PageDto[] = [];
  for (const name of sources) {
    const dto = pageDto(name);
    if (!dto) return null;
    sourceDtos.push(dto);
  }
  try {
    return await backend().beginDirectCrossPageMove(destination, sourceDtos);
  } catch {
    return null;
  }
}

const inFlightDirectMoves = new Set<Promise<unknown>>();

/**
 * Test seam. The cross-page move bracket is deliberately fire-and-forget for a
 * drag — a drag must not await disk I/O — so a test that asserts on what the
 * move wrote needs a way to wait for it. Production never calls this; `dirty`
 * remains the only thing `flushAll` consults.
 */
export async function settleDirectMovesForTest(): Promise<void> {
  while (inFlightDirectMoves.size) await Promise.all([...inFlightDirectMoves]);
}

function trackDirectMove(work: Promise<boolean>): Promise<boolean> {
  const settled = work.catch(() => false);
  const tracked: Promise<unknown> = settled.finally(() => {
    inFlightDirectMoves.delete(tracked);
  });
  inFlightDirectMoves.add(tracked);
  return settled;
}
