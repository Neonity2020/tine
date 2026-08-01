# Full-graph anonymizer

Create a local, graph-shaped reproduction for activation-performance or
byte-projection debugging:

```bash
npm run anonymize-graph -- --source /path/to/graph --destination /path/to/tine-anonymized-graph
```

The destination must not already exist. The command never modifies the source,
never uploads anything, and exports only regular UTF-8 `.md`, `.markdown`, and
`.org` files in Tine's graph-text scope. Hidden components, `node_modules`,
`assets`, `publish`, `.tine-sync`, Logseq recycle/backup/version/trash trees,
and provider conflict copies are excluded before vocabulary collection. The
matching fixed names are case-insensitive. Managed symlinks and hard links,
invalid managed text, unsafe output paths, and portable source or output
identity collisions fail closed without leaving a destination.

Ordinary Logseq `:hidden` vector entries in `logseq/config.edn` are honored
before directory traversal and vocabulary collection. Entries are literal,
case-sensitive graph-relative prefixes: `"archive"` hides `archive`,
`archive/page.md`, and `archive-old/page.md`. One trailing `/` is normalized,
so `"archive/"` has the same effect. Invalid graph-relative entries are inert,
matching Tine's graph-text scope; non-string vector forms are safely skipped.

The standalone Node-only EDN reader understands strings and escapes, comments,
discards, and ordinary nested EDN forms without adding a runtime dependency.
It requires one complete top-level map and enforces byte, depth, form, and entry
limits. Malformed or ambiguous input, duplicate `:hidden` keys, and an empty
hidden entry (Logseq's hide-all spelling) abort before the destination is
created. This conservative refusal means the exporter never treats an unsafe
hidden policy as absent.

Each run uses a fresh in-memory random salt and one-way keyed pseudonyms. It
keeps file byte lengths, line endings, punctuation, supported grammar tokens,
and repeated page/block identities so the result retains graph activation shape.
The salt and reverse map are never written. Review the output before sharing:
anonymization reduces risk; it is not a formal privacy proof.

One-character alphabets have an unavoidable low-entropy limit. If every digit
or ASCII letter is present as its own token, the output must reuse those public
glyphs; a salted collision-free derangement still preserves equality and
distinctness and never leaves a token unchanged. This exception applies only to
single-character tokens. Longer private words and numeric identifiers retain
the strict rule that no source token may be emitted as their fallback. The
portable Unicode collision check is deliberately conservative and may refuse
rare distinct spellings rather than split an identity.

To make an archive without embedding the source directory name, write the
archive beside the export and archive its contents from inside the destination:

```bash
tar -C /path/to/tine-anonymized-graph -czf /path/to/tine-anonymized-graph.tar.gz .
```
