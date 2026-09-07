# Direct query result identities

Source text owns page contents. Runtime block IDs belong to an open Graph
session; SQLite is a disposable projection, not durable identity authority.

The session owner retains compact preorder IDs and child counts for exact page
revisions published through the existing page-cache update boundary. It retains
no block text or Documents. A source-revision and parse-config match, followed
by a complete tree-shape match, is required before restoring IDs into a parse.
No partial restoration is permitted. Exact page deletion removes its map.

Page loads, ordinary live saves, and streaming projection production use that
same owner. Dropping the parsed-page cache alone preserves compatible identities.
An incompatible source revision or parse configuration cannot reuse its old map.
A new Graph starts with no session mappings and derives structural runtime IDs;
it can reuse an unchanged projection without rebuilding the database.

Snapshot jobs capture the projection's set of pages carrying current-session
IDs. A warm stream marks an exact-revision restoration Live and a fresh parse
Structural. This provenance controls whether result construction uses stored
IDs or deterministic structural IDs. Authority serialization is unchanged.

Gates: `edited_page_reload_and_sql_keep_the_same_session_ids`,
`session_identity_survives_parsed_page_eviction`, and
`session_pages_name_exactly_the_pages_this_process_lowered`.

This contract repairs identity consistency. It does not establish that the
reported multiline TODO viewport jump has been reproduced or fixed.
