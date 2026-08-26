import { createSignal } from "solid-js";

export const DEFAULT_STANDARD_CONTENT_WIDTH = 810;
export const DEFAULT_CUSTOM_WIDE_CONTENT_WIDTH = 1280;
export const MIN_CONTENT_WIDTH = 320;
export const MAX_CONTENT_WIDTH = 8192;
export const CONTENT_WIDTH_SLIDER_MAX = 2400;

const STANDARD_KEY = "logseq-claude.standard-content-width";
const WIDE_KEY = "logseq-claude.wide-content-width";

function readWidth(key: string): number | null {
  try {
    const stored = localStorage.getItem(key);
    if (stored === null) return null;
    const value = Number(stored);
    return Number.isFinite(value) && value >= MIN_CONTENT_WIDTH && value <= MAX_CONTENT_WIDTH
      ? Math.round(value)
      : null;
  } catch {
    return null;
  }
}

function saveWidth(key: string, value: number | null): void {
  try {
    if (value === null) localStorage.removeItem(key);
    else localStorage.setItem(key, String(value));
  } catch {
    // Keep the in-memory preference usable when device storage is unavailable.
  }
}

export function normalizeContentWidth(value: number): number {
  if (!Number.isFinite(value)) return MIN_CONTENT_WIDTH;
  return Math.min(MAX_CONTENT_WIDTH, Math.max(MIN_CONTENT_WIDTH, Math.round(value)));
}

export const [standardContentWidth, setStandardContentWidthSignal] =
  createSignal<number | null>(readWidth(STANDARD_KEY));
export const [wideContentWidth, setWideContentWidthSignal] =
  createSignal<number | null>(readWidth(WIDE_KEY));

/** Apply only explicit user overrides; themes retain ownership of both defaults. */
export function applyContentWidths(): void {
  const root = document.documentElement;
  const standard = standardContentWidth();
  const wide = wideContentWidth();
  if (standard === null) root.style.removeProperty("--tine-main-content-max-width");
  else root.style.setProperty("--tine-main-content-max-width", `${standard}px`);
  if (wide === null) root.style.removeProperty("--tine-wide-content-max-width");
  else root.style.setProperty("--tine-wide-content-max-width", `${wide}px`);
}

export function changeStandardContentWidth(value: number): void {
  if (!Number.isFinite(value)) return;
  const next = normalizeContentWidth(value);
  setStandardContentWidthSignal(next);
  saveWidth(STANDARD_KEY, next);
  applyContentWidths();
}

export function resetStandardContentWidth(): void {
  setStandardContentWidthSignal(null);
  saveWidth(STANDARD_KEY, null);
  applyContentWidths();
}

export function changeWideContentWidth(value: number | null): void {
  if (value !== null && !Number.isFinite(value)) return;
  const next = value === null ? null : normalizeContentWidth(value);
  setWideContentWidthSignal(next);
  saveWidth(WIDE_KEY, next);
  applyContentWidths();
}
