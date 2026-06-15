import { For, Show, type JSX } from "solid-js";
import { contextMenu, closeContextMenu, zoomInto, openInRightSidebar } from "../ui";
import { backend } from "../backend";
import {
  ensureBlockId,
  blockSubtreeMarkdown,
  deleteBlock,
  setBlockProperty,
  toggleBlockProperty,
  blockProperty,
  setHeading,
  setCollapsedDeep,
} from "../store";

// Block background colors, matching Logseq's built-in set.
const COLORS = ["yellow", "red", "pink", "green", "blue", "purple", "gray"];
const COLOR_BG: Record<string, string> = {
  yellow: "#fbe69e",
  red: "#f5a3a3",
  pink: "#f3b0d4",
  green: "#a6e3b4",
  blue: "#a8c9f0",
  purple: "#cdb4ee",
  gray: "#d3d6da",
};

// Right-click block context menu — mirrors Logseq's ordering: color row,
// heading row, open-in-sidebar, copy/cut/delete, collapse, numbered list.
export function ContextMenu(): JSX.Element {
  const close = () => closeContextMenu();
  const id = () => contextMenu()!.blockId;

  return (
    <Show when={contextMenu()}>
      <div class="ctx-overlay" onClick={close} onContextMenu={(e) => { e.preventDefault(); close(); }}>
        <div
          class="ctx-menu"
          style={{ left: `${contextMenu()!.x}px`, top: `${contextMenu()!.y}px` }}
          onClick={(e) => e.stopPropagation()}
        >
          {/* Color row */}
          <div class="ctx-row ctx-colors">
            <button
              class="ctx-color ctx-color-none"
              title="No background"
              onClick={() => { setBlockProperty(id(), "background-color", null); close(); }}
            >
              ✕
            </button>
            <For each={COLORS}>
              {(c) => (
                <button
                  class="ctx-color"
                  title={c}
                  style={{ background: COLOR_BG[c] }}
                  onClick={() => { toggleBlockProperty(id(), "background-color", c); close(); }}
                />
              )}
            </For>
          </div>

          {/* Heading row */}
          <div class="ctx-row ctx-headings">
            <For each={[1, 2, 3, 4, 5, 6]}>
              {(h) => (
                <button class="ctx-h" title={`Heading ${h}`} onClick={() => { setHeading(id(), h); close(); }}>
                  H{h}
                </button>
              )}
            </For>
            <button class="ctx-h" title="Remove heading" onClick={() => { setHeading(id(), null); close(); }}>
              ⌫
            </button>
          </div>

          <div class="ctx-sep" />

          <For each={actions(id())}>
            {(it) => (
              <div
                class="ctx-item"
                classList={{ danger: !!it.danger }}
                onClick={() => { it.run(); close(); }}
              >
                {it.label}
              </div>
            )}
          </For>
        </div>
      </div>
    </Show>
  );
}

function actions(id: string): { label: string; run: () => void; danger?: boolean }[] {
  const numbered = blockProperty(id, "logseq.order-list-type") === "number";
  return [
    { label: "Open in sidebar", run: () => openInRightSidebar("block", ensureBlockId(id)) },
    { label: "Zoom into block", run: () => zoomInto(id) },
    { label: "Copy block ref", run: () => void backend().writeText(`((${ensureBlockId(id)}))`) },
    { label: "Copy block", run: () => void backend().writeText(blockSubtreeMarkdown(id)) },
    {
      label: "Cut block",
      run: () => {
        void backend().writeText(blockSubtreeMarkdown(id));
        deleteBlock(id);
      },
    },
    {
      label: numbered ? "Remove numbered list" : "Numbered list",
      run: () => toggleBlockProperty(id, "logseq.order-list-type", "number"),
    },
    { label: "Collapse all", run: () => setCollapsedDeep(id, true) },
    { label: "Expand all", run: () => setCollapsedDeep(id, false) },
    { label: "Delete block", run: () => deleteBlock(id), danger: true },
  ];
}
