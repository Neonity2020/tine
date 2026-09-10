# 0062. Real deletion and Restore by reconstruction

- **Status:** Accepted
- **Date:** 2026-09-10

## Context

Managed Storage keeps interactive state in one catalog document and one shard
per page. Retaining a deleted block's owner, text container, membership and
sparse identity values made shallow checkpoints retain content in proportion
to the graph's deletion history. Restore formerly depended on that retained
live CRDT payload. The identity-preserving Restore requirement that motivated
retention has been dropped, while Restore must still preserve the semantic
BlockId, immutable home, Logseq UUID provenance, content and placement.

Accepted operation history already contains a manifest-bound semantic effect.
A deletion effect can therefore carry the complete before-image without
keeping the deleted text in the current shard. File-sync delivery, cold archive
relocation, editor undo and concurrent edit/delete or move/delete races require
the selected source to be explicit and independently validated.

## Decision

We will physically remove every causally live deleted block's owner, content and
membership keys, and remove paired UUID/origin keys when present. An atomic
same-batch birth/retirement has no accepted before-image, so it retains its
existing tombstone provenance; a later Restore takes placement from its
operation while authenticating content, identity, home and page from that
birth-retirement effect. `DeletePage` otherwise retains only
the catalog tombstone needed for page identity and removes its still-owned live
payload and preamble; it does not remove blocks already moved elsewhere.

Every reconstruction names its authority: normally the exact accepted deletion
batch and block before-image, for conflict resolution also the accepted racing
after-image, and only for activation-era sweep records without a deletion id the
recorded predecessor frontier. Reconstruction retains BlockId, immutable home
and Logseq identity but inserts a new ordinary upstream `LoroText` container.
The author carries that container's document update. The semantic effect marks
reconstruction separately from birth, and receivers validate source, update,
identity, placement and final effect atomically. Editor undo whose text changed
authors reconstruction followed by an ordinary edit in the same transaction.

This is one current format. Operation schema 11, semantic-effect schema 9 and
lazy-genesis schema 7 have no pre-packet decoder or migration; an earlier store
takes the existing backup-and-rebuild path.

## Consequences

Deleted text payload and old text containers no longer survive in current
shallow shard state, and Restore works from a bounded logical batch point read
in either archive tier. Concurrent reconstructions may create different
container identities, but existing semantic conflict and immutable-identity
admission converge them to one visible block.

Loro map deletion retains key tombstone metadata. We accept approximately 28
bytes per deleted block for the owner/member/content keys, plus separately
reported sparse-key residue for UUID-anchored blocks. Current-state growth is
therefore bounded by the live graph plus disclosed per-identity residue, not by
deleted text size, while accepted immutable history remains the recovery
authority. Generation rebaselining remains responsible for bounding lifetime
archive history.
