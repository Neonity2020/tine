//! The depth-bounded BlockDto walk and the rename rewrite bound, plus the
//! pinned save authority and exact-graph validation records.

use super::*;

#[derive(Clone, Copy)]
#[cfg(test)]
struct BlockDtoWalkFrame<'a> {
    blocks: &'a [BlockDto],
    next: usize,
    depth: usize,
}

#[cfg(test)]
pub(super) struct BlockDtoWalk<'a> {
    frames: [BlockDtoWalkFrame<'a>; MAX_BLOCK_DEPTH],
    len: usize,
}

#[cfg(test)]
impl<'a> BlockDtoWalk<'a> {
    pub(super) fn new(blocks: &'a [BlockDto]) -> Self {
        let empty = BlockDtoWalkFrame {
            blocks: &[],
            next: 0,
            depth: 0,
        };
        let mut frames = [empty; MAX_BLOCK_DEPTH];
        let len = usize::from(!blocks.is_empty());
        if len != 0 {
            frames[0] = BlockDtoWalkFrame {
                blocks,
                next: 0,
                depth: 1,
            };
        }
        Self { frames, len }
    }

    pub(super) fn next(&mut self) -> io::Result<Option<(&'a BlockDto, usize)>> {
        loop {
            if self.len == 0 {
                return Ok(None);
            }
            let frame = &mut self.frames[self.len - 1];
            if frame.next == frame.blocks.len() {
                self.len -= 1;
                continue;
            }
            let block = &frame.blocks[frame.next];
            frame.next = frame.next.checked_add(1).ok_or_else(allocation_overflow)?;
            let depth = frame.depth;
            if !block.children.is_empty() {
                if self.len == MAX_BLOCK_DEPTH {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "graph page block nesting exceeds 128 levels",
                    ));
                }
                self.frames[self.len] = BlockDtoWalkFrame {
                    blocks: &block.children,
                    next: 0,
                    depth: depth.checked_add(1).ok_or_else(allocation_overflow)?,
                };
                self.len += 1;
            }
            return Ok(Some((block, depth)));
        }
    }
}

pub(super) fn rename_rewrite_upper_bound(
    content: &str,
    renames: &std::collections::HashMap<String, String>,
    encode_org_file_links: bool,
) -> io::Result<u64> {
    let raw_max_name = usize_to_u64(renames.values().map(String::len).max().unwrap_or(0))?;
    // A reserved-only title can grow to three bytes per input byte in an encoded
    // Org file-link stem. Ordinary refs/tags and the second tags:: pass stay raw.
    let max_name = if encode_org_file_links && content.contains("[[file:") {
        checked_mul_bytes(raw_max_name, 3)?
    } else {
        raw_max_name
    };
    let candidates = checked_add_bytes(
        usize_to_u64(
            content
                .bytes()
                .filter(|byte| matches!(*byte, b'#' | b'[' | b','))
                .count(),
        )?,
        usize_to_u64(if content.contains("::") {
            content
                .lines()
                .filter(|line| {
                    line.as_bytes()
                        .windows(6)
                        .any(|window| window.eq_ignore_ascii_case(b"tags::"))
                })
                .count()
        } else {
            0
        })?,
    )?;
    let replacement_growth = checked_mul_bytes(candidates, checked_add_bytes(max_name, 8)?)?;
    let code_delimiters = usize_to_u64(
        content
            .bytes()
            .filter(|byte| matches!(*byte, b'`' | b'~'))
            .count(),
    )?;
    let code_ranges = checked_mul_bytes(
        code_delimiters,
        checked_mul_bytes(4, usize_to_u64(std::mem::size_of::<usize>())?)?,
    )?;
    let segment_headers = checked_mul_bytes(
        candidates,
        checked_mul_bytes(2, usize_to_u64(std::mem::size_of::<String>())?)?,
    )?;
    let content_len = usize_to_u64(content.len())?;
    let scanner_scratch = checked_mul_bytes(content_len, 3)?;
    checked_add_bytes(
        checked_add_bytes(
            checked_add_bytes(content_len, replacement_growth)?,
            code_ranges,
        )?,
        checked_add_bytes(segment_headers, scanner_scratch)?,
    )
}

/// What entitles a save to write the file its page is pinned to.
pub(super) enum PinnedSaveAuthority<'a> {
    /// An ordinary editor save. Proves exact path ownership here and NOTHING
    /// about physical identity, because the identity decision belongs after the
    /// byte comparison, not before it.
    ///
    /// Ordinary frontend saves carry the loaded revision in the separate
    /// `base_rev` argument; `PageDto.rev` is NOT part of the working-store DTO
    /// that `pageToDto` builds, so it must never be read as one.
    ///
    /// Refusing a changed inode up front (as an earlier journal projection
    /// did) would pre-empt the base-revision check and turn every
    /// rename-based external write (Syncthing, Dropbox, Logseq OG, VS Code, any
    /// temp+rename tool) into a permanent
    /// `path-pinned page does not match its captured exact owner`. The frontend
    /// classifies that as transient and retries it forever, so the page becomes
    /// silently unsaveable — GH #254.
    ///
    /// The byte comparison below is the stronger proof anyway: `base_rev` is
    /// SHA-256 of the exact bytes the editor loaded, and under the storage threat
    /// model (`specs/notes/2026-08-07-trust-model-and-threat-model-decision.md`)
    /// a byte-forging adversary is out of scope. Equal bytes therefore mean the
    /// same state regardless of which inode carries them.
    OrdinaryEditorSave {
        loaded_revision: Option<&'a str>,
        prospective_editor: bool,
    },
    /// The user was shown the conflict and chose to keep their own edits.
    ///
    /// A stale revision and a stale identity ARE the conflict being resolved —
    /// requiring either to match makes "keep mine" refuse exactly when it is
    /// needed, leaving discard-my-work as the only exit the app offers. The pin
    /// must still resolve to a retained file owner at the validated path, so an
    /// override cannot be redirected onto a file this page never came from.
    UserOverride(&'a ConflictSnapshot),
}

pub(super) struct ExactGraphLoadedPage {
    pub(super) entry: PageEntry,
    pub(super) document: Document,
    pub(super) content: String,
    pub(super) revision: String,
    pub(super) file_identity: ContentDigest,
}

pub(super) struct ExactGraphValidation {
    pub(super) target: Option<ExactGraphLoadedPage>,
    pub(super) requested_identity_elsewhere: bool,
    pub(super) creation_proof: Option<DirectCreationProof>,
}
