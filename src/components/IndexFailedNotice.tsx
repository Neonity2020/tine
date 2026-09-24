import { createSignal } from "solid-js";
import { backend } from "../backend";
import { openSettings, pushToast } from "../ui";

/** What a surface that needs the index shows once the index has failed: the
 *  failure's code, Retry (a reopen that builds it again, as the next launch
 *  would) and the way to a diagnostic report. Never a hidden panel or a
 *  "Loading…" with no end (GH #594, index liveness L4). */
export function IndexFailedNotice(props: { subject: string; failure: string }) {
  const [retrying, setRetrying] = createSignal(false);
  const retry = async () => {
    setRetrying(true);
    try {
      await backend().retryIndex();
    } catch (error) {
      pushToast(`Could not retry building the index: ${String(error)}`, "error");
    } finally {
      setRetrying(false);
    }
  };
  return (
    <div class="index-failed" role="alert">
      <span class="index-failed-message">
        {props.subject} need the search index, and it couldn’t be built (code: {props.failure}).
      </span>{" "}
      <button type="button" class="index-failed-retry" disabled={retrying()} onClick={() => void retry()}>
        {retrying() ? "Retrying…" : "Retry"}
      </button>{" "}
      <button type="button" class="index-failed-report" onClick={() => openSettings("diagnostics")}>
        Create diagnostic report
      </button>
    </div>
  );
}
