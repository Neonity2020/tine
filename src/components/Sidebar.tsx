import { For, Show, createEffect, createMemo, createResource, createSignal, onCleanup, type JSX } from "solid-js";
import { openJournals, openPage, openPageInNewTab, openFile, openInNewTab, openPageTarget, openPageTargetInNewTab, route, type PageTarget } from "../router";
import { openRouteInOtherPane } from "../panes";
import {
  addGroup,
  deleteGroup,
  moveFavoriteRow,
  persistFavoritesLayout,
  renameGroup,
  setGroupCollapsed,
} from "../favoritesStore";
import { itemKind, resolveDrop, visibleRows, type FavRow } from "../favoritesLayout";
import { openSwitcher, favorites, favoritesLayout, recentPages, openPageContextMenu, graphMeta, openPageInSidebar, pushToast, resolveAlias, favoritesSectionExpanded, recentSectionExpanded, toggleFavoritesSection, toggleRecentSection, conflictQueue, advanceConflictCursor, openActionContextMenu, type ContextMenuAction } from "../ui";
import { beginRowReorderDrag, rowReorderClickSuppressed, type RowDropTarget } from "./rowReorder";
import { switchGraph, createNewGraph, loadGraphPath, authorizeGraphAccess, reportGraphOpenFailure, type LoadGraphPathOutcome } from "../graph";
import { backend, type KnownGraph } from "../backend";
import { writeClipboardText } from "../clipboard";
import { isMobilePlatform } from "../nativeChrome";
import { allPages as allGraphPages, pageListLabels } from "../pages";
import { EmojiText } from "../render/emoji";
import { internalLinkAuxClick, internalLinkDest, internalLinkMouseDown } from "../linkGesture";
import { NamespaceTree } from "./Namespace";
import type { PageKind } from "../types";
import { registerTransientLayer } from "../transientLayers";

// Cap the rendered "All pages" list. Beyond this, rendering every row (each
// reading route() for its active state) makes both the initial render and every
// navigation O(pages); past a few hundred the list isn't scannable anyway, so we
// show the first N alphabetically and point the rest at search (Ctrl-K).
const ALL_PAGES_CAP = 300;

export function sidebarPageTarget(name: string, kind: PageKind): { name: string; kind: PageKind } {
  return { name: kind === "page" ? resolveAlias(name) : name, kind };
}

export interface SidebarPageOpenDeps {
  normal: (name: string, kind: PageKind) => void;
  sidebar: (name: string, kind: PageKind) => void;
  newTab: (name: string, kind: PageKind) => void;
  context: (x: number, y: number, name: string, kind: PageKind) => void;
}

const sidebarPageOpenDeps: SidebarPageOpenDeps = {
  normal: openPage,
  sidebar: openPageInSidebar,
  newTab: openPageInNewTab,
  context: openPageContextMenu,
};

// Live drop target while a favorites reorder drag is in progress (GH #211).
// Module scope: Sidebar renders once per window.
const [favDropTarget, setFavDropTarget] = createSignal<RowDropTarget | null>(null);
/** Nesting step, in px. Also the width of one pointer "step" to the right when
 *  choosing a drop depth, so the gesture reads the same as the result looks. */
const FAV_INDENT_PX = 16;
/** The depth the live drop would land at — drawn as the indent of the insertion
 *  line, so the user sees WHERE a nested drop goes before releasing. */
const [favDropDepth, setFavDropDepth] = createSignal(0);

/** The depth the pointer is asking for: the dragged row's own depth, plus one
 *  level per indent step it has been dragged to the right (or left). */
function requestedDepth(rows: FavRow[], from: number, dx: number): number {
  const origin = rows[from];
  if (!origin) return 0;
  return origin.depth + Math.round(dx / FAV_INDENT_PX);
}

/** Pointerdown on a favorites row starts a reorder drag — never from an
 *  interactive child. */
function startFavoriteDrag(index: number, event: PointerEvent) {
  if ((event.target as HTMLElement | null)?.closest("button, a, input, [contenteditable=\"true\"]")) return;
  const rows = visibleRows(favoritesLayout());
  beginRowReorderDrag(
    event,
    index,
    "#sidebar-favorites-list .nav-page",
    (target) => {
      if (target) {
        const slot = target.index + (target.before ? 0 : 1);
        setFavDropDepth(
          resolveDropFor(rows, index, slot, requestedDepth(rows, index, target.dx)).depth
        );
      }
      setFavDropTarget(target);
    },
    (from, _to, target) => {
      const slot = target.index + (target.before ? 0 : 1);
      const { parent, index: at } = resolveDropFor(
        rows,
        from,
        slot,
        requestedDepth(rows, from, target.dx)
      );
      const origin = rows[from];
      if (!origin) return;
      void persistFavoritesLayout(moveFavoriteRow(favoritesLayout(), origin.path, parent, at));
    },
    // A drop at the same slot but a different depth re-parents the row, so the
    // flat helper's "nothing moved" shortcut must not swallow it.
    { commitUnchanged: true },
  );
}

/** Resolve a drop, ignoring the rows the dragged subtree itself occupies — a
 *  node may not become its own descendant, and its own rows must not be counted
 *  when working out which parent the slot belongs to. */
function resolveDropFor(
  rows: FavRow[],
  from: number,
  slot: number,
  depth: number,
): { parent: number[]; index: number; depth: number } {
  const origin = rows[from];
  if (!origin) return { parent: [], index: 0, depth: 0 };
  const dragged = new Set<number>();
  rows.forEach((row, i) => {
    if (row.path.length >= origin.path.length && origin.path.every((v, k) => row.path[k] === v)) {
      dragged.add(i);
    }
  });
  const rest = rows.filter((_, i) => !dragged.has(i));
  const restSlot = rows.slice(0, slot).filter((_, i) => !dragged.has(i)).length;
  const resolved = resolveDrop(rest, restSlot, depth);
  const parentRow = resolved.parent.length
    ? rest.find((row) => row.path.length === resolved.parent.length
        && resolved.parent.every((v, k) => row.path[k] === v))
    : null;
  return { ...resolved, depth: parentRow ? parentRow.depth + 1 : 0 };
}

export function openSidebarPageTarget(
  name: string,
  kind: PageKind,
  gesture: "normal" | "sidebar" | "new-tab" | "context",
  point: { x: number; y: number } = { x: 0, y: 0 },
  deps: SidebarPageOpenDeps = sidebarPageOpenDeps,
  onActiveNavigationComplete?: () => void,
) {
  const target = sidebarPageTarget(name, kind);
  if (gesture === "normal") { deps.normal(target.name, target.kind); onActiveNavigationComplete?.(); }
  else if (gesture === "sidebar") deps.sidebar(target.name, target.kind);
  else if (gesture === "new-tab") deps.newTab(target.name, target.kind);
  else deps.context(point.x, point.y, target.name, target.kind);
}

export interface GraphNavigationActions {
  openKnown(path: string, newWindow: boolean): Promise<LoadGraphPathOutcome>;
  openPicked(): Promise<LoadGraphPathOutcome>;
  createNew(): Promise<LoadGraphPathOutcome>;
}

const graphNavigationActions: GraphNavigationActions = {
  openKnown: openKnownGraph,
  openPicked: switchGraph,
  createNew: createNewGraph,
};

export function Sidebar(props: {
  onActiveNavigationComplete?: () => void;
  graphActions?: GraphNavigationActions;
} = {}): JSX.Element {
  // The whole-graph page list is the shared, graph-epoch-keyed resource (see
  // src/pages.ts) — fetched once per graph generation and shared with the
  // namespace tree/macro/hierarchy, not a sidebar-private fetch. Epoch keying
  // still fixes the old "graph still loading at mount" race (it refetches when
  // the graph becomes ready).
  const [showAll, setShowAll] = createSignal(false);
  const [showNs, setShowNs] = createSignal(false);

  // Filter + sort ONCE per page-list change (memoized), not on every render /
  // navigation as the inline `.filter().sort()` in JSX did.
  const allPages = createMemo(() =>
    (allGraphPages() ?? [])
      .filter((p) => p.kind === "page")
      .sort((a, b) => a.name.localeCompare(b.name))
  );
  // Built once per page-list change. Asking each row to disambiguate itself
  // scanned the whole list per row (perf audit F9: 21 ms at 5,000 pages,
  // 39 ms at 20,000, for 300 visible rows).
  const pageLabel = createMemo(() => pageListLabels(allPages()));

  // `path` disambiguates nested pages that share a basename (#21 Phase 2). It's
  // optional: favorites / recent are keyed by name only, so they match on name
  // regardless of which file-path variant is currently routed.
  const isActive = (name: string, path?: string) => {
    const r = route();
    return (
      r.kind === "page" &&
      r.name === name &&
      (path === undefined || !r.path || r.path === path)
    );
  };
  const openEntry = (path: string, name: string) => {
    path ? openFile(path, name, "page") : openPage(name, "page");
    props.onActiveNavigationComplete?.();
  };
  // Sidebar page rows follow the shared internal-link gesture contract (GH #63,
  // GH #283): the shared mousedown guard suppresses native shift-range
  // text-selection AND middle-click autoscroll up front (GH #207 — the old
  // shift-only guard let the middle gesture leak to the browser here).
  const shiftGuard = internalLinkMouseDown;
  // GH #468: the WHOLE row navigates, including the blank space beside a short
  // page name. v0.6.981 briefly gated navigation on the title alone, which is
  // what Logseq does, and it made a page called `test` a ~30px target in a
  // 222px row; #468 reported that within days. Tine diverges from OG here
  // deliberately (Martin, GH #468): a large click target is worth more than the
  // parity, and it costs the drag nothing — rowReorder's 4px threshold and its
  // post-drag click suppression already separate a reorder from a click, so a
  // favourite is moved by moving the pointer, not by aiming at spare width. The
  // pane that genuinely needed a title-only link was the RIGHT sidebar, which
  // is what GH #464 actually reported; see `.rs-item-title` in app.css.
  const openRowMenu = (e: MouseEvent, name: string, kind: PageKind) => {
    e.preventDefault();
    openPageContextMenu(e.clientX, e.clientY, name, kind);
  };

  return (
    <div class="left-sidebar-inner">
      <div class="sidebar-header">
        <div class="app-logo">Tine</div>
        <GraphSwitcher
          onActiveNavigationComplete={props.onActiveNavigationComplete}
          actions={props.graphActions ?? graphNavigationActions}
        />
      </div>

      <div class="nav-contents">
        <div
          class="nav-item"
          classList={{ active: route().kind === "journals" }}
          onMouseDown={internalLinkMouseDown}
          onClick={() => { openJournals(); props.onActiveNavigationComplete?.(); }}
          onAuxClick={(e) => internalLinkAuxClick(e, () => openInNewTab({ kind: "journals" }))}
        >
          <Icon name="journals" />
          <span>Journals</span>
        </div>

        <Show when={favorites().length > 0}>
          <div class="nav-section">
            <button
              type="button"
              class="nav-section-header nav-section-toggle"
              data-sidebar-section="favorites"
              aria-expanded={favoritesSectionExpanded()}
              aria-controls="sidebar-favorites-list"
              onClick={toggleFavoritesSection}
            >
              <span class="nav-toggle-caret" classList={{ open: favoritesSectionExpanded() }}>▸</span>
              FAVORITES
              <span class="nav-section-count">{favorites().length}</span>
            </button>
            <Show when={favoritesSectionExpanded()}>
              <div id="sidebar-favorites-list">
                {/* One flat run of rows, pre-order, indented by depth — not
                    nested markup. The visible index IS `data-row-index`, so the
                    drop target found by elementFromPoint needs no translation,
                    and a label row and a favorite row are draggable on exactly
                    the same terms. */}
                <For each={visibleRows(favoritesLayout())}>
                  {(row, i) => {
                    const indent = () => ({ "padding-left": `${6 + row.depth * FAV_INDENT_PX}px` });
                    const dropping = () => favDropTarget()?.index === i();
                    const rowClasses = () => ({
                      "row-drop-before": dropping() && favDropTarget()!.before,
                      "row-drop-after": dropping() && !favDropTarget()!.before,
                    });
                    const dropIndent = () =>
                      dropping() ? { "--fav-drop-indent": `${6 + favDropDepth() * FAV_INDENT_PX}px` } : {};
                    const toggle = (
                      <Show when={row.node.children.length > 0} fallback={<span class="nav-fav-spacer" />}>
                        <button
                          type="button"
                          class="nav-fav-group-toggle"
                          aria-expanded={!row.node.collapsed}
                          aria-label={row.node.collapsed ? "Expand" : "Collapse"}
                          onClick={() =>
                            void persistFavoritesLayout(
                              setGroupCollapsed(favoritesLayout(), row.path, !row.node.collapsed)
                            )
                          }
                        >
                          <span class="nav-toggle-caret" classList={{ open: !row.node.collapsed }}>▸</span>
                        </button>
                      </Show>
                    );
                    if (row.node.target === null) {
                      return (
                        <div
                          class="nav-page nav-fav-group"
                          data-row-index={i()}
                          classList={rowClasses()}
                          style={{ ...indent(), ...dropIndent() }}
                          onPointerDown={(e) => startFavoriteDrag(i(), e)}
                        >
                          {toggle}
                          {/* Sized to its text, not stretched: a label row is
                              draggable like any other, and an input filling the
                              row would leave nowhere to grab it. */}
                          <input
                            class="nav-fav-group-name"
                            size={Math.max(row.node.raw.length, 4)}
                            value={row.node.raw}
                            aria-label={`Rename group ${row.node.raw}`}
                            onChange={(event) =>
                              void persistFavoritesLayout(
                                renameGroup(favoritesLayout(), row.path, event.currentTarget.value)
                              )
                            }
                          />
                          <button
                            type="button"
                            class="nav-fav-group-delete"
                            /* Deleting a group keeps its favorites — they move up
                               to where it stood — so this needs no confirmation,
                               which is just as well: WebKitGTK's confirm() is a
                               no-op (#confirm). */
                            title="Delete this group (what it holds moves up a level)"
                            aria-label={`Delete group ${row.node.raw}`}
                            onClick={() =>
                              void persistFavoritesLayout(deleteGroup(favoritesLayout(), row.path))
                            }
                          >
                            ×
                          </button>
                        </div>
                      );
                    }
                    const name = row.node.target;
                    const target = () => sidebarPageTarget(name, itemKind(name));
                    return (
                      <div
                        class="nav-page"
                        data-row-index={i()}
                        classList={{ active: isActive(target().name), ...rowClasses() }}
                        style={{ ...indent(), ...dropIndent() }}
                        onPointerDown={(e) => startFavoriteDrag(i(), e)}
                        onMouseDown={shiftGuard}
                        onClick={(e) => {
                          if (rowReorderClickSuppressed()) return;
                          const dest = internalLinkDest(e);
                          if (dest === "pane") {
                            // Alt+click (GH #438): the same other-pane route the
                            // page refs and search results use; alias-resolved
                            // like every other favorite-row destination.
                            const t = target();
                            openRouteInOtherPane({ kind: "page", name: t.name, pageKind: t.kind });
                          } else openSidebarPageTarget(name, itemKind(name), dest === "sidebar" ? "sidebar" : dest === "background" ? "new-tab" : "normal", { x: 0, y: 0 }, sidebarPageOpenDeps, props.onActiveNavigationComplete);
                        }}
                        onAuxClick={(e) =>
                          internalLinkAuxClick(e, () => openSidebarPageTarget(name, itemKind(name), "new-tab"))
                        }
                        onContextMenu={(e) => {
                          e.preventDefault();
                          openSidebarPageTarget(name, itemKind(name), "context", { x: e.clientX, y: e.clientY });
                        }}
                      >
                        {toggle}
                        {/* ⭐ + name via EmojiText: WebKitGTK's Skia COLRv1 path
                            crashes painting a raw color-emoji glyph on hardened
                            libstdc++ (#29); Twemoji <img> never touches the font.
                            `.nav-page-label` is the element that ELLIPSISES a long
                            name (the row is overflow:hidden and a flex item will
                            not shrink below its content without min-width:0). It
                            is not the link — the row is (GH #468). */}
                        <span class="nav-page-label"><EmojiText text={`⭐ ${name}`} /></span>
                      </div>
                    );
                  }}
                </For>
                <button
                  type="button"
                  class="nav-fav-add-group"
                  onClick={() => void persistFavoritesLayout(addGroup(favoritesLayout()))}
                >
                  + New group
                </button>
              </div>
            </Show>
          </div>
        </Show>

        <Show when={recentPages().length > 0}>
          <div class="nav-section">
            <button
              type="button"
              class="nav-section-header nav-section-toggle"
              data-sidebar-section="recent"
              aria-expanded={recentSectionExpanded()}
              aria-controls="sidebar-recent-list"
              onClick={toggleRecentSection}
            >
              <span class="nav-toggle-caret" classList={{ open: recentSectionExpanded() }}>▸</span>
              RECENT
              <span class="nav-section-count">{recentPages().length}</span>
            </button>
            <Show when={recentSectionExpanded()}>
              <div id="sidebar-recent-list">
                <For each={recentPages()}>
                  {(r) => {
                    const target = (): PageTarget => ({ name: r.name, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) });
                    return (
                      <div
                        class="nav-page"
                        classList={{ active: isActive(target().name, target().path) }}
                        onMouseDown={shiftGuard}
                        onClick={(e) => {
                          const dest = internalLinkDest(e);
                          if (dest === "sidebar") openPageInSidebar(target());
                          else if (dest === "background") openPageTargetInNewTab(target());
                          else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target() });
                          else { openPageTarget(target()); props.onActiveNavigationComplete?.(); }
                        }}
                        onAuxClick={(e) => internalLinkAuxClick(e, () => openPageTargetInNewTab(target()))}
                        onContextMenu={(e) => {
                          e.preventDefault();
                          openPageContextMenu(e.clientX, e.clientY, target());
                        }}
                      >
                        <span class="nav-page-label"><EmojiText text={r.name.startsWith("hls__") ? r.name.slice(5) : r.name} /></span>
                      </div>
                    );
                  }}
                </For>
              </div>
            </Show>
          </div>
        </Show>

        <div class="nav-section">
          <div
            class="nav-section-header nav-section-toggle"
            onClick={() => setShowAll(!showAll())}
          >
            <span class="nav-toggle-caret" classList={{ open: showAll() }}>▸</span>
            ALL PAGES
            <Show when={allGraphPages()}>
              <span class="nav-section-count">{allPages().length}</span>
            </Show>
          </div>
          <Show when={showAll() && allGraphPages()}>
            <For each={allPages().slice(0, ALL_PAGES_CAP)}>
              {(p) => (
                <div
                  class="nav-page"
                  classList={{ active: isActive(p.name, p.path) }}
                  onMouseDown={shiftGuard}
                  onClick={(e) => {
                    const dest = internalLinkDest(e);
                    if (dest === "sidebar") openPageInSidebar({ name: p.name, pageKind: "page", path: p.path });
                    else if (dest === "background") p.path
                      ? openInNewTab({ kind: "page", name: p.name, pageKind: "page", path: p.path })
                      : openPageInNewTab(p.name, "page");
                    else if (dest === "pane") openRouteInOtherPane({ kind: "page", name: p.name, pageKind: "page", ...(p.path ? { path: p.path } : {}) });
                    else openEntry(p.path, p.name);
                  }}
                  onAuxClick={(e) =>
                    internalLinkAuxClick(e, () =>
                      p.path
                        ? openInNewTab({ kind: "page", name: p.name, pageKind: "page", path: p.path })
                        : openPageInNewTab(p.name, "page"))
                  }
                  onContextMenu={(e) => {
                    e.preventDefault();
                    openPageContextMenu(e.clientX, e.clientY, { name: p.name, pageKind: "page", path: p.path });
                  }}
                >
                  <span class="nav-page-label"><EmojiText text={pageLabel()(p)} /></span>
                </div>
              )}
            </For>
            <Show when={allPages().length > ALL_PAGES_CAP}>
              <div class="nav-page nav-page-more" onClick={() => openSwitcher()}>
                +{allPages().length - ALL_PAGES_CAP} more — search to open…
              </div>
            </Show>
          </Show>
        </div>

        <div class="nav-section">
          <div
            class="nav-section-header nav-section-toggle"
            onClick={() => setShowNs(!showNs())}
          >
            <span class="nav-toggle-caret" classList={{ open: showNs() }}>▸</span>
            NAMESPACES
          </div>
          <Show when={showNs()}>
            <NamespaceTree onPageContextMenu={openRowMenu} onActiveNavigationComplete={props.onActiveNavigationComplete} />
          </Show>
        </div>
      </div>

      <div class="sidebar-footer">
        <ConflictQueueBadge />
        <button class="new-page-btn" onClick={() => openSwitcher()}>+ New page</button>
      </div>
    </div>
  );
}


// Concord L3: the calm badge. A conflict is a persistent object, not an
// interruption — it waits here, never opens a modal, never blocks anything, and
// survives restarts because the queue is derived from disk. Clicking walks to
// the next conflicted page, where the resolution actually happens.
export function ConflictQueueBadge(): JSX.Element {
  const count = () => conflictQueue().length;
  return (
    <Show when={count()}>
      <button
        class="conflict-queue-badge"
        title="Review the pages that need a decision"
        onClick={() => {
          const next = advanceConflictCursor();
          // Address the exact FILE: a conflict object is about one path, and a
          // duplicate-day journal would otherwise resolve to the canonical file.
          if (next) openPageTarget({ name: next.page_name, pageKind: next.kind, path: next.page_path });
        }}
      >
        <span class="conflict-queue-dot" aria-hidden="true" />
        {count()} conflict{count() === 1 ? "" : "s"}
      </button>
    </Show>
  );
}

// Display name for the active graph = basename of its root folder (OG shows the
// same). Falls back to "No graph" when none is loaded (e.g. mock/first run).
function graphDisplayName(): string {
  const root = graphMeta()?.root;
  if (!root) return "No graph";
  const base = root.replace(/[/\\]+$/, "").split(/[/\\]/).pop();
  return base || root;
}

export interface KnownGraphOpenDeps {
  switchInPlace(path: string): Promise<LoadGraphPathOutcome>;
  openNewWindow(path: string): Promise<LoadGraphPathOutcome>;
}

export interface GraphRowMenuDeps {
  openKnown(path: string, newWindow: boolean): Promise<LoadGraphPathOutcome>;
  reveal(path: string): Promise<void>;
  copyPath(text: string): Promise<void>;
  forget(path: string): Promise<void>;
  /** Peer windows and a file manager both exist only on desktop. */
  desktop: boolean;
  isCurrent: boolean;
}

/** Per-row actions for the graph switcher's right-click menu.
 *
 *  Built as a plain array so the menu's contents are testable without a DOM:
 *  which items a row offers depends on the platform and on whether the row is
 *  the graph this window already has open, and those are exactly the parts that
 *  regress silently. Items that cannot act are kept visible and disabled with
 *  the reason in the label (the `SheetTable` field-header menu does the same) —
 *  hiding them would make the menu change shape row to row, and discoverability
 *  is the entire point of this menu.
 */
export function graphRowMenuActions(
  graph: KnownGraph,
  deps: GraphRowMenuDeps,
): ContextMenuAction[] {
  const items: ContextMenuAction[] = [];
  // Same self-retrying shape as the row's left click: a failed open puts a
  // sticky Retry toast up that re-runs this exact target, not the last one.
  const open = (newWindow: boolean) => {
    const attempt = () => void deps.openKnown(graph.path, newWindow)
      .catch((error) => reportGraphOpenFailure(error, attempt));
    attempt();
  };
  if (deps.desktop) {
    items.push({
      label: deps.isCurrent
        ? "Open in a new window (already open here)"
        : "Open in a new window",
      disabled: deps.isCurrent,
      run: () => open(true),
    });
  }
  items.push({
    label: deps.isCurrent ? "Open here (current graph)" : "Open here",
    disabled: deps.isCurrent,
    run: () => open(false),
  });
  if (deps.desktop) {
    items.push({
      label: "Show in folder",
      run: () => void deps.reveal(graph.path).catch((error) =>
        pushToast(`Couldn't show the graph folder. (${String(error)})`, "error")),
    });
  }
  items.push({
    label: "Copy path",
    run: () => void deps.copyPath(graph.path).catch((error) =>
      pushToast(`Couldn't copy the path. (${String(error)})`, "error")),
  });
  items.push({
    label: "Remove from this list",
    danger: true,
    run: () => void deps.forget(graph.path).catch((error) =>
      pushToast(`Couldn't remove graph. (${String(error)})`, "error")),
  });
  return items;
}

export function openKnownGraph(
  path: string,
  newWindow: boolean,
  deps: KnownGraphOpenDeps = {
    switchInPlace: loadGraphPath,
    openNewWindow: async (target) => {
      if (!(await authorizeGraphAccess(target))) return { kind: "aborted" };
      // A graph opened in a peer window is never an in-place navigation even
      // when the backend had to construct that window, so it must keep the
      // mobile drawer open.
      await backend().openGraphWindow(target);
      return { kind: "focused_existing" };
    },
  }
): Promise<LoadGraphPathOutcome> {
  return newWindow ? deps.openNewWindow(path) : deps.switchInPlace(path);
}

// The current-graph control in the sidebar header. OG puts a graph-name dropdown
// top-left (database icon → switch/new/all-graphs/re-index). Tine lists its known
// graphs here alongside open/create actions; Shift-click opens a peer window.
export function GraphSwitcher(props: {
  onActiveNavigationComplete?: () => void;
  actions: GraphNavigationActions;
}): JSX.Element {
  const [open, setOpen] = createSignal(false);
  const [knownGraphs, { refetch }] = createResource(() => backend().listKnownGraphs());
  const close = () => setOpen(false);

  createEffect(() => {
    if (open()) void refetch();
  });

  createEffect(() => {
    if (!open()) return;
    const unregister = registerTransientLayer({
      id: "graph-switch-menu",
      root: () => document.querySelector(".graph-switch-menu"),
      dismiss: () => { close(); return true; },
    });
    onCleanup(unregister);
  });

  return (
    <div class="graph-switch">
      <button
        class="graph-switch-btn"
        title={graphMeta()?.root ?? undefined}
        onClick={() => setOpen(!open())}
      >
        <svg viewBox="0 0 24 24" class="graph-switch-icon">
          <ellipse cx="12" cy="5" rx="7" ry="3" fill="none" stroke="currentColor" stroke-width="1.6" />
          <path d="M5 5v7c0 1.66 3.13 3 7 3s7-1.34 7-3V5" fill="none" stroke="currentColor" stroke-width="1.6" />
          <path d="M5 12v7c0 1.66 3.13 3 7 3s7-1.34 7-3v-7" fill="none" stroke="currentColor" stroke-width="1.6" />
        </svg>
        <span class="graph-switch-name">{graphDisplayName()}</span>
        <span class="graph-switch-caret">▾</span>
      </button>
      <Show when={open()}>
        <div
          class="graph-switch-backdrop"
          onClick={close}
          onContextMenu={(e) => {
            e.preventDefault();
            close();
          }}
        />
        <div class="ctx-menu graph-switch-menu">
          <For each={knownGraphs() ?? []}>
            {(graph) => (
              <div
                class="ctx-item graph-switch-row"
                classList={{ active: graph.path === graphMeta()?.root }}
                title={graph.path}
                onContextMenu={(event) => {
                  // Local suppression: nothing disables WebKit's own menu
                  // globally, so without this the native menu appears next to
                  // ours. stopPropagation keeps the switcher's backdrop handler
                  // (which closes the switcher) from firing underneath.
                  event.preventDefault();
                  event.stopPropagation();
                  openActionContextMenu(event.clientX, event.clientY, graphRowMenuActions(graph, {
                    openKnown: props.actions.openKnown,
                    reveal: (path) => backend().revealKnownGraph(path),
                    copyPath: writeClipboardText,
                    forget: (path) => backend().forgetKnownGraph(path).then(() => { void refetch(); }),
                    desktop: !isMobilePlatform,
                    isCurrent: graph.path === graphMeta()?.root,
                  }));
                }}
                onClick={(event) => {
                  const newWindow = event.shiftKey;
                  close();
                  const attempt = () => void props.actions.openKnown(graph.path, newWindow)
                    .then((outcome) => {
                      if (!newWindow && (outcome.kind === "loaded" || outcome.kind === "already_current")) {
                        props.onActiveNavigationComplete?.();
                      }
                    })
                    .catch((error) => reportGraphOpenFailure(error, attempt));
                  attempt();
                }}
              >
                <span class="graph-switch-row-name">{graph.name}</span>
                <button
                  class="graph-switch-remove"
                  title={`Remove ${graph.name} from this list`}
                  aria-label={`Remove ${graph.name} from this list`}
                  onClick={(event) => {
                    event.stopPropagation();
                    void backend()
                      .forgetKnownGraph(graph.path)
                      .then(() => refetch())
                      .catch((error) => pushToast(`Couldn't remove graph. (${String(error)})`, "error"));
                  }}
                >
                  ×
                </button>
              </div>
            )}
          </For>
          <Show when={(knownGraphs() ?? []).length > 0}>
            <div class="ctx-separator" />
          </Show>
          <div
            class="ctx-item"
            onClick={() => {
              close();
              void props.actions.openPicked()
                .then((outcome) => {
                  if (outcome.kind === "loaded" || outcome.kind === "already_current") props.onActiveNavigationComplete?.();
                })
                // Target-specific failures are handled by switchGraph after
                // the picker returns a path, so its Retry preserves that path.
                // A rejection here is a picker failure and has no target to retry.
                .catch((error) => pushToast(`Couldn't open the graph picker. (${String(error)})`, "error"));
            }}
          >
            Open graph…
          </div>
          <div
            class="ctx-item"
            onClick={() => {
              close();
              void props.actions.createNew()
                .then((outcome) => {
                  if (outcome.kind === "loaded" || outcome.kind === "already_current") props.onActiveNavigationComplete?.();
                })
                .catch((error) => pushToast(`Couldn't create the graph. (${String(error)})`, "error"));
            }}
          >
            New graph…
          </div>
        </div>
      </Show>
    </div>
  );
}

function Icon(props: { name: string }): JSX.Element {
  // Minimal inline icons; the real Tabler set comes later.
  if (props.name === "journals") {
    return (
      <svg viewBox="0 0 24 24" class="nav-icon">
        <rect x="4" y="5" width="16" height="16" rx="2" fill="none" stroke="currentColor" stroke-width="1.6" />
        <line x1="4" y1="9" x2="20" y2="9" stroke="currentColor" stroke-width="1.6" />
        <line x1="9" y1="3" x2="9" y2="7" stroke="currentColor" stroke-width="1.6" />
        <line x1="15" y1="3" x2="15" y2="7" stroke="currentColor" stroke-width="1.6" />
      </svg>
    );
  }
  return <span />;
}
