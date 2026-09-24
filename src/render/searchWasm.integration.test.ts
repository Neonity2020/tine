import { beforeAll, describe, expect, it } from "vitest";
import { initParser, searchFold, searchFoldMap, searchMatchBatch } from "./parse";
import { publishedBackend, type PublishedSnapshot } from "../publishedBackend";

describe("actual lsdoc Wasm search exports", () => {
  beforeAll(async () => {
    await initParser();
  });

  it("applies A6 once across accents, normalization, Hangul and compatibility forms", () => {
    expect(searchFold("café")).toBe(searchFold("cafe\u0301"));
    expect(searchFold("café")).toBe(searchFold("cafe"));
    expect(searchFold("가")).toBe(searchFold("가"));
    expect(searchFold("Ｔｉｎｅ")).toBe(searchFold("tine"));
    expect(searchFold("ofﬁce")).toBe(searchFold("office"));
    expect(searchFold("𝐀")).toBe("A");
    expect(searchFold(searchFold("𝐀"))).toBe("a");
  });

  it("keeps marks that make another letter and folds only accents", () => {
    expect(searchFold("が")).not.toBe(searchFold("か"));
    expect(searchFold("कु")).not.toBe(searchFold("क"));
    expect(searchFold("й")).not.toBe(searchFold("и"));
    expect(searchFold("ёлка")).toBe(searchFold("елка"));
    expect(searchFold("Łódź")).toBe(searchFold("lodz"));
    expect(searchFold("γειά")).toBe(searchFold("γεια"));
  });

  it("maps folded scalars back to original UTF-16 ranges across emoji", () => {
    const mapped = searchFoldMap("😀 𝐀 café");
    const scalars = Array.from(mapped.text);
    const mathematicalA = scalars.indexOf("A");
    expect(mapped.sources[mathematicalA]).toEqual({ start: 3, end: 5 });
    expect(mapped.sources).toHaveLength(scalars.length);
  });

  it("bounds evidence across every reordered mapped source", async () => {
    const raw = "😀a\u302e\u034f\u1715z";
    const snapshot = {
      schema: 1,
      name: "reordered-evidence",
      exported_at: "2026-09-20T00:00:00Z",
      home: raw,
      pages: [{
        name: raw,
        kind: "page",
        title: raw,
        pre_block: null,
        blocks: [],
        path: "pages/reordered.md",
      }],
      entries: [{ name: raw, kind: "page", date_key: null, path: "pages/reordered.md" }],
      backlinks: {},
      block_ref_counts: {},
      aliases: [],
      icons: {},
      queries: [],
    } as PublishedSnapshot;
    const backend = publishedBackend(() => Promise.resolve(snapshot));

    const answer = await backend.runGraphSearch("\u1715\u302e", 20, 0, "quick-switch", false, undefined, undefined, "ctrl_k");
    expect(answer.hits[0]?.evidence).toEqual([
      { clause_id: 0, field: "page_name", mode: "contains", spans: [{ start: 3, end: 6 }] },
    ]);
  });

  it("keeps page identity accent-sensitive while published text search uses A6", async () => {
    const snapshot = {
      schema: 1,
      name: "identity-control",
      exported_at: "2026-09-20T00:00:00Z",
      home: "café",
      pages: [{
        name: "café",
        kind: "page",
        title: "café",
        pre_block: null,
        blocks: [{ id: "accent", raw: "café", collapsed: false, children: [] }],
        path: "pages/café.md",
      }],
      entries: [{ name: "café", kind: "page", date_key: null, path: "pages/café.md" }],
      backlinks: {},
      block_ref_counts: {},
      aliases: [],
      icons: {},
      queries: [],
    } as PublishedSnapshot;
    const backend = publishedBackend(() => Promise.resolve(snapshot));

    expect((await backend.getPage("cafe", "page"))?.path).toBe("");
    expect((await backend.getPage("café", "page"))?.path).toBe("pages/café.md");
    expect((await backend.search("cafe", 10))[0]?.page).toBe("café");
  });

  it("runs boolean, OR, regex, empty and invalid policies in the native matcher", () => {
    const texts = ["café ready", "draft cafe\u0301", "😀 signal", "other"];
    expect(searchMatchBatch("cafe -draft", texts)).toEqual({
      matches: [true, false, false, false],
      search_error: null,
    });
    expect(searchMatchBatch("café OR 😀", texts)).toEqual({
      matches: [true, true, true, false],
      search_error: null,
    });
    expect(searchMatchBatch("/😀/", texts)).toEqual({
      matches: [false, false, true, false],
      search_error: null,
    });
    const unicodePropertyTexts = ["café", "123", "Καλημέρα", "東京"];
    expect(searchMatchBatch(String.raw`/\p{L}+/`, unicodePropertyTexts)).toEqual({
      matches: [true, false, true, true],
      search_error: null,
    });
    expect(searchMatchBatch(String.raw`/\p{Script=Greek}+/`, unicodePropertyTexts)).toEqual({
      matches: [false, false, true, false],
      search_error: null,
    });
    expect(searchMatchBatch("", texts)).toEqual({
      matches: [true, true, true, true],
      search_error: null,
    });
    const invalid = searchMatchBatch("/[/", texts);
    expect(invalid.matches).toEqual([true, true, true, true]);
    expect(invalid.search_error).not.toBeNull();
  });
});
