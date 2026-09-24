/** The failed platform call behind a Direct Files save failure (GH #538).
 *  Kept apart from `backend.ts` so the diagnostics sanitizer can use it
 *  without importing the backend. */

export interface SavePlatformStep {
  operation: string | null;
  osError: number | null;
}

/** `; <operation>, os error <n>` — both closed backend vocabulary. */
export function describeSavePlatformStep(step: SavePlatformStep | null): string {
  if (!step) return "";
  const parts = [step.operation, step.osError === null ? null : `os error ${step.osError}`]
    .filter((part): part is string => part !== null);
  return parts.length ? `; ${parts.join(", ")}` : "";
}

/** The backend's operation names are fixed strings such as
 *  `renameat2(RENAME_NOREPLACE) publishing the projection`; anything else is
 *  dropped rather than shown. */
const SAVE_OPERATION = /^[A-Za-z0-9 ()|_.,-]{1,120}$/u;

export function readSavePlatformStep(detail: unknown): SavePlatformStep | null {
  if (!detail || typeof detail !== "object") return null;
  const record = detail as Record<string, unknown>;
  const operation = typeof record.operation === "string" && SAVE_OPERATION.test(record.operation)
    ? record.operation
    : null;
  const osError = typeof record.os_error === "number" && Number.isSafeInteger(record.os_error)
    ? record.os_error
    : null;
  return operation === null && osError === null ? null : { operation, osError };
}
