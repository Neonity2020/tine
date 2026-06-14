// Backend abstraction. In the Tauri app we call Rust via `invoke`. In a plain
// browser (Vite dev / Playwright screenshots) we fall back to an in-memory mock
// seeded from a fixture graph, so the whole UI is exercisable without the shell.

import type { GraphMeta, PageDto, PageEntry } from "./types";
import { mockBackend } from "./mock";

export interface Backend {
  loadGraph(path: string): Promise<GraphMeta>;
  listPages(): Promise<PageEntry[]>;
  journalsDesc(limit: number, offset: number): Promise<PageDto[]>;
  getPage(name: string, kind: "journal" | "page"): Promise<PageDto | null>;
  savePage(page: PageDto): Promise<void>;
}

function isTauri(): boolean {
  return typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
}

class TauriBackend implements Backend {
  private invoke!: <T>(cmd: string, args?: Record<string, unknown>) => Promise<T>;
  private ready: Promise<void>;

  constructor() {
    this.ready = import("@tauri-apps/api/core").then((m) => {
      this.invoke = m.invoke;
    });
  }

  private async call<T>(cmd: string, args?: Record<string, unknown>): Promise<T> {
    await this.ready;
    return this.invoke<T>(cmd, args);
  }

  loadGraph(path: string) {
    return this.call<GraphMeta>("load_graph", { path });
  }
  listPages() {
    return this.call<PageEntry[]>("list_pages");
  }
  journalsDesc(limit: number, offset: number) {
    return this.call<PageDto[]>("journals_desc", { limit, offset });
  }
  getPage(name: string, kind: "journal" | "page") {
    return this.call<PageDto | null>("get_page", { name, kind });
  }
  savePage(page: PageDto) {
    return this.call<void>("save_page", { page });
  }
}

let _backend: Backend | null = null;

export function backend(): Backend {
  if (!_backend) {
    _backend = isTauri() ? new TauriBackend() : mockBackend();
  }
  return _backend;
}
