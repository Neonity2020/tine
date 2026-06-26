import { For, Show, type JSX } from "solid-js";
import { toasts, dismissToast, lightbox, setLightbox } from "../ui";

// Bottom-right transient notifications.
export function Toasts(): JSX.Element {
  return (
    <div class="toast-stack">
      <For each={toasts()}>
        {(t) => (
          <div
            class={`toast toast-${t.kind}`}
            classList={{ "toast-sticky": t.sticky }}
            // Transient toasts dismiss on any click; sticky ones only via the ✕.
            onClick={() => !t.sticky && dismissToast(t.id)}
          >
            <span class="toast-msg">{t.message}</span>
            <button
              class="toast-close"
              aria-label="Dismiss"
              onClick={(e) => {
                e.stopPropagation();
                dismissToast(t.id);
              }}
            >
              ×
            </button>
          </div>
        )}
      </For>
    </div>
  );
}

// Full-screen image viewer; click anywhere or press the backdrop to close.
export function Lightbox(): JSX.Element {
  return (
    <Show when={lightbox()}>
      <div class="lightbox-overlay" onClick={() => setLightbox(null)}>
        <img class="lightbox-img" src={lightbox()!} alt="" />
      </div>
    </Show>
  );
}
