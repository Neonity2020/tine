import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { App } from "./App";
import { backend } from "./backend";
import { setFirstLoadDone, setGraphMeta } from "./ui";

let dispose = () => {};

afterEach(() => {
  dispose();
  dispose = () => {};
  vi.restoreAllMocks();
  setGraphMeta(null);
  setFirstLoadDone(false);
  document.body.innerHTML = "";
});

describe("missing remembered graph recovery (GH #250)", () => {
  it("finishes startup and offers a usable graph chooser when the remembered root is gone", async () => {
    const missingRoot = "/Volumes/logseq";
    setGraphMeta(null);
    setFirstLoadDone(false);
    vi.spyOn(backend(), "startupGraphPath").mockResolvedValue(missingRoot);
    const loadGraph = vi.spyOn(backend(), "loadGraph").mockRejectedValue(
      new Error(`No such file or directory: ${missingRoot}`)
    );

    const host = document.createElement("div");
    document.body.appendChild(host);
    dispose = render(() => <App />, host);

    await vi.waitFor(() => {
      expect(loadGraph).toHaveBeenCalledWith(missingRoot);
      expect(host.querySelector(".welcome-overlay")).not.toBeNull();
    });

    const openExisting = [...host.querySelectorAll<HTMLButtonElement>(".welcome-choice")]
      .find((button) => button.textContent?.includes("Open an existing graph"));
    expect(openExisting).toBeDefined();
    expect(openExisting?.disabled).toBe(false);
  });
});
