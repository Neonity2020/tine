import { beforeAll, describe, expect, it, vi } from "vitest";

const wasmMock = vi.hoisted(() => {
  const init = vi.fn().mockResolvedValue({});
  const fold = vi.fn((text: string) => `folded:${text}`);
  const foldMap = vi.fn((text: string) => JSON.stringify({
    text: `folded:${text}`,
    sources: [{ start: 0, end: text.length }],
  }));
  const matchBatch = vi.fn((query: string, textsJson: string) => JSON.stringify({
    matches: (JSON.parse(textsJson) as string[]).map((text) => text.includes(query)),
    search_error: null,
  }));
  return { init, fold, foldMap, matchBatch };
});

vi.mock("./wasm/lsdoc_wasm.js", () => ({
  default: wasmMock.init,
  parse_block_json: vi.fn(() => "[]"),
  __tineReinstantiate: vi.fn(),
  lsdoc_tag: vi.fn(() => "v0.4.1"),
  search_fold: wasmMock.fold,
  search_fold_map_json: wasmMock.foldMap,
  search_match_batch_json: wasmMock.matchBatch,
}));

import { initParser, searchFold, searchFoldMap, searchMatchBatch } from "./parse";

beforeAll(async () => {
  await initParser();
  await initParser();
});

describe("lsdoc Wasm search bridge", () => {
  it("shares parser initialization and decodes the typed search exports", () => {
    expect(wasmMock.init).toHaveBeenCalledTimes(1);
    expect(searchFold("Café")).toBe("folded:Café");
    expect(searchFoldMap("😀")).toEqual({
      text: "folded:😀",
      sources: [{ start: 0, end: 2 }],
    });
    expect(searchMatchBatch("hit", ["hit one", "miss"])).toEqual({
      matches: [true, false],
      search_error: null,
    });
    expect(wasmMock.matchBatch).toHaveBeenLastCalledWith("hit", '["hit one","miss"]');
  });

  it("surfaces Wasm and JSON failures instead of installing a JS fallback", () => {
    wasmMock.fold.mockImplementationOnce(() => {
      throw new Error("native fold failed");
    });
    expect(() => searchFold("x")).toThrow("native fold failed");

    wasmMock.foldMap.mockReturnValueOnce("not JSON");
    expect(() => searchFoldMap("x")).toThrow(SyntaxError);
  });
});
