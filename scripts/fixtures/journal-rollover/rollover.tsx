import { render } from "solid-js/web";
import { PageView } from "../../../src/components/Page";
import { backend } from "../../../src/backend";
import { graphBindingRuntime } from "../../../src/graphBindingRuntime";
import { initParser } from "../../../src/render/parse";
import { journalTitle, localDayKey } from "../../../src/journal";
import { doc, setRaw } from "../../../src/store";
import { editingId, startEditing } from "../../../src/editorController";
import "../../../src/styles/app.css";

await initParser();
graphBindingRuntime.bind(1, { binding_generation: 1 });
const oldTitle = journalTitle(new Date());
let reads = 0;
backend().journalFeedPage = async () => {
  reads++;
  return {
    pages: [{ name: oldTitle, title: oldTitle, kind: "journal", pre_block: null,
      blocks: [{ id: "overnight-block", raw: "existing notes", collapsed: false, children: [] }] }],
    as_of_day: localDayKey(), next_before_day: null, done: true,
  };
};
render(() => <PageView />, document.getElementById("root")!);
Object.assign(window, { rollover: { doc, editingId, startEditing, setRaw, reads: () => reads } });
