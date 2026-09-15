//! Graph's graph-text scope: the canonical resource id, the scope and its binding,
//! admitting a writer, the identity-mutation lock, and resolving a write target.

use super::*;

impl Graph {
    /// Stable identity of the exact no-follow directory capability retained at
    /// graph open. This is the only graph-root identity accepted by projection
    /// enrollment; the ambient path in `Graph::root` is not authority.
    pub fn canonical_resource_id(&self) -> io::Result<CanonicalGraphResourceId> {
        let root = self.projection_root.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "graph has no retained no-follow projection capability",
            )
        })?;
        canonical_graph_resource_id(root)
    }

    pub fn graph_text_scope_version(&self) -> u32 {
        self.graph_text_scope.version()
    }

    /// Snapshot the graph-text discovery policy without granting any write
    /// authority. Launch backups use the same eligibility rules as graph reads.
    pub fn graph_text_scope(&self) -> GraphTextScope {
        self.graph_text_scope.clone()
    }

    /// Bind the precomputed effective graph-text policy to the exact retained
    /// graph-root capability without walking or hashing graph contents.
    pub fn graph_text_scope_binding(&self) -> io::Result<GraphTextScopeBinding> {
        Ok(self
            .graph_text_scope
            .bind_graph_resource(self.canonical_resource_id()?))
    }

    pub(super) fn admit_graph_text_writer(&self) -> io::Result<GraphTextWritePermit> {
        let binding = self.graph_text_write_binding()?;
        graph_text_write_after_admission_hook()?;
        let permit = GraphTextWritePermit {
            root: binding.root.try_clone()?,
            resource_id: binding.resource_id,
        };
        graph_text_write_after_identity_check_hook();
        Ok(permit)
    }

    pub(super) fn admit_retained_graph_text_writer(&self) -> io::Result<GraphTextWritePermit> {
        let binding = self.graph_text_write_binding()?;
        if self.canonical_resource_id()? != binding.resource_id {
            return Err(graph_text_write_identity_mismatch_error());
        }
        Ok(GraphTextWritePermit {
            root: binding.root.try_clone()?,
            resource_id: binding.resource_id,
        })
    }

    pub(super) fn graph_text_write_binding(&self) -> io::Result<&GraphTextWriteBinding> {
        self.graph_text_write_binding.as_ref().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("graph text resource identity is unavailable: {error}"),
            )
        })
    }

    pub(super) fn lock_graph_text_identity_mutation(
        &self,
    ) -> io::Result<GraphTextIdentityMutationGuard<'_>> {
        Ok(self
            .graph_text_write_binding()?
            .gate
            .lock_identity_mutation())
    }

    pub(super) fn graph_text_permit_root<'a>(
        &self,
        permit: &'a GraphTextWritePermit,
    ) -> io::Result<&'a Dir> {
        let binding = self.graph_text_write_binding()?;
        if permit.resource_id != binding.resource_id {
            return Err(graph_text_write_identity_mismatch_error());
        }
        Ok(&permit.root)
    }

    pub(super) fn graph_text_target(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
        create_parent: bool,
    ) -> io::Result<GraphTextTarget> {
        let relative = path.strip_prefix(&self.root).map_err(|_| bad_path())?;
        let components = relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(component) => component
                    .to_str()
                    .filter(|component| projection_component_is_portable(component))
                    .map(str::to_owned)
                    .ok_or_else(bad_path),
                _ => Err(bad_path()),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let (filename, parents) = components.split_last().ok_or_else(bad_path)?;
        let mut chain = vec![self.graph_text_permit_root(permit)?.try_clone()?];
        for component in parents {
            let current = chain
                .last()
                .expect("graph text capability chain contains root");
            match projection_real_directory(current, component) {
                Ok(()) => {}
                Err(error) if create_parent && error.kind() == io::ErrorKind::NotFound => {
                    create_projection_chain_component(current, component)?;
                }
                Err(error) => return Err(error),
            }
            chain.push(open_projection_dir_nofollow(current, component)?);
        }
        Ok(GraphTextTarget {
            chain,
            filename: filename.clone(),
        })
    }

    pub(super) fn graph_text_create_dir_all(
        &self,
        permit: &GraphTextWritePermit,
        path: &Path,
    ) -> io::Result<()> {
        let sentinel = path.join(".tine-capability-directory");
        let target = self.graph_text_target(permit, &sentinel, true)?;
        sync_projection_chain_required(&target.chain)
    }
}
