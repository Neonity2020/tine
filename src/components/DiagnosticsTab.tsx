import { For, Show, createSignal, onCleanup, onMount } from "solid-js";
import {
  OperationCancelledError,
  backend,
  type DiagnosticReport,
  type GraphVerificationProgress,
  type GraphVerificationReport,
} from "../backend";
import { writeClipboardTextResilient } from "../clipboard";
import {
  compareGraphVerificationManifests,
  parseGraphVerificationManifest,
  type GraphVerificationComparison,
} from "../graphVerification";
import { platformKind } from "../platform";
import { pushToast } from "../ui";

export function DiagnosticsTab() {
  const [report, setReport] = createSignal<DiagnosticReport | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [desktop, setDesktop] = createSignal(false);
  const [verification, setVerification] = createSignal<GraphVerificationReport | null>(null);
  const [verificationProgress, setVerificationProgress] = createSignal<GraphVerificationProgress | null>(null);
  const [verificationOperation, setVerificationOperation] = createSignal<string | null>(null);
  const [otherManifest, setOtherManifest] = createSignal("");
  const [comparison, setComparison] = createSignal<GraphVerificationComparison | null>(null);
  let disposed = false;
  let stopVerificationProgress: (() => void) | undefined;

  onMount(() => {
    void platformKind().then((kind) => setDesktop(kind === "desktop")).catch(() => {});
    void backend().onGraphVerificationProgress((progress) => {
      if (progress.operationId === verificationOperation()) setVerificationProgress(progress);
    }).then((stop) => {
      if (disposed) stop();
      else stopVerificationProgress = stop;
    });
  });
  onCleanup(() => {
    disposed = true;
    stopVerificationProgress?.();
  });

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

  const createVerification = async () => {
    const operationId = globalThis.crypto?.randomUUID?.() ?? `${Date.now()}-${Math.random()}`;
    setVerificationOperation(operationId);
    setVerificationProgress({ operationId, processed: 0, total: 0 });
    setComparison(null);
    try {
      const result = await backend().createGraphVerification(operationId);
      setVerification(result);
      if (!result.complete) pushToast("Graph verification was incomplete", "error");
    } catch (error) {
      if (!(error instanceof OperationCancelledError)) {
        pushToast(`Could not verify graph files: ${String(error)}`, "error");
      }
    } finally {
      setVerificationOperation(null);
    }
  };

  const cancelVerification = async () => {
    const operationId = verificationOperation();
    if (operationId) await backend().cancelGraphVerification(operationId);
  };

  const copyVerification = async () => {
    const current = verification();
    if (!current) return;
    try {
      await writeClipboardTextResilient(current.text);
      pushToast("Graph verification report copied", "success");
    } catch (error) {
      pushToast(`Could not copy graph verification report: ${String(error)}`, "error");
    }
  };

  const saveVerification = async () => {
    const current = verification();
    if (!current) return;
    try {
      if (await backend().saveGraphVerificationReport(current.text)) {
        pushToast("Graph verification report saved", "success");
      }
    } catch (error) {
      pushToast(`Could not save graph verification report: ${String(error)}`, "error");
    }
  };

  const compareVerification = () => {
    const current = verification();
    if (!current) return;
    try {
      setComparison(compareGraphVerificationManifests(
        parseGraphVerificationManifest(current.text),
        parseGraphVerificationManifest(otherManifest()),
      ));
    } catch (error) {
      setComparison(null);
      pushToast(`Could not compare graph verification reports: ${String(error)}`, "error");
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
      <div class="diagnostics-verification">
        <h3>Verify synchronized graph</h3>
        <p>
          Compare the exact Markdown and Org file bytes on two devices. The report includes file
          paths and page names, but not file contents. Nothing is uploaded automatically.
        </p>
        <div class="diagnostics-actions">
          <button type="button" class="primary" disabled={verificationOperation() !== null} onClick={() => void createVerification()}>
            {verificationOperation() ? "Verifying..." : "Create graph verification report"}
          </button>
          <Show when={verificationOperation()}>
            <button type="button" onClick={() => void cancelVerification()}>Cancel</button>
          </Show>
          <Show when={verification()}>
            <button type="button" onClick={() => void copyVerification()}>Copy graph report</button>
            <Show when={desktop()}>
              <button type="button" onClick={() => void saveVerification()}>Save graph report...</button>
            </Show>
          </Show>
        </div>
        <Show when={verificationOperation() !== null ? verificationProgress() : null}>
          {(current) => (
            <p class="settings-hint">
              {current().total === 0 ? "Reading graph file list..." : `${current().processed} / ${current().total} files`}
            </p>
          )}
        </Show>
        <Show when={verification()}>
          {(current) => (
            <>
              <p class="settings-hint">
                {current().complete ? "Complete" : "Incomplete"} · {current().totalFiles} files · {current().totalBytes} bytes
              </p>
              <label class="diagnostics-preview">
                <span>Report preview · {current().suggestedFileName}</span>
                <textarea readonly spellcheck={false} value={current().text} />
              </label>
              <label class="diagnostics-preview">
                <span>Report from the other device</span>
                <textarea
                  spellcheck={false}
                  value={otherManifest()}
                  onInput={(event) => setOtherManifest(event.currentTarget.value)}
                  placeholder="Paste the graph verification report here"
                />
              </label>
              <button type="button" class="primary" disabled={!otherManifest().trim()} onClick={compareVerification}>
                Compare reports
              </button>
            </>
          )}
        </Show>
        <Show when={comparison()}>
          {(result) => (
            <div class="diagnostics-comparison">
              <Show when={result().matches}>
                <p><strong>The source file sets and bytes match.</strong></p>
              </Show>
              <Show when={result().incomplete}>
                <p><strong>At least one report is incomplete. No match can be confirmed.</strong></p>
              </Show>
              <For each={[
                ["Only on this device", result().localOnly],
                ["Only on the other device", result().otherOnly],
                ["Different bytes", result().changed],
              ] as const}>
                {([label, paths]) => (
                  <Show when={paths.length > 0}>
                    <h4>{label}</h4>
                    <ul><For each={paths}>{(path) => <li><code>{path}</code></li>}</For></ul>
                  </Show>
                )}
              </For>
            </div>
          )}
        </Show>
      </div>
    </section>
  );
}
