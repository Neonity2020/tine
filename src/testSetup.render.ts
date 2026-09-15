import { beforeEach } from "vitest";
import { delegateEvents, DelegatedEvents } from "solid-js/web";
import { graphBindingRuntime } from "./graphBindingRuntime";

beforeEach(() => {
  // Component fixtures exercise a bound Direct Files graph unless they
  // explicitly clear the binding. A missing binding is fail-closed.
  graphBindingRuntime.bind(1, { binding_generation: 1 });
  delegateEvents([...DelegatedEvents], document);
  // jsdom does not implement this browser method. Install the same no-op
  // boundary before every test because focused viewer tests may replace and
  // remove it while cleaning up their own spies.
  if (typeof HTMLElement.prototype.scrollIntoView !== "function") {
    HTMLElement.prototype.scrollIntoView = () => {};
  }
});
