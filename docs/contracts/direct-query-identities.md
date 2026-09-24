# Direct query result identities

Source text owns page contents. Runtime block IDs belong to an open Graph
session; SQLite is a disposable projection, not durable identity authority.

The session owner retains compact preorder IDs and child counts for exact page
revisions published through the existing page-cache update boundary. It retains
no block text or Documents. A source-revision and parse-config match, followed
by a complete tree-shape match, is required before restoring IDs into a parse.
No partial restoration is permitted. Exact page deletion removes its map.

Page loads, ordinary live saves, and captured full-snapshot projection
production use that same owner. Dropping the parsed-page cache alone preserves
compatible identities. An incompatible source revision or parse configuration
cannot reuse its old map. A new Graph starts with no session mappings and derives structural runtime IDs; it can reuse an unchanged projection without rebuilding the database.

Snapshot jobs capture the projection's set of pages carrying current-session
IDs. Pages lowered from the session's captured Documents are Live; rows an unchanged
reopen's survey kept are Structural. This provenance controls whether result
construction uses stored IDs or deterministic structural IDs. Authority
serialization is unchanged.

Every reader that names a stored block to a Document asks one function,
`ResultIdentity::public_id`: result construction, and the Linked and Unlinked
References candidate filter, which admits only the blocks the index named. A
reader that compares stored IDs directly names blocks no Document carries after
a reopen: the filter did, and a page edited in an earlier session lost its
references (GH #594).

Gates: `edited_page_reload_and_sql_keep_the_same_session_ids`,
`session_identity_survives_parsed_page_eviction`,
`session_pages_name_exactly_the_pages_this_process_lowered`, and
`gh594_a_page_edited_before_a_reopen_keeps_its_references`.

This contract repairs identity consistency. It does not establish that the
reported multiline TODO viewport jump has been reproduced or fixed.
