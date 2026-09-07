// **A PRESENT but empty property is a statement, in both formats** (P5B).
//
// The query's grouping key distinguishes three states, and the difference
// between two of them is the whole point of the key:
//
//   * absent          — nothing said; a Board may fill it with the task marker;
//   * present, empty  — the user said "no grouping"; a Board may NOT;
//   * present, a field id.
//
// That only works if an empty value survives the round trip through the store's
// one property writer and back through the facet reader, in Markdown AND in Org,
// and stays DISTINGUISHABLE from an absent key. Nothing else in the app depended
// on that before, so it is pinned here rather than assumed.

import { beforeAll, afterEach, describe, expect, it } from "vitest";
import { initParser } from "../render/parse";
import { doc, resetStore, setBlockProperty, setDoc, type FeedPage, type Node } from "../store";
import { facetsOf } from "../render/facets";
import { resolveQueryGrouping } from "./queryViewProperties";

beforeAll(async () => {
  await initParser();
});

afterEach(() => resetStore());

function load(format: "md" | "org") {
  const page: FeedPage = {
    name: "Sheet",
    kind: "page",
    title: "Sheet",
    preBlock: null,
    roots: ["query"],
    format,
    readOnly: false,
    guide: false,
  };
  const node: Node = {
    id: "query",
    raw: "{{query (todo TODO)}}",
    collapsed: false,
    parent: null,
    page: "Sheet",
    children: [],
  };
  setDoc({ byId: { query: node }, pages: [page], feed: ["Sheet"], loaded: true });
}

const properties = () => facetsOf(doc.byId.query.raw, doc.pages[0].format).properties;
const read = (key: string) => properties().find(([k]) => k === key);

describe("an empty query property", () => {
  for (const format of ["md", "org"] as const) {
    it(`round trips as present-with-no-value in ${format}`, () => {
      load(format);
      expect(read("tine.group-field")).toBeUndefined();

      setBlockProperty("query", "tine.group-field", "");
      expect(doc.byId.query.raw).toContain(format === "md" ? "tine.group-field::" : ":tine.group-field:");
      // Present, and its value is empty — NOT absent, which is what the Board
      // default keys off.
      expect(read("tine.group-field")).toEqual(["tine.group-field", ""]);
      expect(resolveQueryGrouping(properties())).toEqual({ kind: "cleared" });
    });

    it(`is removed, not emptied, when the key is cleared in ${format}`, () => {
      load(format);
      setBlockProperty("query", "tine.group-field", "");
      setBlockProperty("query", "tine.group-field", null);
      expect(read("tine.group-field")).toBeUndefined();
      expect(resolveQueryGrouping(properties())).toEqual({ kind: "unset" });
    });

    it(`blocks the legacy key behind it in ${format}`, () => {
      load(format);
      setBlockProperty("query", "tine.group-by", "state");
      setBlockProperty("query", "tine.group-field", "");
      // The new key is present, so nothing behind it is read — an explicit
      // clear cannot be undone by a legacy value that outlived it.
      expect(resolveQueryGrouping(properties())).toEqual({ kind: "cleared" });
    });
  }

  it("writing an empty value to a block that never had the key still states it", () => {
    // I-4: clearing a property a block does not have is the identity on its
    // bytes, so this is the one case where the writer must NOT no-op.
    load("md");
    setBlockProperty("query", "tine.group-field", "");
    expect(read("tine.group-field")).toEqual(["tine.group-field", ""]);
  });
});
