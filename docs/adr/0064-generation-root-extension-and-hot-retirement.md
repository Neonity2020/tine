# 0064. Generation root extension and hot-history retirement

- **Status:** Accepted
- **Date:** 2026-09-11

## Context

The first clean checkpoint implementation reopened and retained work in
proportion to accepted device lifetime H. Its payload embedded the covered
manifest/object roster, ordinary namespace open decoded covered originals, and
floor selection enumerated accepted sequences through the age-eligible bound E
once per changed document. Cold publication was additive, so the same covered
bytes also remained in the hot namespace indefinitely.

## Decision

One current checkpoint format replaces those representations. Each publication
extends its predecessor's sealed accepted roots with only C+1..=new C, extends
a covered-object membership root, and records a `(document, acceptance
sequence)` latest-change index. A changed document asks that index for one
predecessor candidate instead of walking 1..=E. The payload carries roots and
bounded live state, never an H-sized roster or object union.

The marker-selected generation is qualified before namespace content
validation. Covered hot names are recognized by point membership and skipped;
only the live tail and explicit hot pins are decoded. Accepted rows remain
point-addressable on disk, and ordinary restore does not materialize them into
the engine's resident maps. Full sequence enumeration remains an explicit
historical consumer, not generation-open work.

Cold relocation visits exactly the newly covered delta. Candidate bytes and
roots are validated, cold originals are published, and `current` is durably
replaced before any hot unlink. The hot-retention set is the union of current
document/projection heads and bounded current-action roots. Retirement verifies
the exact cold manifest and objects before deletion, is idempotent, and resumes
from covered hot names after interruption.

## Consequences

Healthy publication and open work are functions of delta/live/action state,
not accepted lifetime. The observable counters ratchet zero covered-sequence
enumeration and delta-only relocation visits; covered namespace decodes are
ratcheted by the real `namespace_{manifest,object}_decodes` instrumentation and
made unwritable by the `is_covered` early `continue`. A counter is only kept
here when some code path can actually raise it — a field nothing increments
reads zero whether or not the property holds.

The accepted index and cold archive remain immutable recovery authority. A
missing or invalid disposable generation still takes the sequence-zero full
audit and replay. Marker-last ordering means a crash can leave both hot and cold
copies, but never authorizes deletion before an exact cold copy and the new
generation are durable.
