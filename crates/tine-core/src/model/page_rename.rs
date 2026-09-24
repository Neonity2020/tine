//! Graph's page rename and delete: the rename transaction (with its editor
//! lifecycle), page deletion, and the page-mutation target and external-scope guards.

use super::graph_text_targets::PortableListingBatch;
use super::*;
use crate::direct_projection::PageSetChange;

impl Graph {
    /// Rename a page, OG-style. Moves its file to the new name and rewrites every
    /// reference across pages AND journals — inline `[[old]]`/`#old`, the page's
    /// OWN self/sibling refs, and bare `tags:: old` property refs — and CASCADES
    /// to the whole `old/*` namespace subtree (each `old/child` page moves to
    /// `new/child`, its refs rewritten), matching Logseq's `rename-namespace-pages!`.
    /// Journals can't be renamed (their name is their date). Transactional: locks
    /// every touched file, re-verifies each is unchanged since collection, commits,
    /// and rolls back every write on any failure. Aborts (no change) if a target
    /// name already exists or a touched file changed under us.
    pub fn rename_page(&self, old: &str, new: &str) -> io::Result<()> {
        self.rename_page_expected(old, new, None)
    }

    pub fn rename_page_expected(
        &self,
        old: &str,
        new: &str,
        expected_path: Option<&str>,
    ) -> io::Result<()> {
        self.rename_page_reporting(old, new, expected_path)
            .map(|_| ())
    }

    /// `rename_page_expected`, plus what the rename deliberately did NOT do.
    ///
    /// Callers that can surface it to the user should prefer this: a silently
    /// skipped referrer looks identical to a completed rename otherwise.
    pub fn rename_page_reporting(
        &self,
        old: &str,
        new: &str,
        expected_path: Option<&str>,
    ) -> io::Result<RenameOutcome> {
        self.rename_page_guarded(old, new, expected_path, &[])
    }

    /// `rename_page_reporting`, refusing to move or rewrite any file in
    /// `unsaved_paths` (graph-root-relative).
    ///
    /// The frontend passes the pages whose edits it could not save. A rename
    /// no longer needs every page in the graph saved first, only the ones it
    /// touches, and it cannot know which those are until the transaction has
    /// read the graph; rewriting a file under an unsaved edit would turn that
    /// edit into a conflict against bytes the user never saw (GH #535).
    pub fn rename_page_guarded(
        &self,
        old: &str,
        new: &str,
        expected_path: Option<&str>,
        unsaved_paths: &[String],
    ) -> io::Result<RenameOutcome> {
        let write = self.admit_graph_text_writer()?;
        self.list_pages_before_identity_lock();
        let _identity = self.lock_graph_text_identity_mutation()?;
        let old = old.trim();
        let new = new.trim();
        if new.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty name"));
        }
        if old.is_empty() || crate::refs::same_page(old, new) {
            return Ok(RenameOutcome::default()); // nothing to do (case-only rename is intentionally a no-op)
        }
        self.block_external_scope_mutation(&write, old, PageKind::Page, expected_path, "rename")?;
        let mut content_budget = RetainedContentBudget::new(graph_text_inventory_limits());
        let entries = self.configured_text_entries_with_budget(&write, false, &content_budget)?;
        let page_inventory_snapshot = self.current_page_inventory_snapshot();
        self.validate_page_mutation_target(&write, &entries, old, PageKind::Page, expected_path)?;
        // M1: refuse to rename an ambiguous page (same-stem .md/.markdown/.org on
        // disk) — which twin moves, and which content is authoritative, is
        // undecidable here.
        if self.graph_text_has_twin(&write, old, PageKind::Page)?
            || self.graph_text_has_twin(&write, new, PageKind::Page)?
        {
            return Err(twin_error(old));
        }
        let old_n = crate::refs::normalize(old);
        let ns_prefix = format!("{old_n}/");
        let skip = old.chars().count();

        // Phase 0a — the rename SET: the page itself plus every file-backed
        // namespace descendant (`old/*`). Each contributes a file move and an
        // (old_name -> new_name) ref-rewrite pair applied graph-wide. We only match
        // the exact name or the `old/` prefix (never a bare substring), so renaming
        // `work` -> `work1` turns `work/log` into `work1/log`, not `work1/work1log`.
        let mut rename_pairs: Vec<(String, String)> = Vec::new();
        let mut rename_pairs_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename pair vector")?;
        let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut moves_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename move vector")?;
        let mut move_destinations: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        let mut move_destinations_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename destination set")?;
        let mut move_identities: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut move_identities_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename identity set")?;
        let mut primary_is_file = false;
        for entry in entries.iter() {
            if entry.kind != PageKind::Page {
                continue; // journals aren't namespaced pages; their refs still get rewritten in 0b
            }
            let en = crate::refs::normalize(&entry.name);
            let is_primary = en == old_n;
            if !is_primary && !en.starts_with(&ns_prefix) {
                continue;
            }
            let new_name_bound = checked_add_bytes(
                owned_string_upper_bound(new)?,
                owned_string_upper_bound(&entry.name)?,
            )?;
            let new_path_bound = checked_add_bytes(
                owned_path_upper_bound(&self.pages_path())?,
                checked_add_bytes(new_name_bound, 128)?,
            )?;
            rename_pairs_charge.grow(
                checked_add_bytes(
                    conservative_vec_entry_bytes::<(String, String)>()?,
                    checked_add_bytes(owned_string_upper_bound(&entry.name)?, new_name_bound)?,
                )?,
                "graph rename pair vector",
            )?;
            moves_charge.grow(
                checked_add_bytes(
                    conservative_vec_entry_bytes::<(PathBuf, PathBuf)>()?,
                    checked_add_bytes(owned_path_upper_bound(&entry.path)?, new_path_bound)?,
                )?,
                "graph rename move vector",
            )?;
            move_destinations_charge.grow(
                checked_add_bytes(
                    conservative_hash_entry_bytes::<PathBuf, ()>()?,
                    new_path_bound,
                )?,
                "graph rename destination set",
            )?;
            move_identities_charge.grow(
                checked_add_bytes(
                    conservative_hash_entry_bytes::<String, ()>()?,
                    new_name_bound,
                )?,
                "graph rename identity set",
            )?;
            let new_name = if is_primary {
                new.to_string()
            } else {
                // replace the `old` prefix, preserving the descendant's own casing
                let suffix: String = entry.name.chars().skip(skip).collect();
                format!("{new}{suffix}")
            };
            // Keep the page's own physical extension on rename (an .markdown page
            // stays .markdown; an .org page stays .org).
            let encoded_new = encode_page_name(&new_name, self.config().file_name_format);
            let entry_extension = text_extension_from_path(&entry.path).ok_or_else(bad_path)?;
            let new_path = self
                .pages_path()
                .join(format!("{encoded_new}.{entry_extension}"));
            if entries.iter().any(|other| {
                other.kind == PageKind::Page
                    && other.path != entry.path
                    && crate::refs::same_page(&other.name, &new_name)
            }) {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::IdentityNameTaken,
                    io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "target page identity already exists elsewhere in the graph",
                    ),
                ));
            }
            if new_path != entry.path && self.graph_text_exists(&write, &new_path)? {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::IdentityNameTaken,
                    io::Error::new(io::ErrorKind::AlreadyExists, "target page exists"),
                ));
            }
            for other_format_target in
                configured_text_variant_paths(&self.pages_path(), &encoded_new)
            {
                if other_format_target != entry.path
                    && other_format_target != new_path
                    && self.graph_text_exists(&write, &other_format_target)?
                {
                    return Err(DirectSaveError::into_io(
                        DirectSaveFailureCode::IdentityNameTaken,
                        io::Error::new(
                            io::ErrorKind::AlreadyExists,
                            "target page exists in another supported text extension",
                        ),
                    ));
                }
            }
            // Recursive graph directories can contain two distinct files with
            // the same basename/page identity. Both would map to the same flat
            // rename destination; allowing the transaction to continue would let
            // the later atomic rename overwrite the earlier page and remove both
            // sources. Refuse the ambiguous rename before collecting any edits.
            if !move_destinations.insert(new_path.clone()) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "multiple pages map to the same rename target",
                ));
            }
            let normalized_new_name = crate::refs::normalize(&new_name);
            if !move_identities.insert(normalized_new_name) {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "multiple pages map to the same logical rename target",
                ));
            }
            if is_primary {
                primary_is_file = true;
            }
            rename_pairs.push((entry.name.clone(), new_name));
            moves.push((entry.path.clone(), new_path));
        }
        // A page can exist only via references (no file of its own); still rewrite
        // refs to it.
        if !primary_is_file {
            rename_pairs_charge.grow(
                checked_add_bytes(
                    conservative_vec_entry_bytes::<(String, String)>()?,
                    checked_add_bytes(
                        owned_string_upper_bound(old)?,
                        owned_string_upper_bound(new)?,
                    )?,
                )?,
                "graph rename pair vector",
            )?;
            rename_pairs.push((old.to_string(), new.to_string()));
        }

        // Phase 0b — compute every file edit (inline refs + bare `tags::`), across
        // pages AND journals. A moved page's OWN content is rewritten too (self /
        // sibling refs) and lands at its new path.
        struct Edit {
            src: PathBuf,
            dst: PathBuf,
            orig: String,
            _orig_reservation: RetainedContentReservation,
            new_content: String,
            _new_reservation: RetainedContentReservation,
            base_rev: String,
            is_move: bool,
            /// The page as it was BEFORE this edit. A move retires its old
            /// `rel_path` from the projection, which is the only way the
            /// index learns that the file it holds rows for is gone.
            src_entry: PageEntry,
        }
        let _move_map_charge = content_budget.reserve(
            checked_mul_bytes(
                usize_to_u64(moves.len())?,
                conservative_hash_entry_bytes::<PathBuf, PathBuf>()?,
            )?,
            "graph rename move map table capacity",
        )?;
        let move_dst: std::collections::HashMap<PathBuf, PathBuf> = moves.into_iter().collect();
        // The whole rename SET as a normalized(old) -> new map, so each graph file
        // is rewritten ONCE against every descendant in a single pass — not K passes
        // (one per `(old,new)` pair), which made a namespace rename O(graph_text * K)
        // and recomputed code ranges twice per pair per file (perf Codex#2).
        let mut rename_map_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename rewrite map")?;
        for (old_name, new_name) in &rename_pairs {
            rename_map_charge.grow(
                checked_add_bytes(
                    conservative_hash_entry_bytes::<String, String>()?,
                    checked_add_bytes(
                        owned_string_upper_bound(old_name)?,
                        owned_string_upper_bound(new_name)?,
                    )?,
                )?,
                "graph rename rewrite map",
            )?;
        }
        let rename_map: std::collections::HashMap<String, String> = rename_pairs
            .iter()
            .map(|(o, n)| (crate::refs::normalize(o), n.clone()))
            .collect();
        let mut edits: Vec<Edit> = Vec::new();
        let mut skipped_conflicted_referrers: Vec<String> = Vec::new();
        let mut edits_charge =
            RetainedHeapCharge::new(Some(&content_budget), "graph rename edit vector")?;
        for entry in entries.iter() {
            // A file the rename cannot read fails it, naming that file: an
            // error that named none left the user no way to find it (audit
            // R15-09).
            let content = self
                .graph_text_read_to_string_with_budget(
                    &write,
                    &entry.path,
                    &mut content_budget,
                    "graph rename baseline bytes",
                )
                .map_err(|error| {
                    io::Error::new(error.kind(), format!("{}: {error}", entry.rel_path))
                })?;
            let is_org = Format::from_path(&entry.path) == Format::Org;
            // One inline-ref pass + one `tags::` pass per file (each computes code
            // ranges once), regardless of how many descendants are being renamed.
            let mut inline_reservation = content_budget.reserve(
                rename_rewrite_upper_bound(&content, &rename_map, is_org)?,
                "graph rename inline rewrite construction bound",
            )?;
            let inline = crate::refs::rename_refs_multi_with_format(
                &content,
                &rename_map,
                is_org,
                self.config().file_name_format,
            );
            inline_reservation.resize(
                usize_to_u64(inline.capacity())?,
                "graph rename inline rewrite bytes",
            )?;
            let mut updated_reservation = content_budget.reserve(
                rename_rewrite_upper_bound(&inline, &rename_map, false)?,
                "graph rename tags rewrite construction bound",
            )?;
            let mut updated = crate::refs::rename_tags_property_multi(&inline, &rename_map, is_org);
            if move_dst.contains_key(&entry.path) {
                if let Some(new_name) = rename_map.get(&crate::refs::normalize(&entry.name)) {
                    if let Some(rebound) =
                        rebind_matching_page_title_property(&updated, &entry.name, new_name)
                    {
                        updated = rebound;
                    }
                }
            }
            updated_reservation.resize(
                usize_to_u64(updated.capacity())?,
                "graph rename replacement bytes",
            )?;
            drop(inline);
            drop(inline_reservation);
            // H1: a rename must never rewrite a read-only (non-round-tripping) .org
            // file. Abort the whole rename (all-or-nothing) so the user resolves it
            // in Logseq first. A pure file move with no content change (updated ==
            // content) is still allowed — it preserves bytes exactly.
            // A referrer carrying column-0 VCS conflict markers is quarantined: the
            // user (or their merge tool) still owes it a resolution, and its bytes
            // are not ours to touch. Threat: an external-editor / sync-service merge
            // left the file mid-conflict; rewriting refs inside it edits one or both
            // sides of a conflict the user has not adjudicated, behind their back.
            // Leave it byte-identical and report it instead of refusing the whole
            // rename, which would be harsher than the problem.
            let quarantined =
                updated != *content && !crate::doc::vcs_conflict_markers(&content).is_empty();
            let updated = if quarantined {
                skipped_conflicted_referrers.push(entry.path.display().to_string());
                content.to_string()
            } else {
                updated
            };
            let changed = updated != *content;
            if is_org && changed && !crate::org::org_editable(&content) {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "cannot rename: {} is a read-only .org file (does not round-trip)",
                        entry.path.display()
                    ),
                ));
            }
            let BudgetedString {
                value: original,
                reservation: original_reservation,
            } = content;
            let _base_rev_scratch =
                content_budget.reserve(128, "graph rename revision string construction")?;
            let base_rev = content_rev(&original);
            match move_dst.get(&entry.path) {
                Some(dst) => {
                    edits_charge.grow(
                        checked_add_bytes(
                            conservative_vec_entry_bytes::<Edit>()?,
                            checked_add_bytes(
                                owned_path_upper_bound(&entry.path)?,
                                checked_add_bytes(
                                    owned_path_upper_bound(dst)?,
                                    checked_add_bytes(
                                        owned_string_upper_bound(&base_rev)?,
                                        Self::retained_page_entry_bytes(&entry)?,
                                    )?,
                                )?,
                            )?,
                        )?,
                        "graph rename edit vector",
                    )?;
                    edits.push(Edit {
                        src: entry.path.clone(),
                        dst: dst.clone(),
                        base_rev,
                        orig: original,
                        _orig_reservation: original_reservation,
                        new_content: updated,
                        _new_reservation: updated_reservation,
                        is_move: true,
                        src_entry: entry.clone(),
                    });
                }
                None if changed => {
                    edits_charge.grow(
                        checked_add_bytes(
                            conservative_vec_entry_bytes::<Edit>()?,
                            checked_add_bytes(
                                checked_mul_bytes(owned_path_upper_bound(&entry.path)?, 2)?,
                                checked_add_bytes(
                                    owned_string_upper_bound(&base_rev)?,
                                    Self::retained_page_entry_bytes(&entry)?,
                                )?,
                            )?,
                        )?,
                        "graph rename edit vector",
                    )?;
                    edits.push(Edit {
                        src: entry.path.clone(),
                        dst: entry.path.clone(),
                        base_rev,
                        orig: original,
                        _orig_reservation: original_reservation,
                        new_content: updated,
                        _new_reservation: updated_reservation,
                        is_move: false,
                        src_entry: entry.clone(),
                    });
                }
                None => {
                    drop(updated);
                    drop(updated_reservation);
                    drop(original);
                    drop(original_reservation);
                }
            }
        }
        if edits.is_empty() {
            return Ok(RenameOutcome::default()); // page doesn't exist / nothing references it
        }
        if let Some(blocked) = edits
            .iter()
            .find(|edit| unsaved_paths.contains(&self.rel_path(&edit.src)))
        {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "“{}” has changes Tine could not save, and this rename would rewrite it. \
                     Save or discard those changes, then rename again.",
                    blocked.src_entry.name
                ),
            ));
        }

        // Phase 1 — lock every touched path (src + move dst), sorted + deduped
        // (deadlock-free against a single-page save, which only ever holds ONE lock).
        let mut lock_state_bound = 0_u64;
        let mut rollback_state_bound = 0_u64;
        for edit in &edits {
            for path in [&edit.src, &edit.dst] {
                lock_state_bound = checked_add_bytes(
                    lock_state_bound,
                    checked_add_bytes(
                        conservative_vec_entry_bytes::<PathBuf>()?,
                        checked_add_bytes(owned_path_upper_bound(path)?, 256)?,
                    )?,
                )?;
            }
            rollback_state_bound = checked_add_bytes(
                rollback_state_bound,
                conservative_vec_entry_bytes::<(&Edit, Option<PathBuf>)>()?,
            )?;
            if edit.is_move && edit.dst != edit.src {
                rollback_state_bound = checked_add_bytes(
                    rollback_state_bound,
                    checked_add_bytes(
                        checked_mul_bytes(owned_path_upper_bound(&edit.src)?, 3)?,
                        256,
                    )?,
                )?;
            }
        }
        let _lock_state_charge =
            content_budget.reserve(lock_state_bound, "graph rename lock state")?;
        let _rollback_state_charge =
            content_budget.reserve(rollback_state_bound, "graph rename rollback state")?;
        let mut lock_paths: Vec<PathBuf> = Vec::new();
        for e in &edits {
            lock_paths.push(e.src.clone());
            if e.is_move {
                lock_paths.push(e.dst.clone());
            }
        }
        lock_paths.sort();
        lock_paths.dedup();
        let locks: Vec<_> = lock_paths.iter().map(|p| self.page_lock(p)).collect();
        let _guards: Vec<_> = locks.iter().map(|l| l.lock().unwrap()).collect();
        // Phase 2 — re-verify nothing changed under us since Phase 0; abort (no
        // change) on any mismatch (an external editor / Syncthing pull landed).
        for e in &edits {
            if e.is_move && e.dst != e.src && self.graph_text_exists(&write, &e.dst)? {
                return Err(DirectSaveError::into_io(
                    DirectSaveFailureCode::IdentityNameTaken,
                    io::Error::new(io::ErrorKind::AlreadyExists, "target page exists"),
                ));
            }
            // A hard read failure is not the same thing as an empty file. Treating
            // it as empty could let an actually-empty baseline pass verification,
            // after which the transaction would overwrite a file we could no
            // longer inspect.
            if !self.graph_text_content_rev_matches(&write, &e.src, &e.base_rev)? {
                return Err(io::Error::new(io::ErrorKind::AlreadyExists, "conflict"));
            }
        }

        // Phase 3 — commit, tracking writes for rollback. Move sources are
        // atomically staged into recoverable trash rather than unlinked, so an
        // external replacement at the syscall boundary is preserved as an inode.
        let mut written: Vec<(&Edit, Option<PathBuf>)> = Vec::new();
        let result: io::Result<()> = (|| {
            // GH #406: list each directory once for the whole write phase
            // instead of once per written file (see `PortableListingBatch`).
            let listings = PortableListingBatch::begin();
            for e in &edits {
                // Phase 2 can be far in the past for a large graph. Recheck this
                // exact file immediately before its write so an external editor or
                // sync pull that landed while earlier edits committed is preserved.
                if !self.graph_text_content_rev_matches(&write, &e.src, &e.base_rev)? {
                    return Err(io::Error::new(io::ErrorKind::AlreadyExists, "conflict"));
                }
                self.note_self_write(&e.dst, content_rev(&e.new_content));
                if e.is_move && e.dst != e.src {
                    if let Some(parent) = e.dst.parent() {
                        self.graph_text_create_dir_all(&write, parent)?;
                    }
                }
                let publish = || {
                    self.graph_text_atomic_write_from_transaction_inventory(
                        &write,
                        &e.dst,
                        e.new_content.as_bytes(),
                        e.is_move && e.dst != e.src,
                    )
                };
                if e.is_move && e.dst != e.src {
                    listings.suspended(publish)?;
                } else {
                    publish()?;
                }
                written.push((e, None));
                if e.is_move && e.dst != e.src {
                    rename_source_remove_failpoint()?;
                    let trash = typed_trash_dir(&self.root, TrashEntryKind::Page);
                    self.graph_text_create_dir_all(&write, &trash)?;
                    let src_name = e.src.file_name().and_then(|s| s.to_str()).unwrap_or("page");
                    let staged = trash.join(format!("{}__rename__{src_name}", trash_stamp()));
                    self.graph_text_move_noreplace_from_transaction_inventory(
                        &write, &e.src, &staged,
                    )?;
                    written.last_mut().unwrap().1 = Some(staged.clone());
                    // If a sync replacement won just before the atomic move, the
                    // staged bytes no longer match our baseline. Abort and restore
                    // that exact inode instead of completing from stale content.
                    if !self.graph_text_content_rev_matches(&write, &staged, &e.base_rev)? {
                        return Err(io::Error::new(io::ErrorKind::AlreadyExists, "conflict"));
                    }
                }
            }
            drop(listings);
            Ok(())
        })();
        if let Err(err) = result {
            // Roll back in reverse, and drop the self-write markers for bytes that
            // won't survive the rollback so they can't later suppress a real
            // external change (M1).
            let _ = graph_text_write_during_rollback_hook();
            for (e, staged_source) in written.iter().rev() {
                if e.is_move && e.dst != e.src {
                    let source_restored = match staged_source {
                        Some(staged) => {
                            self.graph_text_move_noreplace_from_transaction_inventory(
                                &write, staged, &e.src,
                            )
                            .is_ok()
                                || self.graph_text_exists(&write, &e.src).unwrap_or(false)
                        }
                        None => self.graph_text_exists(&write, &e.src).unwrap_or(false),
                    };
                    if source_restored {
                        // Never compare and unlink the live destination. Detach
                        // whichever inode currently owns the name, inspect it in
                        // recovery, and restore/retain any external replacement.
                        let _ = self.withdraw_file_to_conflict_if_exact(
                            &write,
                            &e.dst,
                            e.new_content.as_bytes(),
                            "rename-rollback-destination",
                        );
                    }
                    self.recent_writes.lock().unwrap().remove(&e.dst);
                } else {
                    let ours = content_rev(&e.new_content);
                    if self
                        .graph_text_content_rev_matches(&write, &e.dst, &ours)
                        .unwrap_or(false)
                    {
                        self.note_self_write(&e.dst, content_rev(&e.orig));
                        let _ = self.graph_text_atomic_write_from_transaction_inventory(
                            &write,
                            &e.dst,
                            e.orig.as_bytes(),
                            false,
                        );
                    } else {
                        self.recent_writes.lock().unwrap().remove(&e.dst);
                    }
                }
            }
            let coming = self.index_delta_coming();
            self.discard_parsed_cache(
                edits
                    .iter()
                    .flat_map(|edit| [edit.src.clone(), edit.dst.clone()])
                    .collect(),
                graph_drift::IndexEffect::Sent(&coming),
            );
            self.reconcile_failed_graph_text_paths(
                &write,
                edits
                    .iter()
                    .flat_map(|edit| [edit.src.as_path(), edit.dst.as_path()]),
            );
            return Err(err);
        }
        // The rename transaction already retains every changed document's final
        // bytes. Update a current page-list memo from those bytes before dropping
        // the parsed cache: unchanged entries preserve their exact physical and
        // effective identity, while only the edited subset is reparsed. A cold or
        // already-stale memo remains cold and retains the ordinary disk rebuild.
        let outcomes = edits
            .iter()
            .map(|edit| {
                self.graph_inventory_entry(&edit.dst)
                    .ok()
                    .flatten()
                    .and_then(|entry| {
                        parse_exact_page(self, &entry, &edit.new_content)
                            .ok()
                            .map(|(effective, _, _)| effective)
                    })
            })
            .collect::<Vec<_>>();
        // What the rename wrote is what the disk holds now: a moved page's
        // old path owns nothing, and a replacement that does not parse is
        // unreadable. Recorded by path, before the discard moves the
        // generation; the discard no longer clears the record (audit R15-02).
        for (edit, parsed) in edits.iter().zip(&outcomes) {
            if edit.dst != edit.src {
                self.note_graph_text_state(&edit.src, true);
            }
            self.note_graph_text_state(&edit.dst, parsed.is_some());
        }
        let updated_page_inventory = page_inventory_snapshot.map(|mut inventory| {
            for (edit, parsed) in edits.iter().zip(outcomes) {
                inventory.retain(|entry| entry.path != edit.src);
                if let Some(entry) = parsed {
                    inventory.push(entry);
                }
            }
            inventory
        });
        let coming = self.index_delta_coming();
        self.discard_parsed_cache(
            edits
                .iter()
                .flat_map(|edit| [edit.src.clone(), edit.dst.clone()])
                .collect(),
            graph_drift::IndexEffect::Sent(&coming),
        );
        if let Some(inventory) = updated_page_inventory {
            self.publish_page_inventory_snapshot(inventory);
        }
        // GH #543: a rename is a PRODUCER, exactly as a delete is.
        // Discarding the parsed cache only moves the generation, and an
        // existing complete committed image may still answer while an
        // ordinary queued delta converges it. With no producer there
        // was nothing to converge and nothing to refuse, so
        // search went on answering with the renamed page's OLD name and path
        // indefinitely, offering a page that no longer existed; and because
        // the query SUCCEEDED it never reached the repair a refusal starts
        // (third audit A3-F1). `cache_remove` has always published its own
        // delta for a delete; the rename published none.
        //
        // The transaction still holds every changed page's final bytes, so
        // these deltas are exact and need no graph-wide reparse. A page whose
        // replacement cannot be parsed keeps its existing rows rather than
        // losing them: stale beats absent, and `page_index_failures` already
        // names it (the same rule as an unreadable page in a warm).
        let projection_generation = self.cache_gen.load(std::sync::atomic::Ordering::Acquire);
        let mut page_set = Vec::new();
        for edit in &edits {
            let replacement = self
                .graph_inventory_entry(&edit.dst)
                .ok()
                .flatten()
                .and_then(|entry| parse_exact_page(self, &entry, &edit.new_content).ok());
            let Some((effective, document, revision)) = replacement else {
                continue;
            };
            if edit.is_move && edit.dst != edit.src {
                page_set.push(PageSetChange::Delete {
                    entry: edit.src_entry.clone(),
                });
            }
            page_set.push(PageSetChange::Replace {
                entry: effective,
                document: Arc::new(document),
                revision,
            });
        }
        self.direct_projection_publish_page_set(projection_generation, page_set);
        let touched = edits
            .iter()
            .map(|edit| RenameTouchedPage {
                name: edit.src_entry.name.clone(),
                kind: edit.src_entry.kind,
                path: self.rel_path(&edit.src),
                renamed_to: (edit.is_move && edit.dst != edit.src)
                    .then(|| {
                        rename_map
                            .get(&crate::refs::normalize(&edit.src_entry.name))
                            .cloned()
                    })
                    .flatten(),
            })
            .collect();
        self.finish_successful_rename_editor_lifecycle(
            &edits
                .iter()
                .map(|edit| (&edit.src, &edit.dst, edit.is_move && edit.dst != edit.src))
                .collect::<Vec<_>>(),
        );
        Ok(RenameOutcome {
            skipped_conflicted_referrers,
            touched,
        })
    }

    /// The owned bytes a retained [`PageEntry`] adds to the edit vector, so
    /// carrying the pre-edit page through the transaction is charged like every
    /// other retained string in it.
    fn retained_page_entry_bytes(entry: &PageEntry) -> io::Result<u64> {
        checked_add_bytes(
            owned_string_upper_bound(&entry.name)?,
            checked_add_bytes(
                owned_string_upper_bound(&entry.rel_path)?,
                owned_path_upper_bound(&entry.path)?,
            )?,
        )
    }

    /// The frontend reloads only the pages a rename touched (GH #535), so only
    /// those lose their editor state here. A MOVED page's editor is destroyed
    /// with its old name, so its activation is retired and a later `Reuse` on
    /// that path cannot inherit it. A page rewritten in place keeps its editor:
    /// a clean one is reloaded by the frontend, which replaces the activation
    /// itself, and one edited during the rename must still save against its own
    /// activation, where the rewrite surfaces as an ordinary reviewable
    /// conflict. Conflict authority observed before the rewrite is revoked for
    /// every touched path. Untouched pages keep everything. An error leaves the
    /// still-mounted editors and their conflict banners intact.
    fn finish_successful_rename_editor_lifecycle(&self, touched: &[(&PathBuf, &PathBuf, bool)]) {
        {
            let mut activations = self.editor_activations.lock().unwrap();
            for (src, _, moved) in touched {
                if *moved {
                    activations.live.remove(*src);
                }
            }
        }
        for (src, dst, _) in touched {
            self.revoke_conflict_authority(src);
            self.revoke_conflict_authority(dst);
        }
    }

    /// Delete a page/journal file. Rather than unlinking, the file is moved to a
    /// graph-local trash (`logseq/.tine-trash/`, outside journals//pages/ so it's
    /// never re-loaded) — so a delete that races an unseen external edit, or a
    /// simple misclick, is recoverable. If the trash move fails, the live file is
    /// left in place and the error is returned.
    pub fn delete_page(&self, name: &str, kind: PageKind) -> io::Result<()> {
        self.delete_page_expected(name, kind, None)
    }

    pub fn delete_page_expected(
        &self,
        name: &str,
        kind: PageKind,
        expected_path: Option<&str>,
    ) -> io::Result<()> {
        let write = self.admit_graph_text_writer()?;
        self.list_pages_before_identity_lock();
        let _identity = self.lock_graph_text_identity_mutation()?;
        self.block_external_scope_mutation(&write, name, kind, expected_path, "delete")?;
        let entries = self.configured_text_entries(&write, false)?;
        // M1: with same-stem .md/.markdown/.org twins, "which file?" is
        // ambiguous — refuse rather than trash an arbitrary one.
        if self.graph_text_has_twin(&write, name, kind)? {
            return Err(twin_error(name));
        }
        let matching: Vec<_> = entries
            .iter()
            .cloned()
            .into_iter()
            .filter(|entry| entry.kind == kind && crate::refs::same_page(&entry.name, name))
            .collect();
        if matching.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "multiple files share this page identity; delete by name is ambiguous",
            ));
        }
        self.validate_page_mutation_target(&write, &entries, name, kind, expected_path)?;
        let removed = matching.into_iter().next();
        if let Some(entry) = &removed {
            let lock = self.page_lock(&entry.path);
            let _guard = lock.lock().unwrap();
            let trash = typed_trash_dir(
                &self.root,
                match entry.kind {
                    PageKind::Journal => TrashEntryKind::Journal,
                    PageKind::Page => TrashEntryKind::Page,
                },
            );
            let fname = entry
                .path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("page.md");
            let dest = trash.join(format!("{}__{fname}", trash_stamp()));
            self.graph_text_move_to_trash(&write, &entry.path, &dest, &trash)?;
        }
        self.cache_remove(name, kind, removed);
        Ok(())
    }

    fn block_external_scope_mutation(
        &self,
        _write: &GraphTextWritePermit,
        name: &str,
        kind: PageKind,
        expected_path: Option<&str>,
        operation: &str,
    ) -> io::Result<()> {
        let expected_is_external = expected_path
            .filter(|path| self.resolve_rel_lexical(path).is_some())
            .is_some_and(|path| self.resolve_configured_rel_lexical(path).is_none());
        if expected_is_external {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{operation} of a document outside configured creation roots is unsupported"
                ),
            ));
        }
        if self.try_list_pages()?.iter().any(|entry| {
            entry.kind == kind
                && crate::refs::same_page(&entry.name, name)
                && self
                    .resolve_configured_rel_lexical(&entry.rel_path)
                    .is_none()
        }) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "{operation} of a document outside configured creation roots is unsupported"
                ),
            ));
        }
        Ok(())
    }

    /// Wait for the page list before taking the identity lock, so that the
    /// scope check under it ([`Self::block_external_scope_mutation`]) reads
    /// the list memo instead of waiting for the index while every save and
    /// page creation waits on the lock (GH #406, GH #594 L3). The check
    /// itself still runs under the lock, on the list current there.
    fn list_pages_before_identity_lock(&self) {
        let _ = self.try_list_pages();
    }

    /// Validate the snapshot captured by a page menu/title before any mutation.
    /// Even an exact path does not authorize choosing one logical duplicate: the
    /// semantics of rewriting `[[page]]` references remain ambiguous.
    fn validate_page_mutation_target(
        &self,
        write: &GraphTextWritePermit,
        entries: &[PageEntry],
        name: &str,
        kind: PageKind,
        expected_path: Option<&str>,
    ) -> io::Result<()> {
        let matching: Vec<_> = entries
            .iter()
            .filter(|entry| entry.kind == kind && crate::refs::same_page(&entry.name, name))
            .collect();
        if matching.len() > 1 {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "multiple files share this page identity; mutation is ambiguous",
            ));
        }
        let Some(expected) = expected_path.filter(|path| !path.trim().is_empty()) else {
            return Ok(());
        };
        let expected_abs = self
            .resolve_graph_text_rel(write, expected)?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "invalid expected page path")
            })?;
        let Some(entry) = matching.first() else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "stale page target"));
        };
        if entry.path != expected_abs {
            return Err(io::Error::new(io::ErrorKind::NotFound, "stale page target"));
        }
        Ok(())
    }
}
