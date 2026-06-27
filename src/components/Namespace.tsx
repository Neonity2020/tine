import { For, Show, createResource, createSignal, type JSX } from "solid-js";
import { backend } from "../backend";
import { openPage } from "../router";
import { graphEpoch } from "../ui";
import { EmojiText } from "../render/emoji";

// Namespace hierarchy for a page named `a/b/c`: a clickable breadcrumb of the
// ancestor namespaces (shown above the title) and a list of direct child pages
// (shown below the page). Mirrors OG's hierarchy component.

/** Breadcrumb of ancestor namespaces, e.g. for "a/b/c" → a › b (clickable). */
export function NamespaceCrumb(props: { name: string }): JSX.Element {
  const parts = () => props.name.split("/");
  return (
    <Show when={parts().length > 1}>
      <div class="ns-crumb">
        <For each={parts().slice(0, -1)}>
          {(_, i) => {
            const prefix = () => parts().slice(0, i() + 1).join("/");
            return (
              <>
                <span class="ns-crumb-item" onClick={() => openPage(prefix(), "page")}>
                  {parts()[i()]}
                </span>
                <span class="ns-crumb-sep">/</span>
              </>
            );
          }}
        </For>
      </div>
    </Show>
  );
}

// --- Sidebar namespace tree -------------------------------------------------

export interface NsNode {
  seg: string;
  full: string;
  children: NsNode[];
}

/** Build a nested namespace tree from page names containing `/`. Intermediate
 *  segments become nodes even if they have no file of their own. */
export function buildNamespaceTree(names: string[]): NsNode[] {
  const roots: NsNode[] = [];
  const byFull = new Map<string, NsNode>();
  for (const name of names) {
    if (!name.includes("/")) continue;
    let level = roots;
    let prefix = "";
    for (const seg of name.split("/")) {
      prefix = prefix ? `${prefix}/${seg}` : seg;
      let node = byFull.get(prefix.toLowerCase());
      if (!node) {
        node = { seg, full: prefix, children: [] };
        byFull.set(prefix.toLowerCase(), node);
        level.push(node);
      }
      level = node.children;
    }
  }
  const sortRec = (ns: NsNode[]) => {
    ns.sort((a, b) => a.seg.localeCompare(b.seg));
    ns.forEach((n) => sortRec(n.children));
  };
  sortRec(roots);
  return roots;
}

function NsNodeView(props: { node: NsNode; depth: number }): JSX.Element {
  const [open, setOpen] = createSignal(props.depth < 1);
  const has = () => props.node.children.length > 0;
  return (
    <div class="ns-node">
      <div class="ns-node-row" style={{ "padding-left": `${props.depth * 12}px` }}>
        <Show when={has()} fallback={<span class="ns-node-spacer" />}>
          <span class="ns-node-toggle" onClick={() => setOpen(!open())}>{open() ? "▾" : "▸"}</span>
        </Show>
        <span class="ns-node-label" onClick={() => openPage(props.node.full, "page")}>
          {props.node.seg}
        </span>
      </div>
      <Show when={has() && open()}>
        <For each={props.node.children}>{(c) => <NsNodeView node={c} depth={props.depth + 1} />}</For>
      </Show>
    </div>
  );
}

/** A collapsible tree of all namespaces in the graph, for the left sidebar. */
export function NamespaceTree(): JSX.Element {
  const [tree] = createResource(
    () => graphEpoch(),
    async () => buildNamespaceTree((await backend().listPages()).map((p) => p.name)),
  );
  return (
    <Show when={(tree() ?? []).length > 0}>
      <div class="ns-tree">
        <For each={tree()}>{(n) => <NsNodeView node={n} depth={0} />}</For>
      </div>
    </Show>
  );
}

// --- {{namespace X}} macro --------------------------------------------------

function collectFulls(nodes: NsNode[], acc: string[]) {
  for (const n of nodes) {
    acc.push(n.full);
    collectFulls(n.children, acc);
  }
}

function NsMacroNode(props: { node: NsNode; depth: number; icons: Record<string, string> }): JSX.Element {
  return (
    <div class="ns-macro-node">
      <div class="ns-macro-row" style={{ "padding-left": `${props.depth * 18}px` }}>
        <Show when={props.icons[props.node.full]}>
          <span class="page-icon">
            <EmojiText text={props.icons[props.node.full]} />
          </span>
        </Show>
        <a class="page-ref" onClick={(e) => { e.stopPropagation(); openPage(props.node.full, "page"); }}>
          <EmojiText text={props.node.seg} />
        </a>
      </div>
      <For each={props.node.children}>
        {(c) => <NsMacroNode node={c} depth={props.depth + 1} icons={props.icons} />}
      </For>
    </div>
  );
}

/** `{{namespace X}}` — the full nested descendant tree of namespace `X`, each
 *  page showing its `icon::` (like OG's namespace macro). */
export function NamespaceMacro(props: { root: string }): JSX.Element {
  const [data] = createResource(
    () => ({ r: props.root, e: graphEpoch() }),
    async ({ r }) => {
      const prefix = `${r}/`.toLowerCase();
      const names = (await backend().listPages())
        .map((p) => p.name)
        .filter((n) => n.toLowerCase().startsWith(prefix));
      const tree = buildNamespaceTree(names);
      const fulls: string[] = [];
      collectFulls(tree, fulls);
      const icons = fulls.length ? await backend().pageIcons(fulls) : {};
      return { tree, icons };
    }
  );
  return (
    <Show
      when={(data()?.tree ?? []).length > 0}
      fallback={<span class="macro">{`{{namespace ${props.root}}}`}</span>}
    >
      {/* OG renders a bold "Namespace " label + the root page link as a header
         (components/block.cljs `namespace-hierarchy`), then the descendant tree
         below it — so render the root's children, not the root, as the tree. */}
      <For each={data()!.tree}>
        {(root) => (
          <div class="ns-macro">
            <div class="ns-macro-head">
              <span class="ns-macro-label">Namespace</span>
              <Show when={data()!.icons[root.full]}>
                <span class="page-icon">
                  <EmojiText text={data()!.icons[root.full]} />
                </span>
              </Show>
              <a class="page-ref" onClick={(e) => { e.stopPropagation(); openPage(root.full, "page"); }}>
                <EmojiText text={root.seg} />
              </a>
            </div>
            <For each={root.children}>
              {(c) => <NsMacroNode node={c} depth={0} icons={data()!.icons} />}
            </For>
          </div>
        )}
      </For>
    </Show>
  );
}

/** OG's automatic "Hierarchy" section (components/hierarchy.cljs `structures`):
 *  rendered below any non-journal page that participates in a namespace. It lists
 *  breadcrumb PATHS — every transitive descendant page as a `/`-joined chain of
 *  clickable page links (each link targets the cumulative path), sorted by name.
 *  Matching OG's `get-relation`: for an existing page, `parent-routes` is empty,
 *  so the set is exactly the descendants; a namespaced LEAF (no descendants) shows
 *  one row — the path of its parent namespace. */
export function NamespaceHierarchy(props: { name: string }): JSX.Element {
  const [rows] = createResource(
    () => ({ n: props.name, e: graphEpoch() }),
    async ({ n }): Promise<string[][]> => {
      const all = (await backend().listPages()).map((p) => p.name);
      const prefix = `${n}/`.toLowerCase();
      const descendants = all
        .filter((name) => name.toLowerCase().startsWith(prefix))
        .sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
      if (descendants.length) return descendants.map((name) => name.split("/"));
      // Namespaced leaf with no descendants → the parent namespace's path.
      if (n.includes("/")) return [n.split("/").slice(0, -1)];
      return [];
    }
  );
  return (
    <Show when={(rows() ?? []).length > 0}>
      <div class="page-hierarchy">
        <div class="references-header">Hierarchy</div>
        <ul class="ns-hierarchy">
          <For each={rows()}>
            {(segs) => (
              <li>
                <For each={segs}>
                  {(seg, i) => {
                    const full = () => segs.slice(0, i() + 1).join("/");
                    return (
                      <>
                        <Show when={i() > 0}>
                          <span class="ns-hier-sep">/</span>
                        </Show>
                        <a
                          class="page-ref"
                          onClick={(e) => { e.stopPropagation(); openPage(full(), "page"); }}
                        >
                          <span class="bracket">[[</span>
                          {seg}
                          <span class="bracket">]]</span>
                        </a>
                      </>
                    );
                  }}
                </For>
              </li>
            )}
          </For>
        </ul>
      </div>
    </Show>
  );
}
