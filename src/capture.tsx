// Quick-capture mini window. A separate, tiny Tauri webview (capture.html) that
// the running app pops on a `tine --capture` signal. On submit it emits a global
// `quick-capture` event; the MAIN window turns that into an append to today's
// journal (App.tsx). This entry deliberately pulls in nothing from the main app
// (no store, no app.css) so it stays a minimal, instant-loading bundle.
import { render } from "solid-js/web";
import { onMount } from "solid-js";
import "./styles/capture.css";

function Capture() {
  let input!: HTMLTextAreaElement;

  const hideWindow = async () => {
    input.value = "";
    autosize();
    try {
      const { getCurrentWindow } = await import("@tauri-apps/api/window");
      await getCurrentWindow().hide();
    } catch {
      // not in Tauri (dev preview) — nothing to hide
    }
  };

  const submit = async () => {
    const text = input.value.trim();
    if (!text) {
      await hideWindow();
      return;
    }
    try {
      const { emit } = await import("@tauri-apps/api/event");
      await emit("quick-capture", { text });
    } catch {
      // ignore — emit only works inside Tauri
    }
    await hideWindow();
  };

  // Grow the textarea up to a few lines as the user types (the window itself is
  // fixed-height; this just avoids a cramped single line for multi-line captures).
  const autosize = () => {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 120)}px`;
  };

  const onKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void submit();
    } else if (e.key === "Escape") {
      e.preventDefault();
      void hideWindow();
    }
  };

  onMount(() => {
    input.focus();
    void (async () => {
      try {
        const { getCurrentWindow } = await import("@tauri-apps/api/window");
        // The window is created once (hidden) at startup, so onMount runs only
        // once. Re-focus the field every time it's shown, and dismiss on blur
        // (click-away / alt-tab) — standard quick-capture behaviour.
        await getCurrentWindow().onFocusChanged(({ payload: focused }) => {
          if (focused) queueMicrotask(() => input.focus());
          else void hideWindow();
        });
      } catch {
        // not in Tauri
      }
    })();
  });

  return (
    <div class="capture">
      <textarea
        ref={input}
        class="capture-input"
        rows={1}
        placeholder="Capture to today's journal…   ⏎ save · Esc cancel"
        onKeyDown={onKeyDown}
        onInput={autosize}
      />
    </div>
  );
}

render(() => <Capture />, document.getElementById("capture-root")!);
