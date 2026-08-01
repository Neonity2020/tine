import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = path.resolve(import.meta.dirname, "..");
const config = JSON.parse(fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8"));
const capability = JSON.parse(fs.readFileSync(path.join(root, "src-tauri/capabilities/default.json"), "utf8"));
const main = fs.readFileSync(path.join(root, "src/main.tsx"), "utf8");
const native = fs.readFileSync(path.join(root, "src-tauri/src/lib.rs"), "utf8");
const app = fs.readFileSync(path.join(root, "src/App.tsx"), "utf8");

describe("stable desktop startup reveal (GH #132)", () => {
  it("starts the main window hidden and reveals it after a stable themed frame", () => {
    expect(config.app.windows.find((window: { label: string }) => window.label === "main")?.visible).toBe(false);
    expect(capability.permissions).toContain("core:window:allow-show");
    expect(main).toContain("revealMainWindowAfterStableFrame");
    expect(main).toContain("queueMicrotask");
    expect(main.indexOf("applyTheme();")).toBeLessThan(main.indexOf("revealMainWindowAfterStableFrame"));
  });

  it("has a bounded native fallback so frontend failure cannot leave an invisible app", () => {
    expect(native).toContain("MAIN_WINDOW_REVEAL_FALLBACK_MS");
    expect(native).toContain('get_webview_window("main")');
    expect(native).toContain("window.show()");
  });

  it("lets the themed window paint before a configured graph can enter managed recovery", () => {
    const setupStart = native.indexOf(".setup(|app|");
    const setupEnd = native.indexOf(".invoke_handler", setupStart);
    const setup = native.slice(setupStart, setupEnd);

    expect(setupStart).toBeGreaterThanOrEqual(0);
    expect(setupEnd).toBeGreaterThan(setupStart);
    expect(setup).not.toContain("load_graph_for_label");
    expect(setup).toContain("defers graph open to the visible webview");
    expect(app).toContain("Opening graph storage…");
  });

  it("keeps frontend-triggered graph recovery off the native command thread", () => {
    const graph = fs.readFileSync(path.join(root, "src-tauri/src/graph.rs"), "utf8");
    const loadStart = graph.indexOf("pub(crate) async fn load_graph(");
    const loadEnd = graph.indexOf("pub(crate) fn load_graph_for_label", loadStart);
    const load = graph.slice(loadStart, loadEnd);

    expect(loadStart).toBeGreaterThanOrEqual(0);
    expect(loadEnd).toBeGreaterThan(loadStart);
    expect(load).toContain("tauri::async_runtime::spawn_blocking");
    expect(load).toContain("get_webview_window(&label).is_none()");
    expect(load).toContain("slot.binding_generation == binding_generation");
  });
});
