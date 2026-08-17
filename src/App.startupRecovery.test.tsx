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
  it("keeps the failed target actionable and offers a usable graph chooser", async () => {
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
      expect(host.querySelector(".startup-recovery-overlay")).not.toBeNull();
    });

    const openExisting = [...host.querySelectorAll<HTMLButtonElement>("button")]
      .find((button) => button.textContent?.includes("Open another graph"));
    expect(openExisting).toBeDefined();
    expect(openExisting?.disabled).toBe(false);
    expect(host.textContent).toContain("Return logseq to Direct Files");
    expect(host.textContent).not.toContain(missingRoot);
  });

  it("routes the recovery-screen escape to cold Direct Files without promising archival", async () => {
    const root = "/Volumes/logseq";
    vi.spyOn(backend(), "startupGraphPath").mockResolvedValue(root);
    vi.spyOn(backend(), "loadGraph").mockRejectedValue(new Error("managed open failed"));
    const confirm = vi.spyOn(backend(), "confirm").mockResolvedValue(true);
    const coldReturn = vi.spyOn(backend(), "cancelSparseV2Cold").mockResolvedValue({
      status: {
        state: "legacy_default",
        runtime: null,
        can_activate: true,
        can_retry: false,
        can_cancel: false,
        cancel_reason: null,
        binding_generation: 9,
        application_page_admission: { binding_generation: 9, authority: "direct" },
      },
      binding_generation: 9,
      recovery_statement: "Direct Files is active.",
    });

    const host = document.createElement("div");
    document.body.appendChild(host);
    dispose = render(() => <App />, host);
    const button = await vi.waitFor(() => {
      const candidate = [...host.querySelectorAll<HTMLButtonElement>("button")]
        .find((item) => item.textContent?.includes("Return logseq to Direct Files"));
      expect(candidate).toBeDefined();
      return candidate!;
    });
    button.click();

    await vi.waitFor(() => expect(coldReturn).toHaveBeenCalledWith(root));
    const message = String(confirm.mock.calls[0]?.[0] ?? "");
    expect(message).toContain("will not open, recover, drain, save, or archive managed state");
    expect(message).not.toContain("will archive its durable managed-storage");
  });
});
