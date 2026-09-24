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

The index stores structural IDs only (R3). Lowering names every block by
`doc_runtime_child(parent, position)`, the same structural ID a fresh parse
derives, in its `result_id`, parent pointer and posting source. No live ID is
written to SQLite, so a reopened session looks up a block on a page edited in
an earlier session by the ID its own parse carries.

A live ID that differs from its structural ID is an exception. Before a page's
lowering commits, the projection records that page's exceptions
(`structural -> live`) with the page's stored source revision, in
`ProjectionShared::session_ids`. The newest `LIVE_ID_REVISIONS` (2) revisions
per page are kept, so a snapshot of the image just replaced still decodes. A
page lowered with no exceptions records an empty entry for its new revision,
and deletion drops the page's entries. Version 4 of
`DIRECT_PROJECTION_FACTS_VERSION` (now 5) re-lowered every page stored by an
earlier version once; each later bump does the same.

A reader captures `ResultIdentity` beside its snapshot
(`capture_result_identity`): the recorded exceptions whose revision equals the
snapshot's stored revision for that page, read in the same snapshot. A
mismatch decodes structurally: degraded IDs, never another block's ID.

Every reader that names a stored block to a Document asks one function,
`ResultIdentity::public_id`: result construction, and the Linked and Unlinked
References candidate filter, which admits only the blocks the index named. A
reader that compares stored IDs directly names blocks no Document carries after
a reopen: the filter did, and a page edited in an earlier session lost its
references (GH #594). The reverse direction asks `ResultIdentity::stored_id`:
a live ID in the captured exceptions translates to its structural ID before a
`result_id` lookup (block resolution, `block_page_hint`).

Gates: `edited_page_reload_and_sql_keep_the_same_session_ids`,
`session_identity_survives_parsed_page_eviction`,
`live_ids_decode_only_with_the_stored_revisions_exceptions`,
`gh594_a_page_edited_before_a_reopen_keeps_its_references`, and
`gh594_a_block_is_found_by_its_id_in_the_session_and_after_a_reopen`.

This contract repairs identity consistency. It does not establish that the
reported multiline TODO viewport jump has been reproduced or fixed.
