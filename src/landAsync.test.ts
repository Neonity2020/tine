import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { resetSaveState } from "./persistence";
import {
  bumpGraphEpoch,
  setGraphMeta,
  setGraphTransitioning,
  setToasts,
  toasts,
} from "./ui";
import {
  captureGraphScope,
  isScopeCurrent,
  landAsync,
  landAsyncOrToast,
} from "./landAsync";

beforeEach(() => {
  setGraphTransitioning(false);
  setGraphMeta({ root: "/graphs/A" } as never);
  setToasts([]);
});

afterEach(() => {
  setGraphTransitioning(false);
  setGraphMeta(null);
  setToasts([]);
});

describe("graph-scoped async landing", () => {
  it("refuses capture during a graph transition", () => {
    setGraphTransitioning(true);
    expect(captureGraphScope()).toBeNull();
  });

  it("drops a result after the binding changes and does not start stale work", async () => {
    const scope = captureGraphScope();
    resetSaveState();
    let called = false;
    await expect(landAsync(scope, () => { called = true; return 1; }))
      .resolves.toEqual({ landed: false });
    expect(called).toBe(false);
  });

  it("ignores a render-epoch bump unless repaint sensitivity is requested", () => {
    const scope = captureGraphScope();
    expect(scope).not.toBeNull();
    bumpGraphEpoch();
    expect(isScopeCurrent(scope)).toBe(true);
    expect(isScopeCurrent(scope, { repaintSensitive: true })).toBe(false);
  });

  it("makes a dropped user action visible", async () => {
    const scope = captureGraphScope();
    resetSaveState();
    await expect(landAsyncOrToast(scope, () => 1, "Graph changed; try again."))
      .resolves.toEqual({ landed: false });
    expect(toasts().map((toast) => toast.message)).toContain("Graph changed; try again.");
  });
});
