import { describe, expect, it, vi } from "vitest";
import { openKnownGraph, type KnownGraphOpenDeps } from "./Sidebar";

describe("known graph open gesture", () => {
  it("uses an in-place switch for an ordinary click", async () => {
    const deps: KnownGraphOpenDeps = {
      switchInPlace: vi.fn().mockResolvedValue(undefined),
      openNewWindow: vi.fn().mockResolvedValue(undefined),
    };
    await openKnownGraph("/graphs/a", false, deps);
    expect(deps.switchInPlace).toHaveBeenCalledWith("/graphs/a");
    expect(deps.openNewWindow).not.toHaveBeenCalled();
  });

  it("opens a new OS window for shift-click", async () => {
    const deps: KnownGraphOpenDeps = {
      switchInPlace: vi.fn().mockResolvedValue(undefined),
      openNewWindow: vi.fn().mockResolvedValue({ kind: "loaded" }),
    };
    await openKnownGraph("/graphs/b", true, deps);
    expect(deps.openNewWindow).toHaveBeenCalledWith("/graphs/b");
    expect(deps.switchInPlace).not.toHaveBeenCalled();
  });
});
