import { describe, expect, it } from "vitest";
import { safeManagedErrorDetail } from "./managedDiagnostics";

describe("managed-storage diagnostics", () => {
  it("preserves the reason while redacting a graph-relative Markdown path", () => {
    expect(safeManagedErrorDetail(
      "pages/My private proposal.md: external Markdown source is read-only because parsing and reserialization change its block structure",
    )).toBe(
      "[path]: external Markdown source is read-only because parsing and reserialization change its block structure",
    );
  });

  it("redacts nested relative source paths inside a structured activation reason", () => {
    expect(safeManagedErrorDetail(
      "shadow import failed: notes/private area/plan.org: parser rejected source",
    )).toBe("shadow import failed: [path]: parser rejected source");
  });
});
