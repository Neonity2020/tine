use serde::{Deserialize, Serialize};

use super::ContentDigest;

/// Semantic root identity retained by current engine records after the old
/// physical index implementation was removed.
///
/// The frozen empty digest preserves current record bytes; storage mechanics
/// no longer interpret or construct a tree behind this value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct SemanticIndexRoot(ContentDigest);

impl SemanticIndexRoot {
    pub(crate) const fn empty() -> Self {
        Self(ContentDigest::from_bytes([
            0xf3, 0x10, 0xe8, 0x1c, 0x3e, 0x02, 0x5d, 0x12, 0xd8, 0x62, 0xc3, 0x27, 0xd6, 0xae,
            0xdf, 0xfd, 0x71, 0x56, 0x64, 0xb1, 0x0f, 0xad, 0x36, 0xec, 0xca, 0x1c, 0xa7, 0x3f,
            0xb8, 0x0e, 0x41, 0xf3,
        ]))
    }

    pub(crate) const fn digest(self) -> ContentDigest {
        self.0
    }

    pub(crate) const fn from_digest(digest: ContentDigest) -> Self {
        Self(digest)
    }
}

impl Default for SemanticIndexRoot {
    fn default() -> Self {
        Self::empty()
    }
}

pub(crate) type LogseqClaimIndexRoot = SemanticIndexRoot;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_empty_root_keeps_the_current_record_identity() {
        let root = SemanticIndexRoot::empty();
        assert_eq!(
            root.digest().to_string(),
            "f310e81c3e025d12d862c327d6aedffd715664b10fad36ecca1ca73fb80e41f3"
        );
        assert_eq!(
            postcard::to_allocvec(&root).unwrap(),
            postcard::to_allocvec(&root.digest()).unwrap(),
            "the Tine-owned semantic wrapper must preserve current record bytes"
        );
        assert_eq!(
            postcard::from_bytes::<SemanticIndexRoot>(&postcard::to_allocvec(&root).unwrap())
                .unwrap(),
            root
        );
    }
}
