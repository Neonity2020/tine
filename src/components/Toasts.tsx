import { For, Show, type JSX } from "solid-js";
import { toasts, dismissToast, lightbox, setLightbox } from "../ui";

// Bottom-right transient notifications.
export function Toasts(): JSX.Element {
  return (
    <div class="toast-stack">
      <For each={toasts()}>
        {(t) => (
          <div class={`toast toast-${t.kind}`} onClick={() => dismissToast(t.id)}>
            {t.message}
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
