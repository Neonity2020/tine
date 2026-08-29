import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  graphSessionUiStateRegistry,
  parsePersistedPdfTarget,
  UI_STATE_LIFETIMES,
} from "./uiStateRegistry";

describe("UI state lifetime registry", () => {
  it("keeps the living contract aligned with the typed session registry", () => {
    const contract = readFileSync(
      fileURLToPath(new URL("../docs/contracts/ui-state-lifetimes.md", import.meta.url)),
      "utf8",
    );

    for (const lifetime of UI_STATE_LIFETIMES) expect(contract).toContain(`\`${lifetime}\``);
    for (const [field, decision] of Object.entries(graphSessionUiStateRegistry)) {
      expect(contract).toContain(`\`${field}\``);
      expect(contract).toContain(`\`${decision.owner}\``);
      expect(contract).toContain(`\`${decision.lifetime}\``);
    }
  });

  it("accepts only bounded stable PDF identity", () => {
    expect(parsePersistedPdfTarget({
      filename: "assets/paper.pdf",
      label: "Paper",
      owner: { graphRoot: "/private", generation: 8 },
      page: 12,
      highlightId: "hl",
    })).toEqual({ filename: "assets/paper.pdf", label: "Paper" });
    expect(parsePersistedPdfTarget({ filename: "assets/paper.pdf", label: 12 })).toBeNull();
    expect(parsePersistedPdfTarget({ filename: "", label: "Paper" })).toBeNull();
    expect(parsePersistedPdfTarget(null)).toBeNull();
  });
});
