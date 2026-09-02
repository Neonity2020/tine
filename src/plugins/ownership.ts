import { graphMeta, graphTransitioning } from "../ui";
import { graphBinding } from "../persistence";
import type { PluginBlockSnapshot } from "./protocol";

export interface PluginGraphOwner {
  readonly graphRoot: string;
  readonly generation: number;
}

export interface OwnedPluginBlockSnapshot {
  readonly owner: PluginGraphOwner;
  readonly block: PluginBlockSnapshot;
}

export function capturePluginGraphOwner(): PluginGraphOwner | null {
  const root = graphMeta()?.root;
  if (!root || graphTransitioning()) return null;
  return Object.freeze({ graphRoot: root, generation: graphBinding() });
}

export function isPluginGraphOwnerCurrent(owner: PluginGraphOwner): boolean {
  return !graphTransitioning()
    && graphMeta()?.root === owner.graphRoot
    && graphBinding() === owner.generation;
}

export function bindPluginBlockSnapshot(block: PluginBlockSnapshot): OwnedPluginBlockSnapshot | null {
  const owner = capturePluginGraphOwner();
  if (!owner || !isPluginGraphOwnerCurrent(owner)) return null;
  return Object.freeze({ owner, block: Object.freeze({ ...block }) });
}
