import type { JSX } from "solid-js";
import { backend } from "../backend";
import { pushToast } from "../ui";
import type { SpanDomAttrs } from "../render/spans";

export interface ExternalLinkProps {
  dest: string;
  class?: string;
  target?: string;
  rel?: string;
  attrs?: SpanDomAttrs;
  open?: () => Promise<unknown> | unknown;
  children: JSX.Element;
}

/**
 * The single outbound-link boundary for graph-authored content.
 *
 * The href stays present for ordinary presentation and copy affordances, but
 * navigation is always prevented and routed through the native boundary. That
 * boundary owns the file/http(s)/mailto scheme allowlist; components must not
 * grow a second scheme parser.
 */
export function ExternalLink(props: ExternalLinkProps): JSX.Element {
  return (
    <a
      class={props.class ?? "external-link"}
      href={props.dest}
      target={props.target}
      rel={props.rel}
      {...(props.attrs ?? {})}
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        const opened = props.open?.() ?? backend().openExternal(props.dest);
        void Promise.resolve(opened).catch((error) => reportLinkOpenFailure(props.dest, error));
      }}
    >
      {props.children}
    </a>
  );
}

/** Make every refused/failed outbound action visible to the initiating user. */
export function reportLinkOpenFailure(dest: string, error: unknown): void {
  pushToast(`Couldn't open ${dest}. (${String(error)})`, "error");
}
