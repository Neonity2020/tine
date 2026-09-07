// Interactive targets that must NOT enter edit on mousedown (mirrors OG's
// target-forbidden-edit?). Their own handlers run on click, so entering edit
// first would swap the DOM out from under them.
export const FORBID_EDIT_SELECTOR = [
  "a",
  "button",
  "input",
  "textarea",
  "select",
  "video",
  "audio",
  "iframe",
  "details",
  "summary",
  ".block-ref",
  // A live block nested inside a macro host (notably {{embed}}) owns its control
  // column. Letting the outer host arm an edit gesture on this mousedown removes
  // the control before the browser can deliver its click.
  ".block-controls",
  ".block-marker",
  ".clock-badge",
  ".date-chip",
  ".hl-prefix",
  ".inline-image-wrap",
  ".media-embed-wrap",
  ".embed-iframe-wrap",
  // Query-macro controls: they run their own action on CLICK (toggle collapse,
  // rename title, navigate to a result page, sort a column).
  ".query-collapse",
  ".query-title-editable",
  ".query-page",
  ".query-crumb",
  ".qt-page",
  ".query-table th",
].join(", ");

export function forbidsEditEntry(e: MouseEvent): boolean {
  const target = e.target as Element | null;
  const host = e.currentTarget as Element;
  // A press that is not in this host's DOM at all still arrives here: Solid
  // delegates `mousedown` and walks a portal's `_$host`, so anything a block
  // renders through `<Portal>` — the query sheet floats over the page in one,
  // because `.query-block`'s `translateZ(0)` would trap a fixed child — is
  // delivered to the block that owns it logically. That press landed on the
  // floating surface, never on this block's text, and entering the editor
  // would unmount the surface before its own click could reach it.
  if (target && !host.contains(target)) return true;
  const hit = target?.closest?.(FORBID_EDIT_SELECTOR);
  return !!hit && host.contains(hit);
}
