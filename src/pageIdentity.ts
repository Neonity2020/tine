/** Page-identity fold — THE frontend answer to "are these two strings the same
 *  page?", in a dependency-free leaf module so every consumer (ui.ts,
 *  favoritesLayout.ts, reference components) can import it without cycles.
 *
 *  Mirror of core `refs::page_key`: trim, Unicode lowercase, remove one boundary
 *  slash at each side, then NFC. Lowercasing is contextual (`ΟΣ` → `ος`),
 *  matching Rust `str::to_lowercase` — never a char-wise fold.
 *
 *  DUP-2/DUP-8 (2026-08-25 duplication audit): this used to live in ui.ts while
 *  four weaker private folds (`trim().toLowerCase()` variants) keyed favorites,
 *  reference-filter chips, and reference-group merging, splitting NFC/NFD and
 *  slash-boundary spellings of one page. Do not add another fold — import this.
 */
export function pageIdentityKey(name: string): string {
  const lowered = name.trim().toLowerCase();
  const withoutLeading = lowered.startsWith("/") ? lowered.slice(1) : lowered;
  const withoutBoundaries = withoutLeading.endsWith("/")
    ? withoutLeading.slice(0, -1)
    : withoutLeading;
  return withoutBoundaries.normalize("NFC");
}
