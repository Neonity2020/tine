import { createSignal, type Accessor } from "solid-js";
import type { ApplicationPageAdmission } from "./types";

/**
 * The native graph binding the frontend currently trusts.
 *
 * `load_graph` publishes a binding generation and the admission envelope for
 * that exact binding. Plans and fences stamp both into themselves so an async
 * continuation can prove, immediately before it lands a result, that the graph
 * it was planned against is still the graph that is bound (I-20). Nothing here
 * decides storage authority — Direct Files is the only authority — this is the
 * one place that remembers which binding the admission belongs to.
 */
export interface GraphBindingSnapshot {
  /** The only graph binding whose results this store may accept. */
  bindingGeneration: number | null;
  /** Native admission for this exact binding, never inferred. */
  applicationPageAdmission: ApplicationPageAdmission | null;
}

const initialSnapshot = (): GraphBindingSnapshot => ({
  bindingGeneration: null,
  applicationPageAdmission: null,
});

export function createGraphBindingRuntime(): {
  snapshot: Accessor<GraphBindingSnapshot>;
  bind: (bindingGeneration: number, applicationPageAdmission: ApplicationPageAdmission) => boolean;
  clear: () => void;
} {
  const [snapshot, setSnapshot] = createSignal<GraphBindingSnapshot>(initialSnapshot());
  const bind = (bindingGeneration: number, applicationPageAdmission: ApplicationPageAdmission) => {
    // An admission is only meaningful for the binding that produced it.
    if (applicationPageAdmission.binding_generation !== bindingGeneration) return false;
    setSnapshot({ bindingGeneration, applicationPageAdmission });
    return true;
  };
  const clear = () => setSnapshot(initialSnapshot());
  return { snapshot, bind, clear };
}

export const graphBindingRuntime = createGraphBindingRuntime();
