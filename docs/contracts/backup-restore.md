# Backup restore publication — living contract

Scope: restoring Markdown/Org, asset sidecars, and `logseq/config.edn` from a
Tine backup. This is a deliberately separate publication stack from ordinary
graph saves and managed storage.

The stack stays capability-bound. Recovery directories and live parents are
opened beneath approved graph or assets roots with `cap-std`; symlink or
non-directory ancestors are refused. It must not be routed through ambient
pathname helpers merely to share implementation.

For each restored file:

1. Reserve recovery on the same filesystem as the live root. Every directory
   traversed for recovery is followed by a parent-directory barrier before use,
   including an existing entry that could be residue from a previously refused
   attempt.
2. If a live name exists, atomically move it into recovery with no replacement.
   A cross-directory rename flushes the recovery parent first and the live
   parent second, so the retained copy is durable before retirement is
   acknowledged.
3. Copy verified snapshot bytes to a unique same-directory temporary file,
   flush the file, and publish it with an atomic no-replace rename.
4. Flush the live parent before reporting publication success.

An unexpected cross-filesystem retirement copies and flushes complete recovery
bytes but leaves the live name in place and returns an error; it never performs
copy-then-delete. A directory flush error that means the filesystem does not
support the operation follows the shared unsupported policy. `EIO`, `ENOSPC`,
and other real failures are returned. After such a refusal a name may already
have moved or appeared, but any present copy must contain complete old or new
bytes; retry/recovery must inspect that state rather than assuming the rejected
operation made no change.

The in-scope threat is crash, power loss, disk failure, and honest concurrent
filesystem activity. An attacker with arbitrary local filesystem mutation is
out of scope.
