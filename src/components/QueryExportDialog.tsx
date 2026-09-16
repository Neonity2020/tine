import { For, Show, createEffect, createResource, createSignal, onCleanup, onMount, type JSX } from "solid-js";
import { backend, QueryUnavailableError } from "../backend";
import { queryExportRequest, closeQueryExport, openSettings, pushToast } from "../ui";
import { registerTransientLayer } from "../transientLayers";
import type { QueryPublicationPlan, QueryPublicationRequest } from "../types";
import { readLatestOr } from "../resourceRead";

/** "Export query results…": plan → review the page set → confirm.
 *
 *  Nothing is held on the backend between the two steps: the plan's
 *  fingerprint is echoed back and the export refuses if the reviewed set moved.
 *  A block-anchored query exports WHOLE pages, and the dialog will not confirm
 *  until that is acknowledged. */
export function QueryExportDialog(): JSX.Element {
  return (
    <Show when={queryExportRequest()}>
      {(request) => <Dialog request={request()} />}
    </Show>
  );
}

type Destination = "create" | "replace" | "separate";

/** Mirrors `QUERY_EXPORT_BUDGET_REASON` in `src-tauri/src/commands/publication_helpers.rs`. */
export const QUERY_EXPORT_BUDGET_REASON = "export_asset_budget_exceeded";

function Dialog(props: { request: QueryPublicationRequest }): JSX.Element {
  let root: HTMLDivElement | undefined;
  createEffect(() => {
    const unregister = registerTransientLayer({
      id: "query-export",
      root: () => root ?? null,
      dismiss: () => {
        closeQueryExport();
        return true;
      },
    });
    onCleanup(unregister);
  });

  const [name, setName] = createSignal(props.request.name);
  // The name the plan was made for; re-plan when it changes on blur/Enter,
  // not on every keystroke.
  const [plannedName, setPlannedName] = createSignal(props.request.name);
  const [destination, setDestination] = createSignal<Destination>("create");
  const [acknowledged, setAcknowledged] = createSignal(false);
  const [busy, setBusy] = createSignal(false);
  const [failure, setFailure] = createSignal<string | null>(null);
  // The export would copy more assets than the device's limit allows: the
  // failure names the limit and offers the setting in one click.
  const [overBudget, setOverBudget] = createSignal(false);

  const [planResource] = createResource(
    () => plannedName(),
    async (forName): Promise<QueryPublicationPlan | { refused: string }> => {
      if (!forName.trim()) return { refused: "Give the export a name." };
      try {
        return await backend().publishQueryPlan({ ...props.request, name: forName, folder: null });
      } catch (e) {
        return { refused: String((e as Error)?.message ?? e) };
      }
    },
  );
  const plan = () => readLatestOr(planResource, undefined, "query export plan");
  const planned = (): QueryPublicationPlan | undefined => {
    const p = plan();
    return p && !("refused" in p) ? p : undefined;
  };
  const refusal = (): string | undefined => {
    const p = plan();
    return p && "refused" in p ? p.refused : undefined;
  };
  const blockAnchored = () => planned()?.anchor === "block";
  const journalCount = () => planned()?.pages.filter((p) => p.journal).length ?? 0;
  const folder = (): string | undefined => {
    const p = planned();
    if (!p) return undefined;
    if (p.exists && destination() === "separate") return p.suggestedFolder ?? undefined;
    return p.folder;
  };
  const destinationPath = (): string | undefined => {
    const p = planned();
    const f = folder();
    if (!p || !f) return undefined;
    return p.path.slice(0, p.path.length - p.folder.length) + f;
  };
  const canExport = () => {
    const p = planned();
    if (!p || busy() || p.pages.length === 0) return false;
    if (blockAnchored() && !acknowledged()) return false;
    if (p.exists && destination() === "create") return false;
    return true;
  };

  const commitName = () => {
    const next = name().trim();
    if (next && next !== plannedName()) {
      setDestination("create");
      setPlannedName(next);
    }
  };

  const doExport = async () => {
    const p = planned();
    const f = folder();
    if (!p || !f || !canExport()) return;
    setBusy(true);
    setFailure(null);
    setOverBudget(false);
    try {
      const outcome = await backend().publishQuery(
        {
          ...props.request,
          name: plannedName(),
          folder: f,
          replace: p.exists && destination() === "replace",
        },
        p.fingerprint,
      );
      closeQueryExport();
      pushToast(
        `Exported ${outcome.pages} page${outcome.pages === 1 ? "" : "s"} to ${outcome.path}` +
          (outcome.retired ? ` (previous export kept at ${outcome.retired})` : ""),
        "success",
        { sticky: true },
      );
      // Omitted assets are a separate, visible fact: the export succeeded but
      // is not complete, and the user must learn that before moving the folder.
      if (outcome.warnings.length > 0) {
        pushToast(
          `${outcome.warnings.length} asset${outcome.warnings.length === 1 ? "" : "s"} omitted: ` +
            outcome.warnings.join(" "),
          "warn",
          { sticky: true },
        );
      }
    } catch (e) {
      setFailure(String((e as Error)?.message ?? e));
      setOverBudget(
        e instanceof QueryUnavailableError && e.reasonCode === QUERY_EXPORT_BUDGET_REASON,
      );
    } finally {
      setBusy(false);
    }
  };
  const adjustLimit = () => {
    closeQueryExport();
    openSettings("graph");
  };

  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        closeQueryExport();
      }
    };
    window.addEventListener("keydown", onKey, true);
    onCleanup(() => window.removeEventListener("keydown", onKey, true));
  });

  return (
    <div class="modal-overlay" onClick={closeQueryExport}>
      <div
        ref={root}
        class="export-modal query-export-modal"
        role="dialog"
        aria-label="Export query results"
        onClick={(e) => e.stopPropagation()}
      >
        <div class="export-head">Export query results</div>

        <div class="export-opts">
          <label class="export-opt-row query-export-name">
            <span class="export-opt-label">Name</span>
            <input
              class="query-export-name-input"
              value={name()}
              placeholder="Export name"
              onInput={(e) => setName(e.currentTarget.value)}
              onBlur={commitName}
              onKeyDown={(e) => {
                e.stopPropagation();
                if (e.key === "Enter") {
                  e.preventDefault();
                  commitName();
                }
              }}
            />
          </label>

          <Show when={refusal()}>
            {(message) => <div class="query-export-refused" role="alert">{message()}</div>}
          </Show>
          <Show when={planResource.loading && !planned()}>
            <div class="query-export-note">Resolving pages…</div>
          </Show>

          <Show when={planned()}>
            {(p) => (
              <>
                <div class="query-export-summary">
                  {p().rowCount} result{p().rowCount === 1 ? "" : "s"} on{" "}
                  <b>{p().pages.length}</b> page{p().pages.length === 1 ? "" : "s"}
                  <Show when={journalCount() > 0}> ({journalCount()} journal{journalCount() === 1 ? "" : "s"})</Show>
                  <Show when={p().sampled}> · sampled</Show>
                  <Show when={p().boundPage}>{(b) => <> · current page: {b()}</>}</Show>
                </div>
                <Show when={p().pages.length === 0}>
                  <div class="query-export-note">The query has no results; nothing to export.</div>
                </Show>
                <ul class="query-export-pages" data-testid="query-export-pages">
                  <For each={p().pages}>
                    {(page) => (
                      <li>
                        <span class="query-export-page-name">{page.name}</span>
                        <Show when={page.journal}>
                          <span class="k">journal</span>
                        </Show>
                        <span class="query-export-page-path">{page.path}</span>
                      </li>
                    )}
                  </For>
                </ul>

                <Show when={blockAnchored() && p().pages.length > 0}>
                  <label class="query-export-ack">
                    <input
                      type="checkbox"
                      checked={acknowledged()}
                      onChange={(e) => setAcknowledged(e.currentTarget.checked)}
                    />
                    All blocks on these pages will be exported — not just the matched blocks.
                  </label>
                </Show>

                <Show when={p().exists}>
                  <div class="query-export-collision" role="group" aria-label="Destination">
                    <div>
                      An export named <code>{p().folder}</code> already exists.
                    </div>
                    <label>
                      <input
                        type="radio"
                        name="query-export-destination"
                        checked={destination() === "replace"}
                        onChange={() => setDestination("replace")}
                      />
                      Replace it (the previous export is kept in recovery)
                    </label>
                    <label>
                      <input
                        type="radio"
                        name="query-export-destination"
                        checked={destination() === "separate"}
                        onChange={() => setDestination("separate")}
                      />
                      Create a separate export as <code>{p().suggestedFolder}</code>
                    </label>
                  </div>
                </Show>

                <div class="query-export-note">
                  Destination: <code>{destinationPath()}</code>
                  <br />
                  This exports these pages regardless of their public setting. It creates a static
                  site and an app version as local files and does not upload them; host the folder
                  to open it as an app. Unsaved edits are not included.
                </div>
              </>
            )}
          </Show>

          <Show when={failure()}>
            {(message) => (
              <div class="query-export-refused" role="alert">
                {message()}
                <Show when={overBudget()}>
                  <div style={{ "margin-top": "6px" }}>
                    <button class="export-btn-secondary" onClick={adjustLimit}>
                      Adjust limit in Settings…
                    </button>
                  </div>
                </Show>
              </div>
            )}
          </Show>
        </div>

        <div class="export-foot">
          <button class="export-btn-secondary" onClick={closeQueryExport}>
            Cancel
          </button>
          <button class="export-btn-primary" disabled={!canExport()} onClick={() => void doExport()}>
            {busy() ? "Exporting…" : "Export"}
          </button>
        </div>
      </div>
    </div>
  );
}
