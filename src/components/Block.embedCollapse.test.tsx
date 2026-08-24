import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { initParser } from "../render/parse";
import { blockProperty, doc, loadSingle, pageByName, resetStore, setCollapsed } from "../store";
import type { BlockDto, PageDto, RefGroup } from "../types";
import { Block } from "./Block";
import { For } from "solid-js";

// GH #360 (Martin-approved embed contract):
//   1. Embedded content is live and source-authoritative: a source fold or
//      unfold updates every visible embed immediately (no page-leave refresh
//      needed).
//   2. Before an occurrence is changed locally it follows the source. Once the
//      occurrence is folded or unfolded, the HOST block owns an explicit
//      `collapsed:: true|false` override. It survives remount/reload without
//      changing the source block or another occurrence.

class AllNearObserver implements IntersectionObserver {
  readonly root = null;
  readonly rootMargin = "0px";
  readonly thresholds = [0];
  constructor(private readonly callback: IntersectionObserverCallback) {}
  disconnect(): void {}
  takeRecords(): IntersectionObserverEntry[] { return []; }
  unobserve(): void {}
  observe(target: Element): void {
    this.callback([{ isIntersecting: true, target } as IntersectionObserverEntry], this);
  }
}

beforeAll(async () => {
  await initParser();
});

beforeEach(() => {
  vi.stubGlobal("IntersectionObserver", AllNearObserver);
});

afterEach(() => {
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
  resetStore();
  document.body.innerHTML = "";
});

function page(name: string, blocks: BlockDto[]): PageDto {
  return { name, title: name, kind: "page", pre_block: null, blocks };
}

function mockBlocks(pageDto: PageDto, targets: BlockDto[]): void {
  const groups = new Map<string, RefGroup>(targets.map((block) => [block.id, {
    page: pageDto.name,
    kind: pageDto.kind,
    blocks: [{ ...block, children: [] }],
  }]));
  vi.spyOn(backend(), "resolveBlocks").mockImplementation(async (ids) =>
    ids.map((id) => groups.get(id) ?? null)
  );
}

// One page: the target (with two visible children) sits alongside TWO host
// blocks, each embedding it — a source + two occurrences of the same block.
function loadEmbedFixture() {
  const target: BlockDto = {
    id: "target",
    raw: "Embed target body",
    collapsed: false,
    children: [
      { id: "target-c1", raw: "child one", collapsed: false, children: [] },
      { id: "target-c2", raw: "child two", collapsed: false, children: [] },
    ],
  };
  const hosts: BlockDto[] = [
    { id: "host-1", raw: "{{embed ((target))}}", collapsed: false, children: [] },
    { id: "host-2", raw: "{{embed ((target))}}", collapsed: false, children: [] },
  ];
  const pg = page("Fixture", [target, ...hosts]);
  mockBlocks(pg, [target]);
  loadSingle(pg);
  return { target, hosts, pg };
}

function mountPageBlocks(): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => (
    <For each={pageByName("Fixture")?.roots ?? []}>{(id) => <Block id={id} />}</For>
  ), root);
  return { root, dispose };
}

function occurrenceHosts(root: HTMLElement): HTMLElement[] {
  return [...root.querySelectorAll<HTMLElement>(".embed-block")];
}

function occurrenceChildrenVisible(host: HTMLElement): string {
  return host.querySelector(".block-children")?.textContent ?? "";
}

function occurrenceToggle(host: HTMLElement): HTMLButtonElement {
  const btn = host.querySelector<HTMLButtonElement>(`.ls-block[data-block-id="target"] .collapse-toggle`);
  if (!btn) throw new Error(`missing collapse toggle inside occurrence: ${host.innerHTML.slice(0, 300)}`);
  return btn;
}

async function tick(times = 3): Promise<void> {
  for (let i = 0; i < times; i++) await new Promise((r) => setTimeout(r, 0));
}

describe("embed collapse contract (GH #360)", () => {
  it("source-authoritative: a source collapse/expand updates ALL visible embeds immediately", async () => {
    loadEmbedFixture();
    const { root, dispose } = mountPageBlocks();
    try {
      const hosts = occurrenceHosts(root);
      expect(hosts).toHaveLength(2);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");

      // Collapse the SOURCE block (exactly what the outline bullet toggle does).
      setCollapsed("target", true);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).not.toContain("child one");

      // And expanding again updates them back — no page-leave needed.
      setCollapsed("target", false);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");
    } finally {
      dispose();
    }
  });

  it("persists a local fold on occurrence-1's host without touching occurrence-2 or the source", async () => {
    loadEmbedFixture();
    const { root, dispose } = mountPageBlocks();
    try {
      const hosts = occurrenceHosts(root);
      await tick();
      occurrenceToggle(hosts[0]).click();
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");
      expect(doc.byId.target.collapsed).toBe(false);
      expect(doc.byId.target.raw).not.toContain("collapsed::");
      expect(blockProperty("host-1", "collapsed")).toBe("true");
      expect(blockProperty("host-2", "collapsed")).toBeNull();

      // A host-owned choice remains independent when the source moves. The
      // untouched sibling continues to follow the source live.
      setCollapsed("target", true);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).not.toContain("child one");
      setCollapsed("target", false);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");
    } finally {
      dispose();
    }
  });

  it("a host-owned fold survives an occurrence remount and remains out of the source raw", async () => {
    loadEmbedFixture();
    const first = mountPageBlocks();
    try {
      const hosts = occurrenceHosts(first.root);
      await tick();
      occurrenceToggle(hosts[0]).click();
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
    } finally {
      first.dispose();
    }
    document.body.innerHTML = "";
    const second = mountPageBlocks();
    try {
      const hosts = occurrenceHosts(second.root);
      await tick(5);
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");
      expect(doc.byId.target.raw).not.toContain("collapsed::");
      expect(doc.byId.target.collapsed).toBe(false);
      expect(blockProperty("host-1", "collapsed")).toBe("true");
    } finally {
      second.dispose();
    }
  });

  it("persists an explicit false override when one occurrence expands a collapsed source", async () => {
    loadEmbedFixture();
    setCollapsed("target", true); // source starts collapsed
    const { root, dispose } = mountPageBlocks();
    try {
      const hosts = occurrenceHosts(root);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).not.toContain("child one");
      // Expand inside occurrence 1 only.
      occurrenceToggle(hosts[0]).click();
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).not.toContain("child one");
      expect(doc.byId.target.collapsed).toBe(true); // source untouched
      expect(blockProperty("host-1", "collapsed")).toBe("false");
      expect(blockProperty("host-2", "collapsed")).toBeNull();
      // The untouched sibling keeps following later source moves, while the
      // explicit false override stays expanded when the source folds again.
      setCollapsed("target", false);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).toContain("child one");
      setCollapsed("target", true);
      await tick();
      expect(occurrenceChildrenVisible(hosts[0])).toContain("child one");
      expect(occurrenceChildrenVisible(hosts[1])).not.toContain("child one");
    } finally {
      dispose();
    }
  });
});
