import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { For } from "solid-js";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import { clearSeededFacets } from "../render/facets";
import { loadSingle, pageByName, resetStore } from "../store";
import type { BlockDto, PageDto } from "../types";
import { Block } from "./Block";

beforeAll(() => initParser());

afterEach(() => {
  resetStore();
  document.body.innerHTML = "";
});

function block(id: string, raw: string, children: BlockDto[] = []): BlockDto {
  return { id, raw, collapsed: false, children };
}

describe("block background geometry DOM", () => {
  it("uses the shared geometry classes for root, nested, and numbered backgrounds", () => {
    const page: PageDto = {
      name: "Geometry",
      title: "Geometry",
      kind: "page",
      format: "md",
      pre_block: null,
      blocks: [
        block("root", "Root\nbackground-color:: green", [
          block("nested", "Nested\nbackground-color:: red"),
        ]),
        block("ordered", "Numbered\nbackground-color:: purple\nlogseq.order-list-type:: number"),
      ],
    };
    loadSingle(page);
    clearSeededFacets();
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(
      () => <For each={pageByName("Geometry")?.roots ?? []}>{(id) => <Block id={id} />}</For>,
      host,
    );
    try {
      for (const id of ["root", "nested", "ordered"]) {
        expect(host.querySelector(`[data-block-id="${id}"] > .block-main .block-content.has-bg`)).not.toBeNull();
      }
      expect(host.querySelector('[data-block-id="ordered"] > .block-main .bullet-container.ordered')).not.toBeNull();
      expect(host.querySelector('[data-block-id="nested"]')?.parentElement?.classList.contains("block-children")).toBe(true);
    } finally {
      dispose();
    }
  });
});
