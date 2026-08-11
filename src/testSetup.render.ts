import { beforeEach } from "vitest";
import { delegateEvents, DelegatedEvents } from "solid-js/web";
import { managedStorageRuntime } from "./managedStorageRuntime";

beforeEach(() => {
  // Component fixtures exercise a loaded legacy graph unless they explicitly
  // install a managed route. A missing production route is fail-closed.
  managedStorageRuntime.bind(1, { binding_generation: 1, authority: "direct" });
  delegateEvents([...DelegatedEvents], document);
  // jsdom does not implement this browser method. Install the same no-op
  // boundary before every test because focused viewer tests may replace and
  // remove it while cleaning up their own spies.
  if (typeof HTMLElement.prototype.scrollIntoView !== "function") {
    HTMLElement.prototype.scrollIntoView = () => {};
  }
});
