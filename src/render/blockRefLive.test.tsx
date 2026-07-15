import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { loadSingle, resetStore, setRaw } from "../store";
import { AstBody } from "./body";
import { initParser } from "./parse";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  resetStore();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

describe("live inline block references (GH #166)", () => {
  it("refreshes the visible reference text after the source block is edited", async () => {
    const id = "16600000-0000-4000-8000-000000000001";
    const originalText = "Original referenced text";
    const updatedText = "Updated referenced text";
    const target = {
      id,
      raw: `${originalText}\nid:: ${id}`,
      collapsed: false,
      children: [],
      properties: [["id", id]] as [string, string][],
    };
    loadSingle({
      kind: "page",
      name: "Reference source",
      title: "Reference source",
      pre_block: null,
      blocks: [target],
    });
    const resolveBlocks = vi.spyOn(backend(), "resolveBlocks").mockResolvedValue([{
      page: "Reference source",
      kind: "page",
      blocks: [target],
    }]);

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <AstBody raw={`See ((${id}))`} />, host);
    try {
      await vi.waitFor(() => expect(host.querySelector(".block-ref")?.textContent).toBe(originalText));
      expect(resolveBlocks).toHaveBeenCalledTimes(1);

      setRaw(id, `${updatedText}\nid:: ${id}`);

      await vi.waitFor(() => expect(host.querySelector(".block-ref")?.textContent).toBe(updatedText));
      expect(resolveBlocks).toHaveBeenCalledTimes(1);
    } finally {
      dispose();
    }
  });
});
