// Concord P5 — the one user-visible conflict policy switch (Obsidian 1.9.7's
// idea, landscape survey U3).
//
// Tine's ONLY silent adoption of external bytes is L0 freshness: a page you have
// open, with nothing unsaved, changes on disk and Tine simply shows the new
// content. That is the right default — it is what VS Code and IntelliJ do, and
// the alternative is a prompt for every keystroke a co-editing tool makes.
//
// It is not right for everyone. If you edit the same graph from a second machine
// through a sync tool, "the page under my eyes just changed" is exactly the
// moment you want told about rather than shown. With ALWAYS ASK on, such a
// change is HELD: the page keeps what you were reading and offers Reload /
// Keep mine.
//
// What the switch deliberately does NOT touch: every existing safety path. A
// dirty page still gets its divergence proof, a mid-edit page still defers
// through the P1 replay machinery, and a conflicted save is still refused. This
// only converts the silent case into an asked one — nothing that used to ask
// stops asking.
//
// Holding is frontend-only by design: the backend cache has already adopted the
// bytes, which is what makes "Keep mine" honest — the next save is refused by
// the base_rev guard and raises the ordinary conflict banner, i.e. exactly
// Obsidian's "create a conflict to review".

import { createSignal } from "solid-js";
import { backend } from "./backend";
import type { GraphChange } from "./backend";

const KEY = "concord_always_ask";

const [alwaysAsk, setAlwaysAskSig] = createSignal(false);

/** Reactive: hold external changes for review instead of applying them silently. */
export const conflictPolicyAlwaysAsk = alwaysAsk;

export function setConflictPolicyAlwaysAsk(on: boolean): void {
  setAlwaysAskSig(on);
  if (!on) clearHeldExternalChanges(); // turning it off releases the queue's grip
  void backend().setAppBool(KEY, on).catch(() => {});
}

/** Set the policy WITHOUT persisting it — tests only. */
export function setConflictPolicyAlwaysAskForTest(on: boolean): void {
  setAlwaysAskSig(on);
}

/** Load the persisted preference at startup. Default OFF (current behavior). */
export async function initConflictPolicy(): Promise<void> {
  try {
    setAlwaysAskSig(await backend().getAppBool(KEY, false));
  } catch {
    /* default off */
  }
}

// --- held external changes ---

export interface HeldExternalChange {
  change: GraphChange;
  binding: number;
}

const [held, setHeld] = createSignal<Record<string, HeldExternalChange>>({});

/** The change waiting for this page's owner to decide, if any. */
export function heldExternalChangeFor(name: string | undefined): HeldExternalChange | undefined {
  return name ? held()[name] : undefined;
}

/** How many pages are waiting on a decision (a calm count, never a modal).
 *  A plain accessor, not a module-scope memo: a memo created outside a root is
 *  never disposed, and this derivation is one Object.keys. */
export function heldExternalChangeCount(): number {
  return Object.keys(held()).length;
}

/** Record a change the policy says must be asked about. Latest observation wins
 *  — applying refetches the DTO, so only the newest change's shape matters. */
export function holdExternalChange(change: GraphChange, binding: number): void {
  setHeld((current) => ({ ...current, [change.name]: { change, binding } }));
}

function take(name: string): HeldExternalChange | undefined {
  const pending = held()[name];
  if (!pending) return undefined;
  setHeld((current) => {
    const next = { ...current };
    delete next[name];
    return next;
  });
  return pending;
}

export function clearHeldExternalChanges(): void {
  setHeld({});
}

let applier: ((change: GraphChange, binding: number) => void) | null = null;

/** Wired once by the watcher handler's module, like P1's replay handler: the
 *  bar must go through the SAME external-change path, not a private reload. */
export function installHeldExternalChangeApplier(
  handler: (change: GraphChange, binding: number) => void
): void {
  applier = handler;
}

/** "Reload from disk": re-dispatch through the ordinary handler, with the policy
 *  bypassed for this one change so it is not held again. Every other gate —
 *  disposition, editor leases, deferred replay — still applies. */
export function applyHeldExternalChange(name: string): void {
  const pending = take(name);
  if (pending) applier?.(pending.change, pending.binding);
}

/** "Keep mine": drop the record. Nothing is written; the page keeps showing what
 *  the user was reading, and the next save meets the base_rev guard and raises
 *  the ordinary conflict banner. */
export function dismissHeldExternalChange(name: string): void {
  take(name);
}
