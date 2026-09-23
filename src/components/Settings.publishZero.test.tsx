import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, graphMeta, openSettings, setGraphMeta } from "../ui";
import { backend, QueryNotReadyError } from "../backend";

// "Export graph to HTML" publishes the public-page capability: only pages
// carrying `public:: true` are exported, as in Logseq. A graph with none
// therefore exports nothing — correct, but it was reported as a broken button
// on Android because the result said only "Exported 0 pages to <dir>"
// (GH #560). The zero has to carry its reason; a non-zero must not.
const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

async function exportFromGraphTab(pages: number) {
  vi.spyOn(backend(), "publishHtml").mockResolvedValue(["/mock/graph/publish", pages]);
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <Settings />, root);
  openSettings("graph");
  await tick();
  const button = [...root.querySelectorAll("button")]
    .find((candidate) => candidate.textContent?.includes("Export graph to HTML"));
  // Precondition, asserted separately from the property under test so a
  // failure here reads as "the tab did not render" rather than as a message bug.
  expect(button, "the Graph tab must offer the export button").toBeTruthy();
  button!.click();
  await tick();
  await tick();
  return { root, dispose };
}

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  vi.restoreAllMocks();
});

describe("Settings → Graph → Export graph to HTML (GH #560)", () => {
  it("says why an export produced nothing", async () => {
    const { root, dispose } = await exportFromGraphTab(0);

    expect(root.textContent).toContain("Exported 0 pages");
    expect(root.textContent).toContain("public:: true");
    dispose();
  });

  it("just reports the count when pages were exported", async () => {
    const { root, dispose } = await exportFromGraphTab(3);

    expect(root.textContent).toContain("Exported 3 pages");
    expect(root.textContent).not.toContain("public:: true");
    dispose();
  });
});

// GH #543, audit R6-05: publication reads its queries from the index, so an
// export asked for while the index is being built waits for it and then
// exports, rather than reporting the wait as a failure.
describe("Settings → Graph → Export graph to HTML while indexing (GH #543)", () => {
  it("waits for the index, then exports", async () => {
    let ready = false;
    vi.spyOn(backend(), "publishHtml").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return ["/mock/graph/publish", 2];
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);
    try {
      openSettings("graph");
      await tick();
      [...root.querySelectorAll("button")]
        .find((candidate) => candidate.textContent?.includes("Export graph to HTML"))!
        .click();
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index"));
      expect(root.textContent).not.toContain("Failed");
      ready = true;
      await vi.waitFor(() => expect(root.textContent).toContain("Exported 2 pages"), { timeout: 3_000 });
    } finally {
      dispose();
    }
  }, 10_000);
});

// GH #543, audit R8-10: every retry asks the backend to export the CURRENT
// graph, so opening another graph while the export waited for the index
// exported that other graph. The export belongs to the graph it started on.
describe("Settings → Graph → Export graph to HTML across a graph switch (GH #543)", () => {
  it("does not export the graph opened while it waited", async () => {
    const { bumpGraphBinding } = await import("../persistence");
    const before = graphMeta();
    setGraphMeta({ ...(before ?? {}), root: "/mock/graph" } as never);
    let ready = false;
    const publish = vi.spyOn(backend(), "publishHtml").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return ["/mock/other-graph/publish", 5];
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);
    try {
      openSettings("graph");
      await tick();
      [...root.querySelectorAll("button")]
        .find((candidate) => candidate.textContent?.includes("Export graph to HTML"))!
        .click();
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index"));
      const attempts = publish.mock.calls.length;
      // Another graph is opened in this window.
      setGraphMeta({ ...(before ?? {}), root: "/mock/other-graph" } as never);
      bumpGraphBinding();
      ready = true;
      await new Promise((resolve) => setTimeout(resolve, 1_500));
      expect(publish.mock.calls.length, "a retry exported the newly opened graph").toBe(attempts);
      expect(root.textContent).not.toContain("Exported 5 pages");
      expect(root.textContent, "the export still says it waits").not.toContain("Waiting for the index");
    } finally {
      dispose();
      setGraphMeta(before);
    }
  }, 10_000);

  // GH #543, audit R13-01: a reopen of the SAME graph (a config.edn change)
  // is not a switch; the export waits on and exports it.
  it("exports the same graph after it is reopened while it waited", async () => {
    const { bumpGraphBinding } = await import("../persistence");
    const before = graphMeta();
    setGraphMeta({ ...(before ?? {}), root: "/mock/graph" } as never);
    let ready = false;
    vi.spyOn(backend(), "publishHtml").mockImplementation(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return ["/mock/graph/publish", 5];
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <Settings />, root);
    try {
      openSettings("graph");
      await tick();
      [...root.querySelectorAll("button")]
        .find((candidate) => candidate.textContent?.includes("Export graph to HTML"))!
        .click();
      await vi.waitFor(() => expect(root.textContent).toContain("Waiting for the index"));
      bumpGraphBinding(); // the backend reopened this graph
      ready = true;
      await vi.waitFor(() => expect(root.textContent).toContain("Exported 5 pages"), { timeout: 3_000 });
    } finally {
      dispose();
      setGraphMeta(before);
    }
  }, 10_000);
});
