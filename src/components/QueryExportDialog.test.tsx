import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { QueryExportDialog, QUERY_EXPORT_BUDGET_REASON } from "./QueryExportDialog";
import { backend, QueryNotReadyError, QueryUnavailableError } from "../backend";
import {
  closeQueryExport,
  closeSettings,
  openQueryExport,
  queryExportRequest,
  settingsOpen,
  settingsTabRequest,
  setToasts,
  toasts,
} from "../ui";
import { clearTransientLayersForTest } from "../transientLayers";
import { notifyGraphRebound } from "../modeHooks";
import "../persistence"; // a rebound moves the graph binding
import type { QueryPublicationPlan, QueryPublicationRequest } from "../types";

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

const request: QueryPublicationRequest = {
  query: "(task TODO)",
  advanced: false,
  simpleDialect: "og",
  currentPage: "Dashboard",
  view: null,
  hostBlockId: "host",
  name: "Open tasks",
  folder: null,
  replace: false,
  assetBudgetBytes: 1024 * 1024,
};

const plan = (over: Partial<QueryPublicationPlan> = {}): QueryPublicationPlan => ({
  anchor: "block",
  rowCount: 3,
  sampled: false,
  boundPage: null,
  pages: [
    { path: "pages/Tasks.md", name: "Tasks", journal: false },
    { path: "journals/2026_01_02.md", name: "Jan 2nd, 2026", journal: true },
  ],
  folder: "open-tasks",
  path: "/graph/published-queries/open-tasks",
  exists: false,
  suggestedFolder: null,
  fingerprint: "fp-1",
  ...over,
});

function mount() {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <QueryExportDialog />, root);
  return { root, dispose };
}
const button = (root: HTMLElement, label: string) =>
  [...root.querySelectorAll<HTMLButtonElement>("button")].find((b) => b.textContent?.trim() === label)!;

afterEach(() => {
  vi.restoreAllMocks();
  closeQueryExport();
  closeSettings();
  clearTransientLayersForTest();
  setToasts([]);
  document.body.innerHTML = "";
});

describe("QueryExportDialog", () => {
  it("shows the reviewed pages and will not export a block query until whole-page export is acknowledged", async () => {
    vi.spyOn(backend(), "publishQueryPlan").mockResolvedValue(plan());
    const publish = vi.spyOn(backend(), "publishQuery").mockResolvedValue({
      path: "/graph/published-queries/open-tasks",
      pages: 2,
      retired: null,
      warnings: [],
    });
    const { root, dispose } = mount();
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.querySelectorAll('[data-testid="query-export-pages"] li').length).toBe(2));
      expect(root.textContent).toContain("Tasks");
      expect(root.textContent).toContain("1 journal");
      // The one thing a user must understand before a block query exports.
      expect(root.textContent).toContain("All blocks on these pages will be exported");
      expect(button(root, "Export").disabled).toBe(true);
      root.querySelector<HTMLInputElement>(".query-export-ack input")!.click();
      await tick();
      expect(button(root, "Export").disabled).toBe(false);
      button(root, "Export").click();
      await vi.waitFor(() => expect(queryExportRequest()).toBeNull());
      // The plan's folder and fingerprint go back unchanged: nothing is
      // recomputed between review and commit.
      expect(publish).toHaveBeenCalledWith(
        expect.objectContaining({ name: "Open tasks", folder: "open-tasks", replace: false }),
        "fp-1",
      );
      expect(toasts().some((t) => t.message.includes("Exported 2 pages"))).toBe(true);
    } finally {
      dispose();
    }
  });

  it("offers Replace or a separate folder when the name is taken, and a page query needs no acknowledgement", async () => {
    vi.spyOn(backend(), "publishQueryPlan").mockResolvedValue(
      plan({ anchor: "page", exists: true, suggestedFolder: "open-tasks-2" }),
    );
    const publish = vi.spyOn(backend(), "publishQuery").mockResolvedValue({
      path: "/graph/published-queries/open-tasks-2",
      pages: 2,
      retired: null,
      warnings: [],
    });
    const { root, dispose } = mount();
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.textContent).toContain("already exists"));
      expect(root.querySelector(".query-export-ack")).toBeNull();
      // Neither destination chosen yet: no silent overwrite.
      expect(button(root, "Export").disabled).toBe(true);
      const radios = root.querySelectorAll<HTMLInputElement>('input[name="query-export-destination"]');
      radios[1].click();
      await tick();
      expect(root.textContent).toContain("open-tasks-2");
      button(root, "Export").click();
      await vi.waitFor(() => expect(publish).toHaveBeenCalled());
      expect(publish.mock.calls[0][0]).toMatchObject({ folder: "open-tasks-2", replace: false });
    } finally {
      dispose();
    }
  });

  it("names the asset limit when the export goes over budget and opens the setting in one click", async () => {
    vi.spyOn(backend(), "publishQueryPlan").mockResolvedValue(plan({ anchor: "page" }));
    vi.spyOn(backend(), "publishQuery").mockRejectedValue(
      new QueryUnavailableError(QUERY_EXPORT_BUDGET_REASON, "Export stopped: copying big.mp4 would pass the 1024 MiB limit."),
    );
    const { root, dispose } = mount();
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(button(root, "Export").disabled).toBe(false));
      button(root, "Export").click();
      await vi.waitFor(() => expect(root.querySelector('[role="alert"]')?.textContent).toContain("big.mp4"));
      const adjust = button(root, "Adjust limit in Settings…");
      expect(adjust).toBeDefined();
      adjust.click();
      await tick();
      expect(queryExportRequest()).toBeNull();
      expect(settingsOpen()).toBe(true);
      expect(settingsTabRequest()).toBe("graph");
    } finally {
      dispose();
    }
  });

  it("shows a plan refusal instead of an export button that would fail", async () => {
    vi.spyOn(backend(), "publishQueryPlan").mockRejectedValue(new Error("Two files claim the page Tasks."));
    const { root, dispose } = mount();
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.querySelector('[role="alert"]')?.textContent).toContain("Two files claim"));
      expect(button(root, "Export").disabled).toBe(true);
    } finally {
      dispose();
    }
  });
});

// GH #543, audit R6-05: a plan asked for while the index is being built waits
// and plans again once it is ready, instead of showing a refusal that nothing
// retries.
describe("QueryExportDialog while the index is being built", () => {
  it("waits for the index and then shows the plan", async () => {
    let ready = false;
    const planCall = vi.spyOn(backend(), "publishQueryPlan").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return plan();
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryExportDialog />, root);
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index to be ready"));
      expect(root.querySelector(".query-export-refused")).toBeNull();
      ready = true;
      await vi.waitFor(() => expect(root.textContent).toContain("Tasks"), { timeout: 3_000 });
      expect(planCall.mock.calls.length).toBeGreaterThan(1);
      expect(root.querySelector(".query-export-refused")).toBeNull();
    } finally {
      dispose();
    }
  }, 10_000);

  it("an export confirmed while the index catches up waits and then exports", async () => {
    vi.spyOn(backend(), "publishQueryPlan").mockResolvedValue(plan({ anchor: "page" }));
    let ready = false;
    const publish = vi.spyOn(backend(), "publishQuery").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("pending_edits");
      return { path: "/graph/published-queries/open-tasks", pages: 2, retired: null, warnings: [] };
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <QueryExportDialog />, root);
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.textContent).toContain("Tasks"));
      const exportButton = [...root.querySelectorAll("button")].find((b) => b.textContent === "Export")!;
      exportButton.click();
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index"));
      expect(root.querySelector(".query-export-failure, [role=alert]")).toBeNull();
      ready = true;
      await vi.waitFor(() => expect(queryExportRequest()).toBeNull(), { timeout: 3_000 });
      expect(publish.mock.calls.length).toBeGreaterThan(1);
    } finally {
      dispose();
    }
  }, 10_000);
});

// GH #543, audits R12-06 and R13-01: the plan's readiness wait ends with the
// dialog, and survives a reopen of the same graph (a config.edn change).
describe("QueryExportDialog's readiness wait", () => {
  it("stops asking for a plan once the dialog is gone", async () => {
    const planCall = vi.spyOn(backend(), "publishQueryPlan").mockRejectedValue(new QueryNotReadyError("indexing"));
    const { root, dispose } = mount();
    openQueryExport(request);
    await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index to be ready"));
    dispose();
    closeQueryExport();
    const atClose = planCall.mock.calls.length;
    await new Promise((resolve) => setTimeout(resolve, 1600));
    expect(planCall.mock.calls.length - atClose, "a closed dialog keeps asking for a plan").toBe(0);
    root.remove();
  }, 10_000);

  it("shows the plan after the same graph is reopened while it waits", async () => {
    let ready = false;
    vi.spyOn(backend(), "publishQueryPlan").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return plan();
    });
    const { root, dispose } = mount();
    try {
      openQueryExport(request);
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index to be ready"));
      notifyGraphRebound();
      ready = true;
      await vi.waitFor(() => expect(root.textContent).toContain("Tasks"), { timeout: 3_000 });
    } finally {
      dispose();
      closeQueryExport();
    }
  }, 10_000);
});
