export type BudgetRow = {
  id: string;
  label: string;
  value: number;
  ceiling: number | null;
  unit: string;
  ok: boolean | null;
};
export function evaluateBudget(measurement: unknown, policy: unknown): { rows: BudgetRow[]; breaches: BudgetRow[] };
export function baselineFrom(measurement: unknown): Record<string, unknown>;
export function formatRows(rows: BudgetRow[]): string;
