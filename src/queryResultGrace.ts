import { createEffect, createSignal, on, onCleanup, untrack, type Accessor } from "solid-js";

/** Presentation-only delay after a user checks or unchecks a live query row. */
export const QUERY_COMPLETION_GRACE_MS = 2_000;

interface CompletionInteraction {
  blockId: string;
  occurredAt: number;
  sequence: number;
}

let completionSequence = 0;
const [completionInteraction, setCompletionInteraction] = createSignal<CompletionInteraction>();

/** Announce the ordinary task interaction; persistence and projection work are
 * unchanged. Query consumers use this only to schedule their next membership
 * read while the interacted row is still displayed. */
export function noteQueryCompletionInteraction(blockId: string, occurredAt = Date.now()): void {
  setCompletionInteraction({ blockId, occurredAt, sequence: ++completionSequence });
}

export interface QueryRefreshGraceOptions {
  revision: Accessor<number>;
  /** Query/graph/presentation identity, excluding the ordinary data revision. */
  identity: Accessor<string | undefined>;
  containsDisplayedBlock: (blockId: string) => boolean;
  now?: () => number;
  graceMs?: number;
}

/** Publish ordinary data revisions immediately except while this consumer owns
 * a completion grace. Only the newest demanded revision is retained. A query,
 * graph, view, or binding identity change cancels the hold synchronously. */
export function createQueryRefreshRevision(options: QueryRefreshGraceOptions): Accessor<number> {
  const now = options.now ?? Date.now;
  const graceMs = options.graceMs ?? QUERY_COMPLETION_GRACE_MS;
  const [published, setPublished] = createSignal(options.revision());
  let latest = options.revision();
  let heldIdentity: string | undefined;
  let heldUntil = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const clearTimer = () => {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  };
  const clearHold = () => {
    clearTimer();
    heldIdentity = undefined;
    heldUntil = 0;
  };
  const release = () => {
    clearHold();
    setPublished(latest);
  };
  const scheduleRelease = () => {
    clearTimer();
    const remaining = heldUntil - now();
    if (remaining <= 0) {
      release();
      return;
    }
    timer = setTimeout(release, remaining);
  };

  createEffect(on(options.identity, (identity, previous) => {
    latest = untrack(options.revision);
    if (previous !== undefined && identity !== previous) clearHold();
    setPublished(latest);
  }));

  createEffect(on(completionInteraction, (interaction) => {
    if (!interaction) return;
    const identity = untrack(options.identity);
    if (!identity || !untrack(() => options.containsDisplayedBlock(interaction.blockId))) return;
    heldIdentity = identity;
    heldUntil = Math.max(heldUntil, interaction.occurredAt + graceMs);
    scheduleRelease();
  }));

  createEffect(on(options.revision, (revision) => {
    latest = revision;
    const identity = untrack(options.identity);
    if (heldIdentity === identity && now() < heldUntil) {
      scheduleRelease();
      return;
    }
    clearHold();
    setPublished(revision);
  }));

  onCleanup(clearTimer);
  return published;
}
