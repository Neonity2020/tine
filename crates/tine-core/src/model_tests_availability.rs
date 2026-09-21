//! Graph availability on unusual storage: rename-flag refusal (GH #538) and
//! non-page entries inside the configured roots (GH #385).

use super::*;

/// Storage that refuses `renameat2(RENAME_NOREPLACE)` the way some Android
/// shared storage does (GH #538): EINVAL for every flagged rename.
fn flag_refusing_mover(_src: &Path, _dest: &Path) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(22))
}

#[test]
fn gh538_a_refused_no_replace_rename_leaves_config_edn_in_place() {
    // Before the fix the live file was retired first and the refused publish
    // AND the refused undo left it stranded as `.config.edn.*.retired`.
    let dir = scratch("gh538-refused-publish");
    let path = dir.join("config.edn");
    fs::write(&path, b"{:base 1}\n").unwrap();

    let result = atomic_replace_expected_with_mover(
        &path,
        b"{:base 1}\n",
        b"{:next 2}\n",
        || Ok(()),
        flag_refusing_mover,
    );

    let error = result.expect_err("the refusal must surface");
    assert!(
        error.to_string().contains("config.edn") && error.to_string().contains("unchanged"),
        "the error must say the file was left alone: {error}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        b"{:base 1}\n",
        "config.edn must stay put"
    );
    let leftovers: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .filter(|name| name.to_string_lossy().starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "left {leftovers:?} behind");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn gh538_a_stranded_config_edn_is_restored_on_flag_refusing_storage() {
    // A graph already hit by the old bug: config.edn missing, its bytes in a
    // `.retired` sibling. The open-time recovery must bring it back rather
    // than fail every open with a bare "Invalid argument".
    let dir = scratch("gh538-stranded-restore");
    fs::write(dir.join(".config.edn.4242.0.retired"), b"{:base 1}\n").unwrap();

    let recovered = restore_retired_files_with(&dir, &[dir.clone()], flag_refusing_mover).unwrap();

    assert_eq!(recovered, 1);
    assert_eq!(fs::read(dir.join("config.edn")).unwrap(), b"{:base 1}\n");
    assert!(!dir.join(".config.edn.4242.0.retired").exists());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn gh538_flag_refusing_recovery_never_overwrites_a_live_config_edn() {
    // The plain-rename fallback applies only to an absent target; a present
    // one still sends the retired copy to trash, never over the live file.
    let dir = scratch("gh538-stranded-superseded");
    fs::write(dir.join("config.edn"), b"current").unwrap();
    fs::write(dir.join(".config.edn.4242.0.retired"), b"older").unwrap();

    let recovered = restore_retired_files_with(&dir, &[dir.clone()], flag_refusing_mover).unwrap();

    assert_eq!(recovered, 0);
    assert_eq!(fs::read(dir.join("config.edn")).unwrap(), b"current");
    let trashed =
        typed_trash_dir(&dir, TrashEntryKind::Conflict).join(".config.edn.4242.0.retired");
    assert_eq!(fs::read(trashed).unwrap(), b"older");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn gh538_a_real_io_error_during_recovery_is_named_not_bare() {
    // Not a flag refusal: the recovery must still fail, but say what it was
    // doing and to which file instead of a bare errno.
    let dir = scratch("gh538-named-error");
    fs::write(dir.join(".config.edn.4242.0.retired"), b"{:base 1}\n").unwrap();

    let error = restore_retired_files_with(&dir, &[dir.clone()], |_: &Path, _: &Path| {
        Err(io::Error::from_raw_os_error(5))
    })
    .unwrap_err();

    let text = error.to_string();
    assert!(text.contains("logseq/.config.edn.4242.0.retired"), "{text}");
    assert!(
        dir.join(".config.edn.4242.0.retired").exists(),
        "nothing moved"
    );
    let _ = fs::remove_dir_all(dir);
}

/// GH #385: entries that can never be a page must not make today's absent
/// journal (or a rename) fail. Each shape below used to fail
/// `activate_absent_editor(.., Journal)` while the feed and "All pages" kept
/// working, which blanked the whole Journals view.
#[cfg(unix)]
#[test]
fn gh385_non_page_entries_do_not_block_absent_journal_activation_or_rename() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    type Setup = fn(&Path);
    let shapes: [(&str, Setup); 7] = [
        ("emacs-lock-symlink", |root| {
            symlink("user@host.1234:1", root.join("journals/.#2026_08_20.md")).unwrap()
        }),
        ("page-symlink", |root| {
            symlink("2026_08_19.md", root.join("journals/link.md")).unwrap()
        }),
        ("unreadable-ds-store", |root| {
            let path = root.join("journals/.DS_Store");
            fs::write(&path, b"x").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        }),
        ("unreadable-attachment", |root| {
            let path = root.join("pages/pic.png");
            fs::write(&path, b"x").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        }),
        ("hidden-hard-link", |root| {
            fs::hard_link(
                root.join("journals/2026_08_19.md"),
                root.join("journals/.hl"),
            )
            .unwrap()
        }),
        ("hidden-unparseable-page-name", |root| {
            fs::write(root.join("pages/.foo?.md"), "- x\n").unwrap()
        }),
        ("icloud-placeholder", |root| {
            fs::write(root.join("journals/.2026_08_18.md.icloud"), b"x").unwrap()
        }),
    ];
    for (tag, setup) in shapes {
        let root = scratch(&format!("gh385-{tag}"));
        fs::write(root.join("journals/2026_08_19.md"), "- older day\n").unwrap();
        fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        setup(&root);
        let graph = Graph::open(&root);
        graph.warm_cache();

        let handle = graph
            .activate_absent_editor("Sep 21st, 2026", PageKind::Journal)
            .unwrap_or_else(|error| panic!("{tag}: today's journal refused: {error}"));
        assert_eq!(handle.target, "journals/2026_09_21.md", "{tag}");
        graph
            .rename_page("Alpha", "Beta")
            .unwrap_or_else(|error| panic!("{tag}: rename refused: {error}"));
        assert!(root.join("pages/Beta.md").exists(), "{tag}");

        for entry in ["journals/.DS_Store", "pages/pic.png"] {
            let _ = fs::set_permissions(root.join(entry), fs::Permissions::from_mode(0o644));
        }
        let _ = fs::remove_dir_all(&root);
    }
}

/// GH #332: one entry the graph-wide read walk cannot admit used to fail the
/// whole inventory, leaving zero pages, so every page opened blank. Each shape
/// must now leave every readable page listed and loadable.
#[cfg(unix)]
#[test]
fn gh332_one_unadmittable_entry_does_not_empty_the_page_list() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    type Setup = fn(&Path);
    let shapes: [(&str, Setup, bool); 5] = [
        (
            "fifo",
            |root| {
                fs::create_dir_all(root.join("misc")).unwrap();
                let path =
                    std::ffi::CString::new(root.join("misc/pipe").as_os_str().as_bytes().to_vec())
                        .unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o644) }, 0);
            },
            false,
        ),
        (
            "non-utf8-image-name",
            |root| {
                fs::create_dir_all(root.join("draws")).unwrap();
                fs::write(
                    root.join("draws").join(OsStr::from_bytes(b"caf\xe9.png")),
                    b"x",
                )
                .unwrap();
            },
            false,
        ),
        (
            "unreadable-folder",
            |root| {
                fs::create_dir_all(root.join("lost+found")).unwrap();
                fs::set_permissions(root.join("lost+found"), fs::Permissions::from_mode(0o000))
                    .unwrap();
            },
            false,
        ),
        (
            "unreadable-nested-page",
            |root| {
                fs::create_dir_all(root.join("notes")).unwrap();
                fs::write(root.join("notes/x.md"), "- x\n").unwrap();
                fs::set_permissions(root.join("notes/x.md"), fs::Permissions::from_mode(0o000))
                    .unwrap();
            },
            true,
        ),
        (
            "unreadable-page",
            |root| {
                fs::write(root.join("pages/Locked.md"), "- locked\n").unwrap();
                fs::set_permissions(
                    root.join("pages/Locked.md"),
                    fs::Permissions::from_mode(0o000),
                )
                .unwrap();
            },
            true,
        ),
    ];
    for (tag, setup, reported) in shapes {
        let root = scratch(&format!("gh332-{tag}"));
        // Steve's sample journal, byte for byte.
        fs::write(root.join("journals/2026_09_01.md"), "- Text Journal\n-\n").unwrap();
        fs::write(root.join("pages/Alpha.md"), "- alpha\n").unwrap();
        setup(&root);
        let graph = Graph::open(&root);

        let names: Vec<_> = graph
            .list_pages()
            .into_iter()
            .map(|entry| entry.rel_path)
            .collect();
        assert!(
            names.contains(&"pages/Alpha.md".to_owned()),
            "{tag}: {names:?}"
        );
        assert!(
            names.contains(&"journals/2026_09_01.md".to_owned()),
            "{tag}: {names:?}"
        );
        let alpha = graph
            .load_named("Alpha", PageKind::Page)
            .unwrap_or_else(|error| panic!("{tag}: {error}"))
            .unwrap_or_else(|| panic!("{tag}: Alpha opened as an absent page"));
        assert_eq!(alpha.blocks[0].raw, "alpha", "{tag}");
        assert_eq!(
            !graph.page_index_failures().is_empty(),
            reported,
            "{tag}: {:?}",
            graph.page_index_failures()
        );

        for entry in ["lost+found", "notes/x.md", "pages/Locked.md"] {
            let _ = fs::set_permissions(root.join(entry), fs::Permissions::from_mode(0o755));
        }
        let _ = fs::remove_dir_all(&root);
    }
}
