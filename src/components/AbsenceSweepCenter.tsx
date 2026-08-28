import { For, Show, type JSX } from "solid-js";

import type { SyncAbsenceSweepEvent } from "../types";
import {
  absenceSweepPanelOpen,
  absenceSweeps,
  closeAbsenceSweepPanel,
  keepAbsenceSweepDeletion,
  openAbsenceSweepPanel,
  reapplyAbsenceSweep,
  restoreAbsenceSweep,
} from "../absenceSweeps";

function pageLabel(path: string): string {
  const filename = path.split("/").at(-1) ?? path;
  return filename.replace(/\.(md|markdown|org)$/i, "");
}

function actionBusy(sweep: SyncAbsenceSweepEvent): boolean {
  return sweep.latest_action?.state === "started" || sweep.latest_action?.state === "progress";
}

function statusLabel(sweep: SyncAbsenceSweepEvent): string {
  const action = sweep.latest_action;
  if (!action) return "Waiting for your decision";
  if (action.state === "failed") return action.action === "restore" ? "Restore failed" : "Action failed";
  if (action.state === "started" || action.state === "progress") {
    if (action.action === "restore") return "Restoring…";
    if (action.action === "reapply") return "Re-applying deletion…";
  }
  if (action.action === "restore" && action.state === "completed") return "Restored";
  if (action.action === "reapply" && action.state === "completed") return "Deletion re-applied";
  if (action.action === "keep_deletion" && action.state === "completed") return "Deletion kept";
  return "Working…";
}

function SweepCard(props: { sweep: SyncAbsenceSweepEvent }): JSX.Element {
  const action = () => props.sweep.latest_action;
  const failedRestore = () => action()?.action === "restore" && action()?.state === "failed";
  return (
    <article class="absence-sweep-card">
      <header class="absence-sweep-card-header">
        <div>
          <span class={`absence-sweep-tier absence-sweep-${props.sweep.tier}`}>
            {props.sweep.tier === "tier3" ? "Tier 3" : "Tier 2"}
          </span>
          <h3>{props.sweep.absence_count} deleted pages</h3>
        </div>
        <span class="absence-sweep-status">{statusLabel(props.sweep)}</span>
      </header>

      <Show when={action()?.action === "restore" && action()?.state === "progress"}>
        <div class="absence-sweep-progress" role="status">
          <span>Chunk {action()!.chunk_ordinal}</span>
          <span>{action()!.remaining_operation_watermark} operations remaining</span>
        </div>
      </Show>
      <Show when={action()?.state === "failed" && action()?.failure_reason}>
        <p class="absence-sweep-failure" role="alert">{action()!.failure_reason}</p>
      </Show>

      <ul class="absence-sweep-members">
        <For each={props.sweep.members}>
          {(member) => (
            <li>
              <span>{pageLabel(member.path)}</span>
              <code>{member.path}</code>
            </li>
          )}
        </For>
      </ul>

      <Show when={props.sweep.disposed_at_unix_ms === null}>
        <div class="absence-sweep-actions">
          <button
            class="absence-sweep-action absence-sweep-action-primary"
            disabled={actionBusy(props.sweep)}
            onClick={() => void restoreAbsenceSweep(props.sweep.sweep_id)}
          >
            {failedRestore() ? "Run Restore again" : "Restore"}
          </button>
          <button
            class="absence-sweep-action"
            disabled={actionBusy(props.sweep)}
            onClick={() => void reapplyAbsenceSweep(props.sweep.sweep_id)}
          >
            Re-apply
          </button>
          <button
            class="absence-sweep-action absence-sweep-action-danger"
            disabled={actionBusy(props.sweep)}
            onClick={() => void keepAbsenceSweepDeletion(props.sweep.sweep_id)}
          >
            Keep deletion
          </button>
        </div>
      </Show>
    </article>
  );
}

export function AbsenceSweepCenter(): JSX.Element {
  const total = () => absenceSweeps()
    .filter((sweep) => sweep.disposed_at_unix_ms === null)
    .reduce((sum, sweep) => sum + sweep.absence_count, 0);
  return (
    <Show when={absenceSweeps().length > 0}>
      <button class="absence-sweep-dock" onClick={openAbsenceSweepPanel}>
        <span class="absence-sweep-dock-icon" aria-hidden="true">↶</span>
        <span>{total() || absenceSweeps()[0].absence_count} deleted pages</span>
        <strong>{total() ? "Review" : "History"}</strong>
      </button>

      <Show when={absenceSweepPanelOpen()}>
        <section class="absence-sweep-panel" aria-label="Deleted page recovery">
          <header class="absence-sweep-panel-header">
            <div>
              <span class="absence-sweep-eyebrow">Managed storage safety</span>
              <h2>Deleted pages</h2>
              <p>Choose what Tine should do with each detected group.</p>
            </div>
            <button
              class="absence-sweep-panel-close"
              aria-label="Close deleted pages panel"
              onClick={closeAbsenceSweepPanel}
            >
              ×
            </button>
          </header>
          <div class="absence-sweep-list">
            <For each={absenceSweeps()}>{(sweep) => <SweepCard sweep={sweep} />}</For>
          </div>
          <p class="absence-sweep-panel-note">
            Closing this panel leaves every sweep waiting.{"\u00a0"}
            Only the three actions above record a decision.
          </p>
        </section>
      </Show>
    </Show>
  );
}
