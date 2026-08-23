import { For, Show, createResource, createSignal, type JSX } from "solid-js";
import { backend } from "../backend";
import { dataRev, graphEpoch } from "../ui";
import { openPage, openPageInNewTab } from "../router";
import { openPageInSidebar, openPageContextMenu } from "../ui";
import { internalLinkDest } from "../linkGesture";
import { LiveRefGroup } from "./LiveRefGroup";
import { shouldOpenTextContextMenu } from "../contextMenuPolicy";
import { blockExternalId } from "../store";

// Block-level "linked references": the blocks that reference THIS block (via
// `((uuid))` / `[..](((uuid)))` / `{{embed ((uuid))}}`), grouped by page. Toggled
// open by the per-block reference-count badge (Block.tsx). Mirrors the page-level
// LinkedReferences, minus the co-reference filter chips (OG doesn't show those on
// the block-ref panel). Refetches when the graph generation changes.
export function BlockReferences(props: { id: string }): JSX.Element {
  const [groups] = createResource(
    () => ({ id: blockExternalId(props.id) ?? props.id, epoch: graphEpoch(), revision: dataRev() }),
    ({ id }) => backend().getBlockReferrers(id)
  );
  const count = () => (groups() ?? []).reduce((acc, g) => acc + g.blocks.length, 0);

  // GH #344: group-level collapse for the compact title-only overview. LOCAL
  // to this view instance — deliberately not conflated with the page-level
  // Linked References section state. A collapsed group shows only its source
  // page title and stays individually expandable back to its full
  // breadcrumb + referenced-block context.
  const [collapsedGroups, setCollapsedGroups] = createSignal<Set<string>>(new Set());
  const groupKey = (g: { page: string; kind: string; path?: string }) =>
    `${g.kind}\0${g.path ?? ""}\0${g.page}`;
  const groupCollapsed = (g: { page: string; kind: string; path?: string }) =>
    collapsedGroups().has(groupKey(g));
  const setGroupCollapsed = (g: { page: string; kind: string; path?: string }, value: boolean) => {
    setCollapsedGroups((current) => {
      const next = new Set(current);
      if (value) next.add(groupKey(g));
      else next.delete(groupKey(g));
      return next;
    });
  };
  const setAllGroups = (value: boolean) => {
    setCollapsedGroups(value ? new Set((groups() ?? []).map(groupKey)) : new Set<string>());
  };

  return (
    <Show when={groups() && groups()!.length > 0}>
      <div class="block-references-inner">
        <div class="block-references-header">
          {count()} Linked Reference{count() === 1 ? "" : "s"}
        </div>
        <Show when={(groups() ?? []).length > 1}>
          <div class="reference-bulk-controls" aria-label="Reference page groups">
            <button type="button" onClick={() => setAllGroups(true)}>Collapse all</button>
            <button type="button" onClick={() => setAllGroups(false)}>Expand all</button>
          </div>
        </Show>
        <For each={groups()}>
          {(g) => (
            <div class="reference-group">
              <div class="reference-group-header">
                <button
                  type="button"
                  class="reference-group-disclosure"
                  aria-expanded={!groupCollapsed(g)}
                  aria-label={`${groupCollapsed(g) ? "Expand" : "Collapse"} references from ${g.page}`}
                  onClick={() => setGroupCollapsed(g, !groupCollapsed(g))}
                >
                  {groupCollapsed(g) ? "▸" : "▾"}
                </button>
                <div
                  class="reference-page"
                  onClick={(e) => {
                    const dest = internalLinkDest(e);
                    if (dest === "sidebar") openPageInSidebar(g.page, g.kind);
                    else if (dest === "background") openPageInNewTab(g.page, g.kind);
                    else openPage(g.page, g.kind);
                  }}
                  onAuxClick={(e) => {
                    if (e.button === 1) {
                      e.preventDefault();
                      openPageInNewTab(g.page, g.kind);
                    }
                  }}
                  onContextMenu={(e) => {
                    if (!shouldOpenTextContextMenu(e.target)) return;
                    e.preventDefault();
                    openPageContextMenu(e.clientX, e.clientY, g.page, g.kind);
                  }}
                >
                  {g.page}
                </div>
              </div>
              <Show when={!groupCollapsed(g)}>
                <div class="reference-blocks">
                  {/* OG shows each referrer's ancestor breadcrumb in the block-ref
                      panel (:breadcrumb-show? true) for "where does this live" context. */}
                  <LiveRefGroup page={g.page} kind={g.kind} blocks={g.blocks} surface="ref" showBreadcrumb />
                </div>
              </Show>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}
