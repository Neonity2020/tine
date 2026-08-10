//! Keep the app-data home usable, so a home directory we cannot write degrades
//! instead of killing the app at launch.
//!
//! Tauri creates the WebView user-data directory (`$XDG_DATA_HOME/<identifier>`)
//! while building the configured windows, inside its OWN `setup()` — see
//! `tauri::manager::webview`, which calls `create_dir_all` there. An IO error at
//! that point is not something the application can catch: Tauri panics with
//! `Failed to setup app: Permission denied (os error 13)` before Tine's setup
//! hook ever runs, so the launch dies with a backtrace and no usable message.
//!
//! Reproduced 2026-08-09 against a stock release binary with an unwritable
//! `$XDG_DATA_HOME`, which is the shape of the real report: a box where
//! `~/.local/share` is `drwxr-xr-t root root`. Note that Tine's own
//! desktop-identity step already reported the same `Permission denied` and
//! explicitly treated it as non-fatal — and the app then crashed anyway.
//!
//! So probe the directory before Tauri needs it and, when it is unusable, move
//! the data home to a writable fallback for this launch. Preferences, session
//! and the WebView store then live somewhere real instead of being lost to a
//! crash, and the frontend toasts once so the relocation is never silent.
//!
//! `XDG_DATA_HOME` is the single lever that redirects every consumer at once —
//! Tauri's path resolver, WebKitGTK's website-data manager, and Tine's own
//! settings — which is why the fallback is applied there rather than per
//! subsystem. It is Linux-shaped and deliberately desktop-Linux-only: Android
//! has its own sandboxed app-data dir, and no report exists for Windows/macOS.

#[cfg(all(desktop, target_os = "linux"))]
use crate::debug::diag;
#[cfg(all(desktop, target_os = "linux"))]
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Set to the fallback data home iff this launch had to relocate. Read and
/// cleared exactly once by `take_data_home_fallback_notice`. A process global
/// because the probe runs long before the Tauri app (and its managed state).
static RELOCATED_TO: Mutex<Option<String>> = Mutex::new(None);

/// Can we actually create and write files under `base`, the way Tauri and
/// WebKitGTK are about to? `create_dir_all` alone is not the question: it
/// succeeds on an existing directory nobody may write to, which is exactly the
/// reported case. Probe the app-data dir when it already exists (its own mode
/// is what matters then) and the base otherwise.
#[cfg(all(desktop, target_os = "linux"))]
fn probe_writable(base: &Path, identifier: &str) -> std::io::Result<()> {
    let app_dir = base.join(identifier);
    let target = if app_dir.is_dir() {
        app_dir
    } else {
        base.to_path_buf()
    };
    std::fs::create_dir_all(&target)?;
    let probe = target.join(format!(".tine-write-probe-{}", std::process::id()));
    std::fs::write(&probe, b"")?;
    std::fs::remove_file(&probe)?;
    Ok(())
}

/// Where to put the data home when the real one is unusable, best first.
///
/// The home directory itself comes first because it is the case actually
/// reported — a root-owned `~/.local/share` under a home the user owns — and it
/// is the only candidate that survives a reboot. `XDG_RUNTIME_DIR` is private
/// and 0700 but cleared at logout; the temp dir is the last resort and is
/// namespaced by the owner of the home directory so it cannot be squatted by
/// another account on a shared box.
#[cfg(all(desktop, target_os = "linux"))]
fn fallback_candidates() -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;

    let mut candidates = Vec::new();
    let home = dirs::home_dir();
    if let Some(home) = &home {
        candidates.push(home.join(".tine-data"));
    }
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").filter(|value| !value.is_empty()) {
        candidates.push(PathBuf::from(runtime).join("tine-data"));
    }
    let uid = home
        .as_ref()
        .and_then(|home| std::fs::metadata(home).ok())
        .map(|meta| meta.uid());
    if let Some(uid) = uid {
        candidates.push(std::env::temp_dir().join(format!("tine-data-{uid}")));
    }
    candidates
}

/// Probe the app-data home and relocate it for this launch if it cannot be
/// written. Call at the TOP of `lib::run()` — before `migrate_identifier`, and
/// necessarily before `tauri::Builder`, because Tauri resolves and creates the
/// directory itself while building the configured windows.
#[cfg(all(desktop, target_os = "linux"))]
pub(crate) fn ensure_usable(identifier: &str) {
    let Some(base) = dirs::data_dir() else {
        return;
    };
    let error = match probe_writable(&base, identifier) {
        Ok(()) => return,
        Err(error) => error,
    };
    diag(format!(
        "app-data home {} is not writable ({error}); looking for a fallback",
        base.display()
    ));
    for candidate in fallback_candidates() {
        if probe_writable(&candidate, identifier).is_err() {
            continue;
        }
        std::env::set_var("XDG_DATA_HOME", &candidate);
        diag(format!(
            "app-data home relocated to {} for this launch",
            candidate.display()
        ));
        *RELOCATED_TO
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(candidate.display().to_string());
        return;
    }
    // Everything is unwritable. Tauri is about to panic on exactly this error;
    // say what is wrong first, in a sentence a user can act on, instead of
    // handing them `Failed to setup app: Permission denied (os error 13)` and a
    // backtrace.
    diag(format!(
        "no writable app-data home; refusing to start (base {}: {error})",
        base.display()
    ));
    eprintln!(
        "Tine cannot start: it has nowhere to keep its application data.\n\
         Tried {} ({error}) and every fallback.\n\
         Fix the permissions on that directory (it is normally owned by you), \n\
         or set XDG_DATA_HOME to a directory you can write.",
        base.display()
    );
    std::process::exit(1);
}

#[cfg(not(all(desktop, target_os = "linux")))]
pub(crate) fn ensure_usable(_identifier: &str) {}

/// Command: return the fallback data home ONCE if this launch had to relocate,
/// then clear it so a reload does not re-toast. `None` on every ordinary launch.
#[tauri::command]
pub(crate) fn take_data_home_fallback_notice() -> Option<String> {
    RELOCATED_TO
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take()
}

#[cfg(all(test, desktop, target_os = "linux"))]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("tine-data-home-{}-{tag}", std::process::id()))
    }

    fn read_only(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    }

    fn writable(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_writable_base_probes_clean_and_leaves_nothing_behind() {
        let base = tmp("clean");
        std::fs::create_dir_all(&base).unwrap();
        probe_writable(&base, "page.tine.Tine").unwrap();
        let residue: Vec<_> = std::fs::read_dir(&base).unwrap().flatten().collect();
        assert!(residue.is_empty(), "probe left {residue:?} behind");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_unwritable_base_is_rejected_even_though_create_dir_all_succeeds() {
        let base = tmp("ro-base");
        std::fs::create_dir_all(&base).unwrap();
        read_only(&base);
        // The exact trap: this is what Tauri relies on, and it reports success.
        assert!(std::fs::create_dir_all(&base).is_ok());
        let error = probe_writable(&base, "page.tine.Tine").unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        writable(&base);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_unwritable_existing_app_dir_is_rejected_under_a_writable_base() {
        let base = tmp("ro-app");
        let app = base.join("page.tine.Tine");
        std::fs::create_dir_all(&app).unwrap();
        read_only(&app);
        assert!(probe_writable(&base, "page.tine.Tine").is_err());
        writable(&app);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn the_notice_is_delivered_once() {
        *RELOCATED_TO.lock().unwrap() = Some("/tmp/example".to_string());
        assert_eq!(
            take_data_home_fallback_notice(),
            Some("/tmp/example".to_string())
        );
        assert_eq!(take_data_home_fallback_notice(), None);
    }
}
