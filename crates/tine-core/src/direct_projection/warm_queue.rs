//! The warm validation's queue side: offering a walk inventory to the
//! worker and waiting for that attempt's own verdict (R6, GH #543).
use super::*;

/// Why the queue refused a warm validation; see
/// [`DirectProjection::enqueue_warm_with_repair`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WarmRefusal {
    /// The worker is gone, or failed with no rebuild queued.
    Unavailable,
    /// A full snapshot or another warm already owns readiness.
    Superseded,
    /// Only a page update newer than the warm's generation was queued.
    Outranked,
}

impl DirectProjection {
    /// R6 warm validation: hand the worker the walk inventory with exact
    /// content revisions and nothing parsed. Refused (`None`) when a newer
    /// mutation or a full snapshot already outranks this generation — the
    /// caller then leaves readiness to the parser fallback, exactly as
    /// `install_built` does on generation drift.
    ///
    /// Queued page updates at or below this generation do not refuse it. The
    /// caller accepted them as matching what it read, and the worker takes
    /// them in the same turn, validating the warm first and applying the
    /// updates after it. Refusing turned every page opened at launch into a
    /// whole-graph parse whenever the worker had not yet taken its update
    /// (GH #543, audit IT-03).
    #[cfg(test)]
    pub(crate) fn enqueue_warm(
        &self,
        generation: u64,
        sources: Vec<(PageEntry, String)>,
        retained: Vec<PageEntry>,
        published: Vec<String>,
        walk_order: Vec<String>,
        parse_config: Arc<ParseConfig>,
        text_bytes: u64,
    ) -> Option<u64> {
        self.enqueue_warm_with_repair(
            generation,
            sources,
            retained,
            published,
            walk_order,
            &mut None,
            parse_config,
            text_bytes,
        )
        .ok()
    }

    /// `enqueue_warm`, carrying the pages a previous attempt named `Changed`.
    ///
    /// `repair` is taken only when the warm is queued, so a refused caller
    /// still holds it for its next attempt. The refusal says why:
    /// [`WarmRefusal::Outranked`] means only that a page update newer than the
    /// warm's generation was queued meanwhile. The caller's inventory is still
    /// good; it accounts for that update and offers again rather than parsing
    /// the graph (GH #543, audit R5-02).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_warm_with_repair(
        &self,
        generation: u64,
        sources: Vec<(PageEntry, String)>,
        retained: Vec<PageEntry>,
        published: Vec<String>,
        walk_order: Vec<String>,
        repair: &mut Option<WarmRepair>,
        parse_config: Arc<ParseConfig>,
        _text_bytes: u64,
    ) -> Result<u64, WarmRefusal> {
        if !self.shared.worker_available.load(Ordering::Acquire) {
            return Err(WarmRefusal::Unavailable);
        }
        let mut pending = self.shared.pending.lock().unwrap();
        let refusal = if self.shared.worker_failed.load(Ordering::Acquire) && !pending.rebuild {
            Some(WarmRefusal::Unavailable)
        } else if pending.full.is_some() || pending.warm.is_some() {
            Some(WarmRefusal::Superseded)
        } else if pending.latest_generation > generation {
            Some(WarmRefusal::Outranked)
        } else {
            None
        };
        if let Some(refusal) = refusal {
            projection_diag(|| {
                format!(
                    "warm refused generation={generation} full={} deltas={} warm={} latest={} failed={}",
                    pending.full.is_some(),
                    pending.deltas.len(),
                    pending.warm.is_some(),
                    pending.latest_generation,
                    self.shared.worker_failed.load(Ordering::Acquire),
                )
            });
            return Err(refusal);
        }
        let sources_len = sources.len();
        self.shared.ready.store(false, Ordering::Release);
        // The walk's own order, pages it could not read included: the image
        // stores positions in that order, and appending the unreadable pages
        // instead shifted every page after them onto a position another page
        // still held (GH #543).
        pending.seed_page_order(walk_order.iter().map(String::as_str));
        pending.place_unseeded_deltas();
        pending.sent = SentSources {
            inventory: Some(SentInventory {
                revisions: sources
                    .iter()
                    .map(|(entry, revision)| (entry.rel_path.clone(), revision.clone()))
                    .collect(),
                digest: parse_config.digest(),
            }),
            pages: HashMap::new(),
        };
        pending.warm_outcome = None;
        pending.warm_attempt += 1;
        let attempt = pending.warm_attempt;
        pending.warm = Some(PendingWarm {
            sources,
            retained,
            published,
            repair: repair.take(),
            parse_config,
        });
        pending.latest_generation = generation;
        pending.revalidate = false;
        self.shared.changed.notify_all();
        projection_diag(|| {
            format!(
                "warm queued attempt={attempt} generation={generation} pages={sources_len} text_mib={:.1}",
                _text_bytes as f64 / (1024.0 * 1024.0)
            )
        });
        Ok(attempt)
    }

    /// Block until the worker has decided the warm `attempt` asked for.
    ///
    /// `attempt` is the id [`Self::enqueue_warm`] returned. A waiter only ever
    /// takes ITS OWN verdict: the slot used to be a single unkeyed value, so a
    /// warm descheduled before reading it could take a later attempt's verdict
    /// and leave that later attempt waiting for a producer that had already
    /// run. Nothing scheduled another — and the loser holds the process-wide
    /// warm mutex, so every later graph warm queued behind it and the app
    /// stopped indexing altogether (GH #543, re-audit A2-B1).
    pub(crate) fn wait_warm_outcome(&self, attempt: u64) -> WarmOutcome {
        let mut pending = self.shared.pending.lock().unwrap();
        loop {
            match pending.warm_outcome.as_ref().map(|(id, _)| *id) {
                // This attempt's own verdict.
                Some(id) if id == attempt => {
                    return pending.warm_outcome.take().expect("just observed").1;
                }
                // Somebody else's. Leave it for them; a verdict for a LATER
                // attempt also proves this one was superseded.
                Some(_) => return WarmOutcome::Superseded,
                None => {}
            }
            if !self.shared.worker_available.load(Ordering::Acquire) || pending.stop {
                return WarmOutcome::Failed;
            }
            // A later warm was admitted, which cleared the slot and took over
            // validation. This attempt's verdict is never coming.
            if pending.warm_attempt != attempt {
                projection_diag(|| {
                    format!(
                        "warm attempt={attempt} superseded by attempt={}",
                        pending.warm_attempt
                    )
                });
                return WarmOutcome::Superseded;
            }
            // Belt and braces for an attempt that is still current but has
            // nothing left to produce its verdict: nothing queued and an idle
            // worker means no outcome is coming.
            if pending.warm.is_none() && !self.shared.worker_busy.load(Ordering::Acquire) {
                projection_diag(|| {
                    "warm outcome taken by another attempt; nothing left to wait for".to_owned()
                });
                return WarmOutcome::Superseded;
            }
            pending = self.shared.changed.wait(pending).unwrap();
        }
    }
}
