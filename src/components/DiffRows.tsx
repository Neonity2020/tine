// The block-diff row renderer.
//
// It was extracted (P4) because two surfaces rendered the same data — the
// Settings merge modal and the in-page resolver — and two independently-written
// renderers drift apart silently; it had already happened in this codebase with
// the two block-facet renderers. P5 finished the job at the level above: the
// modal is gone, so the in-page resolver (Concord L4) is now the ONE resolution
// surface and this is its renderer. The surface-shaped props (column labels, the
// default decision) are kept — they are what a second surface would have to use
// instead of a second copy, should one ever be justified.
import { For, Show, type JSX } from "solid-js";
import type { DiffRow, MergeDecision, RowKind } from "../types";

/** The effective decision for a row, given the surface's own default. */
export function decisionOf(
  decisions: Record<string, MergeDecision>,
  id: string,
  fallback: MergeDecision = "mine"
): MergeDecision {
  return decisions[id] ?? fallback;
}

/** The choice that loses NOTHING for a row of this kind: keep both bodies where
 *  both exist, keep a block only one side has. Concord's no-loss default (L3) —
 *  used where no 3-way suggestion is available to lead with. */
export function noLossDecision(kind: RowKind): MergeDecision {
  if (kind === "added") return "mine"; // present only here — keeping it loses nothing
  if (kind === "removed") return "theirs"; // present only there — pull it in
  return "both";
}

/** The in-page resolver's opening position: the SUGGESTED resolution wherever the
 *  base justifies one (so the normal gesture is glance-and-confirm), and the
 *  no-loss choice everywhere else. Still just a pre-selection. */
export function seedSuggestedOrNoLoss(
  rows: DiffRow[],
  out: Record<string, MergeDecision> = {}
): Record<string, MergeDecision> {
  for (const r of rows) {
    if (r.kind !== "unchanged") out[r.id] = r.suggestion ?? noLossDecision(r.kind);
    if (r.children.length) seedSuggestedOrNoLoss(r.children, out);
  }
  return out;
}

/** Every row that needs a decision (id + kind), flattened, depth-first. */
export function collectRows(
  rows: DiffRow[],
  out: { id: string; kind: RowKind }[] = []
): { id: string; kind: RowKind }[] {
  for (const r of rows) {
    if (r.kind !== "unchanged") out.push({ id: r.id, kind: r.kind });
    if (r.children.length) collectRows(r.children, out);
  }
  return out;
}

export function firstLine(text: string): string {
  const l = text.split("\n").find((s) => s.trim().length) ?? "";
  return l.trim();
}

/** Column/segment wording for one surface. The Settings modal talks about a
 *  "conflict copy"; the in-page resolver names the two sides the artifact itself
 *  named (a git ref, a Syncthing device tag). */
export interface DiffRowLabels {
  mine: string;
  theirs: string;
}

// One diff row (recursive: a modified row shows its aligned children indented).
export function DiffRowView(props: {
  row: DiffRow;
  depth: number;
  decisions: Record<string, MergeDecision>;
  setDecision: (id: string, d: MergeDecision) => void;
  showUnchanged: boolean;
  /** Decision assumed for a row the user hasn't touched. */
  fallback?: MergeDecision;
  /** Per-row segment wording. Defaults to the Settings modal's. */
  labels?: DiffRowLabels;
}): JSX.Element {
  const row = () => props.row;
  const dec = () => decisionOf(props.decisions, row().id, props.fallback ?? "mine");
  const labels = () => props.labels ?? { mine: "Current", theirs: "Copy" };
  const seg = (value: MergeDecision, label: string, side: "mine" | "theirs") => (
    <button
      class="sync-merge-seg"
      classList={{ active: dec() === value }}
      data-side={side}
      data-decision={value}
      onClick={() => props.setDecision(row().id, value)}
    >
      {label}
    </button>
  );
  return (
    <Show when={props.showUnchanged || row().kind !== "unchanged"}>
      <div
        class="sync-merge-row"
        data-kind={row().kind}
        data-row-id={row().id}
        style={{ "padding-left": `${props.depth * 16}px` }}
      >
        <div class="sync-merge-cols">
          <div class="sync-merge-cell mine" classList={{ chosen: row().kind !== "removed" && dec() !== "theirs" }}>
            {row().mine ? firstLine(row().mine!.text) : <span class="sync-merge-absent">—</span>}
            <Show when={(row().mine?.child_count ?? 0) > 0}>
              <span class="sync-merge-kids"> +{row().mine!.child_count}</span>
            </Show>
          </div>
          <div class="sync-merge-cell theirs" classList={{ chosen: dec() === "theirs" || dec() === "both" }}>
            {row().theirs ? firstLine(row().theirs!.text) : <span class="sync-merge-absent">—</span>}
            <Show when={(row().theirs?.child_count ?? 0) > 0}>
              <span class="sync-merge-kids"> +{row().theirs!.child_count}</span>
            </Show>
          </div>
        </div>
        <div class="sync-merge-controls">
          <Show when={row().kind === "modified"}>
            {seg("mine", labels().mine, "mine")}
            {seg("theirs", labels().theirs, "theirs")}
            {seg("both", "Both", "theirs")}
          </Show>
          <Show when={row().kind === "added"}>
            {seg("mine", "Keep", "mine")}
            {seg("theirs", "Drop", "theirs")}
          </Show>
          <Show when={row().kind === "removed"}>
            {seg("mine", "Skip", "mine")}
            {seg("theirs", "Pull in", "theirs")}
          </Show>
          <Show when={row().kind === "unchanged"}>
            <span class="sync-merge-unchanged-tag">unchanged</span>
          </Show>
          <Show when={row().suggestion && dec() === row().suggestion}>
            <span
              class="sync-merge-suggested-tag"
              title="Pre-selected from the last version this file and Tine agreed on"
            >
              suggested
            </span>
          </Show>
        </div>
      </div>
      <For each={row().children}>
        {(child) => (
          <DiffRowView
            row={child}
            depth={props.depth + 1}
            decisions={props.decisions}
            setDecision={props.setDecision}
            showUnchanged={props.showUnchanged}
            fallback={props.fallback}
            labels={props.labels}
          />
        )}
      </For>
    </Show>
  );
}
