// Print / export-to-PDF: render a whole page to a self-contained HTML document in
// the Rust core (assets inlined, no sidebar — see publish::page_print_html), drop
// it into a hidden same-origin <iframe>, and print that frame. The user's native
// print dialog (WebKitGTK / WebView2 / WKWebView) then offers "Save as PDF".
//
// Why an iframe and not window.print() on the live app: the editor virtualizes
// blocks (only the on-screen ones are in the DOM), so printing the live page would
// drop most of a long page. The core-rendered document is complete and unstyled by
// the app chrome, so the PDF is the page, nothing else.
import { backend, OperationCancelledError } from "./backend";
import { pushToast, graphMeta, graphTransitioning } from "./ui";
import { graphBinding } from "./persistence";
import { onGraphRebound } from "./modeHooks";
import { runQueryWhenReady } from "./queryReadiness";
import { failureShape } from "./failureShape";
import type { PrintOpts } from "./types";

/** The default export options (match the Rust `PrintOpts::default`). */
export const DEFAULT_PRINT_OPTS: PrintOpts = {
  expand_collapsed: true,
  font_px: 16,
  margin_mm: 16,
};

export const PRINT_IFRAME_SANDBOX = "allow-same-origin allow-modals";

let printRenderers: Promise<{
  katex: typeof import("katex").default;
  hljs: typeof import("highlight.js/lib/common").default;
}> | null = null;

function loadPrintRenderers() {
  if (!printRenderers) {
    printRenderers = Promise.all([
      import("katex").then(async (module) => {
        await import("katex/contrib/mhchem");
        return module.default;
      }),
      import("highlight.js/lib/common").then((module) => module.default),
    ]).then(([katex, hljs]) => ({ katex, hljs }));
  }
  return printRenderers;
}

function bundledStylesheets(): HTMLLinkElement[] {
  const current = new URL(document.baseURI);
  return [...document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"][href]')]
    .filter((link) => {
      try {
        const url = new URL(link.href, document.baseURI);
        return url.protocol === current.protocol
          && url.host === current.host
          && /\/assets\/[A-Za-z0-9_-]+\.css$/.test(url.pathname);
      } catch {
        return false;
      }
    })
    .map((link) => link.cloneNode(true) as HTMLLinkElement);
}

/**
 * Upgrade the core's inert print markup using only code already bundled with
 * Tine. The returned document contains no scripts or third-party resources; it
 * is safe to load in a same-origin iframe whose sandbox does not allow scripts.
 */
export async function preparePrintHtml(html: string): Promise<string> {
  const parsed = new DOMParser().parseFromString(html, "text/html");
  // Defense in depth against a future core regression: never pass executable or
  // remote stylesheet markup into the privileged app origin.
  parsed.querySelectorAll("script, link[rel=\"stylesheet\"]").forEach((element) => element.remove());

  try {
    const { katex, hljs } = await loadPrintRenderers();
    for (const span of parsed.querySelectorAll<HTMLElement>("span.math")) {
      const raw = span.textContent ?? "";
      const display = span.classList.contains("math-display");
      const left = display ? "\\[" : "\\(";
      const right = display ? "\\]" : "\\)";
      const tex = raw.startsWith(left) && raw.endsWith(right)
        ? raw.slice(left.length, -right.length)
        : raw;
      span.innerHTML = katex.renderToString(tex, { throwOnError: false, displayMode: display });
    }
    for (const code of parsed.querySelectorAll<HTMLElement>("pre.code-block > code")) {
      const source = code.textContent ?? "";
      const language = [...code.classList]
        .find((name) => name.startsWith("language-"))
        ?.slice("language-".length);
      try {
        code.innerHTML = language && hljs.getLanguage(language)
          ? hljs.highlight(source, { language }).value
          : hljs.highlightAuto(source).value;
      } catch {
        code.textContent = source;
      }
      code.classList.add("hljs");
    }
  } catch (error) {
    // A failed optional renderer must not make printing unavailable. The core
    // markup already contains readable raw TeX and escaped plain code.
    console.error("local print rendering failed", failureShape(error));
  }

  for (const link of bundledStylesheets()) parsed.head.appendChild(link);
  return `<!doctype html>${parsed.documentElement.outerHTML}`;
}

/** Export a page to PDF via the OS print dialog. Safe to call repeatedly. */
let activePrint: AbortController | null = null;

export async function exportPagePdf(name: string, opts: PrintOpts = DEFAULT_PRINT_OPTS): Promise<void> {
  activePrint?.abort();
  const controller = new AbortController();
  activePrint = controller;
  const binding = graphBinding();
  const root = graphMeta()?.root;
  const current = () => activePrint === controller && !controller.signal.aborted
    && graphBinding() === binding && graphMeta()?.root === root && !graphTransitioning();
  const abort = () => controller.abort();
  const unbind = onGraphRebound(abort);
  window.addEventListener("pagehide", abort);
  window.addEventListener("beforeunload", abort);
  let iframe: HTMLIFrameElement | undefined;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let pending = false;
  try {
    const prepared = await runQueryWhenReady(() => backend().pagePrintHtml(name, opts), {
      signal: controller.signal, isCurrent: current,
      onPending(error) {
        if (error && !pending) { pending = true; pushToast("Preparing page for PDF…", "info"); }
      },
    });
    const html = await preparePrintHtml(prepared);
    if (!current()) return;
    iframe = document.createElement("iframe");
  iframe.setAttribute("aria-hidden", "true");
  // Keep same-origin DOM access so the parent can wait for fonts and invoke the
  // native print dialog, but categorically disable child scripts. The core also
  // emits script-src 'none'; neither graph markup nor a remote dependency can
  // reach Tauri's privileged parent/IPC surface.
  iframe.setAttribute("sandbox", PRINT_IFRAME_SANDBOX);
  // Off-screen + hidden: the print engine paginates the document at page width
  // regardless of the iframe's on-screen box, so a 0-size hidden frame prints fine.
  iframe.style.cssText =
    "position:fixed;right:0;bottom:0;width:0;height:0;border:0;visibility:hidden";
  iframe.srcdoc = html;

    const frame = iframe;
    await new Promise<void>((resolve) => {
      const cleanup = () => {
        frame.remove();
        controller.signal.removeEventListener("abort", cleanup);
        resolve();
      };
      controller.signal.addEventListener("abort", cleanup, { once: true });
      frame.onload = async () => {
    const win = frame.contentWindow;
    if (!win) {
      cleanup();
      return;
    }
    try {
      // Let the locally bundled styles/fonts settle so pagination measures the
      // final, already-typeset static layout.
      const fonts = frame.contentDocument?.fonts;
      if (fonts?.ready) await fonts.ready;
      await new Promise((r) => setTimeout(r, 400));
      if (!current()) { cleanup(); return; }
      win.addEventListener("afterprint", cleanup, { once: true });
      win.focus();
      win.print();
      // Fallback: if the engine never fires afterprint (or the user cancels without
      // one), reclaim the frame after a minute.
    } catch (e) {
      pushToast("Print failed", "error");
      console.error("iframe print failed", e);
      cleanup();
    }
      };
      if (!current()) { cleanup(); return; }
      timer = setTimeout(cleanup, 60_000);
      document.body.appendChild(frame);
    });
  } catch (error) {
    if (error instanceof OperationCancelledError || !current()) return;
    pushToast(error instanceof Error ? error.message : `Couldn't prepare “${name}” for PDF`, "error");
    console.error("pagePrintHtml failed", failureShape(error));
  } finally {
    iframe?.remove();
    if (timer) clearTimeout(timer);
    unbind();
    window.removeEventListener("pagehide", abort);
    window.removeEventListener("beforeunload", abort);
    if (activePrint === controller) activePrint = null;
  }
}
