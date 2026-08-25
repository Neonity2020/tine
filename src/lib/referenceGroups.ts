import { pageIdentityKey } from "../pageIdentity";
import type { RefGroup } from "../types";

/** Merge reference groups that answer to one page identity (case, NFC/NFD, and
 *  boundary-slash spellings fold together — same key the backend dedups with).
 *  Was duplicated verbatim in LinkedReferences.tsx and UnlinkedReferences.tsx
 *  (DUP-8, 2026-08-25 duplication audit). */
export function mergeReferenceGroups(groups: RefGroup[]): RefGroup[] {
  const merged = new Map<string, RefGroup>();
  for (const group of groups) {
    const key = pageIdentityKey(group.page);
    const existing = merged.get(key);
    if (existing) {
      existing.blocks.push(...group.blocks);
      existing.evidence = [...(existing.evidence ?? []), ...(group.evidence ?? [])];
    } else {
      merged.set(key, { ...group, blocks: [...group.blocks], evidence: [...(group.evidence ?? [])] });
    }
  }
  return [...merged.values()];
}
