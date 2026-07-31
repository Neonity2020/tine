# Full-graph anonymizer

Create a local, graph-shaped reproduction for activation-performance or
byte-projection debugging:

```bash
npm run anonymize-graph -- --source /path/to/graph --destination /path/to/tine-anonymized-graph
```

The destination must not already exist. The command never modifies the source,
never uploads anything, and exports only regular UTF-8 `.md`, `.markdown`, and
`.org` files. It rejects source symlinks, invalid managed text, unsafe output
paths, and output collisions. `.git`, `.tine-sync`, `assets`, and every
non-managed file are excluded.

Each run uses a fresh in-memory random salt and one-way keyed pseudonyms. It
keeps file byte lengths, line endings, punctuation, supported grammar tokens,
and repeated page/block identities so the result retains graph activation shape.
The salt and reverse map are never written. Review the output before sharing:
anonymization reduces risk; it is not a formal privacy proof.

To make an archive without embedding the source directory name, write the
archive beside the export and archive its contents from inside the destination:

```bash
tar -C /path/to/tine-anonymized-graph -czf /path/to/tine-anonymized-graph.tar.gz .
```
