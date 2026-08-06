# Tine — agent working agreement (pointer)

The full, canonical agent working agreement for this project lives **outside this
git repository** — deliberately, so that a `git clean` in the worktree can't
delete it (that happened on 2026-07-10 and wiped the in-tree copy). Read it before
working:

```
/aux/koutecky/logseq/tine-agents/AGENTS.md
```

**Current plan of record (Martin, 2026-08-06): Direct Files bugs outrank managed
storage.** Read before proposing or starting work:

```
/aux/koutecky/logseq/tine-agents/specs/handoffs/2026-08-06-direct-files-program.md
```

Its §0 settles, at source level, the defect the whole program rests on: managed-
storage shadow-import capture machinery runs on the classic Direct Markdown save
path, so every save does a two-pass whole-graph byte-retaining walk plus a parse
of every document — to answer a filename question. Fix it by **cutting**, not by
caching. Don't re-derive that diagnosis, and don't build a repro from the
underlying audit without reading its §11 verification: two of its exemplars are
wrong even where the mechanism is right.

Private engineering & data-safety specs live alongside it under
`/aux/koutecky/logseq/tine-agents/specs/` (audits, perf-batch, data-safety fixes,
notes). The public roadmap is `docs/BACKLOG.md`; architecture decisions are in
`docs/adr/`.

This pointer is intentionally **tracked** (a tracked file survives `git clean`, an
ignored one does not), so both agents — Codex (reads `AGENTS.md`) and Claude Code
(reads `CLAUDE.md`, which imports the same file) — always discover the working
agreement. The canonical file itself is local to this machine and not part of the
public repo.
