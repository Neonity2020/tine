import { describe, expect, it } from "vitest";
import { failureShape, hash64 } from "./failureShape";
import { parserDiagnostic } from "./devtools/lsdoc-diff/diagnostic";

const MARKER = "planted-failure-marker-Zq7Page";

describe("failureShape", () => {
  it("keeps no field a message can reach", () => {
    const shape = failureShape(new TypeError(`saving ${MARKER}.md failed`));
    expect(Object.keys(shape).sort()).toEqual(["bytes", "hash", "kind"]);
    expect(JSON.stringify(shape)).not.toContain(MARKER);
    // I-9: the failure is still identifiable — its type, its size, its identity.
    expect(shape.kind).toBe("TypeError");
    expect(shape.bytes).toBe(new TextEncoder().encode(`saving ${MARKER}.md failed`).length);
    expect(shape.hash).toMatch(/^[0-9a-f]{16}$/);
  });

  it("distinguishes failures and repeats itself", () => {
    const first = failureShape(new Error("one"));
    expect(failureShape(new Error("one")).hash).toBe(first.hash);
    expect(failureShape(new Error("two")).hash).not.toBe(first.hash);
  });

  it("takes a rejected command's bare string without echoing it", () => {
    // The shape a rejected Tauri command actually arrives in: a Rust error
    // string, which is where page names and graph paths live.
    const shape = failureShape(`no such page: ${MARKER}`);
    expect(shape.kind).toBe("string");
    expect(JSON.stringify(shape)).not.toContain(MARKER);
  });

  it("refuses a kind that is not a code identifier", () => {
    const named = new Error("boom");
    // `Error.name` is writable, and a class name is the only thing `kind` may
    // ever be — otherwise a caller could smuggle content through it.
    Object.defineProperty(named.constructor, "name", { value: `class ${MARKER}` });
    expect(failureShape(named).kind).toBe("object");
    expect(failureShape(Object.create(null)).kind).toBe("object");
    expect(failureShape(null).kind).toBe("null");
    expect(failureShape(undefined).kind).toBe("undefined");
  });

  it("survives an object whose rendering throws", () => {
    const hostile = {
      toString() {
        throw new Error("nope");
      },
    };
    expect(() => failureShape(hostile)).not.toThrow();
    expect(failureShape(hostile).kind).toBe("Object");
  });

  it("is the only hash the parser diagnostic uses (I-12)", () => {
    expect(parserDiagnostic("input").inputHash).toBe(hash64("input"));
  });
});
