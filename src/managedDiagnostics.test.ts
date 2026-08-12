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

  it("preserves a join failure while redacting an internal provider path", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at provider scan: sync actor refused request: unsafe provider entry: objects/0123456789abcdef.object",
    )).toBe(
      "managed sync join failed at provider scan: sync actor refused request: unsafe provider entry: [provider path]",
    );
  });

  it("preserves a disappeared manifest diagnosis without exposing its identifier", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at provider scan: provider evidence disappeared during join at manifests/private-id.manifest",
    )).toBe(
      "managed sync join failed at provider scan: provider evidence disappeared during join at [provider path]",
    );
  });
});
