import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import { resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import { Block } from "./Block";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  resetStore();
  document.body.innerHTML = "";
});

function node(
  id: string,
  raw: string,
  parent: string | null,
  children: string[] = [],
  collapsed = false
): StoreNode {
  return { id, raw, collapsed, parent, page: "Jul 9th, 2026", children };
}

describe("collapsed heading blocks", () => {
  it("renders the parent heading and hides only its children", () => {
    const parent = "parent";
    const child = "child";
    const page: FeedPage = {
      name: "Jul 9th, 2026",
      kind: "journal",
      title: "Jul 9th, 2026",
      preBlock: null,
      roots: [parent],
      format: "md",
      readOnly: false,
      guide: false,
    };
    setDoc({
      byId: {
        [parent]: node(
          parent,
          "# Park Ji Hyun Confirmed To Reunite With Song Joong Ki In New Romance Drama\ncollapsed:: true",
          null,
          [child],
          true
        ),
        [child]: node(child, "Child article content", parent),
      },
      pages: [page],
      feed: [page.name],
      loaded: true,
    });

    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <Block id={parent} />, host);
    try {
      const parentEl = host.querySelector(`[data-block-id="${parent}"]`);
      expect(parentEl?.textContent).toContain(
        "Park Ji Hyun Confirmed To Reunite With Song Joong Ki In New Romance Drama"
      );
      expect(parentEl?.textContent).not.toContain("collapsed");
      expect(host.querySelector(`[data-block-id="${child}"]`)).toBeNull();
    } finally {
      dispose();
    }
  });
});
