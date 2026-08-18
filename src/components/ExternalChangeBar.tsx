// Concord P5 — the "always ask" surface.
//
// With the policy off (the default) this never renders: a clean page reloads
// silently, as it always has. With it on, an external change to a page you have
// open is HELD and announced here — a calm bar above the page, never a modal,
// never blocking, exactly like the VCS-marker banner beside it.
//
// Two answers, and neither invents a write path: "Reload from disk" re-enters
// the ordinary external-change handler (so leases, disposition and deferred
// replay all still apply), and "Keep mine" just drops the record — the backend
// cache already holds the new bytes, so the next save meets the base_rev guard
// and raises the ordinary conflict banner.
import { Show, type JSX } from "solid-js";
import {
  applyHeldExternalChange,
  dismissHeldExternalChange,
  heldExternalChangeFor,
} from "../conflictPolicy";

export function ExternalChangeBar(props: { name: string }): JSX.Element {
  return (
    <Show when={heldExternalChangeFor(props.name)}>
      <div class="external-change-bar" role="status">
        <span class="external-change-text">
          This page changed on disk. You asked to be told rather than shown, so Tine is still
          displaying the version you were reading.
        </span>
        <span class="external-change-actions">
          <button
            class="settings-btn settings-btn-primary"
            onClick={() => applyHeldExternalChange(props.name)}
          >
            Reload from disk
          </button>
          <button class="settings-btn" onClick={() => dismissHeldExternalChange(props.name)}>
            Keep mine
          </button>
        </span>
      </div>
    </Show>
  );
}
