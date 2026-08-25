# Managed rapid-edit read mismatch

## Reported outcome

On a joined Android graph, a newly created task survived initial sync. Rapid
marker edits, a temporary multiline edit, deletion of those lines, and final
completion then left every save refusing at `managed_read_block_mismatch`.
Managed Storage would not stop cleanly; forced return to Direct Files lost the
optimistic edits.

## Shared invariant

For one foreground page, the page returned after each durable save, the newest
pending local projection, and the actor's materialized oplog state must describe
the same semantic page. Provider delivery may advance the actor between saves,
but it must not make an older pending projection an unrecoverable read
authority. A disagreement must either converge to the newest durable oplog
state or return an ordinary stale-base conflict; it must never wedge all future
saves or clean shutdown.

## Trust boundary

In scope: honest rapid edits, Android scheduling, same-device provider echo,
peer delivery, projection/provider arrival in either order, app interruption,
clean shutdown, forced process death, and reopen. Out of scope: hostile
filesystem forgery.

## Proof checklist

1. Reproduce the field edit sequence with provider work interleaved and prove
   the earliest structural failure that can strand the later
   `managed_read_block_mismatch` state.
2. Prove every acknowledged save survives clean shutdown and cold reopen.
3. Prove a temporary multiline state that was subsequently deleted does not
   reappear locally or on a peer.
4. Prove clean shutdown completes after the sequence without force.
5. Retain the genuine offline-conflict differential and the projection-first
   task-history regressions.

## Proportionality

This is one actor/read-authority boundary in `tine-core`; the frontend retry UI
is only the reporter. The expected fix is local to selection of the current
managed page authority plus focused runtime tests. A wider storage migration is
not part of this packet.

## Diagnosis and resolution

The failing causal chain began one layer before the reported read refusal. A
file synchronizer can deliver a newly created unstamped block's visible
Markdown before its provider operation. The receiving external-import lane and
the original local mutation then hold different internal block identities at
the same sibling position. Hot materialization already used block identity as
a deterministic tie-break, but projection rejected equal order keys, leaving
the accepted provider batch permanently retained at projection drain. Later
foreground state could consequently disagree with the projection selected for
an application read and surface as `managed_read_block_mismatch`.

Equal sibling order is now treated as a valid concurrent CRDT state and is
projected deterministically. A separate, narrow conflict intent recognizes an
exact projection echo from accepted batch origin, complete target bytes, and
the created unstamped forest's structure. It retires only the unchanged
external duplicate through the ordinary durable mutation path. A differential
test proves that genuinely different concurrent same-position creations remain
two blocks. The field-shaped regression covers rapid marker edits, temporary
continuation lines, provider delivery, a later save, clean shutdown, and cold
reopen.
