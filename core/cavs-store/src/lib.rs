//! Content-addressable index for CAVS-1.
//!
//! Maps chunk hashes to chunk-table indices and tracks reference counts.
//! The packer uses it to deduplicate chunks at ingest time; a future
//! server/client can reuse it for session `have-set` reconciliation and GC.

mod global;
pub mod packfile;
mod segindex;
pub use global::{
    AssetRecord, ChunkInfo, ChunkLocation, CoalesceStats, FragmentationReport, GlobalStore,
    IndexReport, PackFragmentation, RepackOutcome, RepackPlan, Result, StoreError, StoreLayout,
    StoreSegment, StoreStats, StoreTrack, JOURNAL_MIN_BYTES,
};

use cavs_hash::ChunkHash;
use std::collections::HashMap;

/// In-memory content-addressable index: hash -> (chunk index, refcount).
#[derive(Debug, Default)]
pub struct CasIndex {
    map: HashMap<ChunkHash, Entry>,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    index: u32,
    refcount: u64,
}

/// Result of interning a hash into the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Interned {
    /// First time seen; caller must store the payload under this new index.
    New(u32),
    /// Already present; payload must NOT be stored again.
    Existing(u32),
}

impl Interned {
    pub fn index(&self) -> u32 {
        match *self {
            Interned::New(i) | Interned::Existing(i) => i,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Interned::New(_))
    }
}

impl CasIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Intern `hash`. If unseen, it is assigned `next_index` and returned as
    /// `New`; otherwise the existing index is returned and its refcount bumped.
    pub fn intern(&mut self, hash: ChunkHash, next_index: u32) -> Interned {
        match self.map.entry(hash) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                e.get_mut().refcount += 1;
                Interned::Existing(e.get().index)
            }
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert(Entry {
                    index: next_index,
                    refcount: 1,
                });
                Interned::New(next_index)
            }
        }
    }

    pub fn get(&self, hash: &ChunkHash) -> Option<u32> {
        self.map.get(hash).map(|e| e.index)
    }

    pub fn refcount(&self, hash: &ChunkHash) -> u64 {
        self.map.get(hash).map(|e| e.refcount).unwrap_or(0)
    }

    /// Number of distinct chunks known.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Total references across all chunks (i.e. logical chunk count before dedup).
    pub fn total_refs(&self) -> u64 {
        self.map.values().map(|e| e.refcount).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cavs_hash::hash_chunk;

    #[test]
    fn intern_dedupes() {
        let mut idx = CasIndex::new();
        let a = hash_chunk(b"aaa");
        let b = hash_chunk(b"bbb");

        assert_eq!(idx.intern(a, 0), Interned::New(0));
        assert_eq!(idx.intern(b, 1), Interned::New(1));
        assert_eq!(idx.intern(a, 2), Interned::Existing(0));
        assert_eq!(idx.intern(a, 2), Interned::Existing(0));

        assert_eq!(idx.len(), 2);
        assert_eq!(idx.refcount(&a), 3);
        assert_eq!(idx.refcount(&b), 1);
        assert_eq!(idx.total_refs(), 4);
        assert_eq!(idx.get(&a), Some(0));
    }
}
