# UI state lifetimes

Every UI value belongs to one lifetime. A change that persists or resets a
value must name its owner and lifetime before wiring storage. This prevents a
runtime handle from accidentally becoming durable state and prevents a graph
transition from saving state under the wrong graph.

## Lifetimes

- `device-preference`: owned by the installation, independent of any graph;
  reset by an explicit preference change or application-data reset.
- `graph-configuration`: derived from graph files/configuration and reloaded on
  graph bind or configuration refresh; it is not session state.
- `graph-session`: user view state belonging to one graph or named workspace;
  reset or restored at graph/workspace transitions and persisted through the
  audited session boundary.
- `transient-runtime`: generations, native handles, in-flight work, focus and
  navigation intents; reminted or cleared at the owning runtime boundary and
  never serialized.

## Registered graph-session fields

The typed registry in `src/uiStateRegistry.ts` is the code authority for the
fields migrated to this contract. `GraphSessionUiStateSchema` and
`graphSessionUiStateRegistry` form a compile-time completeness pair.

| Field | Owner | Lifetime | Reset trigger | Persisted representation |
|---|---|---|---|---|
| `pdfTarget` | `ui.pdfTarget` | `graph-session` | graph switch, workspace switch, or explicit close | stable `filename` and `label` only |

PDF `owner`, ownership generation, page, highlight intent, viewer/native
handles, and sidecar view state are `transient-runtime`. On restore, Tine uses
the stable resource identity and mints ownership from the current graph bind.
Restoration and transition suspension never schedule a session write. User open
and explicit close do.

The registry is deliberately incremental. Unmigrated UI signals retain their
existing tests and storage contracts; adding them here requires an explicit row
and typed registry decision in the same change.
