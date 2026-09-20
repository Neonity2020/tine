# Contract — Linked References filtering

What the Linked References filter **is**, kept true by same-commit updates and by
`src/components/linkedReferencesFilterContract.test.ts`, which asserts the load-bearing
values below against the source. Decision history for the two behaviors that have one
lives in the regression catalog (`UI-LINKED-REFERENCES-FILTER-001`,
`UI-REFFILTER-INCLUDE-OR-273`).

Implementation: `src/components/LinkedReferences.tsx`.

## 1. Two filters, one conjunction

The panel exposes exactly two controls, and a backlink must satisfy **both**:

1. **Text search** — the `.reference-filter-search` input, parsed and matched by
   Rust's shared `search_query::Matcher` (bare terms, quoted phrases, the shared
   search syntax). Production frontend code neither folds nor matches the
   reference corpus.
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

## 4. Pending state — keep the last safe result while native matching runs

The bounded descendant corpus is searched natively on demand, the first time the
funnel is opened or a saved filter is restored and after each debounced text change.
While the first request for a page/root inventory is loading:

- the **unfiltered** list is shown. The frontend never guesses from a partial local
  corpus, because a fallback miss cannot prove a real miss;
- the summary therefore says `Indexing N references… the filter applies when this
  finishes` **instead of** `N of M references`, which would assert a finished filter
  over an unfiltered list.

After one native result has settled, a newer search keeps that exact rendered result
stable while it is pending. The settled result is reusable only with the same captured
page/root inventory. A page or root replacement shows its own unfiltered inventory,
and late replies are discarded rather than applied to a newer page, search, or root
set.

Typed native readiness refusals retry through the shared query-readiness loop while
that immutable request version remains current. Closing the funnel, including with
Escape, is presentation only: a nonempty text query or active facet selection keeps
its native context and filtered list applied, and reopening does not issue a duplicate
request for the same page/root/search identity.

## 5. Load-bearing values

| Value | Where | Why it matters |
|---|---|---|
| **120 ms** | search input debounce, `updateSearch` | Filtering is per-keystroke-debounced, never on submit; there is no Enter path |
| **100** | `OG_REFERENCE_COLLAPSE_THRESHOLD` | OG parity: the section starts collapsed at or above this many references |

## 6. Performance and ownership

Each debounced request sends raw search text and the already-rendered root inventory to
the existing native backlink-context boundary. Rust bounds visible text and facets by
the existing per-root and response ceilings, applies the shared matcher, and returns
facets plus one match decision per root; descendant text does not cross the bridge.
The native helper resolves only the target-connected alias component and exact
requested source page/kind inventory through the read-only projection. Alias-owner,
page-name, journal-day, and source-path reads use existing indexes; only intersecting
physical source paths are hydrated through the parsed-cache path index, with exact
generation and source-revision checks. It does not enumerate all aliases, pages, or
backlink candidates merely to filter text. The same native match decisions drive both
the chip counts and the reference list.

The frontend binds every promise to one immutable page/root/search request and drops
late replies. This is stale-result suppression, not cancellation of native work.
