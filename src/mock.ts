// In-memory mock backend seeded with a fixture graph. Used only when running
// outside Tauri (browser dev / Playwright screenshots). Mirrors the real
// backend's shape so the UI behaves identically.

import type { Backend } from "./backend";
import type { BlockDto, GraphMeta, PageDto, PageEntry } from "./types";

let _id = 0;
const nid = () => `mock-${_id++}`;

function b(raw: string, children: BlockDto[] = [], collapsed = false): BlockDto {
  return { id: nid(), raw, collapsed, children };
}

const PAGES: PageDto[] = [
  {
    name: "Jun 14th, 2026",
    kind: "journal",
    title: "Jun 14th, 2026",
    pre_block: null,
    blocks: [
      b("## Today"),
      b("Started the [[logseq-claude]] rewrite — aiming for a #fast native feel.", [
        b("The outliner is the core; everything hangs off **blocks**."),
        b("Reading the OG source for the *exact* file format and `mldoc` quirks."),
      ]),
      b("TODO Ship the M0 vertical slice"),
      b("DOING Wire up the [[block editor]] with caret preservation"),
      b("DONE Validate round-trip on the real `shui-graph`"),
      b("Inline math works too: $E = mc^2$ and references like ((mock-2))."),
    ],
  },
  {
    name: "Jun 13th, 2026",
    kind: "journal",
    title: "Jun 13th, 2026",
    pre_block: null,
    blocks: [
      b("Yesterday's notes about [[parameterized complexity]].", [
        b("n-fold IP shows up again — see [[n-fold IP]]."),
        b("Key idea:", [b("decompose the constraint matrix into blocks"), b("solve via dynamic programming over the bricks")]),
      ]),
      b("LATER Read the new #SODA submission"),
    ],
  },
];

const NAMED: PageDto[] = [
  {
    name: "logseq-claude",
    kind: "page",
    title: "logseq-claude",
    pre_block: "title:: logseq-claude\ntags:: project, tooling",
    blocks: [
      b("A fast clone of [[Logseq]] built with **Tauri** + *SolidJS*.", [
        b("Goal: #functional + #visual equivalent."),
        b("Reads the same markdown graph as OG Logseq."),
      ]),
      b("## Architecture"),
      b("Rust core owns parsing; the frontend owns the live editing tree."),
    ],
  },
];

export function mockBackend(): Backend {
  const all = [...PAGES, ...NAMED];
  const find = (name: string) =>
    all.find((p) => p.name.toLowerCase() === name.toLowerCase()) ?? null;

  return {
    async loadGraph(): Promise<GraphMeta> {
      return { root: "/mock/graph", journals_dir: "journals", pages_dir: "pages" };
    },
    async listPages(): Promise<PageEntry[]> {
      return all.map((p) => ({ name: p.name, kind: p.kind, date_key: null }));
    },
    async journalsDesc(limit: number, offset: number): Promise<PageDto[]> {
      return PAGES.slice(offset, offset + limit);
    },
    async getPage(name: string): Promise<PageDto | null> {
      return find(name);
    },
    async savePage(): Promise<void> {
      // no-op in mock
    },
  };
}
