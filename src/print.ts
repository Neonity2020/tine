// Print / export-to-PDF: render a whole page to a self-contained HTML document in
// the Rust core (assets inlined, no sidebar — see publish::page_print_html), drop
// it into a hidden same-origin <iframe>, and print that frame. The user's native
// print dialog (WebKitGTK / WebView2 / WKWebView) then offers "Save as PDF".
//
// Why an iframe and not window.print() on the live app: the editor virtualizes
// blocks (only the on-screen ones are in the DOM), so printing the live page would
// drop most of a long page. The core-rendered document is complete and unstyled by
// the app chrome, so the PDF is the page, nothing else.
import { backend } from "./backend";
import { pushToast } from "./ui";
import type { PrintOpts } from "./types";

/** The default export options (match the Rust `PrintOpts::default`). */
export const DEFAULT_PRINT_OPTS: PrintOpts = {
  expand_collapsed: true,
  font_px: 16,
  margin_mm: 16,
};

/** Export a page to PDF via the OS print dialog. Safe to call repeatedly. */
export async function exportPagePdf(name: string, opts: PrintOpts = DEFAULT_PRINT_OPTS): Promise<void> {
  let html: string;
  try {
    html = await backend().pagePrintHtml(name, opts);
  } catch (e) {
    // `no-page` (deleted mid-action) or any core error — never leave a dangling frame.
    pushToast(`Couldn't prepare “${name}” for PDF`, "error");
    console.error("pagePrintHtml failed", e);
    return;
  }

  const iframe = document.createElement("iframe");
  iframe.setAttribute("aria-hidden", "true");
  // Off-screen + hidden: the print engine paginates the document at page width
  // regardless of the iframe's on-screen box, so a 0-size hidden frame prints fine.
  iframe.style.cssText =
    "position:fixed;right:0;bottom:0;width:0;height:0;border:0;visibility:hidden";
  iframe.srcdoc = html;

  let done = false;
  const cleanup = () => {
    if (done) return;
    done = true;
    iframe.remove();
  };

  iframe.onload = async () => {
    const win = iframe.contentWindow;
    if (!win) {
      cleanup();
      return;
    }
    try {
      // Let webfonts + KaTeX/highlight.js (loaded from CDN inside the frame) settle
      // so pagination measures the final, typeset layout. Best-effort; offline just
      // prints the raw TeX / plain code, same as a published page.
      const fonts = iframe.contentDocument?.fonts;
      if (fonts?.ready) await fonts.ready;
      await new Promise((r) => setTimeout(r, 400));
      win.addEventListener("afterprint", cleanup, { once: true });
      win.focus();
      win.print();
      // Fallback: if the engine never fires afterprint (or the user cancels without
      // one), reclaim the frame after a minute.
      setTimeout(cleanup, 60_000);
    } catch (e) {
      pushToast("Print failed", "error");
      console.error("iframe print failed", e);
      cleanup();
    }
  };

  document.body.appendChild(iframe);
}
