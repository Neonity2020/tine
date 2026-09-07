// **Geometry jsdom does not have, for the picker's viewport only (N2).**
//
// `@tanstack/solid-virtual` decides which rows to mount from the scroll
// element's height and its scroll offset. jsdom has no layout: every element is
// 0×0, so the virtualizer sees a zero-height window and mounts overscan alone —
// which would make a windowing test prove that nine rows exist and nothing else,
// forever, and would make "scroll to a distant entry" unprovable.
//
// So the geometry is stubbed HERE, in the tests, and only for elements carrying
// the picker's own viewport class. Two rules this file exists to keep:
//
//  - **Nothing in production branches on being under test.** The row height the
//    virtualizer estimates is `VOCABULARY_ROW_HEIGHT`, a production constant,
//    in the test and in the app alike. There is no `isTest`, no
//    non-virtualized fallback and no debug global: a windowing bug that only
//    appears when the list is really virtualized is exactly the bug worth
//    catching.
//  - **jsdom proves selection, windowing and mounting. It does not prove
//    layout.** What the picker actually looks like is proved by screenshots
//    (`scripts/shot-query-vocabulary.mjs`), which is the only place a pixel is
//    an oracle.

const VIEWPORT_CLASS = "qs-vocab-viewport";

type Restore = () => void;

/** Give the picker's viewport a real height, a live `scrollTop` and a `scroll`
 *  event, for the duration of one test. Everything else keeps jsdom's own
 *  behaviour, so a stub here cannot quietly change what another component
 *  measures. Call the returned function in `afterEach`. */
export function stubVocabularyGeometry(height = 320, width = 320): Restore {
  const isViewport = (element: Element) => element.classList?.contains(VIEWPORT_CLASS);
  const proto = HTMLElement.prototype as unknown as Record<string, unknown>;

  const priorRect = HTMLElement.prototype.getBoundingClientRect;
  HTMLElement.prototype.getBoundingClientRect = function stubbed(this: HTMLElement): DOMRect {
    if (!isViewport(this)) return priorRect.call(this);
    return {
      x: 0,
      y: 0,
      top: 0,
      left: 0,
      right: width,
      bottom: height,
      width,
      height,
      toJSON: () => ({}),
    } as DOMRect;
  };

  const offsets = new WeakMap<HTMLElement, number>();
  const priorScrollTop = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "scrollTop");
  Object.defineProperty(HTMLElement.prototype, "scrollTop", {
    configurable: true,
    get(this: HTMLElement) {
      if (!isViewport(this)) return priorScrollTop?.get?.call(this) ?? 0;
      return offsets.get(this) ?? 0;
    },
    set(this: HTMLElement, next: number) {
      if (!isViewport(this)) {
        priorScrollTop?.set?.call(this, next);
        return;
      }
      offsets.set(this, next);
      this.dispatchEvent(new Event("scroll"));
    },
  });

  const priorClientHeight = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "clientHeight");
  const sized = (name: string, value: number, prior: PropertyDescriptor | undefined) =>
    Object.defineProperty(HTMLElement.prototype, name, {
      configurable: true,
      get(this: HTMLElement) {
        if (!isViewport(this)) return prior?.get?.call(this) ?? 0;
        return value;
      },
    });
  sized("clientHeight", height, priorClientHeight);
  const priorOffsetHeight = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetHeight");
  sized("offsetHeight", height, priorOffsetHeight);
  const priorOffsetWidth = Object.getOwnPropertyDescriptor(HTMLElement.prototype, "offsetWidth");
  sized("offsetWidth", width, priorOffsetWidth);

  const restoreResizeObserver = stubResizeObserver();

  return () => {
    HTMLElement.prototype.getBoundingClientRect = priorRect;
    for (const [name, prior] of [
      ["scrollTop", priorScrollTop],
      ["clientHeight", priorClientHeight],
      ["offsetHeight", priorOffsetHeight],
      ["offsetWidth", priorOffsetWidth],
    ] as const) {
      if (prior) Object.defineProperty(HTMLElement.prototype, name, prior);
      else delete proto[name];
    }
    restoreResizeObserver();
  };
}

/** A `ResizeObserver` for environments that have none. It reports the stubbed
 *  rect once per observed element, which is what makes the virtualizer pick the
 *  height up after it attaches to a viewport that already had one. */
export function stubResizeObserver(): Restore {
  const target = globalThis as unknown as { ResizeObserver?: unknown };
  if (target.ResizeObserver) return () => {};
  class Stub {
    constructor(private readonly callback: (entries: unknown[]) => void) {}
    observe(element: Element): void {
      this.callback([{ target: element, contentRect: element.getBoundingClientRect() }]);
    }
    unobserve(): void {}
    disconnect(): void {}
  }
  target.ResizeObserver = Stub;
  return () => {
    delete target.ResizeObserver;
  };
}
