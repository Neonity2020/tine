/**
 * Reduce a native managed-storage failure to one bounded, shareable line.
 *
 * Backend errors can contain private graph paths, quoted page text, URLs,
 * credentials, or an arbitrarily long debug chain. Recovery UI may preserve
 * the causal class, but must never echo those values to the screen or clipboard.
 */
export function safeManagedErrorDetail(error: unknown): string {
  const message = error instanceof Error ? error.message : typeof error === "string" ? error : "";
  const firstLine = message.split(/[\r\n]/, 1)[0]?.trim() ?? "";
  if (!firstLine) return "The command did not provide a safe diagnostic detail.";
  let safe = firstLine
    .replace(/\bfile:\/\/\/[^\s"'<>]+/giu, "[path]")
    .replace(/\\\\(?:[^\\\s"'<>]+\\)+[^\\\s"'<>]*/gu, "[path]")
    .replace(/\b[A-Za-z]:\\(?:[^\\\s"'<>]+\\)*[^\\\s"'<>]*/gu, "[path]")
    .replace(/(^|[\s("'=])\/(?:[^/\s"'<>]+\/)*[^/\s"'<>]*/gu, "$1[path]")
    .replace(/\b(?:https?|ssh):\/\/[^\s"'<>]+/giu, "[link]")
    .replace(/\b(authorization|bearer|token|secret|password)\b(?:\s*[:=]\s*|\s+)[^\s,;]+/giu, "$1 [redacted]")
    .replace(/"[^"]*"|'[^']*'/gu, (quoted) => (
      /\[(?:path|link|redacted)\]/u.test(quoted) ? quoted : '"[redacted]"'
    ))
    .replace(/\s+/g, " ")
    .trim();
  const hasDiagnosticClass = /\b(?:activation|archive|actor|binding|blocked|bridge|close|command|conflict|corrupt|database|denied|dispatch|drain|error|failed|failure|invalid|join|lookup|malformed|managed|materialization|open|operation|permission|projection|provider|reason|recovery|refused|save|scratch|setup|sqlite|storage|sync|timeout|unavailable|unresponsive)\b/iu.test(safe) || /lease|contended/iu.test(safe);
  if (
    !safe ||
    !hasDiagnosticClass ||
    /[\u0000-\u001f\u007f]/u.test(safe) ||
    /[{}<>\\/]/u.test(safe) ||
    /\S{80,}/u.test(safe)
  ) {
    return "The command failed without a safe diagnostic detail.";
  }
  const maxLength = 280;
  if (safe.length > maxLength) safe = `${safe.slice(0, maxLength - 1).trimEnd()}…`;
  return safe;
}
