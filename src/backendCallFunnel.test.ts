import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { classifyNativeCallError, SaveConflictError } from "./backend";

// Wave-2 review H2-1: eight native commands guard a Direct Files write with the
// same `conflict` / `conflict:<epoch>` wire code, but only `savePage` was
// classified, so the resolver's disk-changed recovery branch was dead. The
// classification lives at the ONE frontend funnel and nowhere else.

describe("native call error funnel", () => {
  it("types the exact conflict wire code and passes everything else through untouched", () => {
    const typed = classifyNativeCallError("conflict:7");
    expect(typed).toBeInstanceOf(SaveConflictError);
    expect((typed as SaveConflictError).epoch).toBe(7);
    expect(classifyNativeCallError("conflict")).toMatchObject({ kind: "save-conflict", epoch: null });

    const other = new Error("ordinary prose containing conflict");
    expect(classifyNativeCallError(other)).toBe(other);
    expect(classifyNativeCallError("conflict:7 suffix")).toBe("conflict:7 suffix");
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
