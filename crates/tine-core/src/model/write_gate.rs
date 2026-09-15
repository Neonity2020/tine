//! The graph-text write gate: one gate per canonical graph, the identity-
//! mutation guard, the write binding and permit, the resolved GraphTextTarget,
//! and the ConflictSnapshot a guarded read returns.

use super::*;

/// Per-retained-resource serialization for every page or journal mutation.
///
/// Every writer of one retained resource shares this gate, so graph-text
/// identity transitions are totally ordered across all of them.
pub(super) struct GraphTextWriteGate {
    /// Resource-wide serialization for graph-text identity validation, the
    /// corresponding filesystem transition, and retained-index publication.
    ///
    /// This is deliberately reentrant by thread: higher-level transactions
    /// (rename/merge/projection recovery) call the same low-level publication
    /// primitives while retaining one authority window. This lock decides the
    /// total order in which admitted writers change graph-text identity.
    pub(super) identity_mutation: std::sync::Mutex<GraphTextIdentityMutationState>,
    pub(super) identity_mutation_changed: std::sync::Condvar,
}

#[derive(Default)]
pub(super) struct GraphTextIdentityMutationState {
    owner: Option<std::thread::ThreadId>,
    depth: usize,
    #[cfg(test)]
    pub(super) waiters: usize,
    /// Resource-wide version of every graph-text identity transition observed
    /// under this authority. Per-Graph indexes may be reused only at this exact
    /// epoch; their scope and configuration remain instance-local.
    epoch: u64,
}

pub(super) struct GraphTextIdentityMutationGuard<'a> {
    pub(super) gate: &'a GraphTextWriteGate,
}

impl GraphTextWriteGate {
    fn new() -> Self {
        Self {
            identity_mutation: std::sync::Mutex::new(GraphTextIdentityMutationState::default()),
            identity_mutation_changed: std::sync::Condvar::new(),
        }
    }

    pub(super) fn lock_identity_mutation(&self) -> GraphTextIdentityMutationGuard<'_> {
        let caller = std::thread::current().id();
        let mut state = self.identity_mutation.lock().unwrap();
        #[cfg(test)]
        let mut registered_waiter = false;
        while state.owner.as_ref().is_some_and(|owner| owner != &caller) {
            #[cfg(test)]
            if !registered_waiter {
                state.waiters += 1;
                registered_waiter = true;
                self.identity_mutation_changed.notify_all();
            }
            state = self.identity_mutation_changed.wait(state).unwrap();
        }
        #[cfg(test)]
        if registered_waiter {
            state.waiters -= 1;
        }
        if state.owner.is_none() {
            state.owner = Some(caller);
        }
        state.depth = state
            .depth
            .checked_add(1)
            .expect("graph-text identity mutation depth exhausted");
        GraphTextIdentityMutationGuard { gate: self }
    }

    pub(super) fn identity_mutation_epoch(&self) -> u64 {
        self.identity_mutation.lock().unwrap().epoch
    }

    /// The resource epoch, read under this thread's own mutation authority.
    ///
    /// This is an internal precondition, not a threat-model refusal: a caller
    /// that does not hold the gate would be comparing two reads of a value
    /// another thread is free to advance between them, so the comparison means
    /// nothing. It used to be a `debug_assert`, which does not exist in the
    /// shipped release profile — see `graph_text_writers_take_the_identity_gate_before_any_page_lock`
    /// for the static proof that no production path reaches here without it.
    pub(super) fn identity_mutation_epoch_under_authority(&self) -> io::Result<u64> {
        let caller = std::thread::current().id();
        let state = self.identity_mutation.lock().unwrap();
        if state.owner.as_ref() != Some(&caller) || state.depth == 0 {
            return Err(graph_text_admission_unavailable(
                "graph-text identity epoch read without this thread's mutation authority",
            ));
        }
        Ok(state.epoch)
    }

    pub(super) fn advance_identity_mutation_epoch(&self) -> u64 {
        let caller = std::thread::current().id();
        let mut state = self.identity_mutation.lock().unwrap();
        debug_assert_eq!(state.owner.as_ref(), Some(&caller));
        debug_assert_ne!(state.depth, 0);
        state.epoch = state
            .epoch
            .checked_add(1)
            .expect("graph-text identity mutation epoch exhausted");
        state.epoch
    }
}

impl Drop for GraphTextIdentityMutationGuard<'_> {
    fn drop(&mut self) {
        let caller = std::thread::current().id();
        let mut state = self.gate.identity_mutation.lock().unwrap();
        debug_assert_eq!(state.owner.as_ref(), Some(&caller));
        debug_assert_ne!(state.depth, 0);
        state.depth = state.depth.saturating_sub(1);
        if state.depth == 0 {
            state.owner = None;
            self.gate.identity_mutation_changed.notify_all();
        }
    }
}

/// Process-local weak registry of independent writer gates. A live graph keeps
/// its gate alive; dead resources are pruned on the next open.
static GRAPH_TEXT_WRITE_GATE_REGISTRY: std::sync::OnceLock<
    std::sync::Mutex<
        std::collections::HashMap<CanonicalGraphResourceId, std::sync::Weak<GraphTextWriteGate>>,
    >,
> = std::sync::OnceLock::new();

pub(super) struct GraphTextWriteBinding {
    pub(super) resource_id: CanonicalGraphResourceId,
    pub(super) gate: Arc<GraphTextWriteGate>,
    pub(super) root: Dir,
}

pub(super) fn graph_text_write_binding_for_resource(
    root: &Path,
    projection_root: Option<&Dir>,
) -> io::Result<GraphTextWriteBinding> {
    graph_text_write_identity_acquisition_hook()?;
    let retained_root = match projection_root {
        Some(projection_root) => projection_root.try_clone()?,
        None => {
            let resolved = fs::canonicalize(root)?;
            Dir::open_ambient_dir(resolved, ambient_authority())?
        }
    };
    let resource_id = canonical_graph_resource_id(&retained_root)?;

    let registry = GRAPH_TEXT_WRITE_GATE_REGISTRY
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let mut registry = registry.lock().unwrap();
    registry.retain(|_, gate| gate.upgrade().is_some());
    if let Some(gate) = registry.get(&resource_id).and_then(|gate| gate.upgrade()) {
        return Ok(GraphTextWriteBinding {
            resource_id,
            gate,
            root: retained_root,
        });
    }

    let gate = Arc::new(GraphTextWriteGate::new());
    registry.insert(resource_id, Arc::downgrade(&gate));
    Ok(GraphTextWriteBinding {
        resource_id,
        gate,
        root: retained_root,
    })
}

pub(super) fn graph_text_write_identity_mismatch_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "ambient graph root no longer names the retained graph text resource",
    )
}

/// An admitted graph-text writer: the retained root capability every write
/// resolves its paths under, bound to the resource identity it was admitted
/// against.
pub(super) struct GraphTextWritePermit {
    pub(super) root: Dir,
    pub(super) resource_id: CanonicalGraphResourceId,
}

pub(super) struct GraphTextTarget {
    pub(super) chain: Vec<Dir>,
    pub(super) filename: String,
}

impl GraphTextTarget {
    pub(super) fn parent(&self) -> &Dir {
        self.chain
            .last()
            .expect("graph text target retains its parent chain")
    }
}

/// The exact live-path state shown to the user by a resolvable save conflict.
/// Bytes are retained beside this value in `ConflictAuthority`; keeping the
/// authority shape small makes it impossible to mistake ordinary load evidence
/// for override authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ConflictSnapshot {
    Present {
        revision: String,
        resource_identity: ContentDigest,
    },
    Absent,
}
