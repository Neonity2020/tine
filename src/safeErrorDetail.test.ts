import { describe, expect, it } from "vitest";
import { safeErrorDetail } from "./safeErrorDetail";

describe("safe error detail", () => {
  it("renders a tagged clean-open envelope instead of discarding it", () => {
    expect(
      safeErrorDetail('{"kind":"clean-open","reason_code":"clean_open.io"}'),
    ).toBe("clean-open failure: clean_open.io");
  });

  it("keeps the refusal scenario when the envelope carries one", () => {
    expect(
      safeErrorDetail(
        '{"kind":"clean-open","reason_code":"clean_open.projection_store","detail":{"scenario":"MS-REF-PROTOCOL-INCOMPATIBLE"}}',
      ),
    ).toBe("clean-open failure: clean_open.projection_store (MS-REF-PROTOCOL-INCOMPATIBLE)");
  });

  it("sanitizes an envelope whose fields are not the closed backend vocabulary", () => {
    // Only `kind` and `reason_code` drawn from the closed vocabulary bypass the
    // prose sanitizer. A payload carrying graph text takes the ordinary path.
    const rendered = safeErrorDetail(
      '{"kind":"clean-open","reason_code":"pages/My private proposal.md"}',
    );
    expect(rendered).not.toContain("My private proposal");
    expect(rendered).not.toContain("clean_open");
  });

  it("preserves the reason while redacting a graph-relative Markdown path", () => {
    expect(safeErrorDetail(
      "pages/My private proposal.md: external Markdown source is read-only because parsing and reserialization change its block structure",
    )).toBe(
      "[path]: external Markdown source is read-only because parsing and reserialization change its block structure",
    );
  });

  it("redacts nested relative source paths inside a structured activation reason", () => {
    expect(safeErrorDetail(
      "page import failed: notes/private area/plan.org: parser rejected source",
    )).toBe("page import failed: [path]: parser rejected source");
  });

  // Martin's phone, Aug 18: a failure whose detail named a graph-relative path
  // was discarded whole and shown as "The command failed without a safe
  // diagnostic detail" — the redactor recognized absolute paths only, so the
  // surviving "/" tripped the structural reject at the end.
  it("redacts a graph-relative path instead of discarding the message", () => {
    expect(safeErrorDetail(
      "graph open failed at directory scan: .recycle/pages/outbox: Invalid argument (os error 22)",
    )).toBe(
      "graph open failed at directory scan: [path]: Invalid argument (os error 22)",
    );
  });

  // Attributing every refusal site is what cracked the Android save defect.
  // Blanket-redacting quoted text made every attributed refusal read alike; a
  // bare snake_case identifier cannot carry graph text.
  it("keeps an attributed refusal site, which is authored, not user data", () => {
    expect(safeErrorDetail(
      'graph open failed at projection scan: RefusedAt("require_projection_parent_present")',
    )).toBe(
      'graph open failed at projection scan: RefusedAt("require_projection_parent_present")',
    );
  });

  it("still redacts quoted text that is not an authored identifier", () => {
    expect(safeErrorDetail(
      'clean external reconciliation refused: page "My Private Page Title" is already owned',
    )).toBe(
      'clean external reconciliation refused: page "[redacted]" is already owned',
    );
  });

  // Martin's phone, Aug 19: a refusal was ONCE MORE reduced to "The command
  // failed without a safe diagnostic detail". The structural check was
  // all-or-nothing, so any residue it could not vouch for cost the whole
  // sentence — and the stage name in front of that residue is the entire
  // diagnostic value. Redact the token, keep the sentence.
  it("keeps the failing stage when a debug-formatted value trails it", () => {
    expect(safeErrorDetail(
      'graph open failed at directory scan: Os { code: 13, kind: PermissionDenied, message: "Permission denied" }',
    )).toBe("graph open failed at directory scan: Os [details]");
  });

  it("keeps the failing stage when an unvouched token trails it", () => {
    expect(safeErrorDetail(
      "graph open failed at projection reopen: expected <ProjectionDescriptor>, found nothing",
    )).toBe("graph open failed at projection reopen: expected [details], found nothing");
  });

  it("redacts one opaque token rather than the sentence carrying it", () => {
    expect(safeErrorDetail(
      `graph open failed at projection scan: unreadable marker ${"a".repeat(120)}`,
    )).toBe("graph open failed at projection scan: unreadable marker [redacted]");
  });

  // A page path is the one path whose last segment routinely contains spaces,
  // and every whitespace-terminated rule leaked the remainder of the name.
  it("redacts a page path whose last segment contains spaces", () => {
    expect(safeErrorDetail(
      "join failed reading /home/someone/graph/pages/Some Private Page.md",
    )).toBe("join failed reading [path]");
  });
});
