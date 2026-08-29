// Refresh a graph asset's rendered <img> after it was edited in an external app
// (GH #38). The native watcher now observes assets separately from graph text;
// this focus path remains a fallback for a missed platform event and for the
// exact editor Tine launched. The listener installs lazily on first use.

import { invalidateAsset, refreshAsset } from "./assetCache";

const pending = new Set<string>();
let installed = false;

function flush(): void {
  if (!pending.size) return;
  for (const rel of pending) refreshAsset(rel);
  pending.clear();
}

function install(): void {
  if (installed || typeof window === "undefined") return;
  installed = true;
  window.addEventListener("focus", flush);
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden) flush();
  });
}

/** Mark an asset for refresh when Tine next regains focus. Call right after
 *  launching an external editor for it. */
export function refreshAssetOnReturn(rel: string): void {
  if (!rel) return;
  pending.add(rel);
  install();
}

const DEFERRED_MEDIA_EXTENSIONS = new Set([
  "pdf",
  "mp3", "mpeg", "m4a", "aac", "wav", "ogg", "oga", "opus", "flac",
  "mp4", "m4v", "webm", "ogv", "mov", "mkv",
]);

function deferHotSwap(rel: string): boolean {
  const clean = rel.split(/[?#]/, 1)[0];
  const ext = clean.split(".").pop()?.toLowerCase();
  return !!ext && DEFERRED_MEDIA_EXTENSIONS.has(ext);
}

/** Apply one native watcher epoch. Images (and image-like unknown embeds)
 * refresh in place. PDF/audio/video never receive a reactive re-key, so an
 * already-open document or playback session stays on its current bytes; their
 * uncached open paths read current disk bytes the next time they are opened. */
export function applyObservedAssetChanges(paths: string[]): void {
  for (const rel of new Set(paths.filter(Boolean))) {
    if (deferHotSwap(rel)) invalidateAsset(rel);
    else refreshAsset(rel);
  }
}
