import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import type { SparseV2QueryReply } from "./types";

describe("experimental sparse-v2 app boundary", () => {
  const backend = readFileSync("src/backend.ts", "utf8");
  const settings = readFileSync("src/components/Settings.tsx", "utf8");
  const app = readFileSync("src/App.tsx", "utf8");
  const native = readFileSync("src-tauri/src/lib.rs", "utf8");

  it("is explicit-only and refreshes the graph binding after authority transfer", () => {
    expect(settings).toContain("Enable experimental sparse-v2 storage for this graph?");
    expect(settings).toContain("never default-enabled");
    expect(backend).toMatch(
      /activateSparseV2\(\)[\s\S]*activate_sparse_v2[\s\S]*this\.bindingGeneration = result\.binding_generation/
    );
  });

  it("registers only bounded actor commands for the vertical slice", () => {
    for (const command of [
      "sparse_v2_status",
      "activate_sparse_v2",
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
    expect(settings).toContain("Prepare sharing…");
    expect(settings).toContain("Join shared sparse v2…");
    expect(settings).toContain("single enrollment descriptor last");
    expect(settings).toContain("Independent or dirty local history is never auto-merged");
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

  it("keeps the window open when sparse clean shutdown refuses Safe", () => {
    expect(app).toContain("sparse-v2-shutdown-refused");
    expect(app).toContain("The window remains open so you can retry");
    expect(app).toMatch(
      /sparse-v2-shutdown-refused[\s\S]*allowClose = false;[\s\S]*safeClose\.reset\(\);[\s\S]*return;/
    );
  });
});
