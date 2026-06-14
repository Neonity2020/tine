// Small global UI state: theme, left sidebar, and the quick-switcher modal.
import { createSignal } from "solid-js";

export const [theme, setTheme] = createSignal<"light" | "dark">("light");
export function toggleTheme() {
  const next = theme() === "light" ? "dark" : "light";
  setTheme(next);
  document.documentElement.setAttribute("data-theme", next);
}

export const [sidebarOpen, setSidebarOpen] = createSignal(true);
export function toggleSidebar() {
  setSidebarOpen(!sidebarOpen());
}

export const [switcherOpen, setSwitcherOpen] = createSignal(false);
export function openSwitcher() {
  setSwitcherOpen(true);
}
export function closeSwitcher() {
  setSwitcherOpen(false);
}

// The PDF currently open in the side pane (filename within assets/, + label,
// + an optional page to scroll to).
export const [pdfTarget, setPdfTarget] = createSignal<{
  filename: string;
  label: string;
  page?: number;
} | null>(null);
export function openPdf(filename: string, label: string, page?: number) {
  setPdfTarget({ filename, label, page });
}
export function closePdf() {
  setPdfTarget(null);
}
