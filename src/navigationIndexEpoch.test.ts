import { describe, expect, it, vi } from "vitest";
import { __setBackendForTest } from "./backend";
import { mockBackend } from "./mock";
import { refreshAliases, refreshPageIdentities } from "./graph";
import { bumpGraphEpoch, resolveAlias } from "./ui";

describe("navigation index after an epoch-only bump (GH #543)", () => {
  it("keeps real-page precedence after a repaint epoch bump plus one save", async () => {
    const api = mockBackend();
    // Page "Beta" declares alias:: Alpha, and a real page "Alpha" also exists.
    vi.spyOn(api, "pageAliases").mockResolvedValue([["Alpha", "Beta"]] as never);
    vi.spyOn(api, "listPages").mockResolvedValue([
      { name: "Alpha", kind: "page", path: "pages/Alpha.md" },
      { name: "Beta", kind: "page", path: "pages/Beta.md" },
    ] as never);
    __setBackendForTest(api);
    await Promise.all([refreshAliases(), refreshPageIdentities()]);
    expect(resolveAlias("alpha")).toBe("Alpha"); // real page wins
    bumpGraphEpoch(); // e.g. setTypographyMode / journal-title format repaint
    await refreshAliases(); // App.tsx on(dataRev) after the next save or projection commit
    expect(resolveAlias("alpha")).toBe("Alpha");
  });
});
