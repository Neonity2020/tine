# Contract — Favorites arrangement

What the Favorites arrangement **is**. Kept true by same-commit updates and by
`src/favoritesLayout.test.ts`, `src/favoritesStore.test.ts`,
`src/components/Sidebar.favGroups.test.tsx`, and the Rust tests named below.

## 1. Two stores, one direction each

```
arrangement page  --project-->  config.edn :favorites   (membership, flat, ordered)
config.edn        --reconcile-> arrangement            (external changes)
```

- **`config.edn :favorites`** stays the shared, Logseq-readable answer to *which*
  pages are favorited, in display order. Logseq keeps working unchanged.
- **The arrangement page** owns *nesting, order and collapse*. It is an ordinary
  graph page, so it merges deterministically through the oplog on Managed
  Storage and reaches the Concord conflict UI on Direct Files — for free, and
  with **no new write path**: it saves through the audited `save_page`.

A blob (JSON or an EDN key) was rejected for exactly this reason: it has no
merge semantics, so two devices editing favorites is last-writer-wins, which is
the documented cause of Obsidian's total bookmark loss across Sync.

## 2. Identity

`config.edn :tine/favorites-page "Name"` names the page. Logseq ignores unknown
keys, so this is invisible to it.

Identity is **not** a reserved page name — that would silently change behaviour
on a user's own page called "Favorites" — and **not** a page property, which
would need a whole-graph property scan on the reference path where config is
already loaded. The page still carries `tine/favorites:: true` as a human-visible
label, but nothing keys off it.

`Graph::set_favorites_page` writes the key with the same surgical, key-local
editor as `set_favorites`: unknown keys, comments and formatting elsewhere in
the file survive. Tested by
`config::tests::favorites_page_key_round_trips_and_preserves_the_rest_of_the_file`.

## 3. The page shape

```
tine/favorites:: true

- [[Alpha]]            <- a favorite
- Work                 <- a label: PLAIN TEXT, never a page
	- [[Beta]]
	- Active             <- labels nest
		- [[Gamma]]
- [[Delta]]            <- favorites nest under favorites too
	- [[Epsilon]]
```

- **One node type, at any depth.** A bullet is a *favorite* when it is
  **nothing but one `[[link]]`**, and a *label* otherwise; both nest and both
  carry their children. There is no "group containing items" structure, because
  the page does not have one — which is why arbitrary depth costs nothing here
  and why nothing has to be flattened on read or invented on write.
- `see [[Alpha]]` is not a favorite. Links are used so **renames follow for
  free** — the staleness class GH #79 had to fix by hand for the flat list.
- A label is plain text. Making it a page would couple sidebar structure to page
  lifecycle (rename? delete? does clicking it open the page or collapse it?) for
  no benefit.
- **Every bullet round-trips verbatim, at every depth.** This is a real page and
  the user may type in it. The one deletion is a bullet whose text is empty —
  it can be neither rendered nor named — and even then its children are kept.
- `layoutMembers` walks the tree **pre-order**, and that is the order projected
  into `config.edn :favorites`, so Logseq sees a sensible flattening.

## 3a. Drop placement

The sidebar draws one flat run of visible rows (pre-order, skipping what a
collapsed row hides), so `data-row-index` **is** the visible index and a drop
target found by `elementFromPoint` needs no translation.

Depth is taken from how far the pointer has travelled **from where the drag
started**, not from the row's left edge — otherwise where in a row the user
happened to grab would decide the nesting. A straight vertical drag therefore
can never nest, which is what keeps depth opt-in.

`depthRange` applies the ordinary outliner rule: no deeper than one level under
the row above, no shallower than the row below. A depth the tree cannot honour
is **clamped**, never satisfied by inventing intermediate parents. A node may
not become its own descendant, and its own rows are excluded before the slot is
resolved.

## 4. Reference exclusion

The arrangement page is **never a reference source** — its links are a sidebar
arrangement, not a mention. This is enforced by `refs::ReferenceSourceExclusions`,
the single predicate shared by both reference engines; Direct Files and managed
storage previously open-coded the same comparison at eight sites, which is how
they drift. **A new exclusion belongs in that type, not at a call site.**

An *unmarked* page named "Favorites" remains an ordinary reference source.
Tested by `query::tests::favorites_layout_page_is_never_a_reference_source`.

## 5. Lifecycle rules

| Event | Rule |
|---|---|
| Graph open | `config.edn :favorites` is the authority on membership; the arrangement is reconciled against it |
| Favorite added in Tine | Appended at the top level |
| Favorite removed anywhere | Removed wherever it sat, and **its children take its place** — unstarring a parent must not silently unstar what was nested under it |
| Re-favorited later | Lands at the **end**, not silently back in its old place |
| Label deleted | What it held moves up one level. **Deleting a label never unfavorites anything** (the Capacities contract; Obsidian's bookmark folders lose the reference) |
| Label renamed to an existing name | Disambiguated (`Work 2`), never merged. Re-committing a label's own unchanged name is a no-op |
| Row dropped into a collapsed row | That row expands, so the move stays visible |
| Page renamed | Followed automatically, because members are `[[links]]` |
| Arrangement page missing or unreadable | Favorites still work, flat, from `config.edn` |

## 6. Lazy materialization

**No page is created until the arrangement carries something `config.edn` cannot
express — a label, or a row nested under another.** A user who never groups anything never grows a page in
their graph they did not ask for, and their favorites behave exactly as they did
before this existed.

## 7. Failure ordering

The page is written **first** (the richer artifact); if the projection then
fails, `config.edn` still holds the previous membership and the next reconcile
folds the page back into agreement. If the page write fails, membership is
**still** projected — losing an arrangement is survivable, losing the favorites
is not. Both directions are tested.

## 8. Live re-reads

Both stores are re-read while Tine runs; neither is a graph-open snapshot any
more.

- **`config.edn`** — the watcher queues `logseq/config.edn` separately from
  graph text (it is not graph text, so the page-path filter discards it) and
  refreshes the graph when the bytes differ from those the running `Graph` was
  opened with. See `docs/contracts/config-live-reload.md`.
- **The arrangement page** — an edit in Tine's own editor, or one arriving from
  outside, re-reads the page and updates the sidebar. A page edit **is a
  membership statement**: deleting a `[[link]]` bullet unfavorites that page and
  adding one favorites it, so the reloaded tree is projected into
  `config.edn :favorites` exactly as a drag would be. The page is never written
  back in response — that would echo the user's own keystrokes — and the
  revision the store last saved is ignored, so Tine's own write is not adopted
  as an external one.

This also gives favorites reordering a keyboard route: the arrangement is an
ordinary page, so it can be edited with the keyboard like any other. Pointer
drag remains the only way to reorder *in the sidebar* (WCAG 2.2 SC 2.5.7 is
still open, deferred with GH #211).
