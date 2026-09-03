# Small-file writes — living contract

Scope: the small files Tine rewrites in place outside the oplog storage path —
`logseq/config.edn` and the PDF/highlight `.edn` sidecars. Page saves are NOT
covered here; they have their own audited path and their own contract.

This is a *living* contract: it is updated in the same commit as the code it
describes.

## The threat this exists for

Tine is not the only writer of these files. Syncthing delivers a peer's
`config.edn`; Logseq writes the same file; an external editor saves a sidecar;
a second Tine instance runs. These are **honest concurrent writers**, and they
are in scope. A byte-forging attacker with local write access is NOT (trust
decision, 2026-08-07) — nothing here defends against one.

## Why a lock and a recheck are not enough

`CONFIG_LOCK` serializes Tine's own writers only. `atomic_update` therefore
re-reads the file immediately before publishing and retries its key-local edit
if the bytes moved. That narrows the window; it cannot close it. The recheck and
the rename are two operations, and an external writer can land between them —
reproduced byte-for-byte on both `config.edn` and a PDF highlight sidecar during
the 2026-08-24 audit: external write observed on disk, Tine's write returns
`Ok`, external bytes gone. No conflict copy, no refusal, no trace.

That is the class ADR 0007 forbids for pages. These paths predate it.

## The protocol

`atomic_replace_expected(path, expected, next)` publishes only if `path` still
holds `expected`. The capture **is** the rename, so no check-then-act window
exists:

1. **Stage** — write `next` to a unique same-directory temp, fsync the file.
2. **Retire** — `rename(path -> .{name}.{pid}.{seq}.retired)`. This atomically
   captures whatever is current, on a name no one else is writing.
3. **Verify** — read the retired copy. If it is not `expected`, an external
   write landed: rename it back, discard the temp, and return
   `ExternalChanged(found)`. **Nothing is published.**
4. **Publish** — no-replace rename of the temp into the vacated name. An
   `AlreadyExists` here means an external *create* won the window; its bytes are
   not ours to replace, so this too returns `ExternalChanged`.
5. **Commit** — fsync the directory (see below), then delete the retired copy.

Creation is unchanged: `atomic_write_new` is already no-clobber
(`create_new` + no-replace rename).

### Caller contracts

| Path | On `ExternalChanged` |
|---|---|
| `atomic_update` (config.edn, device settings) | Retry the key-local edit against the new bytes — up to 4 attempts, then `WouldBlock`. The external write is preserved and Tine's edit is re-applied on top. |
| Sidecar updates | Refuse; the caller surfaces the conflict. |

### The crash window, and how it is closed

Between retire and publish the target name does not exist. A crash there would
otherwise look like a deleted file. `Graph::recover_interrupted_publishes()`
runs on every checked open and sweeps the **registered directories only**
(`logseq/`) — never the whole graph:

- target missing → restore the retired copy (byte-identical);
- target present → the publish completed or an external writer recreated the
  file, so the retired copy is superseded. It still holds the only copy of those
  bytes, so it goes to **recoverable trash** (`logseq/.tine-trash/conflicts/`),
  never to deletion.

Readers of these files already tolerate the window: `atomic_update` treats
`NotFound` as "no baseline" and the config read path defaults on `NotFound`.

## Directory fsync policy

A rename is only durable once the directory entry is. The dir-fsync used to be
best-effort with a discarded `Result`, so a failed fsync still reported durable
success — a false ack on crash. It is now propagated, **except** for errors that
mean "this filesystem does not offer it", which is why it was best-effort in the
first place:

- `Unsupported`, `InvalidInput`, `PermissionDenied`, `NotFound` (Windows has no
  directory handle to open this way);
- `EBADF`, `EACCES`, `EISDIR`, `EINVAL` (several NFS and FUSE implementations).

`EIO`, `ENOSPC` and everything else surface to the caller. Page saves keep their
own `sync_projection_chain_required` contract, untouched.

Pinned by `src/livingContracts.contract.test.ts`: production retains
`atomic_replace_expected`, `Graph::recover_interrupted_publishes()`,
`RETIRED_SUFFIX = ".retired"`, and the four-attempt `atomic_update` retry
bound.
