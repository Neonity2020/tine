fn main() {
    // `safe-back` is an INLINED plugin — it lives in this crate rather than in
    // one of its own, so nothing generates an ACL manifest for it unless this
    // build script does. Without one, `plugin:safe-back|registerListener` is
    // refused before it reaches Android, the frontend's Back listener never
    // registers, and Tine's native owner consumes every gesture with nowhere
    // to send it. That is invisible from Rust and from the emulator alike.
    tauri_build::try_build(
        tauri_build::Attributes::new().plugin(
            "safe-back",
            tauri_build::InlinedPlugin::new()
                .commands(&["registerListener", "removeListener"])
                .default_permission(tauri_build::DefaultPermissionRule::AllowAllCommands),
        ),
    )
    .expect("failed to run tauri-build");
}
