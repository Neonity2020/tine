import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import type { SparseV2QueryReply } from "./types";

describe("Tine-managed storage app boundary", () => {
  const backend = readFileSync("src/backend.ts", "utf8");
  const settings = readFileSync("src/components/Settings.tsx", "utf8");
  const app = readFileSync("src/App.tsx", "utf8");
  const native = readFileSync("src-tauri/src/lib.rs", "utf8");

  it("is explicit-only and reacts once to the native terminal receipt", () => {
    expect(settings).toContain("Enable Tine-managed storage for this graph?");
    expect(backend).toMatch(
      /activateSparseV2\(\)[\s\S]*activate_sparse_v2[\s\S]*this\.bindingGeneration = result\.binding_generation/
    );
    expect(backend).toMatch(
      /cancelSparseV2\(\)[\s\S]*cancel_sparse_v2[\s\S]*this\.bindingGeneration = result\.binding_generation/
    );
    expect(settings).not.toContain("captureAuthorityReadiness");
    expect(settings).not.toContain("expectedPages");
    expect(settings).not.toContain("refreshAuthorityState");
    expect(settings).not.toContain("getPageByPath(representative.path)");
    expect(settings).not.toContain("enableStage");
    expect(settings).not.toContain("no new phase for");
    expect(settings).toContain("storageTransitionRuntime.active()");
    expect(settings).toContain("managedStorageRuntime.acceptNativeTransition(result)");
    expect(settings).toContain("retained_runtime_projection_repair");
  });

  it("registers only bounded actor commands for the vertical slice", () => {
    for (const command of [
      "sparse_v2_status",
      "activate_sparse_v2",
      "cancel_sparse_v2",
      "prepare_sparse_v2_share",
      "join_sparse_v2_shared",
      "adopt_sparse_v2_shared",
      "sparse_v2_recovery_location",
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
      "Join a synced graph from another device...",
      "Join this synced graph...",
      "Adopt the graph your other device is sharing?",
      "Return to Direct files",
    ]) {
      expect(settings).toContain(copy);
    }
    // Internal vocabulary must never reach the panel's copy. Three references
    // are not copy and are allowed by name: the on-disk path a joining device
    // waits for (a user has to look for that exact file), the constant holding
    // it, and the native function whose message the panel re-authors.
    const copy = settings
      .replaceAll(".tine-sync/v2/shared/outbox/enrollment/shared-enrollment-v1.json", "")
      .replaceAll("SHARED_ENROLLMENT_RELATIVE_PATH", "")
      .replaceAll("shared_enrollment_not_here_yet", "");
    expect(copy).not.toMatch(/sparse v2|sparse-v2|enrollment/i);
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
      {
        kind: "search_building",
        value: { horizon_sequence: 12 },
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
      {
        kind: "search_building",
        value: { horizon_sequence: 12 },
      },
    ]);
  });

  it("keeps the window open when managed storage cannot stop safely", () => {
    expect(app).toContain("SparseShutdownRefusedError");
    expect(app).toContain("Tine-managed storage could not verify a clean stop.");
    expect(app).toContain("The window remains open so you can retry");
    expect(app).toMatch(
      /instanceof SparseShutdownRefusedError[\s\S]*allowClose = false;[\s\S]*safeClose\.reset\(\);[\s\S]*return;/
    );
  });
});
