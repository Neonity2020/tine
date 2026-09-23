// GH #543, audit R11-12: the indexing bar keeps one follower per graph
// binding, and a follower that ended on its own is not one. It was kept as
// the binding's follower, so a later pass in the same binding showed no bar.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

const followers: number[] = [];
vi.mock("./indexingProgress", async (orig) => ({
  ...(await orig<typeof import("./indexingProgress")>()),
  followIndexingProgress: vi.fn((binding: number) => {
    followers.push(binding);
    return Promise.resolve(); // it ended on its own
  }),
}));

import { IndexingProgressBar } from "./components/IndexingProgressBar";
import { bumpGraphEpoch } from "./ui";

let dispose: (() => void) | null = null;
afterEach(() => {
  dispose?.();
  dispose = null;
  followers.length = 0;
});
const settle = async (n = 5) => {
  for (let i = 0; i < n; i++) await new Promise((r) => setTimeout(r, 0));
};

describe("indexing bar follower", () => {
  it("starts again after its follower ended, within one binding", async () => {
    dispose = render(() => <IndexingProgressBar />, document.createElement("div"));
    expect(followers).toHaveLength(1);
    await settle();
    bumpGraphEpoch();
    await settle();
    expect(followers).toHaveLength(2);
    expect(followers[1]).toBe(followers[0]);
  });
});
