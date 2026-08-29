export const UI_STATE_LIFETIMES = [
  "device-preference",
  "graph-configuration",
  "graph-session",
  "transient-runtime",
] as const;

export type UiStateLifetime = (typeof UI_STATE_LIFETIMES)[number];

export interface PersistedPdfTarget {
  filename: string;
  label: string;
}

export interface GraphSessionUiStateSchema {
  pdfTarget: PersistedPdfTarget | null;
}

interface UiStateDecision {
  owner: string;
  lifetime: UiStateLifetime;
  resetTrigger: string;
  persistence: string;
}

/**
 * Compile-time-complete registry for the graph-session UI state migrated to the
 * audited session boundary. Adding a key to GraphSessionUiStateSchema requires
 * an explicit lifetime/reset/persistence decision here.
 */
export const graphSessionUiStateRegistry: {
  [K in keyof GraphSessionUiStateSchema]: UiStateDecision;
} = {
  pdfTarget: {
    owner: "ui.pdfTarget",
    lifetime: "graph-session",
    resetTrigger: "graph switch, workspace switch, or explicit close",
    persistence: "stable filename and label only",
  },
};

export function parsePersistedPdfTarget(value: unknown): PersistedPdfTarget | null {
  if (!value || typeof value !== "object") return null;
  const input = value as Record<string, unknown>;
  if (typeof input.filename !== "string" || !input.filename || input.filename.length > 4096) return null;
  if (typeof input.label !== "string" || input.label.length > 4096) return null;
  return { filename: input.filename, label: input.label };
}
