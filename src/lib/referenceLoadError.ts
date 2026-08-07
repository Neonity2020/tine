/**
 * Shared classification for a failed Linked/Unlinked References fetch.
 *
 * This used to be two copies of a two-line function that mapped every error
 * except `result-too-large:` to "backend" and threw the message away. When the
 * backend started refusing reference queries under Tine-managed storage, it
 * said so in plain words -- "This action is unavailable while Tine-managed
 * storage is active." -- and the UI replaced that with "the backend request
 * failed", which reads as transient. Diagnosing it took a source-level
 * investigation that the discarded string would have answered outright.
 *
 * So: classify for the message we render, and keep the detail for the user who
 * asks for it and for the console.
 */
export type ReferenceLoadErrorKind = "bounded" | "backend";

export type ReferenceLoadError = {
  kind: ReferenceLoadErrorKind;
  /** The backend's own message, never discarded. */
  detail: string;
};

export function classifyReferenceLoadError(error: unknown): ReferenceLoadError {
  const detail = error instanceof Error ? error.message : String(error);
  return {
    kind: detail.startsWith("result-too-large:") ? "bounded" : "backend",
    detail,
  };
}

/** The sentence shown in the references banner. */
export function referenceLoadErrorMessage(error: ReferenceLoadError): string {
  if (error.kind === "bounded") {
    return "Couldn’t load references: the bounded result limit was exceeded.";
  }
  // A backend refusal usually explains itself; prefer its words over ours.
  return error.detail.trim().length > 0
    ? `Couldn’t load references: ${error.detail}`
    : "Couldn’t load references because the backend request failed.";
}
