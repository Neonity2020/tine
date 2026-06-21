// Tab-based routing with per-tab navigation history. Each tab holds a back/
// forward stack of routes; the active tab's current route drives the page view.
// Middle-click opens links in a new tab. The whole tab session is persisted, so
// relaunching restores every tab in order, with its zoom state, and the same tab
// focused. `route()`/`openPage()`/`openJournals()` keep their old meaning (acting
// on the active tab) so existing call sites are unchanged.

import { createSignal } from "solid-js";
import { pushRecent, resolveAlias } from "./ui";
import { doc, persistentBlockRef } from "./store";

export type Route =
  | { kind: "journals" }
  | { kind: "page"; name: string; pageKind: "journal" | "page"; block?: string };

export interface Tab {
  id: string;
  // Navigation history (back/forward stack) and the cursor into it.
  history: Route[];
  pos: number;
  pinned: boolean;
}

let counter = 0;
const newId = () => `tab-${counter++}`;

const initial = restore();
const [tabs, setTabs] = createSignal<Tab[]>(initial.tabs);
const [activeId, setActiveId] = createSignal<string>(initial.tabs[initial.active].id);

export { tabs, activeId };

export function activeTab(): Tab {
  return tabs().find((t) => t.id === activeId()) ?? tabs()[0];
}

/** The route a tab is currently showing. */
export function tabRoute(t: Tab): Route {
  return t.history[t.pos];
}

export function route(): Route {
  return tabRoute(activeTab());
}

export function routeTitle(r: Route): string {
  if (r.kind === "journals") return "Journals";
  if (r.name.startsWith("hls__")) return r.name.slice(5);
  return r.name;
}

export function sameRoute(a: Route, b: Route): boolean {
  if (a.kind !== b.kind) return false;
  if (a.kind === "journals") return true;
  const bb = b as typeof a;
  return a.name === bb.name && a.pageKind === bb.pageKind && a.block === bb.block;
}

// Navigate the active tab to a new route, pushing it onto the history stack
// (dropping any forward entries — standard browser behaviour). Re-navigating to
// the current route is a no-op so the back stack doesn't fill with duplicates.
function navigate(r: Route) {
  setTabs(
    tabs().map((t) => {
      if (t.id !== activeId()) return t;
      if (sameRoute(tabRoute(t), r)) return t;
      const history = [...t.history.slice(0, t.pos + 1), r];
      return { ...t, history, pos: history.length - 1 };
    })
  );
  persist();
}

export function openPage(name: string, pageKind: "journal" | "page" = "page") {
  // Resolve aliases so the route + working-set key use the canonical page name.
  if (pageKind === "page") name = resolveAlias(name);
  navigate({ kind: "page", name, pageKind });
  pushRecent(name, pageKind);
}

export function openJournals() {
  navigate({ kind: "journals" });
}

/** Zoom the active tab into a block (or back out, when null). Zoom is part of the
 *  route, so it joins the per-tab back/forward history and a block can be opened
 *  pre-zoomed in its own tab via openPageInNewTab(name, kind, uuid).
 *
 *  Zooming navigates to the block's OWN page (not whichever route you're on), so
 *  it works from the journals feed, a linked-reference, or the command palette —
 *  not only when you're already on that page. Same destination as a middle-click,
 *  just in the current tab. persistentBlockRef pins the uuid (writes id:: once)
 *  so a zoomed tab survives a reload/restart, exactly like the new-tab path. */
export function focusBlock(id: string | null) {
  if (id === null) {
    // Zoom out: stay on the current page, drop the block. (No-op off a page.)
    const r = route();
    if (r.kind === "page") navigate({ kind: "page", name: r.name, pageKind: r.pageKind });
    return;
  }
  if (!doc.byId[id]) return; // block no longer loaded — nothing to zoom into
  const ref = persistentBlockRef(id);
  navigate({ kind: "page", name: ref.page, pageKind: ref.pageKind, block: ref.uuid });
}

/** Open a page and scroll the given block into view (block search results jump
 *  to the specific block, not just the page top). */
export function openPageAtBlock(name: string, pageKind: "journal" | "page", blockId: string) {
  openPage(name, pageKind);
  // Let the page render, then scroll + briefly highlight the target block.
  let tries = 0;
  const tick = () => {
    const el = document.querySelector(`.ls-block[data-block-id="${blockId}"]`);
    if (el) {
      el.scrollIntoView({ block: "center", behavior: "smooth" });
      el.classList.add("block-flash");
      setTimeout(() => el.classList.remove("block-flash"), 1200);
    } else if (tries++ < 20) {
      setTimeout(tick, 50);
    }
  };
  setTimeout(tick, 60);
}

export function openInNewTab(r: Route) {
  // Open in a *background* tab without switching to it — matches a browser's
  // middle-click. Use openPage if you want to navigate there.
  const id = newId();
  setTabs([...tabs(), { id, history: [r], pos: 0, pinned: false }]);
  persist();
}

export function openPageInNewTab(
  name: string,
  pageKind: "journal" | "page" = "page",
  block?: string
) {
  if (pageKind === "page") name = resolveAlias(name);
  openInNewTab({ kind: "page", name, pageKind, block });
  pushRecent(name, pageKind);
}

// ---- back / forward ----

export function canGoBack(): boolean {
  return activeTab().pos > 0;
}

export function canGoForward(): boolean {
  const t = activeTab();
  return t.pos < t.history.length - 1;
}

export function goBack() {
  if (!canGoBack()) return;
  setTabs(tabs().map((t) => (t.id === activeId() ? { ...t, pos: t.pos - 1 } : t)));
  persist();
}

export function goForward() {
  if (!canGoForward()) return;
  setTabs(tabs().map((t) => (t.id === activeId() ? { ...t, pos: t.pos + 1 } : t)));
  persist();
}

export function setActiveTab(id: string) {
  setActiveId(id);
  persist();
}

/** Close the currently-active tab (Ctrl+W). No-op when it's the only tab. */
export function closeActiveTab() {
  closeTab(activeId());
}

export function closeTab(id: string) {
  const list = tabs();
  if (list.length === 1) return; // always keep one tab
  const idx = list.findIndex((t) => t.id === id);
  const next = list.filter((t) => t.id !== id);
  setTabs(next);
  if (activeId() === id) setActiveId(next[Math.max(0, idx - 1)].id);
  persist();
}

export function togglePin(id: string) {
  setTabs(tabs().map((t) => (t.id === id ? { ...t, pinned: !t.pinned } : t)));
  persist();
}

/** Move the dragged tab to the position of the target tab. */
export function reorderTab(dragId: string, targetId: string) {
  if (dragId === targetId) return;
  const list = [...tabs()];
  const from = list.findIndex((t) => t.id === dragId);
  const to = list.findIndex((t) => t.id === targetId);
  if (from < 0 || to < 0) return;
  const [moved] = list.splice(from, 1);
  list.splice(to, 0, moved);
  setTabs(list);
  persist();
}

// ---- persistence (full session) ----
//
// The whole tab session is saved on every change: each tab (not just pinned
// ones), in order, with its full back/forward history and which entry it's on,
// plus which tab is active. Routes already carry the zoomed-in block, so a tab
// zoomed into a bullet comes back zoomed. Saved on launch, restored verbatim.

const KEY = "logseq-claude.session";

interface PersistedSession {
  tabs: { history: Route[]; pos: number; pinned: boolean }[];
  activeIndex: number;
}

function persist() {
  try {
    const list = tabs();
    const session: PersistedSession = {
      tabs: list.map((t) => ({ history: t.history, pos: t.pos, pinned: t.pinned })),
      activeIndex: Math.max(0, list.findIndex((t) => t.id === activeId())),
    };
    localStorage.setItem(KEY, JSON.stringify(session));
  } catch {
    // ignore (e.g. storage disabled)
  }
}

function restore(): { tabs: Tab[]; active: number } {
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) {
      const s = JSON.parse(raw) as PersistedSession;
      const tabs: Tab[] = (s?.tabs ?? [])
        .filter((t) => t && Array.isArray(t.history) && t.history.length > 0)
        .map((t) => ({
          id: newId(),
          history: t.history,
          pos: Math.min(Math.max(0, t.pos | 0), t.history.length - 1),
          pinned: !!t.pinned,
        }));
      if (tabs.length) {
        const active = Math.min(Math.max(0, s.activeIndex | 0), tabs.length - 1);
        return { tabs, active };
      }
    }
  } catch {
    // fall through to a fresh session
  }
  // No (or unreadable) saved session: start on a single journals tab.
  return { tabs: [{ id: newId(), history: [{ kind: "journals" }], pos: 0, pinned: false }], active: 0 };
}
