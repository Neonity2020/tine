import { createSignal } from "solid-js";

// A focus-driven disk rescan must finish before a new edit starts. This module
// is intentionally dependency-free so both the focus coordinator and the
// editor controller can share the gate without creating an App/store cycle.
const [freshnessPending, setFreshnessPending] = createSignal(false);
const [freshnessVisible, setFreshnessVisible] = createSignal(false);
let deferredEditorStart: (() => void) | null = null;
let visibilityTimer: ReturnType<typeof setTimeout> | null = null;

export { freshnessPending, freshnessVisible };

export function beginFreshnessBarrier(): void {
  setFreshnessPending(true);
  if (visibilityTimer === null) {
    visibilityTimer = setTimeout(() => {
      visibilityTimer = null;
      if (freshnessPending()) setFreshnessVisible(true);
    }, 120);
  }
}

export function endFreshnessBarrier(): void {
  setFreshnessPending(false);
  if (visibilityTimer !== null) {
    clearTimeout(visibilityTimer);
    visibilityTimer = null;
  }
  setFreshnessVisible(false);
  const deferred = deferredEditorStart;
  deferredEditorStart = null;
  deferred?.();
}

/** Defer the newest attempted editor activation until fresh page state is
 * installed. Returns true when the caller must stop now. */
export function deferEditorStartUntilFresh(start: () => void): boolean {
  if (!freshnessPending()) return false;
  deferredEditorStart = start;
  return true;
}

/** Existing textareas can retain focus across suspend. Block input capture at
 * the window boundary too; startEditing alone only covers new activations. */
export function installFreshnessInputGate(): void {
  if (typeof window === "undefined") return;
  const block = (event: Event) => {
    if (!freshnessPending()) return;
    event.preventDefault();
    event.stopImmediatePropagation();
  };
  window.addEventListener("beforeinput", block, true);
  window.addEventListener("compositionstart", block, true);
  window.addEventListener("keydown", block, true);
}
