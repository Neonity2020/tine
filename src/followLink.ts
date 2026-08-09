// GH #274: open the page under the caret without reaching for the mouse.
//
// OG bindings this reproduces (`frontend/modules/shortcut/config.cljs`):
//   :editor/follow-link          mod+o        -> editor-handler/follow-link-under-cursor!
//   :editor/open-link-in-sidebar mod+shift+o  -> editor-handler/open-link-in-sidebar!
//
// The "which link" rule lives in `editor/nearestLink.ts`, transcribed from OG's
// `thingatpt.cljs`. This module is only the dispatch: what to DO with what was
// found, and how to read the caret out of the live editor.

import { nearestLink, type NearestLink } from "./editor/nearestLink";
import { openPage, openPageAtBlock } from "./router";
import { openPageInSidebar, openBlockInSidebar, pushToast } from "./ui";
import { blockExternalId, doc } from "./store";
import { backend } from "./backend";

/** The focused block editor's text and caret, or null when not editing. */
function caretContext(): { text: string; caret: number } | null {
  if (typeof document === "undefined") return null;
  const active = document.activeElement;
  if (!(active instanceof HTMLTextAreaElement)) return null;
  return { text: active.value, caret: active.selectionStart ?? 0 };
}

/** Resolve a bare `((uuid))` to the page that holds it. Unlike a parsed inline
 *  block ref, the raw text carries no page, and Tine's store is keyed by page.
 *  Best-effort by design: a block outside the loaded working set is simply not
 *  found, which matches OG (`open-link-in-sidebar!` gives up when `db/get-page`
 *  misses). */
function blockOwner(uuid: string): { page: string; pageKind: "journal" | "page" } | null {
  // The store key is not always the durable uuid: a freshly created block keeps
  // its transient `b…` key even after Copy block ref writes an `id::` into raw,
  // so the external identity has to come from `blockExternalId`.
  const key = doc.byId[uuid]
    ? uuid
    : Object.keys(doc.byId).find((id) => blockExternalId(id) === uuid);
  const page = key ? doc.byId[key]?.page : undefined;
  if (!page) return null;
  const owner = doc.pages.find((p) => p.name === page);
  return owner ? { page, pageKind: owner.kind } : null;
}

export interface FollowLinkDeps {
  read?(): { text: string; caret: number } | null;
}

/** `mod+o`. URLs open externally; everything else navigates in place. */
export function followLinkUnderCaret(deps: FollowLinkDeps = {}): boolean {
  const context = (deps.read ?? caretContext)();
  if (!context) return false;
  const link = nearestLink(context.text, context.caret, { includeUrls: true });
  if (!link) return false;
  return dispatch(link, "here");
}

/** `mod+shift+o`. No URL handling — a URL has no sidebar representation, which
 *  is also why OG's sidebar command omits the url pattern. */
export function openLinkUnderCaretInSidebar(deps: FollowLinkDeps = {}): boolean {
  const context = (deps.read ?? caretContext)();
  if (!context) return false;
  const link = nearestLink(context.text, context.caret);
  if (!link) return false;
  return dispatch(link, "sidebar");
}

function dispatch(link: NearestLink, where: "here" | "sidebar"): boolean {
  if (link.kind === "url") {
    void backend().openExternal(link.value).catch(() => {
      pushToast(`Couldn't open ${link.value}`, "error");
    });
    return true;
  }
  if (link.kind === "block") {
    const owner = blockOwner(link.value);
    if (!owner) return false;
    if (where === "sidebar") openBlockInSidebar({ uuid: link.value, ...owner });
    else openPageAtBlock(owner.page, owner.pageKind, link.value);
    return true;
  }
  // Page and tag are the same destination; OG strips the `#` and routes both
  // through the page name.
  if (where === "sidebar") openPageInSidebar(link.value);
  else openPage(link.value);
  return true;
}
