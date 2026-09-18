import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import {
  classifyNativeCallError,
  diagnosticFailureReason,
  DirectSaveFailureError,
  OperationCancelledError,
  QueryNotReadyError,
  QueryUnavailableError,
  SaveConflictError,
} from "./backend";
import { isRetryableSaveFailure, isSaveConflictFailure } from "./persistence";

// Every native rejection is classified once at the frontend funnel. Direct
// save codes and conflict epochs arrive as tagged fields, never prose.

describe("native call error funnel", () => {
  it("classifies the exact legacy literals emitted by the phase-A producer manifest", () => {
    const conflict = '{"detail":{"epoch":7,"io_error_kind":"AlreadyExists"},"kind":"save-conflict","reason_code":"conflict.base_rev"}';
    const failure = '{"detail":{"io_error_kind":"PermissionDenied"},"kind":"direct-save-failure","reason_code":"unknown"}';

    const classifiedConflict = classifyNativeCallError(conflict);
    expect(classifiedConflict).toBeInstanceOf(SaveConflictError);
    expect(classifiedConflict).toMatchObject({
      reasonCode: "conflict.base_rev",
      epoch: 7,
    });
    const classifiedFailure = classifyNativeCallError(failure);
    expect(classifiedFailure).toBeInstanceOf(DirectSaveFailureError);
    expect(classifiedFailure).toMatchObject({
      reasonCode: "unknown",
      ioErrorKind: "PermissionDenied",
    });
    expect(classifyNativeCallError("query-too-large: 2049 bytes")).toBe("query-too-large: 2049 bytes");
    expect(classifyNativeCallError("legacy prose")).toBe("legacy prose");
  });

  it("types the tagged conflict payload and passes prose through untouched", () => {
    const typed = classifyNativeCallError(JSON.stringify({
      kind: "save-conflict",
      reason_code: "conflict.base_rev",
      detail: { io_error_kind: "AlreadyExists", epoch: 7 },
    }));
    expect(typed).toBeInstanceOf(SaveConflictError);
    expect((typed as SaveConflictError).epoch).toBe(7);
    expect(typed).toMatchObject({ reasonCode: "conflict.base_rev" });

    const other = new Error("ordinary prose containing conflict");
    expect(classifyNativeCallError(other)).toBe(other);
    expect(classifyNativeCallError("conflict")).toBe("conflict");
    expect(classifyNativeCallError("conflict:7 suffix")).toBe("conflict:7 suffix");
  });

  it("routes a page-title collision by the producer code rather than conflict prose", () => {
    const failure = classifyNativeCallError(JSON.stringify({
      kind: "direct-save-failure",
      reason_code: "precheck.symlink",
      detail: {
        io_error_kind: "InvalidInput",
        message: "pages/path-pinned page does not match its captured exact owner.md is a symlink",
      },
    }));

    expect(failure).toMatchObject({
      kind: "direct-save-failure",
      reasonCode: "precheck.symlink",
    });
    expect(isRetryableSaveFailure(failure)).toBe(false);
    expect(isSaveConflictFailure(failure)).toBe(false);

    const conflict = classifyNativeCallError(JSON.stringify({
      kind: "save-conflict",
      reason_code: "conflict.pinned_owner",
      detail: { io_error_kind: "AlreadyExists", epoch: 19 },
    }));
    expect(conflict).toBeInstanceOf(SaveConflictError);
    expect(conflict).toMatchObject({ epoch: 19 });
  });

  it("is applied once, inside TauriBackend.call, and by no individual command wrapper", () => {
    const source = readFileSync("src/backend.ts", "utf8");
    const callStart = source.indexOf("  private async call<");
    expect(callStart).toBeGreaterThan(0);
    const callEnd = source.indexOf("\n  }\n", callStart);
    const call = source.slice(callStart, callEnd);
    expect(call).toContain("throw classifyNativeCallError(error);");

    const production = source.slice(0, source.indexOf("export function classifyNativeCallError"));
    const rest = source.slice(callEnd);
    for (const chunk of [production, rest]) {
      expect(chunk).not.toContain("classifySaveConflictWire(");
      expect(chunk).not.toContain("classifyNativeCallError(");
    }
  });
});

// GH #543: a reporter's diagnostic recorded 7,520 failed `run_query` calls
// with no reason, so nobody could tell a retryable wait for the index from a
// terminally unavailable projection. The classification must be a short fixed
// code, and it must never be the thrown value's own text — an arbitrary
// message can carry a path or graph content into a published report.
describe("diagnostic failure reason", () => {
  it("names which waiting state a refused query was in", () => {
    expect(diagnosticFailureReason(new QueryNotReadyError("indexing"))).toBe(
      "not-ready:indexing"
    );
    expect(diagnosticFailureReason(new QueryNotReadyError("recovering"))).toBe(
      "not-ready:recovering"
    );
  });

  it("separates a terminal failure and a cancellation from a wait", () => {
    expect(
      diagnosticFailureReason(new QueryUnavailableError("worker-stopped", "gone"))
    ).toBe("unavailable:worker-stopped");
    expect(diagnosticFailureReason(new OperationCancelledError())).toBe("cancelled");
  });

  it("never reports a thrown value's own text", () => {
    const leaky = new Error("/home/someone/graph/pages/Private Page.md is missing");
    expect(diagnosticFailureReason(leaky)).toBe("other");
    expect(diagnosticFailureReason("/home/someone/graph")).toBe("other");
    expect(diagnosticFailureReason({ message: "secret" })).toBe("other");
  });
});
