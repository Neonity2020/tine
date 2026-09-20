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

export type SearchScalingRow = {
  id: string;
  label: string;
  smallP95Ms: number | null;
  blocksLargeP95Ms: number | null;
  namesLargeP95Ms: number | null;
  largeP95Ms: number | null;
  pageP95Ms: number | null;
  ratio: number | null;
  bothGrowingRatio: number | null;
  ceiling: number | null;
  ratioOk: boolean | null;
  hardCeilingMs: number | null;
  hardCeilingOk: boolean | null;
  nameGrowth: {
    executor: string;
    smallOwnerCount: number;
    namesLargeOwnerCount: number;
    ownerCountRatio: number;
    smallPhysicalPageCount: number;
    namesLargePhysicalPageCount: number;
    smallFixedOwnerCount: number;
    namesLargeFixedOwnerCount: number;
    smallAugmentationOwnerCount: number;
    namesLargeAugmentationOwnerCount: number;
    cases: Array<{
      sourceLabel: string;
      smallP95Ms: number;
      namesLargeP95Ms: number;
      timeRatio: number;
      normalizedLinearity: number;
    }>;
    maxNormalizedLinearity: number;
  } | null;
  ok: boolean | null;
  status: "PASS" | "BREACH" | "DIAGNOSTIC";
  exception: string | null;
  baselineComparison: Record<string, number> | null;
};
export function evaluateSearchScaling(input: unknown, policy: unknown): { rows: SearchScalingRow[]; breaches: SearchScalingRow[] };
export function baselineFromSearchScaling(input: unknown, policy: unknown): Record<string, unknown>;
export function formatSearchScalingRows(rows: SearchScalingRow[]): string;
