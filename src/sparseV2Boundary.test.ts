import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import type { SparseV2QueryReply } from "./types";

describe("Tine-managed storage app boundary", () => {
  const backend = readFileSync("src/backend.ts", "utf8");
  const settings = readFileSync("src/components/Settings.tsx", "utf8");
  const app = readFileSync("src/App.tsx", "utf8");
  const native = readFileSync("src-tauri/src/lib.rs", "utf8");

  it("is explicit-only and refreshes the graph binding after setup", () => {
    expect(settings).toContain("Enable Tine-managed storage for this graph?");
    expect(backend).toMatch(
      /activateSparseV2\(\)[\s\S]*activate_sparse_v2[\s\S]*this\.bindingGeneration = result\.binding_generation/
    );
    expect(backend).toMatch(
      /cancelSparseV2\(\)[\s\S]*cancel_sparse_v2[\s\S]*this\.bindingGeneration = result\.binding_generation/
    );
  });

  it("registers only bounded actor commands for the vertical slice", () => {
    for (const command of [
      "sparse_v2_status",
      "activate_sparse_v2",
      "cancel_sparse_v2",
      "prepare_sparse_v2_share",
      "join_sparse_v2_shared",
      "sparse_v2_query",
      "sparse_v2_editor_load",
      "sparse_v2_editor_save",
      "sparse_v2_tick",
      "sparse_v2_clean_shutdown",
    ]) {
      expect(native).toContain(command);
    }
    for (const copy of [
      "Storage & sync",
      "Enable Tine-managed storage...",
      "Retry setup",
      "Tine-managed storage active",
      "Set up sync with another device...",
      "Join an existing synced graph...",
      "Join this synced graph...",
      "Return to Direct files",
    ]) {
      expect(settings).toContain(copy);
    }
    expect(settings).not.toMatch(/sparse v2|sparse-v2|enrollment/i);
  });

  it("models the adjacent-tagged query reply wire shape", () => {
    const replies = [
      {
        kind: "page_name",
        value: { status: "missing" },
      },
      {
        kind: "search",
        value: [
          {
            entity: { entity_type: "block", id: "block-opaque" },
            page_id: "page-opaque",
            text: "exact wire",
            rank: -0.25,
          },
        ],
      },
    ] satisfies SparseV2QueryReply[];

    expect(replies).toEqual([
      { kind: "page_name", value: { status: "missing" } },
      {
        kind: "search",
        value: [
          {
            entity: { entity_type: "block", id: "block-opaque" },
            page_id: "page-opaque",
            text: "exact wire",
            rank: -0.25,
          },
        ],
      },
    ]);
  });

  it("keeps the window open when managed storage cannot stop safely", () => {
    expect(app).toContain("sparse-v2-shutdown-refused");
    expect(app).toContain("Tine-managed storage could not verify a clean stop.");
    expect(app).toContain("The window remains open so you can retry");
    expect(app).toMatch(
      /sparse-v2-shutdown-refused[\s\S]*allowClose = false;[\s\S]*safeClose\.reset\(\);[\s\S]*return;/
    );
  });
});
