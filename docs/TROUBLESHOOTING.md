# Troubleshooting

For a problem that occurs after the window opens, use **Settings → Diagnostics**.
Tine keeps a small, bounded record of fixed operation names, outcomes, timings
and counts for the current and previous run. You can preview the complete JSON,
then choose whether to copy or save it. Nothing is uploaded automatically, and
the report excludes graph content, file paths, page titles, queries, URLs and
credentials.

For a bad startup or a directed investigation, debug mode provides a more
detailed and less privacy-restricted trace. If Tine won't start cleanly (e.g. the
window never appears), run it with debug logging on. It writes a timestamped trace to a file — the environment (renderer, session type, AppImage, graph), every
startup milestone, any panic (with backtrace), and the frontend's own boot/errors — so one file is
usually enough to diagnose it:

```bash
TINE_DEBUG=1 tine                 # or:  tine --debug
TINE_DEBUG=1 ./Tine-*.AppImage    # AppImage
```

Tine prints the log path on startup; it defaults to `/tmp/tine-debug.log`
(override with `TINE_DEBUG_LOG=/path`). Reproduce the problem, inspect the file,
then send it only if you choose. This detailed file can contain paths,
environment values and error text; it is never included in the privacy-safe
report.

## Graphics problems on Linux

- **GPU compositing (smooth scrolling) is on by default.** On the rare GPU/compositor combo where
  WebKitGTK's DMABUF renderer aborts (the window fails to appear, or you see
  `EGL_BAD_PARAMETER` on the console), set `TINE_GPU=0` to fall back to software rendering — slower,
  but it always starts. If Tine detects it's painting on the CPU it says so with a banner.
- Prefer the **raw binary** over an AppImage on Linux — an AppImage's bundled graphics libraries can
  clash with the host GPU and silently drop you to (slow) software rendering. The `.deb`/`.rpm`
  packages use your system's drivers and don't have this problem.
