import { describe, expect, it } from "vitest";
import { managedJoinErrorDetail, safeManagedErrorDetail } from "./managedDiagnostics";

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

  it("keeps the content-free join summary and drops private path detail lines", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at provider scan: sync actor refused request: sync join refused: notes not in the shared provider frontier; local-pages=1045 shared-pages=1045 local-only=0 shared-only=0 changed=1 (kind=0 preamble=0 outline=0 explicit-ids=1); authorities unchanged\nclean join mismatch detail: changed path=\"pages/Private Page.md\" categories=explicit-ids",
    )).toBe(
      "managed sync join failed at provider scan: sync actor refused request: sync join refused: notes not in the shared provider frontier; local-pages=1045 shared-pages=1045 local-only=0 shared-only=0 changed=1 (kind=0 preamble=0 outline=0 explicit-ids=1); authorities unchanged",
    );
  });

  it("shows bounded clean-join paths only through the join-specific local formatter", () => {
    const error = new Error(
      "managed sync join failed at provider scan: sync actor refused request: sync join refused: notes not in the shared provider frontier; local-pages=1059 shared-pages=1059 local-only=0 shared-only=0 changed=1 (kind=0 preamble=0 outline=1 explicit-ids=0); authorities unchanged\n"
      + 'clean join mismatch detail: changed path="journals/2026_08_25.md" categories=outline',
    );

    expect(managedJoinErrorDetail(error)).toEqual({
      visible: expect.stringContaining(
        'Affected note: Changed (blocks, content, or order): "journals/2026_08_25.md"',
      ),
      copy: expect.stringContaining(
        '- Changed (blocks, content, or order): "journals/2026_08_25.md"',
      ),
    });
    expect(safeManagedErrorDetail(error)).not.toContain("2026_08_25");
  });

  it("does not expose arbitrary continuation lines as join paths", () => {
    const error = new Error(
      "managed sync join failed at provider scan: permission denied\n"
      + 'clean join mismatch detail: changed path="pages/private.md" categories=outline',
    );
    const detail = managedJoinErrorDetail(error);
    expect(detail.visible).not.toContain("private.md");
    expect(detail.copy).not.toContain("private.md");
  });

  it("keeps a long affected-path list compact on screen and complete when copied", () => {
    const lines = Array.from(
      { length: 5 },
      (_, index) => `clean join mismatch detail: local-only path="pages/page-${index}.md"`,
    );
    const detail = managedJoinErrorDetail(
      `sync join refused: notes not in the shared provider frontier\n${lines.join("\n")}`,
    );
    expect(detail.visible).toContain("2 more in Copy details");
    expect(detail.visible).not.toContain("page-4.md");
    expect(detail.copy).toContain("page-4.md");
  });

  it("preserves a disappeared manifest diagnosis without exposing its identifier", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at provider scan: provider evidence disappeared during join at manifests/private-id.manifest",
    )).toBe(
      "managed sync join failed at provider scan: provider evidence disappeared during join at [provider path]",
    );
  });

  // Martin's phone, Aug 18: a join failure whose detail named a graph-relative
  // provider path was discarded whole and shown as "The command failed without
  // a safe diagnostic detail" — the redactor recognized absolute paths only,
  // so the surviving "/" tripped the structural reject at the end.
  it("redacts a graph-relative provider path instead of discarding the message", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at provider discovery: .tine-sync/v2/shared/outbox: Invalid argument (os error 22)",
    )).toBe(
      "managed sync join failed at provider discovery: [path]: Invalid argument (os error 22)",
    );
  });

  // Attributing every refusal site is what cracked the Android save defect.
  // Blanket-redacting quoted text made every attributed refusal read alike; a
  // bare snake_case identifier cannot carry graph text.
  it("keeps an attributed refusal site, which is authored, not user data", () => {
    expect(safeManagedErrorDetail(
      'managed sync join failed at provider scan: ActorRefusedAt("require_pending_publication_absent")',
    )).toBe(
      'managed sync join failed at provider scan: ActorRefusedAt("require_pending_publication_absent")',
    );
  });

  it("still redacts quoted text that is not an authored identifier", () => {
    expect(safeManagedErrorDetail(
      'clean external reconciliation refused: page "My Private Page Title" is already owned',
    )).toBe(
      'clean external reconciliation refused: page "[redacted]" is already owned',
    );
  });

  // Martin's phone, Aug 19: a join refusal was ONCE MORE reduced to "The
  // command failed without a safe diagnostic detail". The structural check was
  // all-or-nothing, so any residue it could not vouch for cost the whole
  // sentence — and the stage name in front of that residue is the entire
  // diagnostic value. Redact the token, keep the sentence.
  it("keeps the failing stage when a debug-formatted value trails it", () => {
    expect(safeManagedErrorDetail(
      'managed sync join failed at provider discovery: Os { code: 13, kind: PermissionDenied, message: "Permission denied" }',
    )).toBe("managed sync join failed at provider discovery: Os [details]");
  });

  it("keeps the failing stage when an unvouched token trails it", () => {
    expect(safeManagedErrorDetail(
      "managed sync join failed at runtime reopen: expected <SharedDescriptor>, found nothing",
    )).toBe("managed sync join failed at runtime reopen: expected [details], found nothing");
  });

  it("redacts one opaque token rather than the sentence carrying it", () => {
    expect(safeManagedErrorDetail(
      `managed sync join failed at provider scan: unreadable marker ${"a".repeat(120)}`,
    )).toBe("managed sync join failed at provider scan: unreadable marker [redacted]");
  });

  // A page path is the one path whose last segment routinely contains spaces,
  // and every whitespace-terminated rule leaked the remainder of the name.
  it("redacts a page path whose last segment contains spaces", () => {
    expect(safeManagedErrorDetail(
      "join failed reading /home/someone/graph/pages/Some Private Page.md",
    )).toBe("join failed reading [path]");
  });
});
