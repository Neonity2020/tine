import { createSignal, type Accessor } from "solid-js";
import type { StorageTransitionEvent, StorageTransitionKind } from "./types";
import { validStorageTransition } from "./startupRecovery";

export interface StorageTransitionSnapshot {
  latestOperationId: number;
  event: StorageTransitionEvent | null;
}

const relevant = new Set<StorageTransitionKind>([
  "activate_managed",
  "join_managed",
  "return_gracefully",
  "return_emergency",
]);

export function createStorageTransitionRuntime() {
  const [snapshot, setSnapshot] = createSignal<StorageTransitionSnapshot>({
    latestOperationId: 0,
    event: null,
  });

  const receive = (event: StorageTransitionEvent): boolean => {
    if (!validStorageTransition(event)) return false;
    const current = snapshot();
    if (event.operationId < current.latestOperationId) return false;
    setSnapshot({ latestOperationId: event.operationId, event });
    return true;
  };

  const active = (): StorageTransitionEvent | null => {
    const event = snapshot().event;
    return event && relevant.has(event.kind) && !event.terminal ? event : null;
  };

  const clear = () => setSnapshot({ latestOperationId: 0, event: null });

  return { snapshot: snapshot as Accessor<StorageTransitionSnapshot>, receive, active, clear };
}

export const storageTransitionRuntime = createStorageTransitionRuntime();
