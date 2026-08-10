import { For, Show, type JSX } from "solid-js";
import { conflicts, clearConflict } from "../ui";
import { backend } from "../backend";
import {
  dropObservation,
  reobserve,
  saveBaselineFor,
  shownObservationFor,
  graphBinding,
} from "../persistence";
import {
  canForceSave,
  notifyPageBecameReplaceable,
  editGeneration,
  editorActivationFor,
  forceSave,
  forgetPage,
  pageByName,
  pageInstanceGeneration,
  reloadPage,
} from "../store";

// Global save-conflict surface. A save is refused (not clobbered) when the file
// changed on disk under us (external edit / Syncthing). Such a page is parked in
// `conflicts` and skipped by every future save batch until resolved — so it MUST
// be surfaced no matter where the page lives (main view, journals feed, sidebar,
// or a query result), or its edits would be silently stuck and lost on close.
export function ConflictBar(): JSX.Element {
  const reload = async (name: string) => {
    const page = pageByName(name);
    // "Use disk version" is an authority-answering action, exactly like "Keep
    // mine", so it PRESENTS the observation it was clicked under and lets the
    // backend decide. A locally recorded epoch cannot be trusted here: the
    // raw-watcher path revokes an observation with no page event to react to, so
    // every local value can still compare equal while the authority is already
    // gone. (GH #254 increment 3.)
    const shown = shownObservationFor(name);
    const activation = editorActivationFor(name);
    const baseline = saveBaselineFor(name);
    // Captured AT THE CLICK, so later input can be told apart from what the user
    // was actually looking at when they chose to discard it. This must be the
    // CONTENT-edit counter: `pageInstanceGeneration` advances only on page
    // install/retire, so typing leaves it unchanged and the final check below
    // would wave through exactly the input it exists to protect.
    const generation = editGeneration(name);
    const instance = pageInstanceGeneration(name);
    // The GRAPH BINDING, required by acceptance row D2 and never actually
    // captured here. A reopen between the click and the read replaces the core's
    // `Graph` and may migrate journal filenames, so bytes read against the old
    // binding describe a file this graph may not have — installing them replaces
    // the user's unsaved work with stale content and clears the banner.
    // (GH #254 increment 3, round 15.)
    const binding = graphBinding();
    const clickIsLive = () => (
      editGeneration(name) === generation
      && pageInstanceGeneration(name) === instance
      && shownObservationFor(name) === shown
      && editorActivationFor(name) === activation
      && graphBinding() === binding
    );
    // Resolve the file this editor is actually pinned to. Two files can carry
    // one page name (the duplicate-day stray of #21, or same-titled pages in
    // different folders), and resolving by name reaches the backend's CANONICAL
    // owner — which would re-point the tab at a different file and discard the
    // user's edits to this one. Falling back to the name when the pinned file is
    // gone would do the same, so an absent pinned file drops the page instead.
    // A page with no pin (never saved) has only its name to resolve by.
    let dto;
    try {
      dto = page?.path
        ? await backend().getPageByPath(page.path)
        : await backend().getPage(name, page?.kind ?? "page");
    } catch {
      // Reads happen before native compare/consume. A failure therefore leaves
      // the shown banner and retained draft untouched and cannot reverse the
      // button into a save of "mine" when disk is back at baseline.
      return;
    }

    const presentationPath = page?.path || dto?.path;
    if (!page || shown === null || activation === undefined || !presentationPath) return;

    if (dto) {
      let presented: "authorised" | "superseded" | "withdrawn" | null = null;
      const refusal = await reloadPage(dto, {
        // `ensurePageLoaded` invokes this again synchronously after BOTH awaited
        // activation and presentation, in the same turn as installation. It is
        // the actual D2 boundary: exact activation, edit generation, shown
        // observation, graph binding, plus ensurePageLoaded's page-instance check.
        isRequestLive: clickIsLive,
        beforeInstall: async () => {
          presented = await backend().presentConflictOverride(
            presentationPath,
            baseline,
            activation,
            shown,
          );
          if (presented === "superseded") return false;
          if (presented === "withdrawn") {
            // Equality is proved by the same DTO whose bytes would be installed.
            // A divergent read must instead mint a fresh live observation.
            return (dto.rev ?? null) === baseline;
          }
          return true;
        },
      });
      if (refusal) {
        // If presentation ran, its observation is spent/dead/superseded. Drop it
        // before re-observing. If identity moved before presentation, retain it:
        // the re-observing save itself safely supersedes or revives it.
        if (presented !== null) dropObservation(name);
        // Activation failure occurs before presentation and leaves a live banner;
        // no save is needed or authorised. Identity aborts do require the guarded
        // re-observing intent so post-click typing remains durable.
        if (presented !== null || !clickIsLive()) void reobserve(name);
        return;
      }
      // Both authorised and withdrawn-equal presentation consume/dead-end the
      // local observation. The install completed the discard without a write.
      dropObservation(name);
      clearConflict(name);
      // The real "Use disk version" transition: `reloadPage` then `clearConflict`
      // makes the page replaceable and produces no save at all, so nothing else
      // announces it. (GH #254 increment 3.)
      notifyPageBecameReplaceable(name);
    } else {
      // There is no replacement activation for an absent file, so do the native
      // presentation after the fallible read and synchronously re-check identity
      // before accepting deletion. Withdrawn absence diverges from a present
      // baseline and must re-observe rather than silently complete.
      let presented: "authorised" | "superseded" | "withdrawn";
      try {
        presented = await backend().presentConflictOverride(
          presentationPath,
          baseline,
          activation,
          shown,
        );
      } catch {
        void reobserve(name);
        return;
      }
      if (presented !== "authorised" || !clickIsLive()) {
        dropObservation(name);
        void reobserve(name);
        return;
      }
      // The file is gone on disk (deleted/renamed externally). "Use disk version"
      // = accept that: drop the page and its unsaved edits from the store, rather
      // than clearing the conflict and leaving untracked content to be lost silently.
      dropObservation(name);
      forgetPage(name);
    }
  };
  const keepMine = async (name: string) => {
    // Only clear the conflict if the overwrite actually landed — otherwise the
    // edit is still unsaved and must stay surfaced.
    if (await forceSave(name)) clearConflict(name);
  };

  return (
    <Show when={conflicts().length > 0}>
      <div class="conflict-stack">
        <For each={conflicts()}>
          {(name) => (
            <div class="conflict-banner">
              <span class="conflict-msg">
                <strong>“{name}” changed outside this editor</strong> (edited elsewhere or synced in). Your
                unsaved changes weren't written.
              </span>
              <span class="conflict-actions">
                <button class="conflict-btn" onClick={() => void reload(name)}>
                  Use current version
                </button>
                <button
                  class="conflict-btn keep"
                  disabled={!canForceSave(name)}
                  title={canForceSave(name)
                    ? "Replace the current version with your retained draft"
                    : "Keep mine is unavailable because the current managed page could not be identified"}
                  onClick={() => void keepMine(name)}
                >
                  Keep mine
                </button>
              </span>
            </div>
          )}
        </For>
      </div>
    </Show>
  );
}
