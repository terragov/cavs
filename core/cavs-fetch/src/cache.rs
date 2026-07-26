//! Persistent content-addressable chunk cache. Wire-compatible with the
//! `cavs-client` cache layout (`<root>/<ab>/<hex>`, raw payloads), so an application
//! embedding this library shares a cache with the CLI.

use anyhow::{Context, Result};
use cavs_hash::{hash_chunk, to_hex, ChunkHash};
use std::path::{Path, PathBuf};

pub struct ChunkCache {
    root: PathBuf,
}

impl ChunkCache {
    pub fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root)
            .with_context(|| format!("cannot create cache dir {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn path_for_hex(&self, hex: &str) -> PathBuf {
        self.root.join(&hex[..2]).join(hex)
    }

    pub fn contains(&self, hex: &str) -> bool {
        hex.len() == 64 && self.path_for_hex(hex).is_file()
    }

    pub fn put(&self, hash: &ChunkHash, payload: &[u8]) -> Result<()> {
        let hex = to_hex(hash);
        let path = self.path_for_hex(&hex);
        if path.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(path.parent().unwrap())?;
        // The missing set is deduplicated, so no two workers write the same
        // hash concurrently; a per-hash `.tmp` + atomic rename is enough to
        // keep a crashed write from leaving a torn entry.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, payload)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn get(&self, hash: &ChunkHash) -> Result<Option<Vec<u8>>> {
        let hex = to_hex(hash);
        let path = self.path_for_hex(&hex);
        let Ok(payload) = std::fs::read(&path) else {
            return Ok(None);
        };
        if hash_chunk(&payload) != *hash {
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
        Ok(Some(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ChunkCache::open(dir.path()).unwrap();
        let payload = b"hello chunk".to_vec();
        let hash = hash_chunk(&payload);
        assert!(!cache.contains(&to_hex(&hash)));
        cache.put(&hash, &payload).unwrap();
        assert!(cache.contains(&to_hex(&hash)));
        assert_eq!(cache.get(&hash).unwrap(), Some(payload));
    }
}
