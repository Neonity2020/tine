import { For, Show, createMemo, type JSX } from "solid-js";
import { doc, formatForBlock } from "../store";
import { AstBody } from "../render/body";
import { facetsOf } from "../render/facets";
import { sheetConfig } from "../sheet/config";
import { buildMatrix, type MatrixCell } from "../sheet/matrix";
import { SheetCellContext, type SheetCellCtx } from "../sheet/context";
import {
  cellOwner,
  cellSel,
  cellSurfaceKey,
  setCellSel,
  startCellEditing,
} from "../sheet/selection";
import { editorOffsetFromRenderedRange } from "../render/spans";
import { isBuiltinHidden } from "../editor/properties";
import { forbidsEditEntry } from "../editor/editTargets";
import { editingId, editingOwner } from "../editorController";
import { Editor, SurfaceContext } from "./Block";

const MAX_GRID_DEPTH = 5;

function configForBlock(id: string) {
  const node = doc.byId[id];
  return sheetConfig(node ? facetsOf(node.raw, formatForBlock(id)).properties : []);
}

function blockChildren(id: string): string[] {
  return doc.byId[id]?.children ?? [];
}

export function SheetGrid(props: { id: string }): JSX.Element {
  return <SheetGridInner id={props.id} depth={0} />;
}

function SheetGridInner(props: { id: string; depth: number }): JSX.Element {
  const config = createMemo(() => configForBlock(props.id));
  const rows = createMemo(() =>
    blockChildren(props.id).map((id) => ({
      id,
      cellIds: blockChildren(id),
    }))
  );
  const matrix = createMemo(() => buildMatrix(rows()));
  const columns = createMemo(() => {
    const widths = config().colWidths;
    const tracks: string[] = [];
    for (let col = 0; col < matrix().cols; col++) {
      const px = widths.get(col);
      tracks.push(px == null ? "max-content" : `${px}px`);
    }
    return tracks.join(" ");
  });

  return (
    <Show when={props.depth < MAX_GRID_DEPTH} fallback={<SheetOutline ids={blockChildren(props.id)} depth={props.depth} />}>
      <Show when={rows().length > 0} fallback={<div class="sheet-grid sheet-empty">empty grid</div>}>
        <div
          class="sheet-grid"
          data-sheet-grid-id={props.id}
          tabIndex={-1}
          style={{ "grid-template-columns": columns() }}
        >
          <For each={matrix().cells}>
            {(cell) => (
              <SheetGridCell
                gridId={props.id}
                cell={cell}
                header={config().header && cell.row === 0}
                depth={props.depth}
              />
            )}
          </For>
        </div>
      </Show>
    </Show>
  );
}

function sameSelectedCell(gridId: string, cell: MatrixCell): boolean {
  const sel = cellSel();
  return !!sel && sel.gridId === gridId && sel.row === cell.row && sel.col === cell.col;
}

function clickOffset(e: MouseEvent, contentRef: HTMLDivElement | undefined, raw: string): number | null {
  if (!contentRef) return null;
  const d = document as Document & { caretRangeFromPoint?: (x: number, y: number) => Range | null };
  const range = d.caretRangeFromPoint?.(e.clientX, e.clientY);
  if (!range) return null;
  return editorOffsetFromRenderedRange(contentRef, range, raw, isBuiltinHidden);
}

function SheetGridCell(props: { gridId: string; cell: MatrixCell; header: boolean; depth: number }): JSX.Element {
  const sel = (): SheetCellCtx => ({ gridId: props.gridId, row: props.cell.row, col: props.cell.col });
  let contentRef: HTMLDivElement | undefined;
  const onMouseDown = (e: MouseEvent) => {
    if (e.button !== 0 || e.shiftKey || e.ctrlKey || e.metaKey || e.altKey) return;
    if (forbidsEditEntry(e)) return;
    e.preventDefault();
    e.stopPropagation();
    const blockId = props.cell.blockId;
    if (!blockId) {
      setCellSel(sel());
      return;
    }
    const node = doc.byId[blockId];
    const offset = node ? clickOffset(e, contentRef, node.raw) : null;
    if (offset == null) setCellSel(sel());
    else startCellEditing(sel(), offset);
  };

  return (
    <div
      class="sheet-cell"
      classList={{
        "sheet-header-cell": props.header,
        "sheet-hole": !props.cell.blockId,
        "sheet-cell-selected": sameSelectedCell(props.gridId, props.cell),
      }}
      data-sheet-grid-id={props.gridId}
      data-block-id={props.cell.blockId ?? undefined}
      data-row={props.cell.row}
      data-col={props.cell.col}
      onMouseDown={onMouseDown}
    >
      <Show when={props.cell.blockId}>
        {(blockId) => (
          <SheetBlock
            id={blockId()}
            depth={props.depth + 1}
            cell={sel()}
            bodyRef={(el) => {
              contentRef = el;
            }}
          />
      )}
      </Show>
    </div>
  );
}

function SheetBlock(props: { id: string; depth: number; cell?: SheetCellCtx; bodyRef?: (el: HTMLDivElement) => void }): JSX.Element {
  const node = () => doc.byId[props.id];
  const fmt = () => formatForBlock(props.id);
  const facets = createMemo(() => (node() ? facetsOf(node().raw, fmt()) : null));
  const config = createMemo(() => (facets() ? sheetConfig(facets()!.properties) : null));
  const children = () => node()?.children ?? [];
  const editing = () => {
    const cell = props.cell;
    return !!cell && editingId() === props.id && editingOwner() === cellOwner(cell);
  };

  return (
    <Show when={node()}>
      {(n) => (
        <>
          <div
            class="sheet-cell-body"
            ref={(el) => props.bodyRef?.(el)}
          >
            <Show
              when={editing() && props.cell}
              fallback={<AstBody raw={n().raw} format={fmt()} headingLevel={facets()?.headingLevel ?? null} />}
            >
              {(cell) => (
                <SheetCellContext.Provider value={cell()}>
                  <SurfaceContext.Provider value={cellSurfaceKey(cell().gridId)}>
                    <Editor id={props.id} />
                  </SurfaceContext.Provider>
                </SheetCellContext.Provider>
              )}
            </Show>
          </div>
          <Show when={children().length > 0 || config()?.view === "grid"}>
            <Show
              when={config()?.view === "grid"}
              fallback={<SheetOutline ids={children()} depth={props.depth} />}
            >
              <SheetGridInner id={props.id} depth={props.depth} />
            </Show>
          </Show>
        </>
      )}
    </Show>
  );
}

function SheetOutline(props: { ids: readonly string[]; depth: number }): JSX.Element {
  return (
    <div class="sheet-nested-lines">
      <For each={props.ids}>
            {(id) => (
          <div class="sheet-nested-line" style={{ "padding-left": `${Math.max(0, props.depth) * 14}px` }}>
            <SheetBlock id={id} depth={props.depth + 1} />
          </div>
        )}
      </For>
    </div>
  );
}
