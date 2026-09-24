# 0069. A CJK unigram and bigram index answers one- and two-character searches

- **Status:** Accepted (Martin, 2026-09-24)
- **Date:** 2026-09-24

## Context

Ctrl+K searches block text through a trigram FTS5 index. A needle shorter than
three characters cannot use it, so the planner scans blocks in recency order
(`CandidatePlan::Scan`) under a visit budget and says **More matches exist**
when it stops early. For Latin text that is the right trade: one or two
letters match almost everything and the user types a third.

For Chinese, Japanese and Korean it is not. One or two characters are a whole
word there (`会议`, `東京`, `회의`), and users stop typing at that point. A
budgeted scan answered such a word from the newest blocks only: 0 of 26
matches for one measured word at 10k pages. So 82978565 (2026-09-24) exempts
a needle in these scripts from the budget (`is_short_word_script` in
`query/candidate.rs`). That answer is complete, but it is an unbudgeted scan.
A rare word visits every block: 1.5 s on a 10k-page CJK graph (616k blocks),
on every keystroke (`gh543-ctrlk-latency`, 2026-09-24). v0.6.982 answered the
same needles completely in about 0.2 s with a plain substring walk.

The alternatives on the table:

- **Raise the scan budget.** This only moves the cliff. The scan is O(blocks)
  for a rare needle.
- **Scan `block_text` with `instr`.** O(graph) per keystroke, the same cost.
- **Bigram-only index, with prefix queries for single characters.** This
  misses a character that only ends bigrams (the last character of a run).
- **A custom FTS5 tokenizer.** It would need registering in every platform's
  SQLite, including the Android and iOS builds, for a tokenization Rust can do
  before the row is written.
- **A second contentless FTS5 table of CJK unigrams and bigrams, tokenized in
  Rust.** This is the proposal.

Martin approved the approach on 2026-09-24 (BW7-1, decision journal
"launch readiness and CJK search"). This ADR records the design and its cost
for his sign-off, as required for a persisted per-edit artifact.

## Decision

We will add one contentless FTS5 table to the Direct projection. It is keyed
like the trigram table and holds, for each block, the unigrams and bigrams of
every maximal run of characters in the block's folded search text that
satisfy `is_short_word_script`: Han, kana and Hangul, the same predicate that
routes the query, so the index and the router cannot disagree on a script. The tokens
are space-joined and indexed with the stock `ascii` tokenizer, `detail=none`,
`contentless_delete=1`.

*Implementation note (2026-09-24, before any release):* `ascii` replaced the
proposed `unicode61`. `unicode61` treats combining marks as separators, so a
decomposed kana voicing mark (U+3099, inside the router's kana range) would
have split its bigram; `ascii` splits only on ASCII separators and keeps every
non-ASCII scalar inside its token, byte for byte. The unit cost is unchanged.
The tables live in tine-storage 0.28.0 (`short_word_fts`, schema 31); the
tokens are `query::candidate::short_word_tokens`.

- A block with no CJK text writes no row, so a graph without CJK pays nothing.
- A one- or two-character needle that is all CJK asks this table
  (`MATCH '"<needle>"'`) instead of the unbudgeted scan. The answer is
  complete, with the same verification, ranking and window as trigram
  candidates. A needle of three or more characters keeps using trigram. A
  short needle that mixes CJK with other text keeps today's path.
- The table is written in the same transaction as the trigram rows, from the
  same folded text, so the two can never disagree about a block.
- It ships in `DIRECT_PROJECTION_FACTS_VERSION` 5. The search-fold change
  (a3e7bfda, BZ3) already moved the facts version to 5 and has not been
  released, so this costs no second rebuild (BZ5).

Unit cost: +12.4 KB of WAL per edit of a 1-block CJK page (98.9 KB with the
index against 86.5 KB without) and +25–33 KB per edit of a 60-block CJK page
(132–140 KB against 107 KB); 0 bytes on a page without CJK (identical WAL
growth; deleting absent short-word rowids writes nothing). 0 new files per
edit (the same SQLite file). 0 transport bytes (the projection is local and
never synced). Measured 2026-09-24 through Tine's own writer: WAL growth per
save on a scratch graph, six saves each, one 36-character Chinese line per
block, with the tokens on and forced empty. Graph-wide, from the earlier
Python model of the tables (sqlite 3.53.1, a 616k-block all-CJK graph,
`subagent-tasks/notes/2026-09-24-ctrlk-0.6.982-parity.md` §5): +37 MB, and a
4.7 s build alongside the trigram build's 5.2 s. That model had estimated the
per-edit costs at 24.5 KB and 93 KB; the writer measures lower.

## Consequences

- One- and two-character CJK searches are complete and fast: 16.8 ms for a
  bigram with 400k candidates, under 0.1 ms for a rare one (measured).
- A CJK page edit writes about as much again as trigram already does. Most of
  the 60-block cost is the whole-page rewrite that trigram shares. Replacing
  FTS rows per block instead of per page would shrink both, and is a separate
  item.
- The projection on a CJK-heavy graph grows by about the size of the trigram
  index.
- The script ranges are code, shared with the router. A range they miss
  falls back to today's budgeted scan, never to a wrong answer.
- The table joins the projection's storage census, the SQL statement census
  and the doc-code tests like every other projection table.
