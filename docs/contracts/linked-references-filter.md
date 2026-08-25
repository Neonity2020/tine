# Contract — Linked References filtering

What the Linked References filter **is**, kept true by same-commit updates and by
`src/components/linkedReferencesFilterContract.test.ts`, which asserts the load-bearing
values below against the source. Decision history for the two behaviors that have one
lives in the regression catalog (`UI-LINKED-REFERENCES-FILTER-001`,
`UI-REFFILTER-INCLUDE-OR-273`).

Implementation: `src/components/LinkedReferences.tsx`.

## 1. Two filters, one conjunction

The panel exposes exactly two controls, and a backlink must satisfy **both**:

1. **Text search** — the `.reference-filter-search` input, parsed by
   `parseSearchQuery` (bare terms, quoted phrases, the shared search syntax).
2. **Reference facets** — the `.ref-filter-chip` buttons, each cycling
   off → include → exclude → off.

Include chips **OR** with each other: a backlink survives when **any** included
facet is present, and zero include chips leaves the facet side unconstrained.
Exclude chips are **cumulative**: any excluded facet present removes the backlink.

## 2. Scope — the rule that must not drift

**Matching is evaluated at backlink-root scope, over the root's entire subtree.**

A backlink "root" is one top-level entry in a page group. Its searchable text is the
concatenation of its own text and all of its descendants', and its facet set is the
union of the facets found anywhere in that subtree.

Consequences, all deliberate:

- A match that occurs only in a **descendant** keeps the **root** visible, together
  with the rest of its subtree — including descendants that do not match. This
  preserves an editable root and its context, which is the point of the panel.
- Filtering **never** rewrites the tree it shows. It removes whole roots, and removes a
  page group once it has lost every root.
- Counts (`N of M references`) count **roots**, not matches and not descendants.

This is the single most complained-about aspect of the equivalent feature in both
Logseq and Obsidian, in both cases because the scope was never written down. Changing
it is a contract change, not an implementation detail.

**Not specified here:** whether a facet chip for a page also matches that page's
aliases. There is no test either way; treat it as unknown rather than as either
guarantee.

## 3. The chips follow the text, not the chips

The facet chip list is the set the user picks **from**, so:

- Chips and their counts are computed over the **text-matched** backlinks — typing
  narrows the chip list, matching Logseq's "Search in linked pages".
- Chips are **not** narrowed by the chip selections themselves; selecting one must
  never remove the controls needed to undo it.
- An **active** include/exclude chip whose last backlink the text query filtered away
  is still listed, at count `0`, so a filter can never become unreachable.
- `coRefs()` must not depend on `filters()`. Folding the active-chip rule into it would
  make every chip click re-create every chip node mid-cycle, which breaks the
  off → include → exclude cycle on a held element reference.

## 4. Pending state — the list is unfiltered while the index loads

The descendant corpus is fetched on demand, the first time the funnel is opened or a
saved filter is restored. While that resource is loading:

- the **unfiltered** list is shown. The local fallback corpus is a *subset* of the
  native one, so a fallback miss cannot prove a real miss, and dropping a root on one
  could hide a genuine match;
- the summary therefore says `Indexing N references… the filter applies when this
  finishes` **instead of** `N of M references`, which would assert a finished filter
  over an unfiltered list.

## 5. Load-bearing values

| Value | Where | Why it matters |
|---|---|---|
| **120 ms** | search input debounce, `updateSearch` | Filtering is per-keystroke-debounced, never on submit; there is no Enter path |
| **100** | `OG_REFERENCE_COLLAPSE_THRESHOLD` | OG parity: the section starts collapsed at or above this many references |

## 6. Performance

The normalized searchable corpus is built **once per entry** and cached behind memos;
it is not rebuilt per keystroke or per search evaluation. The text pass runs once and
both the chip list and the reference list read its result. A test spies on
`toLowerCase` to keep this honest.
