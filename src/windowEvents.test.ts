import { readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";

// GH #543, audit R10-05: Tauri hands a listener registered with target `Any`
// every window's copy of an event addressed to one window. A window-scoped
// event is heard only through `listenHere` (src/windowEvents.ts). An event
// listened to with a bare `listen` must be a broadcast whose payload names
// its target, or one only the singleton capture window hears.
const BROADCASTS: Record<string, string> = {
  "graph-rescan-complete": "app.emit; each window matches the sequence it requested",
  "graph-verification-progress": "app.emit; the payload carries the operation id",
  "quick-capture": "sent by the capture window; the payload names the target window",
  "capture-request-theme": "sent by the capture window; the payload names the target window",
  "capture-request-shortcuts": "sent by the capture window; the payload names the target window",
  "quick-capture-ack": "heard only by the singleton capture window",
  "capture-apply-theme": "heard only by the singleton capture window",
  "capture-apply-shortcuts": "heard only by the singleton capture window",
  "capture-shown": "heard only by the singleton capture window",
  "capture-focus-editor": "heard only by the singleton capture window",
  "history-navigate": "window.emit broadcasts it (native_mouse_history.rs); the payload names the target window",
};

function files(dir: string, suffixes: string[]): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) return files(path, suffixes);
    return suffixes.some((suffix) => name.endsWith(suffix)) ? [path] : [];
  });
}

describe("window-addressed events (GH #543, R10-05)", () => {
  it("are heard only through listenHere", () => {
    const bare: string[] = [];
    for (const path of files("src", [".ts", ".tsx"])) {
      if (/\.test\.tsx?$/.test(path)) continue;
      const source = readFileSync(path, "utf8");
      // `\(\s*"`: a long type argument makes the formatter put the event
      // name on the next line, which `\("` never saw (audit R11-11).
      for (const match of source.matchAll(/\blisten(?:<(?:[^<>]|<[^<>]*>)*>)?\(\s*"([^"]+)"/g)) {
        if (!(match[1] in BROADCASTS)) bare.push(`${path}: ${match[1]}`);
      }
    }
    expect(
      bare,
      "listen to a window-scoped event with listenHere (src/windowEvents.ts); a bare listen hears every window's copy",
    ).toEqual([]);
  });

  it("are never sent to one graph window under a broadcast name", () => {
    const targeted = new Set<string>();
    for (const path of files("src-tauri/src", [".rs"])) {
      const source = readFileSync(path, "utf8");
      for (const match of source.matchAll(/emit_to\(\s*([^,]+),\s*"([^"]+)"/g)) {
        if (match[1].trim() !== '"capture"') targeted.add(match[2]);
      }
    }
    expect(targeted.size).toBeGreaterThan(5);
    expect([...targeted].filter((name) => name in BROADCASTS)).toEqual([]);
  });
});

describe("listenHere", () => {
  it("registers with this window's own target, never Any", async () => {
    vi.resetModules();
    const listen = vi.fn(async () => () => {});
    vi.doMock("@tauri-apps/api/event", () => ({ listen }));
    vi.doMock("@tauri-apps/api/webviewWindow", () => ({
      getCurrentWebviewWindow: () => ({ label: "graph-2" }),
    }));
    const { listenHere } = await import("./windowEvents");
    await listenHere("warm-cache-done", () => {});
    expect(listen).toHaveBeenCalledWith("warm-cache-done", expect.any(Function), {
      target: { kind: "WebviewWindow", label: "graph-2" },
    });
    vi.doUnmock("@tauri-apps/api/event");
    vi.doUnmock("@tauri-apps/api/webviewWindow");
  });
});
