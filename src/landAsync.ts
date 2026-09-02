import { graphBinding } from "./persistence";
import { graphEpoch, graphMeta, graphTransitioning, pushToast } from "./ui";

/**
 * Frozen graph ownership for asynchronous work.
 *
 * Graph identity is `graphBindingRev` (see persistence.ts:362), never the
 * render epoch. Capture before the first await, re-check after every await and
 * immediately before the first graph-scoped IPC. Opt into epoch comparison
 * only when the result itself is repaint-sensitive.
 */
export interface GraphScope {
  readonly root: string;
  readonly binding: number;
  readonly epoch: number;
}

export type Landed<T> =
  | { readonly landed: true; readonly value: T }
  | { readonly landed: false };

export function captureGraphScope(): GraphScope | null {
  const root = graphMeta()?.root;
  if (!root || graphTransitioning()) return null;
  return Object.freeze({ root, binding: graphBinding(), epoch: graphEpoch() });
}

export function isScopeCurrent(
  scope: GraphScope | null,
  opts: { repaintSensitive?: boolean } = {},
): scope is GraphScope {
  return scope !== null
    && !graphTransitioning()
    && graphMeta()?.root === scope.root
    && graphBinding() === scope.binding
    && (!opts.repaintSensitive || graphEpoch() === scope.epoch);
}

export async function landAsync<T>(
  scope: GraphScope | null,
  fn: () => Promise<T> | T,
): Promise<Landed<T>> {
  if (!isScopeCurrent(scope)) return { landed: false };
  const value = await fn();
  return isScopeCurrent(scope) ? { landed: true, value } : { landed: false };
}

export async function landAsyncOrToast<T>(
  scope: GraphScope | null,
  fn: () => Promise<T> | T,
  message: string,
): Promise<Landed<T>> {
  const result = await landAsync(scope, fn);
  if (!result.landed) pushToast(message, "warn");
  return result;
}
