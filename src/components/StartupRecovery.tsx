import { Show, type JSX } from "solid-js";
import { isTauri } from "../backend";
import {
  STARTUP_PROGRESS_VISIBLE_MS,
  startupGraphName,
  startupPhaseLabel,
  type createStartupRecoveryController,
} from "../startupRecovery";
import { isMac, osDrawsWindowControls } from "../nativeChrome";
import { WindowControls } from "./WindowChrome";

type StartupController = ReturnType<typeof createStartupRecoveryController>;

function elapsedLabel(milliseconds: number): string {
  if (milliseconds < 1_000) return `${Math.max(0, Math.round(milliseconds))} ms`;
  return `${(milliseconds / 1_000).toFixed(milliseconds < 10_000 ? 1 : 0)} s`;
}

export function StartupRecoveryLayer(props: { controller: StartupController }): JSX.Element {
  const current = props.controller.snapshot;
  const visible = () => current().mode === "recovery"
    || (current().mode === "working" && current().elapsedMs >= STARTUP_PROGRESS_VISIBLE_MS);
  const phase = () => current().phase;
  const targetName = () => current().target ? startupGraphName(current().target) : null;
  const canReturnToDirectFiles = () => Boolean(current().target)
    && (current().mode === "recovery"
      || (current().mode === "working" && current().operation === "graph_open"));
  const canOpenAnother = () => current().mode !== "idle";

  return (
    <Show when={visible()}>
      <div
        class="startup-recovery-overlay"
        role={current().mode === "recovery" ? "alertdialog" : "status"}
        aria-modal={current().mode === "recovery" ? "true" : undefined}
        aria-live="polite"
        data-startup-phase={phase()}
      >
        <Show when={isTauri() && !isMac && !osDrawsWindowControls()}>
          <div class="startup-recovery-winchrome" data-tauri-drag-region>
            <WindowControls />
          </div>
        </Show>
        <div class="startup-recovery-card">
          <div class="startup-recovery-mark" aria-hidden="true">T</div>
          <h1>
            {current().mode === "recovery"
              ? "Tine needs help opening this workspace"
              : "Opening Tine"}
          </h1>
          <Show when={targetName()}>
            {(name) => <p class="startup-recovery-target">Workspace: <strong>{name()}</strong></p>}
          </Show>
          <p class="startup-recovery-phase">{startupPhaseLabel(phase())}</p>
          <p class="startup-recovery-elapsed">
            {elapsedLabel(current().elapsedMs)}
          </p>
          <Show when={current().mode === "recovery"}>
            <p class="startup-recovery-detail">{current().detail}</p>
            <p class="startup-recovery-safety">
              Tine has not discarded managed-storage data. You can retry, choose another graph,
              or close and relaunch Tine before attempting manual recovery.
            </p>
          </Show>
          <Show when={canOpenAnother() || canReturnToDirectFiles()}>
            <div class="startup-recovery-actions">
              <Show when={current().mode === "recovery"}>
                <button type="button" class="settings-btn primary" onClick={() => props.controller.retry()}>
                  Retry lookup
                </button>
              </Show>
              <Show when={canOpenAnother()}>
                <button type="button" class="settings-btn" onClick={() => void props.controller.openAnother()}>
                  Open another graph…
                </button>
              </Show>
              <Show when={canReturnToDirectFiles()}>
                <button type="button" class="settings-btn danger" onClick={() => void props.controller.returnToDirectFiles()}>
                  Return {targetName()} to Direct Files…
                </button>
              </Show>
              <Show when={current().mode === "recovery"}>
                <button type="button" class="settings-btn" onClick={() => void props.controller.copyDetails()}>
                  Copy details
                </button>
              </Show>
            </div>
          </Show>
          <details class="startup-recovery-technical">
            <summary>Technical phase</summary>
            <code>{phase()}</code>
          </details>
        </div>
      </div>
    </Show>
  );
}
