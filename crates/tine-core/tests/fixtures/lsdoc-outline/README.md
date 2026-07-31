# Vendored lsdoc outline corpus

`public-harness.json` contains all 1,895 tracked, public cases from lsdoc's
eight released harness arrays: the fixed, blockgate, block-focused, inline,
mined Markdown, fixed Org, mined Org, and reported-divergence corpora. It
deliberately excludes generated-at-test-time cases, real-graph exports, and any
private corpus.

The fixture records its upstream repository, exact revision, source paths, and
selection rule. Refresh it deterministically from an lsdoc checkout:

```bash
rtk node scripts/refresh-lsdoc-outline-corpus.mjs /path/to/lsdoc "$(rtk git -C /path/to/lsdoc rev-parse HEAD)"
```

The refresh refuses a revision argument that is not the source checkout's
current `HEAD`.

The Rust differential test compares parser event count, source order, kind
mapping, topology, exact source spans, and Tine parse/serialize semantics. It
does not assert implementation digests.
