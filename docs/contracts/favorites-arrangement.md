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
- **The arrangement page** owns *groups, order and collapse*. It is an ordinary
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

- [[Alpha]]            <- ungrouped favorite
- Work                 <- group header: PLAIN TEXT, never a page
	- [[Beta]]
	- [[Gamma]]
```

- A favorite is a bullet that is **nothing but one `[[link]]`**. `see [[Alpha]]`
  is not a favorite. Links are used so **renames follow for free** — the
  staleness class GH #79 had to fix by hand for the flat list.
- A group header is plain text. Making it a page would couple sidebar structure
  to page lifecycle (rename? delete? does clicking it open the page or collapse
  the group?) for no benefit.
- **Groups are one level.** The stored data may nest deeper without corrupting;
  the sidebar renders one level.
- **Anything unrecognized is preserved verbatim.** This is a real page and the
  user may type in it.

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
| Favorite added in Tine | Appended to the ungrouped section |
| Favorite removed anywhere | Removed from its group. The arrangement **never resurrects** an unfavorited page |
| Re-favorited later | Lands at the **end of ungrouped**, not silently back in its old group |
| Group deleted | Its favorites move to the ungrouped section. **Deleting a group never unfavorites anything** (the Capacities contract; Obsidian's bookmark folders lose the reference) |
| Group renamed to an existing name | Disambiguated (`Work 2`), never merged |
| Page renamed | Followed automatically, because members are `[[links]]` |
| Arrangement page missing or unreadable | Favorites still work, flat, from `config.edn` |

## 6. Lazy materialization

**No page is created until the arrangement carries something `config.edn` cannot
express — a group.** A user who never groups anything never grows a page in
their graph they did not ask for, and their favorites behave exactly as they did
before this existed.

## 7. Failure ordering

The page is written **first** (the richer artifact); if the projection then
fails, `config.edn` still holds the previous membership and the next reconcile
folds the page back into agreement. If the page write fails, membership is
**still** projected — losing an arrangement is survivable, losing the favorites
is not. Both directions are tested.

## 8. Known gap

`config.edn :favorites` is read at graph open and **not re-read while Tine
runs**, so a favorite added or removed in Logseq during a live Tine session can
be overwritten by Tine's next projection. This predates the arrangement (the
flat list had the same hole) and closing it needs a config-file watcher, which
does not exist yet.
