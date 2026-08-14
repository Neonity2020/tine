# 0054. Managed activation imports one lazy genesis

- **Status:** Accepted — implementation gated
- **Date:** 2026-08-14
- **Supersedes:** [0051](0051-single-user-multipart-bootstrap-authority.md)
- **Amends:** [0049](0049-oplog-first-sparse-storage.md)

## Context

The first managed-storage activation currently expands an existing graph into
interactive create/edit operations, eagerly constructs mutable documents, and
then independently rebuilds or revalidates substantially the same graph in the
engine, authenticated reference indexes, SQLite, the exact-source shadow,
backup, and promotion. A 13,001-page Windows trace spent 244 seconds building
reference-posting Patricia trees even though those same derived reference facts
are subsequently stored in SQLite. A 1,000-page Linux instruction profile found
about eight parser invocations per page and repeated full validation of the same
newly written object set.

Before activation commits, the Markdown/Org tree is the sole authority. The
private construction products are therefore disposable. Treating every
same-process boundary as though it followed a crash or hostile private-state
replacement adds work without protecting an in-scope failure.

## Decision

Activation consumes one bounded canonical page stream. Each page is parsed once
and yields exact-source evidence, deterministic page/block identity, the minimum
identity facts needed by future operations, and disposable query/reference
facts. The same records feed lazy genesis and SQLite, while their sealed exact
source pack is adopted directly as both initial projection evidence and the
rollback backup. Activation does not copy and independently prove those bytes
under three different artifact vocabularies.

The durable initial state is a versioned lazy-genesis pack, not simulated edit
history. An untouched page resolves from its immutable genesis capsule. Its
first local or remote mutation deterministically materializes the existing
ordinary managed document/checkpoint representation; the ordinary receipt then
supersedes that page's baseline. Later changes continue through the existing
oplog and sync protocol.

Durable genesis retains semantic content, stable page/block identity, causal
base, canonical page/path ownership, persisted block-UUID ownership, and other
identity constraints demonstrably needed to accept later operations. Their
truth belongs to genesis plus ordinary accepted operations; their lookup
indexes are not a second durable authority. One complete frontier-stamped
SQLite current-state projection owns the unique page-name/path, page/block and
persisted block-UUID lookup indexes alongside search, tasks, properties,
aliases, reference postings, backlinks, unlinked candidates and reference
counts. If SQLite is missing, stale or corrupt, Tine rebuilds it before
accepting another mutation. The accepted frontier and ordinary accepted-event
evidence bind semantic history, not SQLite bytes and not authenticated
reference or identity Patricia roots.

The uninterrupted constructor passes move-only receipts between publication,
SQLite, backup/shadow, and promotion. A receipt proves only what this process
just constructed and is never serialized. Cold open, crash recovery, and an
explicit integrity audit retain independent validators. Activation performs
one final byte-exact live-tree scan without reparsing, accounts for watcher
events crossing that scan, and publishes one small authority marker last.

Fresh activation writes only the new format. Before 0.7, experimental older
managed state may be rebuilt from a verified complete Markdown projection. If
that cannot be proved, Tine preserves the old bytes for an explicit offline
recovery tool; it does not keep the old activation/runtime path indefinitely.
Direct Files remains an independent first-class backend.

## Crash-state contract

| Durable state at restart | Authority | Required behavior |
|---|---|---|
| No final marker; no private episode | Direct Files | Start or retry from the current graph. |
| No final marker; partial capture/genesis/SQLite/backup | Direct Files | Ignore or quarantine the episode; rebuild from current graph. |
| Final scan found a changed path or byte | Direct Files | Refuse promotion, preserve diagnostics, and retry from a new capture. |
| Final scan receipt exists only in process memory | Direct Files | A restart cannot inherit it; rebuild or independently revalidate. |
| Marker publication was started but the final marker is absent | Direct Files | Treat all prepared products as uncommitted. |
| Final marker is present and names a complete genesis root | Managed genesis/history | Admit managed storage; rebuild missing or stale disposable projections. |
| Final marker is present but its genesis cannot be validated | Neither silently | Refuse managed admission and offer explicit recovery or Return to Direct Files; never combine partial authorities. |
| First mutation has no committed ordinary receipt | Genesis capsule | Discard partial materialization and retry deterministically. |
| First mutation's ordinary receipt is committed | Ordinary managed document | The receipt supersedes that page's genesis capsule. |

## Consequences

Activation work becomes proportional to source bytes and block count rather
than to interactive history machinery or derived authenticated index updates.
The format and engine gain a lazy baseline and first-mutation seam, and the old
multipart bootstrap runtime becomes obsolete. Same-process receipts reduce
redundant reads but do not weaken crash/recovery validation. SQLite can be
rebuilt and is never sync authority. Direct Files behavior and durability are
unchanged.

The production switch is gated on differential semantic tests, crash cuts, one
parser call per page, zero interactive replay, zero eagerly opened mutable
documents for untouched pages, zero reference-posting Patricia nodes in the new
format, Linux and Windows performance receipts, and Android activation, share,
join, reopen, and Return-to-Direct-Files journeys.
