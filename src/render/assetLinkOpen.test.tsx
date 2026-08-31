import { describe, it, expect, beforeAll, afterEach, vi } from "vitest";
import { render } from "solid-js/web";
import { InlineText } from "./inline";
import { initParser } from "./parse";
import { backend } from "../backend";
import { setToasts, toasts } from "../ui";

// GH #367: a labeled link into `assets/` (`[image](./assets/quick-capture.png)`)
// is not an external URL — clicking it must reach the OS opener for that asset
// (file: system viewer; directory/empty path: file manager), while a genuine
// http(s) link must keep routing to the URL opener.

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
});

function mountLink(raw: string, format: "md" | "org" = "md"): { host: HTMLElement; dispose: () => void } {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const dispose = render(() => <InlineText text={raw} format={format} />, host);
  return { host, dispose: () => { dispose(); host.remove(); } };
}

function click(host: HTMLElement): void {
  const a = host.querySelector("a.external-link");
  expect(a, "expected a rendered fallback link").not.toBeNull();
  a!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
}

describe("asset link opens through the OS opener", () => {
  it("a labeled link to an asset file opens the asset, not an external URL", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[image](./assets/quick-capture.png)");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("quick-capture.png");
      expect(openExternal).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });

  it("the empty asset path `[path](./assets/)` opens the assets directory", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[path](./assets/)");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("");
    } finally {
      dispose();
    }
  });

  it("the bare `./assets` root (no trailing slash) also opens the directory", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[path](./assets)");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("");
    } finally {
      dispose();
    }
  });

  it("a directory link resolves the nested directory, decoded", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[folder](./assets/some%20dir)");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("some dir");
    } finally {
      dispose();
    }
  });

  it("nested, spaced and Unicode paths are decoded before opening", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[notes](./assets/some%20dir/%E6%8A%A5%E8%A1%A8.md)");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("some dir/报表.md");
    } finally {
      dispose();
    }
  });

  it("an Org link into assets follows the same opener route", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[[../assets/quick-capture.png][image]]", "org");
    try {
      click(host);
      expect(openAsset).toHaveBeenCalledWith("quick-capture.png");
    } finally {
      dispose();
    }
  });

  it("an https URL that merely contains assets/ stays an external URL", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[doc](https://example.com/assets/a.docx)");
    try {
      click(host);
      expect(openExternal).toHaveBeenCalledWith("https://example.com/assets/a.docx");
      expect(openAsset).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });

  it("a non-asset relative link is untouched by the asset route", () => {
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[x](../journals/2026_08_23.md)");
    try {
      click(host);
      expect(openAsset).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });
});

describe("remote PDF URLs are external links, not the PDF viewer (GH #442)", () => {
  const expectExternalPdf = (host: HTMLElement, url: string) => {
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const a = host.querySelector("a.external-link");
    expect(a, "expected a rendered external link").not.toBeNull();
    expect(a!.classList.contains("pdf-link"), "a remote PDF URL must not render as the in-app viewer chip").toBe(false);
    click(host);
    expect(openExternal).toHaveBeenCalledWith(url);
  };

  it("a labeled https PDF link opens in the browser, exactly like other remote links", () => {
    const url = "https://aclanthology.org/2025.acl-long.879.pdf";
    const { host, dispose } = mountLink(`[a paper](${url})`);
    try {
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("a bare https PDF URL stays an external URL too", () => {
    const url = "https://example.org/papers/summary.pdf";
    const { host, dispose } = mountLink(url);
    try {
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("an image-syntax remote PDF is also an external link, not an image or the viewer", () => {
    const url = "https://example.org/papers/figure.pdf";
    const { host, dispose } = mountLink(`![figure](${url})`);
    try {
      expect(host.querySelector("img.inline-image"), "a remote .pdf URL is not an embeddable image").toBeNull();
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("an Org remote PDF link follows the same external route", () => {
    const url = "https://example.org/2026.pdf";
    const { host, dispose } = mountLink(`[[${url}][paper]]`, "org");
    try {
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("a plain http (not https) PDF URL is just as remote", () => {
    const url = "http://example.org/old/paper.pdf";
    const { host, dispose } = mountLink(`[paper](${url})`);
    try {
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("an uppercase .PDF suffix on a remote URL is still external", () => {
    const url = "https://example.org/REPORT.PDF";
    const { host, dispose } = mountLink(`[report](${url})`);
    try {
      expectExternalPdf(host, url);
    } finally {
      dispose();
    }
  });

  it("graph asset PDFs keep entering the in-app viewer (unchanged local routing)", () => {
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink("[paper](../assets/paper.pdf)");
    try {
      const a = host.querySelector("a.external-link.pdf-link");
      expect(a, "a local asset PDF must keep its in-app viewer chip").not.toBeNull();
      a!.dispatchEvent(new MouseEvent("click", { bubbles: true, cancelable: true }));
      expect(openExternal).not.toHaveBeenCalled();
      expect(openAsset).not.toHaveBeenCalled();
    } finally {
      dispose();
    }
  });
});

// GH #444: `file:` links written by Logseq (`file://D:\test.txt`) and Obsidian
// (`<file:///D:\test.txt>`) rendered as links but did nothing at all when
// clicked — the backend refused every scheme but http/https/mailto, and the
// frontend discarded the rejection. Both halves are covered here: the link must
// reach `openExternal` with the destination as written, and a refusal must
// become a visible message instead of silence.
describe("local file: links (GH #444)", () => {
  const expectOpensExternally = (raw: string) => {
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const openAsset = vi.spyOn(backend(), "openAsset").mockResolvedValue(undefined);
    const { host, dispose } = mountLink(raw);
    try {
      click(host);
      expect(openAsset).not.toHaveBeenCalled();
      expect(openExternal).toHaveBeenCalledTimes(1);
      return String(openExternal.mock.calls[0][0]);
    } finally {
      dispose();
    }
  };

  it("the Logseq shape the reporter used reaches the opener, backslashes intact", () => {
    expect(expectOpensExternally("[Test](file://D:\\test.txt)")).toBe("file://D:\\test.txt");
  });

  it("the Obsidian angle-bracket shape reaches the opener too", () => {
    expect(expectOpensExternally("[Test](<file:///D:\\test.txt>)")).toBe("file:///D:\\test.txt");
  });

  it("a POSIX path and a directory are the same route", () => {
    expect(expectOpensExternally("[notes](file:///home/user/notes.txt)")).toBe("file:///home/user/notes.txt");
    expect(expectOpensExternally("[folder](file:///home/user/notes/)")).toBe("file:///home/user/notes/");
  });

  it("a percent-escaped filename is handed over untouched, for the backend to decode", () => {
    expect(expectOpensExternally("[spaced](file:///home/user/a%20b.txt)")).toBe("file:///home/user/a%20b.txt");
  });

  it("a file: link is never mistaken for a graph asset", () => {
    expect(expectOpensExternally("[x](file:///graph/assets/a.png)")).toBe("file:///graph/assets/a.png");
  });

  it("says so when the link cannot be opened, instead of doing nothing visible", async () => {
    setToasts([]);
    vi.spyOn(backend(), "openExternal").mockRejectedValue("that file link does not name a local file");
    const { host, dispose } = mountLink("[broken](file://not-a-path)");
    try {
      click(host);
      await vi.waitFor(() => expect(toasts()).toHaveLength(1));
      expect(toasts()[0].kind).toBe("error");
      expect(toasts()[0].message).toContain("file://not-a-path");
    } finally {
      setToasts([]);
      dispose();
    }
  });
});
