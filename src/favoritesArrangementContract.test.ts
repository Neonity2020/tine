// Doc-code consistency for docs/contracts/favorites-arrangement.md and
// docs/contracts/config-live-reload.md. A contract that can drift silently is
// not a contract; this fails CI instead of letting the documents rot
// (AGENTS.md §2, living contracts).
import { existsSync, readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { modelModuleSource } from "./rustModelSource.test-helpers";

function rustModuleSource(path: string): string {
  const files = [path];
  const visit = (directory: string) => {
    for (const entry of readdirSync(directory, { withFileTypes: true })) {
      const child = join(directory, entry.name);
      if (entry.isDirectory()) visit(child);
      else if (entry.isFile() && entry.name.endsWith(".rs")) files.push(child);
    }
  };
  const moduleDirectory = path.replace(/\.rs$/, "");
  if (existsSync(moduleDirectory)) visit(moduleDirectory);
  return files.sort().map((file) => readFileSync(file, "utf8")).join("\n");
}

const arrangement = readFileSync("docs/contracts/favorites-arrangement.md", "utf8");
const reload = readFileSync("docs/contracts/config-live-reload.md", "utf8");
const layout = readFileSync("src/favoritesLayout.ts", "utf8");
const store = readFileSync("src/favoritesStore.ts", "utf8");
const sidebar = readFileSync("src/components/Sidebar.tsx", "utf8");
const watcher = rustModuleSource("src-tauri/src/watcher.rs");
const model = modelModuleSource();
const graph = readFileSync("src/graph.ts", "utf8");

describe("favorites arrangement contract matches the source", () => {
  it("names the property the page carries as a human-visible label", () => {
    expect(arrangement).toContain("tine/favorites:: true");
    expect(layout).toContain('FAVORITES_PAGE_PROPERTY = "tine/favorites"');
  });

  it("keeps the config.edn identity key the doc documents", () => {
    expect(arrangement).toContain(":tine/favorites-page");
    expect(readFileSync("crates/tine-core/src/config.rs", "utf8")).toContain(
      '":tine/favorites-page"'
    );
  });

  // §3 — the whole reason arbitrary depth is cheap: there is no group/item
  // split to translate. If a second node shape ever appears, this fails.
  it("has ONE node type, so depth needs no special case", () => {
    expect(arrangement).toContain("**One node type, at any depth.**");
    expect(layout).toContain("export interface FavNode");
    expect(layout).not.toContain("interface FavLayoutGroup");
    expect(layout).toContain("target: string | null");
  });

  // §6 — the promise that a user who never nests anything never grows a page.
  it("materializes the page only for something config.edn cannot express", () => {
    expect(arrangement).toContain("a label, or a row nested under another");
    expect(store).toContain("node.target === null || node.children.length > 0");
  });

  // §3a — depth is measured from the grab point, which is what keeps a plain
  // vertical drag from nesting by accident.
  it("measures drop depth from where the drag started", () => {
    expect(arrangement).toContain("from where the drag\nstarted");
    expect(sidebar).toContain("requestedDepth(rows: FavRow[], from: number, dx: number)");
    expect(readFileSync("src/components/rowReorder.ts", "utf8")).toContain(
      "dx: ev.clientX - startX"
    );
  });
});

describe("config live-reload contract matches the source", () => {
  it("names the separate queue configuration events use", () => {
    expect(reload).toContain("`Pending::config_paths`");
    expect(watcher).toContain("config_paths: HashSet<PathBuf>");
    expect(watcher).toContain("fn path_is_config_file_name");
  });

  it("names the two digests the cheapness gate compares", () => {
    expect(reload).toContain("`Graph::open_config_description()`");
    expect(reload).toContain("`model::config_file_description(root)`");
    expect(model).toContain("pub fn open_config_description(&self)");
    expect(model).toContain("pub fn config_file_description(root: &Path)");
  });

  it("routes every settings write through one funnel that records it", () => {
    expect(reload).toContain("`Graph::write_config` is therefore the single funnel");
    // No setter may go around it, or the watcher stops being able to tell
    // Tine's own write from an outside one. Count the lock-taking write at any
    // module path and anywhere in the crate: K5 moved `atomic_update` out of
    // `model`, and a pattern spelled with the old path went blind (I-11).
    const crateRoot = "crates/tine-core/src";
    const crate = (readdirSync(crateRoot, { recursive: true }) as string[])
      .filter((file) => file.endsWith(".rs"))
      .sort()
      .map((file) => readFileSync(join(crateRoot, file), "utf8"))
      .join("\n");
    expect([...crate.matchAll(/\batomic_update\([^)]*&CONFIG_LOCK\b/g)]).toHaveLength(1);
    const funnelStart = model.indexOf("fn write_config(");
    expect(funnelStart).toBeGreaterThan(-1);
    const funnel = model.slice(funnelStart, model.indexOf("\n    }\n", funnelStart));
    expect(funnel).toMatch(/\batomic_update\(path, &CONFIG_LOCK\b/);
    expect(watcher).toContain("lease.recent_config_write() == disk");
  });

  it("keeps GraphMeta comparable, which is what suppresses a no-op announcement", () => {
    expect(reload).toContain("`GraphMeta` derives `PartialEq`");
    expect(model).toContain("#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]\npub struct GraphMeta");
    expect(watcher).toContain("if after != before {");
  });

  it("keeps ONE producer of config-derived frontend state", () => {
    expect(reload).toContain("exactly **one** producer");
    expect(graph).toContain("export function applyConfigDerivedState");
    // The graph-open path must go through it too, rather than re-implementing.
    expect(graph).toContain("applyConfigDerivedState(meta, null)");
  });

  it("never blocks the watcher thread on the storage transition lane", () => {
    expect(reload).toContain("RefreshOutcome::Deferred");
    expect(watcher).toContain("RefreshLaneWait::TryOnce");
    expect(readFileSync("src-tauri/src/state.rs", "utf8")).toContain(
      "RefreshLaneWait::TryOnce => match transition_gate.try_lock()"
    );
  });
});
