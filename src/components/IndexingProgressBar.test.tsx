// GH #543 (audit R10-11): the indexing bar follows one graph BINDING. A
// repaint that moves only the render epoch (typography, journal title format)
// keeps the running follower, so a bar that is showing does not vanish for a
// fresh grace period; a rebind starts a new follower and stops the old one.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

const followers: { binding: number; signal?: AbortSignal }[] = [];
vi.mock("../indexingProgress", async (orig) => ({
  ...(await orig<typeof import("../indexingProgress")>()),
  followIndexingProgress: vi.fn((binding: number, _publish: unknown, _deps: unknown, signal?: AbortSignal) => {
    followers.push({ binding, signal });
    return new Promise<void>(() => {});
  }),
}));

import { IndexingProgressBar } from "./IndexingProgressBar";
import { bumpGraphEpoch } from "../ui";
import { notifyGraphRebound } from "../modeHooks";

let dispose: (() => void) | null = null;
afterEach(() => { dispose?.(); dispose = null; followers.length = 0; });

describe("IndexingProgressBar", () => {
  it("keeps its follower across a repaint and replaces it on a rebind", () => {
    const host = document.createElement("div");
    dispose = render(() => <IndexingProgressBar />, host);
    expect(followers).toHaveLength(1);

    bumpGraphEpoch(); // repaint only
    expect(followers).toHaveLength(1);
    expect(followers[0].signal?.aborted).toBe(false);

    notifyGraphRebound(); // the backend reopened the graph …
    bumpGraphEpoch(); // … and every rebind is followed by a repaint
    expect(followers).toHaveLength(2);
    expect(followers[1].binding).not.toBe(followers[0].binding);
    expect(followers[0].signal?.aborted).toBe(true);
    expect(followers[1].signal?.aborted).toBe(false);
  });
});
