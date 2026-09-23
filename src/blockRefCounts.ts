import { createResource, createRoot } from "solid-js";
import { backend } from "./backend";
import { dataRev, graphEpoch } from "./ui";
import { waitForWarmCache } from "./warmCache";
import { blockExternalId } from "./store";
import { readOr } from "./resourceRead";
import { readLane } from "./readLane";

// One graph-wide `block uuid → referrer count` map, fetched once per graph and
// after each landed save, and shared by every block's count badge (Block.tsx). Reading
// `blockRefCount(id)` inside a tracking scope subscribes to the map, so all badges
// update together when the graph changes (a new ref is saved → graphEpoch bumps →
// refetch). Created in its own root: it lives for the app's lifetime by design.
const countsMap = createRoot(() => {
  // The warm wait covers the launch pass only; the lane covers a later one
  // (a repair), where every save used to leave one more read waiting (R11-09).
  const lane = readLane();
  const [countsResource] = createResource(
    () => ({ epoch: graphEpoch(), revision: dataRev() }),
    async ({ epoch, revision }) => {
      if (!(await waitForWarmCache(epoch))) return {};
      // A save during the pass has already asked again: each stale waiter
      // issuing its own whole-graph read at hand-over cost N+1 of them
      // (GH #543, audit R10-09). Solid drops a superseded fetch's value.
      if (epoch !== graphEpoch() || revision !== dataRev()) return {};
      return lane(
        () => epoch === graphEpoch() && revision === dataRev(),
        () => backend().getBlockRefCounts().catch(() => ({}) as Record<string, number>),
      );
    }
  );
  // Read by `blockRefCount` from inside Block.tsx's render; a throw here would
  // cost the whole page for a badge. No counts means no badges.
  return () => readOr(countsResource, undefined, "block reference counts");
});

/** Number of blocks that reference block `id` in the current graph (0 if none /
 *  not yet loaded). Reactive: re-runs when the map (re)loads. */
export function blockRefCount(id: string): number {
  const externalId = blockExternalId(id) ?? id;
  return countsMap()?.[externalId] ?? 0;
}
