import { createSignal } from "solid-js";
import type { LayoutNode } from "./panes";

export type PaneTarget =
  | { kind: "pane"; paneId: string }
  | { kind: "seam"; path: number[] }
  | { kind: "edge"; side: "left" | "right" | "top" | "bottom" };

export type PaneDirection = "left" | "right" | "up" | "down";

export interface PaneRect {
  paneId: string;
  rect: Rect;
}

export interface SeamRect {
  path: number[];
  dir: "row" | "col";
  rect: Rect;
}

export interface EdgeRect {
  side: "left" | "right" | "top" | "bottom";
  rect: Rect;
}

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface PaneGeometry {
  panes: PaneRect[];
  seams: SeamRect[];
  edges: EdgeRect[];
}

const ROOT_RECT: Rect = { x: 0, y: 0, w: 1, h: 1 };
const EPS = 1e-9;

export const [paneSel, setPaneSel] = createSignal<PaneTarget | null>(null);

let previousPaneTarget: string | null = null;

function samePath(a: readonly number[], b: readonly number[]): boolean {
  return a.length === b.length && a.every((n, i) => n === b[i]);
}

export function samePaneTarget(a: PaneTarget | null, b: PaneTarget | null): boolean {
  if (!a || !b || a.kind !== b.kind) return false;
  if (a.kind === "pane") return a.paneId === (b as Extract<PaneTarget, { kind: "pane" }>).paneId;
  if (a.kind === "edge") return a.side === (b as Extract<PaneTarget, { kind: "edge" }>).side;
  return samePath(a.path, (b as Extract<PaneTarget, { kind: "seam" }>).path);
}

export function computePaneGeometry(root: LayoutNode, rect: Rect = ROOT_RECT): PaneGeometry {
  const panes: PaneRect[] = [];
  const seams: SeamRect[] = [];

  const walk = (node: LayoutNode, box: Rect, path: number[]) => {
    if (node.kind === "pane") {
      panes.push({ paneId: node.paneId, rect: box });
      return;
    }

    if (node.dir === "row") {
      const leftW = box.w * node.ratio;
      const seamX = box.x + leftW;
      seams.push({ path, dir: node.dir, rect: { x: seamX, y: box.y, w: 0, h: box.h } });
      walk(node.children[0], { x: box.x, y: box.y, w: leftW, h: box.h }, [...path, 0]);
      walk(node.children[1], { x: seamX, y: box.y, w: box.w - leftW, h: box.h }, [...path, 1]);
      return;
    }

    const topH = box.h * node.ratio;
    const seamY = box.y + topH;
    seams.push({ path, dir: node.dir, rect: { x: box.x, y: seamY, w: box.w, h: 0 } });
    walk(node.children[0], { x: box.x, y: box.y, w: box.w, h: topH }, [...path, 0]);
    walk(node.children[1], { x: box.x, y: seamY, w: box.w, h: box.h - topH }, [...path, 1]);
  };

  walk(root, rect, []);
  return {
    panes,
    seams,
    edges: [
      { side: "left", rect: { x: rect.x, y: rect.y, w: 0, h: rect.h } },
      { side: "right", rect: { x: rect.x + rect.w, y: rect.y, w: 0, h: rect.h } },
      { side: "top", rect: { x: rect.x, y: rect.y, w: rect.w, h: 0 } },
      { side: "bottom", rect: { x: rect.x, y: rect.y + rect.h, w: rect.w, h: 0 } },
    ],
  };
}

function center(rect: Rect): { x: number; y: number } {
  return { x: rect.x + rect.w / 2, y: rect.y + rect.h / 2 };
}

function targetRect(geom: PaneGeometry, target: PaneTarget): Rect | null {
  switch (target.kind) {
    case "pane":
      return geom.panes.find((p) => p.paneId === target.paneId)?.rect ?? null;
    case "seam":
      return geom.seams.find((s) => samePath(s.path, target.path))?.rect ?? null;
    case "edge":
      return geom.edges.find((e) => e.side === target.side)?.rect ?? null;
  }
}

function allTargets(geom: PaneGeometry): PaneTarget[] {
  return [
    ...geom.panes.map((p): PaneTarget => ({ kind: "pane", paneId: p.paneId })),
    ...geom.seams.map((s): PaneTarget => ({ kind: "seam", path: s.path })),
    ...geom.edges.map((e): PaneTarget => ({ kind: "edge", side: e.side })),
  ];
}

function isAhead(from: { x: number; y: number }, to: { x: number; y: number }, dir: PaneDirection): boolean {
  switch (dir) {
    case "left": return to.x < from.x - EPS;
    case "right": return to.x > from.x + EPS;
    case "up": return to.y < from.y - EPS;
    case "down": return to.y > from.y + EPS;
  }
}

function primaryDistance(from: { x: number; y: number }, to: { x: number; y: number }, dir: PaneDirection): number {
  return dir === "left" || dir === "right" ? Math.abs(to.x - from.x) : Math.abs(to.y - from.y);
}

function crossDistance(from: { x: number; y: number }, to: { x: number; y: number }, dir: PaneDirection): number {
  return dir === "left" || dir === "right" ? Math.abs(to.y - from.y) : Math.abs(to.x - from.x);
}

function targetRank(t: PaneTarget): number {
  if (t.kind === "seam") return 0;
  if (t.kind === "pane") return 1;
  return 2;
}

function resolveTarget(geom: PaneGeometry, target: PaneTarget | null): PaneTarget {
  if (target && targetRect(geom, target)) return target;
  return geom.panes[0] ? { kind: "pane", paneId: geom.panes[0].paneId } : { kind: "edge", side: "left" };
}

export function stepPaneTarget(root: LayoutNode, target: PaneTarget | null, dir: PaneDirection): PaneTarget {
  const geom = computePaneGeometry(root);
  const current = resolveTarget(geom, target);
  const currentRect = targetRect(geom, current);
  if (!currentRect) return current;
  const from = center(currentRect);
  const candidates = allTargets(geom)
    .filter((candidate) => !samePaneTarget(candidate, current))
    .map((candidate) => ({ candidate, rect: targetRect(geom, candidate) }))
    .filter((x): x is { candidate: PaneTarget; rect: Rect } => !!x.rect)
    .map((x) => ({ ...x, c: center(x.rect) }))
    .filter((x) => isAhead(from, x.c, dir))
    .sort((a, b) => {
      const ap = primaryDistance(from, a.c, dir);
      const bp = primaryDistance(from, b.c, dir);
      if (Math.abs(ap - bp) > EPS) return ap - bp;
      const ac = crossDistance(from, a.c, dir);
      const bc = crossDistance(from, b.c, dir);
      if (Math.abs(ac - bc) > EPS) return ac - bc;
      return targetRank(a.candidate) - targetRank(b.candidate);
    });
  return candidates[0]?.candidate ?? current;
}

export function readingOrderPanes(root: LayoutNode): PaneRect[] {
  return [...computePaneGeometry(root).panes].sort((a, b) => {
    const ay = a.rect.y + a.rect.h / 2;
    const by = b.rect.y + b.rect.h / 2;
    if (Math.abs(ay - by) > EPS) return ay - by;
    const ax = a.rect.x + a.rect.w / 2;
    const bx = b.rect.x + b.rect.w / 2;
    if (Math.abs(ax - bx) > EPS) return ax - bx;
    return a.paneId.localeCompare(b.paneId);
  });
}

export function nearestPane(root: LayoutNode, sourcePaneId: string, exclude = sourcePaneId): string | null {
  const geom = computePaneGeometry(root);
  const source = geom.panes.find((p) => p.paneId === sourcePaneId) ?? geom.panes[0];
  if (!source) return null;
  const from = center(source.rect);
  const candidates = geom.panes
    .filter((p) => p.paneId !== exclude)
    .map((p) => {
      const c = center(p.rect);
      const dx = c.x - from.x;
      const dy = c.y - from.y;
      return { paneId: p.paneId, distance: dx * dx + dy * dy };
    })
    .sort((a, b) => a.distance - b.distance || a.paneId.localeCompare(b.paneId));
  return candidates[0]?.paneId ?? null;
}

export function nearestPaneInDirection(root: LayoutNode, sourcePaneId: string, dir: PaneDirection): string | null {
  const geom = computePaneGeometry(root);
  const source = geom.panes.find((p) => p.paneId === sourcePaneId);
  if (!source) return null;
  const from = center(source.rect);
  const candidates = geom.panes
    .filter((p) => p.paneId !== sourcePaneId)
    .map((p) => ({ paneId: p.paneId, c: center(p.rect) }))
    .filter((p) => isAhead(from, p.c, dir))
    .sort((a, b) => {
      const ap = primaryDistance(from, a.c, dir);
      const bp = primaryDistance(from, b.c, dir);
      if (Math.abs(ap - bp) > EPS) return ap - bp;
      const ac = crossDistance(from, a.c, dir);
      const bc = crossDistance(from, b.c, dir);
      if (Math.abs(ac - bc) > EPS) return ac - bc;
      return a.paneId.localeCompare(b.paneId);
    });
  return candidates[0]?.paneId ?? null;
}

export function enterPaneSelect(paneId: string): void {
  previousPaneTarget = paneId;
  setPaneSel({ kind: "pane", paneId });
}

export function exitPaneSelect(): void {
  setPaneSel(null);
}

export function previousPaneSelectionTarget(): string | null {
  return previousPaneTarget;
}

export function movePaneSelection(root: LayoutNode, dir: PaneDirection): PaneTarget {
  const current = paneSel();
  if (current?.kind === "pane") previousPaneTarget = current.paneId;
  const next = stepPaneTarget(root, current, dir);
  if (next.kind === "pane") previousPaneTarget = next.paneId;
  setPaneSel(next);
  return next;
}
