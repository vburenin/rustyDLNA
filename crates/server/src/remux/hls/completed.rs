//! Process-local reuse of completed indexes. Entries hold metadata only, never
//! descriptors or disk sidecars; cache eviction is therefore free to unlink the
//! media. Reopening still requires the normal validated completion stamp.
use super::Index;
use std::collections::VecDeque;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::sync::{LazyLock, Mutex};

const PARSER_VERSION: u32 = 1;
const MAX_ENTRIES: usize = 16;
const MAX_BYTES: usize = 16 * 1024 * 1024;
// Reserve the entire bounded container, including vacant entry capacity. The
// per-index estimate additionally counts its inline state conservatively.
const INDEX_BYTES_BUDGET: usize =
    MAX_BYTES - MAX_ENTRIES * std::mem::size_of::<(Identity, Index, usize)>();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Identity {
    parser_version: u32,
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Identity {
    pub(super) fn same_content(self, other: Self) -> bool {
        // Rename/unlink changes ctime without modifying an already pinned inode.
        self.parser_version == other.parser_version
            && self.device == other.device
            && self.inode == other.inode
            && self.length == other.length
            && self.modified == other.modified
    }

    pub(super) fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            parser_version: PARSER_VERSION,
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Default)]
struct CompletedIndexes {
    entries: VecDeque<(Identity, Index, usize)>,
    bytes: usize,
}

impl CompletedIndexes {
    fn get(&mut self, identity: Identity) -> Option<Index> {
        let position = self.entries.iter().position(|entry| entry.0 == identity)?;
        let entry = self.entries.remove(position)?;
        let index = entry.1.clone();
        self.entries.push_back(entry);
        Some(index)
    }

    fn insert(&mut self, identity: Identity, index: &Index) {
        if !index.finalized || index.fragments.is_empty() {
            return;
        }
        let bytes = index.retained_bytes();
        if bytes > INDEX_BYTES_BUDGET {
            return;
        }
        if let Some(position) = self.entries.iter().position(|entry| entry.0 == identity) {
            if let Some(entry) = self.entries.remove(position) {
                self.bytes -= entry.2;
            }
        }
        while self.entries.len() >= MAX_ENTRIES || self.bytes + bytes > INDEX_BYTES_BUDGET {
            if let Some(entry) = self.entries.pop_front() {
                self.bytes -= entry.2;
            } else {
                break;
            }
        }
        let mut index = index.clone();
        // A newly attached playlist generation may choose the known complete
        // maximum; an earlier growing generation's target is not transferable.
        index.native_targets.clear();
        self.entries.push_back((identity, index, bytes));
        self.bytes += bytes;
    }
}

static COMPLETED: LazyLock<Mutex<CompletedIndexes>> =
    LazyLock::new(|| Mutex::new(CompletedIndexes::default()));

pub(super) fn get(identity: Identity) -> Option<Index> {
    crate::lock_recover(&COMPLETED).get(identity)
}

pub(super) fn insert(identity: Identity, index: &Index) {
    crate::lock_recover(&COMPLETED).insert(identity, index);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_identity_and_lru_budget_control_reuse() {
        let mut cache = CompletedIndexes::default();
        let mut identity = Identity {
            parser_version: PARSER_VERSION,
            device: 1,
            inode: 1,
            length: 100,
            modified: (1, 0),
            changed: (1, 0),
        };
        let mut index = Index::default();
        cache.insert(identity, &index);
        assert!(
            cache.get(identity).is_none(),
            "incomplete indexes are rejected"
        );
        index.fragments.push(super::super::Segment {
            offset: 32,
            length: 68,
            duration: 1.0,
        });
        index.finalized = true;
        cache.insert(identity, &index);
        assert!(cache.get(identity).is_some());
        for changed in [
            Identity {
                parser_version: PARSER_VERSION + 1,
                ..identity
            },
            Identity {
                inode: 2,
                ..identity
            },
            Identity {
                length: 99,
                ..identity
            },
            Identity {
                modified: (1, 1),
                ..identity
            },
            Identity {
                changed: (1, 1),
                ..identity
            },
        ] {
            assert!(cache.get(changed).is_none());
        }
        let initial = identity;
        for inode in 2..=20 {
            identity.inode = inode;
            cache.insert(identity, &index);
        }
        assert!(cache.entries.len() <= MAX_ENTRIES);
        assert!(cache.bytes <= MAX_BYTES);
        assert!(cache.get(initial).is_none());
        assert!(cache.get(identity).is_some());
    }

    #[test]
    fn byte_budget_evicts_large_histories_before_the_entry_ceiling() {
        let identity = Identity {
            parser_version: PARSER_VERSION,
            device: 1,
            inode: 1,
            length: 100,
            modified: (1, 0),
            changed: (1, 0),
        };
        let mut index = Index {
            finalized: true,
            ..Index::default()
        };
        for _ in 0..100_000 {
            let segment = super::super::Segment {
                offset: 32,
                length: 16,
                duration: 1.0,
            };
            index.fragments.push(segment);
            index.segments.push(segment);
            index.fragment_timing.push(Some((0.0, 0.0)));
        }
        let mut cache = CompletedIndexes::default();
        for inode in 0..MAX_ENTRIES as u64 {
            cache.insert(Identity { inode, ..identity }, &index);
            assert!(cache.bytes <= MAX_BYTES);
        }
        assert!(cache.entries.len() < MAX_ENTRIES);
        assert!(cache
            .get(Identity {
                inode: 0,
                ..identity
            })
            .is_none());
        assert!(cache
            .get(Identity {
                inode: MAX_ENTRIES as u64 - 1,
                ..identity
            })
            .is_some());
    }
}
