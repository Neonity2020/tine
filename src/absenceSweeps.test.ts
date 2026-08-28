import { beforeEach, describe, expect, it } from "vitest";

import {
  absenceSweepPanelOpen,
  absenceSweeps,
  ingestAbsenceSweepEvent,
  openAbsenceSweepPanel,
  rebindAbsenceSweepScope,
  resetAbsenceSweepStateForTest,
} from "./absenceSweeps";
import type { SyncAbsenceSweepEvent } from "./types";

function sweep(overrides: Partial<SyncAbsenceSweepEvent> = {}): SyncAbsenceSweepEvent {
  return {
    sweep_id: "sweep-1",
    tier: "tier3",
    absence_count: 8,
    pages_at_open: 20,
    opened_at_unix_ms: 1_000,
    closed_at_unix_ms: null,
    grace_deadline_unix_ms: null,
    disposed_at_unix_ms: null,
    members: [{ page_id: "p1", path: "pages/One.md" }],
    latest_action: null,
    ...overrides,
  };
}

describe("rebindAbsenceSweepScope", () => {
  beforeEach(() => {
    resetAbsenceSweepStateForTest();
  });

  it("keeps the sweep list and an open panel across a same-generation rebind", () => {
    // A runtime status event on the SAME binding (restore completion emits
    // several) re-runs the App listener effect. That must not close a panel
    // the user is looking at, nor transiently wipe durable sweep records.
    rebindAbsenceSweepScope(1);
    ingestAbsenceSweepEvent(sweep());
    openAbsenceSweepPanel();

    rebindAbsenceSweepScope(1);

    expect(absenceSweeps()).toHaveLength(1);
    expect(absenceSweepPanelOpen()).toBe(true);
  });

  it("clears the surface when the graph binding actually changes", () => {
    rebindAbsenceSweepScope(1);
    ingestAbsenceSweepEvent(sweep());
    openAbsenceSweepPanel();

    rebindAbsenceSweepScope(2);

    expect(absenceSweeps()).toHaveLength(0);
    expect(absenceSweepPanelOpen()).toBe(false);
  });

  it("clears when the binding is withdrawn (generation null) and on the return", () => {
    rebindAbsenceSweepScope(1);
    ingestAbsenceSweepEvent(sweep());
    openAbsenceSweepPanel();

    rebindAbsenceSweepScope(null);
    expect(absenceSweeps()).toHaveLength(0);
    expect(absenceSweepPanelOpen()).toBe(false);

    ingestAbsenceSweepEvent(sweep());
    openAbsenceSweepPanel();
    rebindAbsenceSweepScope(null);
    expect(absenceSweeps()).toHaveLength(1);
    expect(absenceSweepPanelOpen()).toBe(true);
  });
});
