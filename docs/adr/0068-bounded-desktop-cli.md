# 0068. The desktop binary exposes a bounded, scriptable CLI

**Status:** Accepted (Martin, 2026-09-20)

## Context

The desktop executable accepted a graph path, `--capture`, and `--debug`, but
silently treated most terminal input as a GUI launch. It could not identify its
version, explain its arguments, or run the existing static and live publishers
without opening a webview. Ad hoc flags for every operation would duplicate
argument interpretation across cold launch, single-instance forwarding, and
future packaging documentation.

## Decision

One typed command schema owns parsing, help, version output, the generated man
page, cold GUI launch, and forwarded second-instance launch:

- `tine open GRAPH` and `tine capture` name the existing GUI actions;
- `tine export static GRAPH` and `tine export live GRAPH` run headlessly;
- `tine doctor GRAPH` performs read-only discovery, parse, and identity checks;
- `tine --help`, `tine --version`, `tine help`, and `tine version` are terminal
  operations;
- `tine GRAPH`, `tine --capture`, and `tine --debug` remain compatible.

Exports preserve the publication capability boundary. `--output` is a
graph-relative directory, installation is create-only by default, and
`--replace` retires the previous occupant through the existing publication
recovery protocol. `--all-pages` changes selection only in memory for that run.
The live exporter takes its frontend assets from the invoking release binary,
so its snapshot and app shell are one build.

The initial surface excludes a long-running server and every graph mutation or
repair command. Those operations need lifecycle, failure, and data-safety
contracts of their own. Query-specific export remains in the reviewed in-app
flow until a CLI can express the same scope and confirmation.

## Consequences

Scripts get stable exit codes and composable exports without starting the GUI.
The desktop binary gains a small parser dependency. Linux `.deb` and `.rpm`
packages install the generated manual; every platform retains `--help` as the
canonical reference. Adding a command now requires changing the schema and its
behavior together, and the checked-in manual fails tests when it drifts.

