import { backend } from "./backend";
import { graphBinding } from "./persistence";
import type { PageEntry } from "./types";
import { graphEpoch, pageInventoryRev } from "./ui";

let last: { key: string; pages: Promise<PageEntry[]> } | null = null;

/** The graph's physical page list: ONE `list_pages` per graph binding, render
 *  epoch and page-inventory revision, shared by every reader. All Pages
 *  (`pages.ts`) and the navigation index (`graph.ts`) each listed the pages
 *  themselves, so every graph open and every create or delete read the whole
 *  list twice (GH #543, audit R10-10). Everything that changes the list moves
 *  one of the three keys. A failed read is not kept: the next reader asks
 *  again. */
export function listGraphPages(): Promise<PageEntry[]> {
  const key = `${graphBinding()}\0${graphEpoch()}\0${pageInventoryRev()}`;
  if (last?.key === key) return last.pages;
  const pages = backend().listPages();
  const entry = { key, pages };
  last = entry;
  pages.catch(() => {
    if (last === entry) last = null;
  });
  return pages;
}
