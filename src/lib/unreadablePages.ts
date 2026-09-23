/** The notice for pages Tine could not read or parse (`graph-unreadable-pages`).
 *  Tine indexes the rest of the graph around such a page and never rewrites
 *  its file, so the notice names the page and what fixes it. */
export function unreadablePagesMessage(paths: readonly string[]): string {
  const shown = paths.slice(0, 3).join(", ");
  const more = paths.length > 3 ? ` and ${paths.length - 3} more` : "";
  const subject = paths.length === 1 ? "it" : "them";
  return (
    `Tine couldn't read ${shown}${more}, so search, queries and references may be missing ${subject}. ` +
    `Fix or restore the file; Tine picks it up again once it is readable. The rest of the graph is unaffected.`
  );
}
