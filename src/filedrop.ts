// OS file drag-and-drop → insert as graph assets. Tauri captures the native file
// drop (so HTML drag events never reach the webview) and hands us the absolute
// paths + a physical-pixel drop position; we map that to the block under the
// cursor and insert each file as a new sibling block (timestamped name + the
// `![](../assets/…)` media form, like the picker/paste paths).

import { getCurrentWebview } from "@tauri-apps/api/webview";
import { backend } from "./backend";
import { assetFileName, assetMarkdown } from "./media";
import { matrixGridNode, delimitedCellCount } from "./sheet/conversions";
import { parseDelimitedText, type DelimitedKind } from "./sheet/tsv";
import {
  bulkRouteFenceCurrent,
  captureBulkRouteFence,
  consumeManagedBulkInsertionAdmission,
  depthOf,
  doc,
  formatForBlock,
  insertOutlineAfter,
  MANAGED_BULK_INSERTION_UNAVAILABLE_TOAST,
  managedBulkOutlinePlan,
  pageByName,
  preflightManagedBulkInsertion,
  reportManagedBulkInsertionRefusal,
  trackAssetWrite,
  visibleOrder,
  withUndoUnit,
} from "./store";
import { pushToast } from "./ui";
import type { OutlineNode } from "./editor/outline";
import { dispatchDroppedFileInsertion } from "./storageDispatch";

const MAX_DROPPED_CELLS = 5000;

function basename(path: string): string {
  return path.split(/[\\/]/).pop() ?? "";
}

function delimitedKind(path: string): DelimitedKind | null {
  const lower = basename(path).toLowerCase();
  if (lower.endsWith(".csv")) return "csv";
  if (lower.endsWith(".tsv")) return "tsv";
  return null;
}

function titleWithoutExtension(path: string, kind: DelimitedKind): string {
  const name = basename(path);
  return name.slice(0, Math.max(0, name.length - kind.length - 1)) || "Dropped table";
}

type PlannedDrop =
  | { kind: "grid"; node: OutlineNode }
  | { kind: "asset"; path: string; originalName: string | undefined };

async function planManagedDrop(paths: readonly string[]): Promise<PlannedDrop[]> {
  const plan: PlannedDrop[] = [];
  for (const path of paths) {
    const kind = delimitedKind(path);
    if (kind) {
      const text = await backend().readTextFile(path);
      const matrix = parseDelimitedText(text, kind);
      const cells = delimitedCellCount(matrix);
      if (cells > MAX_DROPPED_CELLS) {
        pushToast(`"${basename(path)}" has ${cells} cells; CSV/TSV drops are limited to ${MAX_DROPPED_CELLS}.`, "error");
        continue;
      }
      plan.push({ kind: "grid", node: matrixGridNode(titleWithoutExtension(path, kind), matrix) });
      continue;
    }
    plan.push({ kind: "asset", path, originalName: basename(path) || undefined });
  }
  return plan;
}

async function materializeManagedDrop(afterId: string, plan: readonly PlannedDrop[]): Promise<OutlineNode[]> {
  const page = pageByName(doc.byId[afterId].page);
  const format = formatForBlock(afterId);
  const nodes: OutlineNode[] = [];
  for (const item of plan) {
    if (item.kind === "grid") {
      nodes.push(item.node);
      continue;
    }
    const saved = await trackAssetWrite(backend().importAsset(item.path, assetFileName(item.originalName)));
    nodes.push({
      raw: assetMarkdown(saved, { label: item.originalName, pagePath: page?.path, format }),
      children: [],
    });
  }
  return nodes;
}

async function insertDroppedFilesDirect(afterId: string, paths: readonly string[]): Promise<void> {
  // Direct Files does its IO one file at a time, so this routine can be mid-read
  // when the user finishes switching the graph to managed storage. The route it
  // chose is only true for as long as the fence holds; resuming past it would
  // insert an unbounded outline with no admission at all (GH #325).
  const fence = captureBulkRouteFence(afterId);
  if (!fence) return;
  const nodes: OutlineNode[] = [];
  for (const path of paths) {
    // Stop reading further files the moment the fence breaks: the outline can no
    // longer be inserted, and the target block may already be gone.
    if (!bulkRouteFenceCurrent(fence)) break;
    const kind = delimitedKind(path);
    if (kind) {
      const text = await backend().readTextFile(path);
      const matrix = parseDelimitedText(text, kind);
      const cells = delimitedCellCount(matrix);
      if (cells > MAX_DROPPED_CELLS) {
        pushToast(`"${basename(path)}" has ${cells} cells; CSV/TSV drops are limited to ${MAX_DROPPED_CELLS}.`, "error");
        continue;
      }
      nodes.push(matrixGridNode(titleWithoutExtension(path, kind), matrix));
      continue;
    }
    const orig = basename(path) || undefined;
    const saved = await trackAssetWrite(backend().importAsset(path, assetFileName(orig)));
    if (!bulkRouteFenceCurrent(fence)) break;
    const page = pageByName(fence.targetPage);
    nodes.push({
      raw: assetMarkdown(saved, {
        label: orig,
        pagePath: page?.path,
        format: formatForBlock(afterId),
      }),
      children: [],
    });
  }
  if (!nodes.length) return;
  if (!bulkRouteFenceCurrent(fence)) {
    pushToast("Couldn't insert the dropped files: this graph changed while they were being read.", "error");
    return;
  }
  withUndoUnit("file-drop", [doc.byId[afterId].page], () => insertOutlineAfter(afterId, nodes));
  pushToast(`Inserted ${nodes.length} file${nodes.length === 1 ? "" : "s"}`, "success");
}

/** Insert an already-native-resolved file drop. Managed bindings construct the
 * complete grid/asset-reference outline first so a certain cap refusal cannot
 * leave a preceding ordinary asset on disk. Direct Files retains its historical
 * sequential IO and unbounded page behavior. */
export async function insertDroppedFiles(afterId: string, paths: readonly string[]): Promise<void> {
  try {
    // Authority is selected once, by the dispatcher (I-6). The three arms are
    // the arms this function always had, including the unavailable arm's report
    // through the managed bulk-insertion preflight rather than a shared toast.
    await dispatchDroppedFileInsertion<void>(
      { afterId, paths },
      {
        direct: () => insertDroppedFilesDirect(afterId, paths),
        unavailable: () => {
          reportManagedBulkInsertionRefusal(MANAGED_BULK_INSERTION_UNAVAILABLE_TOAST);
        },
        managed: async (managedAdmission) => {
          const plan = await planManagedDrop(paths);
          if (!plan.length) return;
          const plannedNodes = plan.map((item): OutlineNode => item.kind === "grid"
            ? item.node
            // Generated asset filename/path bytes are actor-authoritative. The one
            // reference node is still a certain block-count fact before import.
            : { raw: "", children: [] });
          const admission = preflightManagedBulkInsertion(
            managedAdmission,
            afterId,
            (limits) => managedBulkOutlinePlan(
              plannedNodes,
              depthOf(afterId) + 1,
              0,
              limits,
            ),
          );
          if (admission.kind === "refused") {
            reportManagedBulkInsertionRefusal(admission.toast);
            return;
          }

          const nodes = await materializeManagedDrop(afterId, plan);
          if (!nodes.length) return;
          if (admission.kind === "admitted" && !consumeManagedBulkInsertionAdmission(admission.token, afterId)) return;
          withUndoUnit("file-drop", [doc.byId[afterId].page], () => insertOutlineAfter(afterId, nodes));
          pushToast(`Inserted ${nodes.length} file${nodes.length === 1 ? "" : "s"}`, "success");
        },
      },
    );
  } catch (e) {
    pushToast(`Couldn't insert dropped file: ${String(e)}`, "error");
  }
}

/** Install the OS file-drop handler. Returns an uninstaller. No-op outside the
 *  Tauri shell (browser mock / tests). */
export async function installFileDrop(): Promise<() => void> {
  let webview: ReturnType<typeof getCurrentWebview>;
  try {
    webview = getCurrentWebview();
  } catch {
    return () => {};
  }
  const setActive = (on: boolean) => document.body.classList.toggle("file-drop-active", on);

  const unlisten = await webview.onDragDropEvent(async (event) => {
    const p = event.payload;
    if (p.type === "enter" || p.type === "over") return setActive(true);
    if (p.type === "leave") return setActive(false);
    if (p.type !== "drop") return;
    setActive(false);
    const paths = p.paths ?? [];
    if (!paths.length) return;

    // Resolve the drop target: the block under the drop point, else the last
    // visible block (drops in the page whitespace land at the end). Tauri gives a
    // physical-pixel position; elementFromPoint wants CSS pixels.
    const dpr = window.devicePixelRatio || 1;
    const el = document.elementFromPoint(p.position.x / dpr, p.position.y / dpr);
    const onBlock = el?.closest("[data-block-id]")?.getAttribute("data-block-id") ?? null;
    const order = visibleOrder();
    const afterId = onBlock ?? order[order.length - 1] ?? null;
    if (!afterId || !doc.byId[afterId]) {
      pushToast("Drop a file onto a block to insert it.", "error");
      return;
    }

    await insertDroppedFiles(afterId, paths);
  });

  return () => {
    setActive(false);
    unlisten();
  };
}
