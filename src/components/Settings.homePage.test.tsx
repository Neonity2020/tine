// GH #245: the Graph settings row for the optional per-graph home page —
// picking an existing page, clearing it, and surfacing a stale value.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { Settings } from "./Settings";
import { closeSettings, openSettings, setGraphMeta, setToasts } from "../ui";
import { backend } from "../backend";
import { resetStore } from "../store";
import type { GraphMeta, PageDto } from "../types";

const ROOT = "/tmp/home-page-graph";
const KEY = `home.page.${ROOT}`;

const META: GraphMeta = {
  root: ROOT,
  journals_dir: "journals",
  pages_dir: "pages",
  preferred_workflow: "now",
  shortcuts: {},
  start_of_week: 6,
  block_hidden_properties: [],
  default_journal_template: null,
  favorites: [],
  journal_page_title_format: "MMM do, yyyy",
  journal_file_name_format: "yyyy_MM_dd",
  preferred_format: "md",
  macros: {},
  enable_timetracking: true,
  show_brackets: true,
  logbook_with_second_support: true,
  logbook_enabled_in_timestamped_blocks: false,
  logbook_enabled_in_all_blocks: false,
  guide_announced: true,
};

const DIRECTORY: PageDto = {
  name: "Directory",
  kind: "page",
  title: "Directory",
  pre_block: null,
  format: "md",
  blocks: [{ id: "b1", raw: "Home", collapsed: false, children: [] }],
};

async function mount(): Promise<{ root: HTMLDivElement; dispose: () => void }> {
  const root = document.createElement("div");
  document.body.append(root);
  const dispose = render(() => <Settings />, root);
  openSettings();
  await new Promise((r) => setTimeout(r, 0));
  const nav = [...root.querySelectorAll<HTMLElement>(".settings-nav-item")].find(
    (b) => b.textContent === "Graph"
  );
  nav!.click();
  await new Promise((r) => setTimeout(r, 0));
  return { root, dispose };
}

const field = (root: HTMLElement) =>
  root.querySelector('[data-setting-label="Home page"]') as HTMLElement | null;
const settle = () => new Promise((r) => setTimeout(r, 220));

afterEach(() => {
  closeSettings();
  setGraphMeta(null);
  resetStore();
  setToasts([]);
  localStorage.clear();
  vi.restoreAllMocks();
  document.body.innerHTML = "";
});

describe("Settings home page row (GH #245)", () => {
  it("picks an existing page and persists it per graph", async () => {
    await backend().setAppString(KEY, "");
    vi.spyOn(backend(), "getPage").mockResolvedValue(DIRECTORY);
    const quickSwitch = vi.spyOn(backend(), "quickSwitch").mockResolvedValue([
      { name: "Directory", kind: "page", date_key: null, path: "pages/directory.md" },
    ]);
    const setAppString = vi.spyOn(backend(), "setAppString");
    setGraphMeta(META);

    const { root, dispose } = await mount();
    await vi.waitFor(() =>
      expect(field(root)?.querySelector("input.settings-input")).not.toBeNull()
    );
    const input = field(root)!.querySelector("input.settings-input") as HTMLInputElement;

    input.value = "dir";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await settle();
    expect(quickSwitch).toHaveBeenCalled();
    const suggestion = [...field(root)!.querySelectorAll("button")].find(
      (b) => b.textContent === "Directory"
    ) as HTMLButtonElement;
    suggestion.click();
    await vi.waitFor(() =>
      expect(setAppString).toHaveBeenCalledWith(`home.page.${ROOT}`, "Directory")
    );
    await vi.waitFor(() =>
      expect(field(root)!.querySelector("[data-home-page-value]")?.textContent).toBe("Directory")
    );
    dispose();
  });

  it("surfaces a stored value that no longer resolves, and clears it", async () => {
    await backend().setAppString(KEY, "Ghost");
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    const setAppString = vi.spyOn(backend(), "setAppString");
    setGraphMeta(META);

    const { root, dispose } = await mount();
    await vi.waitFor(() =>
      expect(field(root)?.querySelector("[data-home-page-value]")?.textContent).toBe("Ghost")
    );
    await vi.waitFor(() =>
      expect(field(root)!.querySelector("[data-home-page-missing]")).not.toBeNull()
    );
    expect(field(root)!.textContent).toContain("deleted or renamed");

    const clear = [...field(root)!.querySelectorAll("button")].find(
      (b) => b.textContent === "Clear"
    ) as HTMLButtonElement;
    clear.click();
    await vi.waitFor(() => expect(setAppString).toHaveBeenCalledWith(KEY, ""));
    await vi.waitFor(() => expect(field(root)!.querySelector("[data-home-page-value]")).toBeNull());
    expect(field(root)!.querySelector("input.settings-input")).not.toBeNull();
    dispose();
  });
});
