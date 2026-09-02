import { beforeEach, describe, expect, it, vi } from "vitest";
import { backend } from "../backend";
import {
  initThemePackages,
  installThemePackage,
  installedThemes,
} from "./manager";

function manifest(id: string) {
  return {
    schemaVersion: 1 as const,
    id,
    name: id,
    version: "1.0.0",
    apiVersion: "0.1" as const,
    description: "Concurrent persistence fixture.",
    author: "Tine",
    license: "MIT",
    source: `https://example.invalid/${id}`,
    modes: { dark: { "--ls-primary-background-color": "#010203" } },
    screenshots: [],
  };
}

describe("theme package persistence", () => {
  beforeEach(async () => {
    vi.restoreAllMocks();
    vi.spyOn(backend(), "getAppString").mockResolvedValue("[]");
    await initThemePackages();
  });

  it("serializes overlapping mutations of the shared package array", async () => {
    let releaseFirst!: () => void;
    const firstBlocked = new Promise<void>((resolve) => { releaseFirst = resolve; });
    let calls = 0;
    let stored = "[]";
    vi.spyOn(backend(), "setAppString").mockImplementation(async (_key, value) => {
      calls += 1;
      if (calls === 1) await firstBlocked;
      stored = value;
    });

    const first = installThemePackage(manifest("page.tine.theme.concurrent-a"));
    const second = installThemePackage(manifest("page.tine.theme.concurrent-b"));
    await vi.waitFor(() => expect(calls).toBeGreaterThan(0));
    releaseFirst();
    await Promise.all([first, second]);

    const persistedIds = JSON.parse(stored).map((value: { id: string }) => value.id).sort();
    const signalIds = installedThemes().map((theme) => theme.manifest.id).sort();
    expect(persistedIds).toEqual([
      "page.tine.theme.concurrent-a",
      "page.tine.theme.concurrent-b",
    ]);
    expect(signalIds).toEqual(persistedIds);
  });
});
