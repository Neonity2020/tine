import { createMemo, type JSX } from "solid-js";
import { doc, type FeedPage } from "../store";
import { facetsOf } from "../render/facets";
import { OPEN_MARKERS } from "../markers";

const IN_PROGRESS_MARKERS: ReadonlySet<string> = new Set([
  "DOING",
  "NOW",
  "STARTED",
  "IN-PROGRESS",
]);

export interface PageTaskSummary {
  open: number;
  inProgress: number;
}

/** Count the already-loaded page tree. Facets come from the backend-seeded
 * lsdoc cache (or the one currently edited block), so this adds no graph scan,
 * IPC, or second task parser. */
export function summarizePageTasks(page: FeedPage): PageTaskSummary {
  let open = 0;
  let inProgress = 0;
  const pending = [...page.roots];
  while (pending.length > 0) {
    const node = doc.byId[pending.pop()!];
    if (!node) continue;
    pending.push(...node.children);
    const marker = facetsOf(node.raw, page.format).marker;
    if (!marker || !OPEN_MARKERS.has(marker)) continue;
    open += 1;
    if (IN_PROGRESS_MARKERS.has(marker)) inProgress += 1;
  }
  return { open, inProgress };
}

export function TodayTaskSummary(props: { page: FeedPage }): JSX.Element {
  const summary = createMemo(() => summarizePageTasks(props.page));
  const tasks = () => `${summary().open} ${summary().open === 1 ? "task" : "tasks"} today`;
  const progress = () => `${summary().inProgress} in progress`;

  return (
    <div class="today-task-summary" aria-label={`${tasks()}, ${progress()}`}>
      <span>{tasks()}</span>
      <span aria-hidden="true">, </span>
      <span>{progress()}</span>
    </div>
  );
}
