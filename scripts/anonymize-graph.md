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

This standalone Node tool cannot safely reuse Tine's private, arbitrary-EDN
`:hidden` parser without making every export build and launch the Rust core.
Therefore a `logseq/config.edn` containing `:hidden` (including an ambiguous or
malformed occurrence) is diagnosed and refused before inventory. Make any
temporary policy change only in a disposable copy of a graph, never in the
original.

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
