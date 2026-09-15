//! Graph-text capture and inventory limits, and the retained-content budget
//! with its reservations and budgeted wrappers.

use super::*;

const MAX_GRAPH_TEXT_CAPTURE_FILES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_RAW_BYTES: u64 = 512 * 1024 * 1024;
pub(super) const MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH: usize = 256;
const MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES: usize = 2_000_000;
const MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES: usize = 1_000_000;
const MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_TEXT_ADMISSION_INDEX_BYTES: u64 = 512 * 1024 * 1024;
const MAX_GRAPH_TEXT_ADMISSION_BUILD_PEAK_BYTES: u64 = 1024 * 1024 * 1024;
pub(super) const MAX_GRAPH_TEXT_EXACT_FEED_BATCH_RAW_BYTES: u64 = 64 * 1024 * 1024;
/// Peak content retained while mutable graph-text preparation has both parsed
/// and raw/projection representations alive. This matches the 512 MiB initial
/// shadow raw-byte ceiling, while accounting for those simultaneous copies.
const MAX_GRAPH_TEXT_RETAINED_CONTENT_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) struct GraphTextCaptureLimits {
    pub(super) graph_text_files: usize,
    pub(super) raw_bytes: u64,
    pub(super) directory_depth: usize,
    pub(super) all_entries: usize,
    pub(super) directories: usize,
    pub(super) pending_directories: usize,
    pub(super) path_bytes: u64,
    pub(super) permanent_index_bytes: u64,
    pub(super) peak_build_bytes: u64,
}

pub(super) const GRAPH_TEXT_CAPTURE_LIMITS: GraphTextCaptureLimits = GraphTextCaptureLimits {
    graph_text_files: MAX_GRAPH_TEXT_CAPTURE_FILES,
    raw_bytes: MAX_GRAPH_TEXT_CAPTURE_RAW_BYTES,
    directory_depth: MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH,
    all_entries: MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES,
    directories: MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES,
    pending_directories: MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES,
    path_bytes: MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES,
    permanent_index_bytes: MAX_GRAPH_TEXT_ADMISSION_INDEX_BYTES,
    peak_build_bytes: MAX_GRAPH_TEXT_ADMISSION_BUILD_PEAK_BYTES,
};

/// Bounds for mutable graph-text inventories. These deliberately reuse the
/// capture limits so a mutable inventory cannot be driven beyond the memory
/// and traversal envelope already accepted for the initial capture.
#[derive(Clone, Copy)]
pub(super) struct GraphTextInventoryLimits {
    pub(super) graph_text_files: usize,
    pub(super) directory_depth: usize,
    pub(super) all_entries: usize,
    pub(super) directories: usize,
    pub(super) pending_directories: usize,
    pub(super) path_bytes: u64,
    pub(super) retained_content_bytes: u64,
}

pub(super) const GRAPH_TEXT_INVENTORY_LIMITS: GraphTextInventoryLimits = GraphTextInventoryLimits {
    graph_text_files: MAX_GRAPH_TEXT_CAPTURE_FILES,
    directory_depth: MAX_GRAPH_TEXT_CAPTURE_DIRECTORY_DEPTH,
    all_entries: MAX_GRAPH_TEXT_CAPTURE_ALL_ENTRIES,
    directories: MAX_GRAPH_TEXT_CAPTURE_DIRECTORIES,
    pending_directories: MAX_GRAPH_TEXT_CAPTURE_PENDING_DIRECTORIES,
    path_bytes: MAX_GRAPH_TEXT_CAPTURE_PATH_BYTES,
    retained_content_bytes: MAX_GRAPH_TEXT_RETAINED_CONTENT_BYTES,
};

/// A single preparation budget spans mutable inventory consumers. Every
/// reservation is atomic and RAII-owned: a failed admission leaves the counter
/// unchanged, and every success is released exactly once when its token drops.
///
/// Tokens charge requested/retained capacities, not logical string lengths.
/// Construction sites first reserve a source-derived upper bound for their
/// temporary allocations, then reconcile the token to the capacity-aware size
/// of the retained value.
#[derive(Clone)]
pub(super) struct RetainedContentBudget {
    state: Rc<RetainedContentBudgetState>,
}

#[derive(Debug)]
struct RetainedContentBudgetState {
    limit: u64,
    retained: Cell<u64>,
    #[cfg(test)]
    peak: Cell<u64>,
}

#[derive(Debug)]
pub(super) struct RetainedContentReservation {
    state: Rc<RetainedContentBudgetState>,
    bytes: u64,
}

impl RetainedContentBudget {
    pub(super) fn new(limits: GraphTextInventoryLimits) -> Self {
        #[cfg(test)]
        GRAPH_TEXT_BUDGET_LAST_PEAK.with(|peak| peak.set(0));
        Self {
            state: Rc::new(RetainedContentBudgetState {
                limit: limits.retained_content_bytes,
                retained: Cell::new(0),
                #[cfg(test)]
                peak: Cell::new(0),
            }),
        }
    }

    pub(super) fn reserve(
        &self,
        bytes: u64,
        _resource: &'static str,
    ) -> io::Result<RetainedContentReservation> {
        let candidate = self
            .state
            .retained
            .get()
            .checked_add(bytes)
            .ok_or_else(|| graph_text_inventory_limit_error("aggregate retained content bytes"))?;
        if candidate > self.state.limit {
            return Err(graph_text_inventory_limit_error(
                "aggregate retained content bytes",
            ));
        }
        self.state.retained.set(candidate);
        #[cfg(test)]
        self.state.peak.set(self.state.peak.get().max(candidate));
        Ok(RetainedContentReservation {
            state: Rc::clone(&self.state),
            bytes,
        })
    }

    #[cfg(test)]
    pub(super) fn retained(&self) -> u64 {
        self.state.retained.get()
    }
}

#[cfg(test)]
impl Drop for RetainedContentBudget {
    fn drop(&mut self) {
        GRAPH_TEXT_BUDGET_LAST_PEAK.with(|peak| peak.set(self.state.peak.get()));
    }
}

impl RetainedContentReservation {
    pub(super) fn resize(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        if bytes > self.bytes {
            let increase = bytes - self.bytes;
            let candidate = self
                .state
                .retained
                .get()
                .checked_add(increase)
                .ok_or_else(|| {
                    graph_text_inventory_limit_error("aggregate retained content bytes")
                })?;
            if candidate > self.state.limit {
                return Err(graph_text_inventory_limit_error(
                    "aggregate retained content bytes",
                ));
            }
            self.state.retained.set(candidate);
            #[cfg(test)]
            self.state.peak.set(self.state.peak.get().max(candidate));
        } else {
            let decrease = self.bytes - bytes;
            let retained = self.state.retained.get();
            assert!(
                retained >= decrease,
                "released unreserved graph content for {resource}"
            );
            self.state.retained.set(retained - decrease);
        }
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for RetainedContentReservation {
    fn drop(&mut self) {
        let retained = self.state.retained.get();
        assert!(
            retained >= self.bytes,
            "double release of graph content reservation"
        );
        self.state.retained.set(
            retained
                .checked_sub(self.bytes)
                .expect("reservation release was range-checked"),
        );
    }
}

pub(super) struct BudgetedString {
    pub(super) value: String,
    pub(super) reservation: RetainedContentReservation,
}

pub(super) struct BudgetedPageEntries {
    pub(super) entries: Vec<PageEntry>,
    pub(super) _reservation: RetainedContentReservation,
}

impl std::ops::Deref for BudgetedPageEntries {
    type Target = [PageEntry];

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

pub(super) struct RetainedHeapCharge {
    pub(super) reservation: Option<RetainedContentReservation>,
    bytes: u64,
}

impl RetainedHeapCharge {
    pub(super) fn new(
        budget: Option<&RetainedContentBudget>,
        resource: &'static str,
    ) -> io::Result<Self> {
        Ok(Self {
            reservation: budget
                .map(|budget| budget.reserve(0, resource))
                .transpose()?,
            bytes: 0,
        })
    }

    pub(super) fn grow(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        let next = checked_add_bytes(self.bytes, bytes)?;
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.resize(next, resource)?;
        }
        self.bytes = next;
        Ok(())
    }

    pub(super) fn shrink(&mut self, bytes: u64, resource: &'static str) -> io::Result<()> {
        let next = self
            .bytes
            .checked_sub(bytes)
            .expect("retained heap charge releases only admitted bytes");
        if let Some(reservation) = self.reservation.as_mut() {
            reservation.resize(next, resource)?;
        }
        self.bytes = next;
        Ok(())
    }
}

impl std::ops::Deref for BudgetedString {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

impl AsRef<str> for BudgetedString {
    fn as_ref(&self) -> &str {
        &self.value
    }
}

pub(super) fn checked_add_bytes(left: u64, right: u64) -> io::Result<u64> {
    left.checked_add(right).ok_or_else(allocation_overflow)
}

pub(super) fn checked_mul_bytes(left: u64, right: u64) -> io::Result<u64> {
    left.checked_mul(right).ok_or_else(allocation_overflow)
}

pub(super) fn conservative_vec_entry_bytes<T>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(usize_to_u64(std::mem::size_of::<T>())?, 2)?,
        16,
    )
}

pub(super) fn conservative_vec_capacity_upper_bound<T>(entries: u64) -> io::Result<u64> {
    checked_mul_bytes(entries, conservative_vec_entry_bytes::<T>()?)
}

pub(super) fn conservative_hash_entry_bytes<K, V>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(
            checked_add_bytes(
                usize_to_u64(std::mem::size_of::<K>())?,
                usize_to_u64(std::mem::size_of::<V>())?,
            )?,
            4,
        )?,
        64,
    )
}

pub(super) fn conservative_btree_entry_bytes<K, V>() -> io::Result<u64> {
    checked_add_bytes(
        checked_mul_bytes(
            checked_add_bytes(
                usize_to_u64(std::mem::size_of::<K>())?,
                usize_to_u64(std::mem::size_of::<V>())?,
            )?,
            2,
        )?,
        256,
    )
}

pub(super) fn owned_string_upper_bound(value: &str) -> io::Result<u64> {
    owned_string_len_upper_bound(u64::try_from(value.len()).map_err(|_| allocation_overflow())?)
}

pub(super) fn owned_string_len_upper_bound(len: u64) -> io::Result<u64> {
    checked_add_bytes(
        conservative_vec_capacity_upper_bound::<u8>(len)?,
        usize_to_u64(std::mem::size_of::<String>())?,
    )
}

pub(super) fn owned_path_upper_bound(value: &Path) -> io::Result<u64> {
    owned_string_len_upper_bound(
        u64::try_from(value.as_os_str().len()).map_err(|_| allocation_overflow())?,
    )
}

pub(super) fn page_entry_clone_upper_bound(entry: &PageEntry) -> io::Result<u64> {
    let mut bytes = usize_to_u64(std::mem::size_of::<PageEntry>())?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.name)?)?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.rel_path)?)?;
    checked_add_bytes(bytes, owned_path_upper_bound(&entry.path)?)
}

pub(super) fn graph_text_page_entry_retained_upper_bound(entry: &PageEntry) -> io::Result<u64> {
    let mut bytes = usize_to_u64(std::mem::size_of::<PageEntry>())?;
    bytes = checked_add_bytes(
        bytes,
        checked_add_bytes(
            usize_to_u64(std::mem::size_of::<String>())?,
            usize_to_u64(entry.name.capacity())?,
        )?,
    )?;
    bytes = checked_add_bytes(bytes, owned_string_upper_bound(&entry.rel_path)?)?;
    checked_add_bytes(bytes, owned_path_upper_bound(&entry.path)?)
}
