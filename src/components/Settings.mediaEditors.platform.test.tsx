import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

// The "Desktop only; device-local." hint on Diagram editors was enforced by prose
// alone: `MediaEditorsSection` rendered on every platform while both backend
// commands it drives (`edit_asset_external`, `detect_media_editor`) are
// `#[cfg(not(desktop))]` refusals on Android/iOS. Pin the gate, in both
// directions, so the hint stays a fact.
let platform: "android" | "ios" | "desktop" | undefined = "desktop";

vi.mock("../platform", () => ({
  platformKind: async () => {
    if (!platform) throw new Error("platform unavailable");
    return platform;
  },
}));

const { Settings } = await import("./Settings");
const { closeSettings, openSettings } = await import("../ui");

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

async function mountFilesTab() {
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => <Settings />, root);
  openSettings("files");
  await tick();
  await tick();
  return { root, dispose };
}

afterEach(() => {
  closeSettings();
  document.body.innerHTML = "";
  localStorage.clear();
  platform = "desktop";
});

describe("Diagram editors are gated to the platform their hint claims", () => {
  it("offers the section on desktop", async () => {
    platform = "desktop";
    const { root, dispose } = await mountFilesTab();
    const advanced = root.querySelector<HTMLButtonElement>(".settings-advanced-toggle");
    expect(advanced).not.toBeNull();
    advanced!.click();
    await tick();
    expect(root.textContent).toContain("Diagram editors");
    dispose();
  });

  for (const mobile of ["android", "ios"] as const) {
    it(`hides the section, and its search result, on ${mobile}`, async () => {
      platform = mobile;
      const { root, dispose } = await mountFilesTab();
      expect(root.querySelector(".settings-advanced-toggle")).toBeNull();
      expect(root.textContent).not.toContain("Diagram editors");

      const search = root.querySelector<HTMLInputElement>(".settings-search-input")!;
      search.value = "drawio";
      search.dispatchEvent(new Event("input", { bubbles: true }));
      await tick();
      expect(root.textContent).not.toContain("Diagram editors");
      dispose();
    });
  }

  it("fails closed when the platform cannot be established", async () => {
    platform = undefined;
    const { root, dispose } = await mountFilesTab();
    expect(root.querySelector(".settings-advanced-toggle")).toBeNull();
    expect(root.textContent).not.toContain("Diagram editors");
    dispose();
  });
});
