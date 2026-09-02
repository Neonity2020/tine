# Frontend asynchronous landing

Graph-scoped asynchronous work is owned by `graphBindingRev`, exposed through
`graphBinding()`. Capture a frozen scope before the first `await`, re-check it
after every `await`, and check again immediately before the first graph-scoped
IPC or UI/store commit. A stale background result is dropped; a stale
user-initiated result is dropped with a local toast.

`graphEpoch()` is a render epoch, not graph identity. Typography, journal-title,
and other display changes may bump it without changing the graph. It is compared
only when a result is explicitly repaint-sensitive.

`captureGraphScope`, `isScopeCurrent`, `landAsync`, and `landAsyncOrToast` in
`src/landAsync.ts` provide the standard shape and a discriminated landing result.
Existing specialized exemplars remain `pdfOwnership.ts`, RightSidebar's
`useEnsurePage`, Block's `editorIsCurrent`, and graph's
`journalTemplateOwnerIsCurrent`. The source guard in
`src/frontendStaleness.guard.test.ts` pins the token rule to
`persistence.ts:362` and I-20.
