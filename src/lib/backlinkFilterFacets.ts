import type { BlockDto } from "../types";
import { pageIdentityKey } from "../pageIdentity";

/** DTO-owned fallback facets for a visible backlink subtree. The parser-owned
 *  native context is richer; this deliberately does not infer references from
 *  raw text. */
export function backlinkFilterFacets(block: BlockDto): string[] {
  const facets = new Map<string, string>();
  const visit = (current: BlockDto) => {
    for (const tag of current.tags ?? []) {
      const key = pageIdentityKey(tag);
      if (!facets.has(key)) facets.set(key, tag);
    }
    if (current.marker) {
      const key = pageIdentityKey(current.marker);
      if (!facets.has(key)) facets.set(key, current.marker);
    }
    for (const child of current.children) visit(child);
  };
  visit(block);
  return [...facets.values()];
}
