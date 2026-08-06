import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { initParser } from "../render/parse";
import { resetStore, type FeedPage } from "../store";
import { journalTitle, localDateFromDayKey, localDayKey, setCurrentDayKeyForTest } from "../journal";
import { CarryActions } from "./Page";

// Midnight rollover: the carry buttons under a journal title must swap when
// the calendar day changes. Before the fix the today/past-day choice was
// computed once at mount from a bare `new Date()`, so when midnight rolled
// over, yesterday's journal kept today's pull-in buttons ("Carry last N days")
// instead of switching to the push-to-today button.
beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  setCurrentDayKeyForTest(localDayKey()); // wall clock, in case a test advanced it
  resetStore();
  document.body.innerHTML = "";
});

function journalPage(name: string): FeedPage {
  return { name, kind: "journal", title: name, preBlock: null, roots: [], format: "md", readOnly: false, guide: false };
}
function namedPage(name: string): FeedPage {
  return { name, kind: "page", title: name, preBlock: null, roots: [], format: "md", readOnly: false, guide: false };
}

function mount(page: FeedPage) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  return { host, dispose: render(() => <CarryActions page={page} />, host) };
}

const text = (host: HTMLElement) => host.textContent ?? "";

describe("carry buttons at calendar rollover", () => {
  it("today's journal shows the pull-in buttons", () => {
    const m = mount(journalPage(journalTitle(new Date())));
    try {
      expect(text(m.host)).toContain("Carry from previous day");
      expect(text(m.host)).toContain("Carry last");
      expect(text(m.host)).not.toContain("Carry unfinished tasks → today");
    } finally {
      m.dispose();
    }
  });

  it("a past journal shows only the push-to-today button", () => {
    const past = journalTitle(new Date(Date.now() - 86400000));
    const m = mount(journalPage(past));
    try {
      expect(text(m.host)).toContain("Carry unfinished tasks → today");
      expect(text(m.host)).not.toContain("Carry from previous day");
    } finally {
      m.dispose();
    }
  });

  it("when midnight rolls over, the old today swaps pull-in buttons for push-to-today", () => {
    const todayName = journalTitle(new Date());
    const m = mount(journalPage(todayName));
    try {
      expect(text(m.host)).toContain("Carry last");
      // Simulate local calendar rollover while the app stays open.
      setCurrentDayKeyForTest(localDayKey() + 1);
      expect(text(m.host)).toContain("Carry unfinished tasks → today");
      expect(text(m.host)).not.toContain("Carry from previous day");
      expect(text(m.host)).not.toContain("Carry last");
    } finally {
      m.dispose();
    }
  });

  it("and the NEW day gains the pull-in buttons", () => {
    // Simulate rollover first: the freshly-mounted current day (as created by
    // the midnight feed refresh in the app) must offer the pull-in buttons.
    setCurrentDayKeyForTest(localDayKey() + 1);
    const tomorrow = journalTitle(localDateFromDayKey(localDayKey() + 1));
    const m = mount(journalPage(tomorrow));
    try {
      expect(text(m.host)).toContain("Carry from previous day");
      expect(text(m.host)).toContain("Carry last");
      expect(text(m.host)).not.toContain("Carry unfinished tasks → today");
    } finally {
      m.dispose();
    }
  });

  it("named pages show no carry buttons, before or after rollover", () => {
    const m = mount(namedPage("Plain Page"));
    try {
      expect(text(m.host)).not.toContain("Carry");
      setCurrentDayKeyForTest(localDayKey() + 1);
      expect(text(m.host)).not.toContain("Carry");
    } finally {
      m.dispose();
    }
  });
});
