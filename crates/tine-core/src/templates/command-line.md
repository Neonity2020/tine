icon:: ⌨️

- # Command line
	- The installed desktop program also has a small command-line interface. Run `tine --help` for the current command list and `tine --version` (or `tine version`) for the exact build version.
- ## Open Tine
	- `tine open GRAPH` opens a graph. The older shorthand `tine GRAPH` remains supported.
	- `tine capture` opens Quick Capture. The older `tine --capture` spelling remains supported, so existing desktop shortcuts keep working.
- ## Publish a graph
	- `tine export static GRAPH` writes the static HTML publication. `tine export live GRAPH` writes the read-only Tine app and keeps the static HTML site as its no-JavaScript and `file://` fallback.
	- Publication follows the graph's existing privacy settings: only pages with `public:: true` are included, unless `:publishing/all-pages-public?` is enabled. Add `--all-pages` to override that selection for one export.
	- Output defaults to `publish/` inside the graph. Use `--output exports/site` for another graph-relative directory. Tine will not overwrite an existing output unless you add `--replace`; replaced output is retained in Tine's recovery area.
	- A live export chooses the configured home page when it is public, then **Welcome to Tine**, then the first public page. Use `--home "Page name"` or `--name "Site name"` to choose explicitly.
	- Example: `tine export live ~/notes --all-pages --output public-site --name "My notes"`
- ## Check before publishing
	- `tine doctor GRAPH` reads and parses the graph, reports duplicate page identities or unreadable pages, and does not change the graph.
- ## Full reference
	- Every command and subcommand has `--help`, for example `tine export live --help`. Linux `.deb` and `.rpm` packages also install the same generated reference for `man tine`.
