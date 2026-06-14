import { describe, it, expect } from "vitest";
import { parseOutline } from "./outline";

describe("parseOutline", () => {
  it("flat multiline text -> sibling blocks", () => {
    expect(parseOutline("one\ntwo\nthree")).toEqual([
      { raw: "one", children: [] },
      { raw: "two", children: [] },
      { raw: "three", children: [] },
    ]);
  });

  it("skips blank lines in flat text", () => {
    expect(parseOutline("a\n\nb")).toEqual([
      { raw: "a", children: [] },
      { raw: "b", children: [] },
    ]);
  });

  it("bulleted outline with nesting", () => {
    const text = "- parent\n\t- child a\n\t- child b\n- sibling";
    expect(parseOutline(text)).toEqual([
      {
        raw: "parent",
        children: [
          { raw: "child a", children: [] },
          { raw: "child b", children: [] },
        ],
      },
      { raw: "sibling", children: [] },
    ]);
  });

  it("space-indented bullets nest too", () => {
    const text = "- a\n  - b\n    - c";
    expect(parseOutline(text)).toEqual([
      { raw: "a", children: [{ raw: "b", children: [{ raw: "c", children: [] }] }] },
    ]);
  });
});
