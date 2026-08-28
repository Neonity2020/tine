import { createSignal } from "solid-js";

import { backend } from "./backend";
import type { SyncAbsenceSweepEvent } from "./types";
import { pushToast } from "./ui";

export const [absenceSweeps, setAbsenceSweeps] = createSignal<SyncAbsenceSweepEvent[]>([]);
export const [absenceSweepPanelOpen, setAbsenceSweepPanelOpen] = createSignal(false);

const announced = new Set<string>();

export function openAbsenceSweepPanel(): void {
  setAbsenceSweepPanelOpen(true);
}

export function closeAbsenceSweepPanel(): void {
  setAbsenceSweepPanelOpen(false);
}

export function clearAbsenceSweeps(): void {
  setAbsenceSweeps([]);
  setAbsenceSweepPanelOpen(false);
}

// `undefined` = never bound; distinct from `null` (bound with no generation).
let boundGeneration: number | null | undefined;

/**
 * Reset the sweep surface only when the graph binding actually changed.
 * Durable sweep records outlive runtime-status churn: a status event or an
 * authority flap on the SAME binding must not wipe the list or close a panel
 * the user is looking at (restoring a sweep emits exactly such events, and
 * clearing here closed the recovery panel at the moment Restore completed).
 */
export function rebindAbsenceSweepScope(bindingGeneration: number | null): void {
  if (boundGeneration === bindingGeneration) return;
  boundGeneration = bindingGeneration;
  clearAbsenceSweeps();
}

export function ingestAbsenceSweepEvent(
  sweep: SyncAbsenceSweepEvent,
  options: { announce?: boolean } = {},
): void {
  setAbsenceSweeps((current) => {
    const next = current.filter((candidate) => candidate.sweep_id !== sweep.sweep_id);
    next.push(sweep);
    return next.sort((left, right) => right.opened_at_unix_ms - left.opened_at_unix_ms);
  });

  const announcement = `${sweep.sweep_id}:${sweep.tier}`;
  if (!options.announce || sweep.disposed_at_unix_ms !== null || announced.has(announcement)) return;
  announced.add(announcement);
  const message = sweep.tier === "tier3"
    ? `${sweep.absence_count} pages were deleted together. Review this group deletion.`
    : `${sweep.absence_count} deleted pages need your review.`;
  pushToast(message, "warn", {
    sticky: true,
    action: { label: "Review", run: openAbsenceSweepPanel },
  });
}

export async function refreshAbsenceSweeps(options: { announce?: boolean } = {}): Promise<void> {
  const sweeps = await backend().listAbsenceSweeps();
  for (const sweep of sweeps) ingestAbsenceSweepEvent(sweep, options);
}

async function runAction(action: () => Promise<unknown>, failure: string): Promise<void> {
  try {
    await action();
    await refreshAbsenceSweeps();
  } catch (error) {
    pushToast(`${failure}: ${error instanceof Error ? error.message : String(error)}`, "error", {
      sticky: true,
    });
  }
}

export function restoreAbsenceSweep(sweepId: string): Promise<void> {
  return runAction(
    () => backend().restoreAbsenceSweep(sweepId),
    "Restore could not finish",
  );
}

export function reapplyAbsenceSweep(sweepId: string): Promise<void> {
  return runAction(
    () => backend().reapplyAbsenceSweep(sweepId),
    "The deletion could not be re-applied",
  );
}

export function keepAbsenceSweepDeletion(sweepId: string): Promise<void> {
  return runAction(
    () => backend().keepAbsenceSweepDeletion(sweepId),
    "The deletion decision could not be recorded",
  );
}

export function resetAbsenceSweepStateForTest(): void {
  setAbsenceSweeps([]);
  setAbsenceSweepPanelOpen(false);
  announced.clear();
  boundGeneration = undefined;
}
