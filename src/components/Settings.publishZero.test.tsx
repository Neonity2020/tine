import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings } from "../ui";
import { backend } from "../backend";

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
