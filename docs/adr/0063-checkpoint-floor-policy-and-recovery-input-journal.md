# 0063. Checkpoint floor policy and recovery-input journal

- **Status:** Accepted
- **Date:** 2026-09-11

## Context

A shallow CRDT checkpoint can discard operation history while retaining the
current document state. That is useful only if the cut is conservative: an
offline device may later deliver an original operation whose dependency is
older than the installed native shallow floor. The receiver must then rebuild
from retained originals and admit the incoming operation under its original
BatchId. Treating age as permission to cut would make ordinary absence create
recovery work even when retained history is small; assuming the CRDT library
uses the requested boundary exactly would make the policy claim a floor it did
not install.

The receiver also needs crash-safe custody of that incoming original before it
acknowledges or dequeues provider delivery. Neither existing local journal can
hold it honestly. The foreground journal records locally authored semantic
operations and its sequence is the application-save ordering authority. A peer
original is not local authorship.

The projection-turn journal records work derived from an already accepted
operation and may refer to accepted archive bytes because acceptance is its
authority boundary. A below-floor original is, by definition, not accepted.
Its bytes may never have entered the receiver's accepted archive, and its
provider source may disappear immediately after custody is acknowledged.
Putting only its BatchId or a path/reference in a projection turn would leave
no durable owner of the canonical manifest and objects across a crash. Putting
the bytes in a field on a projection turn would also change that journal from a
derived-publication continuation into an input-custody log with a different
commit and cleanup boundary.

## Decision

Checkpoint floor selection uses two independent gates. Acceptance age supplies
only an eligibility upper bound. Byte pressure supplies the reason to cut. For
each document the worker measures the current image and latest-state image,
examines causally closed candidates in increasing K order, and chooses the
oldest verified candidate that meets hysteresis. The native shallow export is
re-imported and its actual floor is recorded; requested K never substitutes for
that result. Age alone retains the current floor.

We add a third local-journal domain, the recovery-input journal. It is scoped to
the graph and receiving endpoint and stores the exact canonical incoming
manifest and required objects, plus their workspace, lineage, source and
receiver bindings. Its durable append is the custody commit point and precedes
provider acknowledgement. It records custody, not acceptance, and cannot grant
archive, projection, or cleanup authority.

Full-history reconstruction fences the foreground, projection-turn and
recovery-input domains independently. It replays accepted originals from
genesis, replays retained foreground work through the existing transition, and
admits recovery inputs under their original identities. Only after a recovered
checkpoint is published, reopened, and all three fences are re-proved may a new
empty recovery-input generation become current and ordinary application
admission resume.

Not yet in effect on the live capture path. The live path supplies eligibility
E=0 (`LIVE_FLOOR_ELIGIBILITY_DISABLED`), so `choose_floor` returns `Keep` on
every call and no image has ever been cut; `docs/storage-sync-contract.md`
states the same fact and a doc-code test pins the two together. This ADR records
the decision, not a running mechanism. The slice that observes acceptance age
and supplies a real E deletes that constant and this paragraph together.

## Consequences

Long absence does not by itself increase checkpoint churn. A chosen cut is
explained by measured bytes and qualified native behavior, and the recorded
requested/actual floors expose normalization instead of concealing it.

Recovery-input storage duplicates canonical provider bytes temporarily, but it
gives those unaccepted bytes a real durable owner. The third sequence domain is
additional protocol surface; keeping it separate prevents peer custody from
masquerading as local authorship or accepted projection work and gives crashes
an unambiguous restart trigger and cleanup fence.

The living wire formats, ordered recovery steps, retry behavior, diagnostics,
and refusal scenarios remain specified in `docs/storage-sync-contract.md`.
