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
   the pre-fix failure is `managed_read_block_mismatch`.
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
