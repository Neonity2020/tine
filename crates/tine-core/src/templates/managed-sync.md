icon:: 🔄

- # Managed storage & sync
	- **Known to be buggy.** Tine-managed storage does not yet fully work in our own testing; we're actively working on it. Use it only on a graph you are comfortable testing. Your normal mode is **Direct files**: Tine reads and writes your existing Markdown or Org graph, and Syncthing, Dropbox, Logseq, and other tools can keep synchronizing those files exactly as before. Nothing changes unless you explicitly enable this mode for one graph.
- ## What the opt-in does
	- Tine stores operation history in that graph's `.tine-sync/` directory while keeping your Markdown/Org files in place as an editable projection. Logseq and other file-based tools can still read those files; Tine conservatively reconciles outside edits.
	- This is a separate mode, not a replacement for ordinary Syncthing/Dropbox. Do not enable it merely because you already synchronize your graph folders.
- ## Current scope
	- Managed storage currently covers **page and journal text** only. Assets, PDF sidecars, and configuration remain ordinary provider-synchronized files.
	- Setup first makes a safety backup and checks that Tine can reconstruct the same Markdown/Org tree. If that cannot be proven, it stops instead of changing graph authority.
- ## Try it on a test graph
	- 1. Open Settings (**t s**) → **Backups & recovery** → **Storage & sync**.
	- 2. Read the **Known to be buggy** warning, then choose **Enable Tine-managed storage...** for a graph you are comfortable testing.
	- 3. Wait for setup to finish. Your Markdown/Org files remain beside the new `.tine-sync/` data; do not edit the internal sync directory yourself.
	- 4. To prepare a second test device, first let your existing provider deliver the same test graph folder there. Then use **Set up sync with another device...** on the first device and **Join this synced graph...** on the other one. Tine verifies that both devices are joining the same graph history.
	- 5. What you should see: Direct files remains available for ordinary graphs, and managed storage is enabled only for the graph where you chose it.
- ## Return to Direct files
	- The same panel offers **Return to Direct files** when Tine can first preserve complete recovery state. If it says that safety could not be verified, leave the graph alone and resolve that state rather than forcing a file-mode switch.
