import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

import type { SyncAbsenceSweepEvent } from "../types";
import { __setBackendForTest, type Backend } from "../backend";
import { setToasts } from "../ui";
import { Toasts } from "./Toasts";
import { AbsenceSweepCenter } from "./AbsenceSweepCenter";
import {
  absenceSweeps,
  ingestAbsenceSweepEvent,
  openAbsenceSweepPanel,
  resetAbsenceSweepStateForTest,
} from "../absenceSweeps";

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

function sweep(
  latestAction: SyncAbsenceSweepEvent["latest_action"] = null,
  disposedAt: number | null = null,
): SyncAbsenceSweepEvent {
  return {
    sweep_id: "11111111-1111-4111-8111-111111111111",
    tier: "tier2",
    absence_count: 4,
    pages_at_open: 80,
    opened_at_unix_ms: 1_777_000_000_000,
    closed_at_unix_ms: 1_777_000_060_000,
    grace_deadline_unix_ms: null,
    disposed_at_unix_ms: disposedAt,
    members: [
      { page_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", path: "pages/Alpha.md" },
      { page_id: "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", path: "journals/2026_08_28.md" },
      { page_id: "cccccccc-cccc-4ccc-8ccc-cccccccccccc", path: "notes/Gamma.org" },
      { page_id: "dddddddd-dddd-4ddd-8ddd-dddddddddddd", path: "pages/Delta.md" },
    ],
    latest_action: latestAction,
  };
}

function mount() {
  const host = document.createElement("div");
  document.body.append(host);
  const dispose = render(() => <AbsenceSweepCenter />, host);
  return { host, dispose };
}

beforeEach(() => {
  resetAbsenceSweepStateForTest();
  setToasts([]);
});

afterEach(() => {
  document.body.innerHTML = "";
  __setBackendForTest(null);
  resetAbsenceSweepStateForTest();
  setToasts([]);
});

describe("absence sweep surfacing", () => {
  it("lists a surfaced sweep and its member pages", async () => {
    ingestAbsenceSweepEvent(sweep());
    const { host, dispose } = mount();
    try {
      const dock = host.querySelector<HTMLButtonElement>(".absence-sweep-dock")!;
      expect(dock.textContent).toContain("4 deleted pages");
      dock.click();
      await flush();
      expect(host.querySelector(".absence-sweep-panel")?.textContent).toContain("Tier 2");
      expect(host.querySelector(".absence-sweep-panel")?.textContent).toContain("Alpha");
      expect(host.querySelector(".absence-sweep-panel")?.textContent).toContain("2026_08_28");
    } finally {
      dispose();
    }
  });

  it("renders durable Restore progress and disables competing actions", () => {
    ingestAbsenceSweepEvent(sweep({
      action_id: "22222222-2222-4222-8222-222222222222",
      action: "restore",
      state: "progress",
      recorded_at_unix_ms: 1_777_000_070_000,
      authored_batch_ids: ["batch-1", "batch-2"],
      chunk_ordinal: 2,
      remaining_operation_watermark: 17,
      nondecreasing_retries: 0,
      failure_reason: null,
    }));
    openAbsenceSweepPanel();
    const { host, dispose } = mount();
    try {
      expect(host.textContent).toContain("Restoring…");
      expect(host.textContent).toContain("Chunk 2");
      expect(host.textContent).toContain("17 operations remaining");
      const actions = [...host.querySelectorAll<HTMLButtonElement>(".absence-sweep-action")];
      expect(actions.length).toBeGreaterThan(0);
      expect(actions.every((button) => button.disabled)).toBe(true);
    } finally {
      dispose();
    }
  });

  it("shows a failed Restore cause and re-runs Restore explicitly", async () => {
    const restore = vi.fn(async () => ({
      sweep_id: sweep().sweep_id,
      action_id: "33333333-3333-4333-8333-333333333333",
      authored_batch_ids: [],
      fidelity: [],
    }));
    __setBackendForTest({ restoreAbsenceSweep: restore } as unknown as Backend);
    ingestAbsenceSweepEvent(sweep({
      action_id: "22222222-2222-4222-8222-222222222222",
      action: "restore",
      state: "failed",
      recorded_at_unix_ms: 1_777_000_070_000,
      authored_batch_ids: ["batch-1"],
      chunk_ordinal: 1,
      remaining_operation_watermark: 23,
      nondecreasing_retries: 3,
      failure_reason: "Concurrent edits kept changing the page after three retries.",
    }));
    openAbsenceSweepPanel();
    const { host, dispose } = mount();
    try {
      expect(host.textContent).toContain("Concurrent edits kept changing the page after three retries.");
      const rerun = [...host.querySelectorAll<HTMLButtonElement>("button")]
        .find((button) => button.textContent?.includes("Run Restore again"))!;
      expect(rerun).toBeTruthy();
      rerun.click();
      await flush();
      expect(restore).toHaveBeenCalledOnce();
      expect(restore).toHaveBeenCalledWith(sweep().sweep_id);
    } finally {
      dispose();
    }
  });

  it("keeps a disposed sweep visible with its deliberate disposition", () => {
    ingestAbsenceSweepEvent(sweep({
      action_id: "44444444-4444-4444-8444-444444444444",
      action: "keep_deletion",
      state: "completed",
      recorded_at_unix_ms: 1_777_000_080_000,
      authored_batch_ids: [],
      chunk_ordinal: null,
      remaining_operation_watermark: null,
      nondecreasing_retries: null,
      failure_reason: null,
    }, 1_777_000_080_000));
    openAbsenceSweepPanel();
    const { host, dispose } = mount();
    try {
      expect(host.textContent).toContain("Deletion kept");
      expect(host.querySelectorAll(".absence-sweep-action")).toHaveLength(0);
    } finally {
      dispose();
    }
  });

  it("never disposes a sweep when its toast or panel is dismissed", async () => {
    const keep = vi.fn(async () => {});
    __setBackendForTest({ keepAbsenceSweepDeletion: keep } as unknown as Backend);
    ingestAbsenceSweepEvent(sweep(), { announce: true });

    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => (
      <>
        <AbsenceSweepCenter />
        <Toasts />
      </>
    ), host);
    try {
      host.querySelector<HTMLButtonElement>(".toast-close")!.click();
      host.querySelector<HTMLButtonElement>(".absence-sweep-dock")!.click();
      await flush();
      host.querySelector<HTMLButtonElement>(".absence-sweep-panel-close")!.click();
      await flush();

      expect(keep).not.toHaveBeenCalled();
      expect(absenceSweeps()).toHaveLength(1);
      expect(host.querySelector(".absence-sweep-dock")).not.toBeNull();
    } finally {
      dispose();
    }
  });
});
