// GH #283: ONE modified-click decision for internal page/block links, used by
// every surface (main content, references, query results, search results,
// Favorites, Recent, namespaces), so destinations can only be chosen here —
// not drifted per surface:
//
//   plain click                     → the surface's ordinary navigation;
//   Shift+click                     → right sidebar (Tine's convention);
//   Ctrl(Win/Linux)/Cmd(macOS)+click → background tab;
//   middle-click (per-surface aux)  → background tab.
//
// Combined modifiers deliberately keep plain-click meaning (matching
// GH #288's switcher guard chain) and interfaces stay modifier-free: there is
// no setting for this contract (Martin's approved product decision).
export type InternalLinkDest = "default" | "sidebar" | "background";

export function internalLinkDest(
  e: Pick<MouseEvent, "shiftKey" | "ctrlKey" | "metaKey" | "altKey">,
): InternalLinkDest {
  if (e.altKey) return "default";
  if (e.shiftKey && !e.ctrlKey && !e.metaKey) return "sidebar";
  if (!e.shiftKey && (e.ctrlKey || e.metaKey)) return "background";
  return "default";
}
