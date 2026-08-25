import { Show, createSignal, onMount } from "solid-js";
import { backend, type DiagnosticReport } from "../backend";
import { writeClipboardTextResilient } from "../clipboard";
import { platformKind } from "../platform";
import { pushToast } from "../ui";

export function DiagnosticsTab() {
  const [report, setReport] = createSignal<DiagnosticReport | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [desktop, setDesktop] = createSignal(false);

  onMount(() => void platformKind().then((kind) => setDesktop(kind === "desktop")).catch(() => {}));

  const createReport = async () => {
    setBusy(true);
    try {
      setReport(await backend().diagnosticReport(__GIT_COMMIT__, __BUILD_TIME__));
    } catch (error) {
      pushToast(`Could not create diagnostic report: ${String(error)}`, "error");
    } finally {
      setBusy(false);
    }
  };

  const copyReport = async () => {
    const current = report();
    if (!current) return;
    try {
      await writeClipboardTextResilient(current.text);
      pushToast("Diagnostic report copied", "success");
    } catch (error) {
      pushToast(`Could not copy diagnostic report: ${String(error)}`, "error");
    }
  };

  const saveReport = async () => {
    try {
      if (await backend().saveDiagnosticReport(__GIT_COMMIT__, __BUILD_TIME__)) {
        pushToast("Diagnostic report saved", "success");
      }
    } catch (error) {
      pushToast(`Could not save diagnostic report: ${String(error)}`, "error");
    }
  };

  const clearReport = async () => {
    try {
      await backend().clearDiagnostics();
      setReport(null);
      pushToast("Recorded diagnostic events cleared", "success");
    } catch (error) {
      pushToast(`Could not clear diagnostic events: ${String(error)}`, "error");
    }
  };

  return (
    <section class="diagnostics-tab settings-section">
      <h2>Diagnostics</h2>
      <p>
        Tine keeps a small, bounded flight recorder for the current and previous run. It records
        operation names, outcomes, timings, counts, platform and build information.
      </p>
      <p class="settings-hint diagnostics-privacy">
        It does not record graph content, file paths, page titles, queries, URLs, credentials, or
        the detailed opt-in debug log. Nothing is uploaded automatically. You choose whether to
        copy or save a report and share it.
      </p>
      <div class="diagnostics-actions">
        <button type="button" class="primary" disabled={busy()} onClick={() => void createReport()}>
          {busy() ? "Creating…" : "Create diagnostic report"}
        </button>
        <Show when={report()}>
          <button type="button" onClick={() => void copyReport()}>Copy report</button>
          <Show when={desktop()}>
            <button type="button" onClick={() => void saveReport()}>Save report…</button>
          </Show>
        </Show>
        <button type="button" class="danger" onClick={() => void clearReport()}>
          Clear recorded events
        </button>
      </div>
      <Show when={report()}>
        {(current) => (
          <label class="diagnostics-preview">
            <span>Report preview · {current().suggestedFileName}</span>
            <textarea readonly spellcheck={false} value={current().text} />
          </label>
        )}
      </Show>
    </section>
  );
}
