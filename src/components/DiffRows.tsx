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
import { For, Show, createMemo, createSignal, type JSX } from "solid-js";
import type { DiffRow, MergeDecision, MergedSource, RowKind } from "../types";

/** Why this body is on offer. The two sources carry different guarantees, so
 *  the strip says which one produced the text: Tine composed it from two edits
 *  that touch different parts of the body, or the merge tool that left the
 *  markers proposed it and Tine only checked that it is still one block.
 *  Neither is ever applied without the user's confirmation. */
export function mergedTitle(source: MergedSource): string {
  return source === "artifact"
    ? "Proposed by your merge tool's suggested resolution — still applied only when you confirm."
    : "Both edits combined — offered because they touch different parts of the same body";
}

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
 *  no-loss choice everywhere else. Still just a pre-selection.
 *
 *  A row whose two edits were disjoint suggests `"merged"` and seeds like any
 *  other suggestion — the fourth outcome is a suggestion, never an auto-apply. */
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

/** The "Apply all suggested" sweep: like [seedSuggestedOrNoLoss], except a row
 *  whose suggestion is a merge TOOL's own text (artifact source) keeps its
 *  current decision. The batch button vouches only for what Tine computed
 *  itself; it never flips a row back to text Tine cannot vouch for — such rows
 *  keep their initial pre-selection until the user touches them, and an
 *  explicit per-row choice is never overridden by the sweep. */
export function seedSuggestedExceptArtifact(
  rows: DiffRow[],
  out: Record<string, MergeDecision>
): Record<string, MergeDecision> {
  for (const r of rows) {
    const artifactMerge = r.suggestion === "merged" && r.merged?.source === "artifact";
    if (r.kind !== "unchanged" && !artifactMerge) {
      out[r.id] = r.suggestion ?? noLossDecision(r.kind);
    }
    if (r.children.length) seedSuggestedExceptArtifact(r.children, out);
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

/** Human wording for a sync tool's conflict-copy tag.
 *
 *  The raw tag ("sync-conflict-20260705-141233-A2B2C3D", "Martin's conflicted
 *  copy 2026-07-05") identifies the FILE, but as a side label it drowns every
 *  layout it appears in — the legend, the bulk buttons, and (fatally, on a
 *  phone) the per-row segments. Display "Sync copy · Jul 5" instead and keep
 *  the raw tag as the tooltip. Non-matching labels pass through untouched. */
export function humanizeSideLabel(label: string): { text: string; title?: string } {
  const syncthing = label.match(/^sync-conflict-(\d{4})(\d{2})(\d{2})-\d{6}-[A-Za-z0-9]+$/);
  const dropbox = label.match(/conflicted copy (\d{4})-(\d{2})-(\d{2})/i);
  const m = syncthing ?? dropbox;
  if (!m) return { text: label };
  const [, y, mo, d] = m;
  const months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
  const month = months[Number(mo) - 1] ?? mo;
  const year = new Date().getFullYear() === Number(y) ? "" : ` ${y}`;
  return { text: `Sync copy · ${month} ${Number(d)}${year}`, title: label };
}

/** The line index a collapsed row previews.
 *
 *  A block body can be many lines (a logbook, a multi-line quote) while the two
 *  sides differ far down it; previewing line 0 then showed two identical strings
 *  next to a decision the user could not make. So: preview the first line that
 *  actually differs.
 *
 *  INVARIANT (zero regression on the common case): when the two first non-blank
 *  lines already differ this returns 0, and 0 renders exactly `firstLine` — the
 *  single-line row every conflict used to be is byte-identical to before. Only
 *  once those agree do we walk raw lines, so the index is an index into
 *  `split("\n")`, not into the non-blank subsequence. Identical bodies have no
 *  differing line and also return 0. */
export function firstDifferingLine(mine: string, theirs: string): number {
  if (firstLine(mine) !== firstLine(theirs)) return 0;
  const a = mine.split("\n");
  const b = theirs.split("\n");
  const max = Math.max(a.length, b.length);
  for (let i = 0; i < max; i++) {
    if (a[i] !== b[i]) return i;
  }
  return 0;
}

/** What one column shows for a collapsed row: its own line `k`, trimmed, or
 *  `null` when this side has no such line (shorter body → the absent marker).
 *  `k === 0` is `firstLine` verbatim, per the invariant above. */
export function previewLine(text: string, k: number): string | null {
  if (k === 0) return firstLine(text);
  const lines = text.split("\n");
  return k < lines.length ? lines[k].trim() : null;
}

/** Whether the collapsed preview can be the whole story. It cannot when some
 *  involved body has more than one line, or when the bodies disagree on a line
 *  other than the previewed one — either way the user needs the expander to see
 *  what a decision actually costs. */
export function needsExpander(texts: (string | null | undefined)[], previewIndex: number): boolean {
  const bodies = texts.filter((t): t is string => t != null).map((t) => t.split("\n"));
  if (bodies.length < 2) return false;
  if (bodies.some((b) => b.length > 1)) return true;
  const max = bodies.reduce((n, b) => Math.max(n, b.length), 0);
  for (let i = 0; i < max; i++) {
    if (i === previewIndex) continue;
    if (bodies.some((b) => b[i] !== bodies[0][i])) return true;
  }
  return false;
}

/** Per body, which of its lines are NOT identical across every displayed body at
 *  the same index. Plain per-line equality — v1 has no intraline spans, and for
 *  the two-body case this is exactly "differs from the other side". */
export function differingLineFlags(bodies: string[][]): boolean[][] {
  const max = bodies.reduce((n, b) => Math.max(n, b.length), 0);
  const shared: boolean[] = [];
  for (let i = 0; i < max; i++) {
    shared.push(bodies.every((b) => b[i] === bodies[0][i]));
  }
  return bodies.map((b) => b.map((_, i) => !shared[i]));
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
  // Expansion is per-row and opt-in: collapsed stays one line per column, so a
  // logbook-heavy block cannot claim a phone screen until the user asks it to.
  const [expanded, setExpanded] = createSignal(false);
  // `short` is the narrow-container variant: a color dot (tying the option to
  // its side's hue, which the legend maps to the real name once) plus a fixed
  // short word — because a side LABEL repeated on every row is exactly what
  // starved the text cells of width on a phone. CSS container queries switch
  // between the two spans; only side-labeled segments carry a short form.
  const seg = (
    value: MergeDecision,
    label: string,
    side: "mine" | "theirs" | "merged",
    short?: string
  ) => (
    <button
      class="sync-merge-seg"
      classList={{ active: dec() === value }}
      data-side={side}
      data-decision={value}
      title={short && short !== label ? label : undefined}
      onClick={() => props.setDecision(row().id, value)}
    >
      <Show when={short} fallback={label}>
        <span class="sync-merge-seg-dot" data-side={side} aria-hidden="true" />
        <span class="sync-merge-seg-long">{label}</span>
        <span class="sync-merge-seg-short">{short}</span>
      </Show>
    </button>
  );
  // One index for the whole row: both columns and the merged strip preview the
  // SAME line, so they stay comparable side by side.
  const previewIndex = createMemo(() => {
    const r = row();
    return r.mine && r.theirs ? firstDifferingLine(r.mine.text, r.theirs.text) : 0;
  });
  const preview = (text: string) => {
    const k = previewIndex();
    const line = previewLine(text, k);
    return (
      <>
        <Show when={k > 0}>
          <span class="sync-merge-elided" title="Earlier lines are the same on both sides">
            …
          </span>
        </Show>
        {line === null ? <span class="sync-merge-absent">—</span> : line}
      </>
    );
  };
  const bodies = createMemo(() => {
    const r = row();
    const out: { side: "mine" | "theirs" | "merged"; label: string; lines: string[] }[] = [];
    if (r.mine) out.push({ side: "mine", label: labels().mine, lines: r.mine.text.split("\n") });
    if (r.theirs) out.push({ side: "theirs", label: labels().theirs, lines: r.theirs.text.split("\n") });
    if (r.merged) out.push({ side: "merged", label: "Merged", lines: r.merged.text.split("\n") });
    return out;
  });
  const expandable = createMemo(
    () =>
      row().kind === "modified"
      && needsExpander(
        [row().mine?.text, row().theirs?.text, row().merged?.text],
        previewIndex()
      )
  );
  const lineCount = createMemo(() => bodies().reduce((n, b) => Math.max(n, b.lines.length), 0));
  const expandedBodies = createMemo(() => {
    if (!expanded()) return [];
    const bs = bodies();
    const flags = differingLineFlags(bs.map((b) => b.lines));
    return bs.map((b, i) => ({ ...b, flags: flags[i] }));
  });
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
            {row().mine ? preview(row().mine!.text) : <span class="sync-merge-absent">—</span>}
            <Show when={(row().mine?.child_count ?? 0) > 0}>
              <span class="sync-merge-kids"> +{row().mine!.child_count}</span>
            </Show>
          </div>
          <div class="sync-merge-cell theirs" classList={{ chosen: dec() === "theirs" || dec() === "both" }}>
            {row().theirs ? preview(row().theirs!.text) : <span class="sync-merge-absent">—</span>}
            <Show when={(row().theirs?.child_count ?? 0) > 0}>
              <span class="sync-merge-kids"> +{row().theirs!.child_count}</span>
            </Show>
          </div>
        </div>
        <div class="sync-merge-controls">
          <Show when={row().kind === "modified"}>
            {seg("mine", labels().mine, "mine", "Mine")}
            {seg("theirs", labels().theirs, "theirs", "Theirs")}
            {seg("both", "Both", "theirs")}
            <Show when={row().merged}>{seg("merged", "Merged", "merged")}</Show>
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
          <Show when={expandable()}>
            <button
              class="sync-merge-expand"
              title={expanded() ? "Hide the full bodies" : `Show all ${lineCount()} lines`}
              aria-expanded={expanded()}
              onClick={() => setExpanded(!expanded())}
            >
              {expanded() ? "⌃" : `⌄ ${lineCount()}`}
            </button>
          </Show>
        </div>
        {/* The merged proposal is a FULL-WIDTH strip under the two columns, not a
            third column: three columns do not survive a phone. */}
        <Show when={row().kind === "modified" ? row().merged : null}>
          {(merged) => (
            <div
              class="sync-merge-cell merged"
              classList={{ chosen: dec() === "merged" }}
              data-side="merged"
              data-source={merged().source}
              title={mergedTitle(merged().source)}
            >
              <span class="sync-merge-mergedtag">
                {merged().source === "artifact" ? "Merged (tool)" : "Merged"}
              </span>
              {preview(merged().text)}
            </div>
          )}
        </Show>
        <Show when={expanded()}>
          <div class="sync-merge-expanded">
            <For each={expandedBodies()}>
              {(body) => (
                <div class="sync-merge-fulltext" data-side={body.side}>
                  <div class="sync-merge-fulltext-label">{body.label}</div>
                  <div class="sync-merge-fulltext-body">
                    <For each={body.lines}>
                      {(line, i) => (
                        <div class="sync-merge-fulltext-line" classList={{ differs: body.flags[i()] }}>
                          {line}
                        </div>
                      )}
                    </For>
                  </div>
                </div>
              )}
            </For>
          </div>
        </Show>
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
