import { For, Show, createEffect, createSignal, onCleanup, type JSX } from "solid-js";
import { formatForPage } from "../store";
import { openExportNodesModal } from "../ui";
import { visibleBody } from "../render/block";
import { EmojiText } from "../render/emoji";
import { blockDtosToExportNodes } from "./ExportModal";
import type { ExportNode } from "../editor/exportText";
import type { BlockDto, RefGroup } from "../types";
import { registerTransientLayer } from "../transientLayers";

// GH #348: the batch entry point for a reference section (Linked / Unlinked
// References). A quiet chooser — every entry pre-selected for the "copy all"
// case, uncheck to take a subset — that hands the SAME Copy / export modal a
// node forest grouped by source page (the shape query export already uses).
// Read-only throughout: it never touches the graph, only the DTO list the
// section already holds. Normal page export is a separate, unchanged path.
export function ReferenceExportChooser(props: {
  subject: string;
  groups: RefGroup[];
  onClose: () => void;
}): JSX.Element {
  let root: HTMLDivElement | undefined;
  createEffect(() => {
    const unregister = registerTransientLayer({
      id: `reference-export:${props.subject}`,
      root: () => root ?? null,
      dismiss: () => {
        props.onClose();
        return true;
      },
    });
    onCleanup(unregister);
  });

  const entryKey = (g: RefGroup, b: BlockDto) => `${g.kind}${g.page}${b.id}`;
  const allKeys = () => props.groups.flatMap((g) => g.blocks.map((b) => entryKey(g, b)));
  // "Copy all" is the headline case (GH #348), so start with everything chosen;
  // the subset flow is unchecking what you do not want.
  const [selected, setSelected] = createSignal<Set<string>>(new Set(allKeys()));
  const count = () => selected().size;
  const total = () => allKeys().length;
  const firstLine = (block: BlockDto) => visibleBody(block.raw)[0] ?? "";

  const toggle = (key: string, checked: boolean) => {
    setSelected((current) => {
      const next = new Set(current);
      if (checked) next.add(key);
      else next.delete(key);
      return next;
    });
  };

  const exportSelection = () => {
    const nodes = props.groups.flatMap((g): ExportNode[] => {
      const chosen = g.blocks.filter((b) => selected().has(entryKey(g, b)));
      if (!chosen.length) return [];
      const format = formatForPage(g.page);
      return [{
        raw: g.page,
        format,
        children: blockDtosToExportNodes(chosen, format),
      }];
    });
    if (!nodes.length) return;
    openExportNodesModal(nodes, count());
    props.onClose();
  };

  return (
    <div class="modal-overlay" onClick={props.onClose}>
      <div ref={root} class="export-modal ref-export-chooser" role="dialog" aria-label={`Copy / export ${props.subject}`} onClick={(e) => e.stopPropagation()}>
        <div class="export-head">
          Copy / export {props.subject}
          <span class="export-count">{count()} of {total()}</span>
        </div>
        <div class="ref-export-actions" role="group" aria-label="Reference selection">
          <button type="button" disabled={!total()} onClick={() => setSelected(new Set(allKeys()))}>All</button>
          <button type="button" disabled={!count()} onClick={() => setSelected(new Set())}>None</button>
        </div>
        <div class="ref-export-list" role="group" aria-label={`${props.subject} entries`}>
          <For each={props.groups}>
            {(g) => (
              <div class="ref-export-group">
                <div class="ref-export-group-name"><EmojiText text={g.page} /></div>
                <For each={g.blocks}>
                  {(block) => {
                    const key = entryKey(g, block);
                    return (
                      <label class="ref-export-row">
                        <input
                          type="checkbox"
                          checked={selected().has(key)}
                          onChange={(e) => toggle(key, e.currentTarget.checked)}
                        />
                        <span class="ref-export-text"><EmojiText text={firstLine(block)} /></span>
                      </label>
                    );
                  }}
                </For>
              </div>
            )}
          </For>
          <Show when={total() === 0}>
            <div class="ref-export-empty">No entries in this section.</div>
          </Show>
        </div>
        <div class="export-foot">
          <button class="export-btn-secondary" onClick={props.onClose}>Cancel</button>
          <button class="export-btn-primary" disabled={!count()} onClick={exportSelection}>Copy / export…</button>
        </div>
      </div>
    </div>
  );
}
