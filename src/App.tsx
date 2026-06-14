import { createSignal, onMount, type JSX } from "solid-js";
import { Sidebar } from "./components/Sidebar";
import { PageView } from "./components/Page";
import { backend } from "./backend";

export function App(): JSX.Element {
  const [theme, setTheme] = createSignal<"light" | "dark">("light");

  onMount(() => {
    // The Tauri shell passes the graph path; in browser/mock this is ignored.
    const graphPath = (window as any).__GRAPH_PATH__ ?? "";
    void backend().loadGraph(graphPath);
  });

  const toggleTheme = () => {
    const next = theme() === "light" ? "dark" : "light";
    setTheme(next);
    document.documentElement.setAttribute("data-theme", next);
  };

  return (
    <div class="app-container">
      <div class="left-sidebar">
        <Sidebar />
      </div>
      <div class="main-container">
        <header class="topbar">
          <div class="topbar-left" />
          <div class="topbar-right">
            <button class="icon-btn" title="Toggle theme" onClick={toggleTheme}>
              <svg viewBox="0 0 24 24" class="nav-icon">
                <circle cx="12" cy="12" r="5" fill="none" stroke="currentColor" stroke-width="1.6" />
                <line x1="12" y1="2" x2="12" y2="5" stroke="currentColor" stroke-width="1.6" />
                <line x1="12" y1="19" x2="12" y2="22" stroke="currentColor" stroke-width="1.6" />
                <line x1="2" y1="12" x2="5" y2="12" stroke="currentColor" stroke-width="1.6" />
                <line x1="19" y1="12" x2="22" y2="12" stroke="currentColor" stroke-width="1.6" />
              </svg>
            </button>
          </div>
        </header>
        <main class="main-content">
          <div class="main-content-inner">
            <PageView />
          </div>
        </main>
      </div>
    </div>
  );
}
