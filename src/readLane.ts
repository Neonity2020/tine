// One index-backed read in flight per source (GH #543, audit R11-09).
//
// A fetch keyed on `dataRev` runs again on every landed save. While a
// whole-graph index pass is running, each index-backed command waits in the
// backend until the pass ends, so a surface that re-fetched per save left one
// waiting read per save, all answering in a burst afterwards. A lane runs its
// source's reads one at a time, and a read whose revision moved while it
// queued is skipped: a burst of saves costs one read in flight and one more
// for the newest revision. `dataRevReads.guard.test.ts` keeps every
// dataRev-keyed resource on a lane; `blockRefCounts.ts` is the exemplar.
//
// A lane belongs to one graph binding and render epoch. A read parked on the
// old graph is never waited on by a read for the new one, which would
// otherwise queue behind it for as long as it takes to answer (the same rule
// as `refreshAliases`, audit R10-09).
import { graphBinding } from "./persistence";
import { graphEpoch } from "./ui";

/** Run `read` after every earlier read on this lane, unless `current()` says
 *  its revision was superseded meanwhile (then `undefined`, which a Solid
 *  resource drops as the answer to a superseded fetch). */
export type ReadLane = <T>(current: () => boolean, read: () => Promise<T>) => Promise<T | undefined>;

export function readLane(): ReadLane {
  // An idle lane starts its read at once: a lane adds waiting only behind a
  // read already out, never a tick of its own.
  let lane = { key: "", tail: Promise.resolve() as Promise<unknown>, active: 0 };
  return <T>(current: () => boolean, read: () => Promise<T>): Promise<T | undefined> => {
    const key = `${graphBinding()}\0${graphEpoch()}`;
    if (key !== lane.key) lane = { key, tail: Promise.resolve(), active: 0 };
    const own = lane;
    const run = () => (current() ? read() : undefined);
    const turn: Promise<T | undefined> = own.active === 0
      ? new Promise((resolve) => resolve(run()))
      : own.tail.then(run);
    own.active += 1;
    own.tail = turn.catch(() => undefined).finally(() => { own.active -= 1; });
    return turn;
  };
}
