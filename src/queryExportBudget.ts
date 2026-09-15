// The byte budget one query export may spend on copied assets.
//
// A query export is a movable folder, so every referenced local asset is
// copied into it. Without a ceiling one stray video turns "export this
// reading list" into a multi-gigabyte write; with a silent ceiling the folder
// is quietly incomplete. So the export FAILS when it would pass the budget and
// names this setting (Martin, 2026-09-14: "export failed because it went over
// budget, adjust here" — one click to the setting). Device-local, persisted in
// the same app-string store as the other remembered preferences.

import { createSignal } from "solid-js";
import { backend } from "./backend";

const KEY = "query_export_asset_budget_mib";
export const DEFAULT_QUERY_EXPORT_BUDGET_MIB = 1024;
export const MIN_QUERY_EXPORT_BUDGET_MIB = 1;
export const MAX_QUERY_EXPORT_BUDGET_MIB = 1024 * 1024;

const [budgetMiB, setBudgetMiBSignal] = createSignal(DEFAULT_QUERY_EXPORT_BUDGET_MIB);

/** Reactive: the current limit in MiB. */
export const queryExportBudgetMiB = budgetMiB;

/** The limit as the byte count the export request carries. */
export function queryExportBudgetBytes(): number {
  return budgetMiB() * 1024 * 1024;
}

export function normalizeQueryExportBudgetMiB(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_QUERY_EXPORT_BUDGET_MIB;
  return Math.min(MAX_QUERY_EXPORT_BUDGET_MIB, Math.max(MIN_QUERY_EXPORT_BUDGET_MIB, Math.round(value)));
}

export function changeQueryExportBudgetMiB(value: number): void {
  if (!Number.isFinite(value)) return;
  const next = normalizeQueryExportBudgetMiB(value);
  setBudgetMiBSignal(next);
  void backend().setAppString(KEY, String(next)).catch(() => {});
}

export function resetQueryExportBudget(): void {
  changeQueryExportBudgetMiB(DEFAULT_QUERY_EXPORT_BUDGET_MIB);
}

/** Load the remembered limit at startup. Default: 1 GiB. */
export async function initQueryExportBudget(): Promise<void> {
  try {
    const stored = await backend().getAppString(KEY, "");
    const parsed = Number.parseInt(stored, 10);
    if (Number.isFinite(parsed)) setBudgetMiBSignal(normalizeQueryExportBudgetMiB(parsed));
  } catch {
    /* keep the default */
  }
}
