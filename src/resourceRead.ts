import type { Resource } from "solid-js";
import { failureShape } from "./failureShape";

/**
 * Reading a Solid resource is this app's most common way to throw into render.
 *
 * `read()` (solid-js 1.9.13, dist/solid.js:318-322) does `if (err !== undefined
 * && !pr) throw err;`, and so does the `latest` getter (:390-397). So `data()`
 * on a rejected resource throws — and `runUpdates` discards the whole pending
 * effect queue before `handleError` runs, which is how one failed fetch used to
 * blank the window. `src/components/FailureBoundary.tsx` is the other half of
 * that story: it stops the rethrow and offers a Retry.
 *
 * What the boundary cannot fix is that the throw happens at all, at whatever
 * granularity the nearest seam has. And nearly every call site here had ALREADY
 * written what to do without a value — `hljs()` falls back to escaped plain
 * text, `katex()` shows the raw TeX, `icons() ?? {}`, `contentDays() ?? []`,
 * `preview()?.blocks ?? []`. None of those branches could ever run on a
 * rejection, because the read threw first. `readOr` is what makes them
 * reachable: it consults `resource.error`, which never throws, and hands back
 * the fallback the site already had.
 *
 * `what` is a fixed label chosen in source — never a page name, path, or query
 * — so the one-per-failure record stays content-free (I-5).
 *
 * This is NOT a way to swallow failures that matter. Where an empty value would
 * tell the user something untrue ("no references" when we merely could not load
 * them), the site must also SHOW the failure; `resource.error` is how, and
 * `ResourceFailure` is the shared row for it.
 */
export function readOr<T, F>(resource: Resource<T>, fallback: F, what: string): T | F {
  if (resource.error !== undefined) {
    report(what, resource.error);
    return fallback;
  }
  return resource() as T;
}

/**
 * `readOr` over `resource.latest` — the value that survives a refetch, so a
 * reload does not flash empty. Same failure handling; `latest` throws on a
 * rejection exactly as `read()` does (dist/solid.js:390-397).
 */
export function readLatestOr<T, F>(resource: Resource<T>, fallback: F, what: string): T | F {
  if (resource.error !== undefined) {
    report(what, resource.error);
    return fallback;
  }
  return resource.latest as T;
}

/** Failures already recorded, so re-rendering does not repeat one. */
const recorded = new Set<string>();

function report(what: string, error: unknown): void {
  const shape = failureShape(error);
  const key = `${what} ${shape.kind} ${shape.hash}`;
  if (recorded.has(key)) return;
  recorded.add(key);
  console.warn("tine.resource-failed", { what, ...shape });
}

/** Test seam: the dedupe set is module state and outlives a single test. */
export function resetResourceReportsForTests(): void {
  recorded.clear();
}
