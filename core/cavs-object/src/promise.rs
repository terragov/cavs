//! Objects a store knows about but does not hold.
//!
//! A metadata-only clone has the shape of a repository and almost none of its
//! bytes. Every tree page names chunks that are not here. Without somewhere to
//! record *why* they are not here, that store is indistinguishable from a
//! damaged one: verification reports gaps, garbage collection sees dangling
//! references, and a read fails with the same error it would give for
//! corruption.
//!
//! A promise is the difference. It says: this object is absent on purpose, and
//! here is who has it. That single fact turns three failures into three
//! ordinary states — a verification that passes, a collection that has nothing
//! to do, and a read that knows where to go.
//!
//! What a promise is not is a substitute for the hash. The id was known before
//! the object was, so whatever arrives is checked against it; a remote that
//! sends the wrong bytes fails exactly as it would have on any other path.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{ObjectError, Result};
use crate::id::ObjectId;

/// What a store can say about an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectPresence {
    /// Here, and readable.
    Local,
    /// Not here, deliberately, and this is who has it.
    Promised { remote: String },
    /// Not here, and nobody said it would be. This is the one that is a
    /// problem.
    Missing,
}

impl ObjectPresence {
    pub fn is_local(&self) -> bool {
        matches!(self, ObjectPresence::Local)
    }

    pub fn is_promised(&self) -> bool {
        matches!(self, ObjectPresence::Promised { .. })
    }

    /// Is this an absence that needs explaining?
    pub fn is_a_problem(&self) -> bool {
        matches!(self, ObjectPresence::Missing)
    }
}

/// Which remote promised what.
///
/// Append-only on disk, so recording a promise is one small write and a crash
/// mid-write loses at most the last line — which costs a re-fetch, not
/// correctness. It is compacted when the appends outgrow the live set.
#[derive(Debug, Default)]
pub struct PromiseLog {
    entries: BTreeMap<ObjectId, String>,
    /// Lines on disk, live or superseded, so compaction can be triggered on
    /// the ratio rather than on a timer.
    lines: usize,
}

impl PromiseLog {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn remote_for(&self, id: &ObjectId) -> Option<&str> {
        self.entries.get(id).map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ObjectId, &str)> {
        self.entries
            .iter()
            .map(|(id, remote)| (id, remote.as_str()))
    }

    /// Every object promised by one remote.
    pub fn by_remote(&self, remote: &str) -> Vec<ObjectId> {
        self.entries
            .iter()
            .filter(|(_, who)| who.as_str() == remote)
            .map(|(id, _)| *id)
            .collect()
    }

    /// The remotes that promised anything, and how much.
    pub fn remotes(&self) -> BTreeMap<&str, usize> {
        let mut out: BTreeMap<&str, usize> = BTreeMap::new();
        for remote in self.entries.values() {
            *out.entry(remote.as_str()).or_default() += 1;
        }
        out
    }

    fn load(path: &Path) -> Result<PromiseLog> {
        let raw = match fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PromiseLog::default()),
            Err(e) => return Err(io_err("read the promise log")(e)),
        };
        let mut log = PromiseLog::default();
        for line in raw.lines() {
            log.lines += 1;
            let Some((id, remote)) = line.split_once(' ') else {
                continue;
            };
            let Some(id) = ObjectId::parse_hex(id) else {
                continue;
            };
            if remote == FORGOTTEN {
                log.entries.remove(&id);
            } else {
                log.entries.insert(id, remote.to_string());
            }
        }
        Ok(log)
    }
}

/// Written in place of a remote name to record that a promise was kept, since
/// the log is append-only and lines are never edited.
const FORGOTTEN: &str = "-";

/// Compact once the log holds this many times more lines than live promises.
const COMPACT_RATIO: usize = 4;

/// The promise log inside an object store.
pub struct Promises {
    path: PathBuf,
}

impl Promises {
    pub fn open(store_root: &Path) -> Result<Promises> {
        Ok(Promises {
            path: store_root.join("promises"),
        })
    }

    pub fn read(&self) -> Result<PromiseLog> {
        PromiseLog::load(&self.path)
    }

    /// Record that `remote` has these objects.
    pub fn promise(&self, remote: &str, ids: &[ObjectId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        if remote.contains(char::is_whitespace) || remote == FORGOTTEN || remote.is_empty() {
            return Err(ObjectError::Corrupt(format!(
                "'{remote}' is not a usable remote name in a promise log"
            )));
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io_err("open the promise log"))?;
        let mut buffer = String::with_capacity(ids.len() * 72);
        for id in ids {
            buffer.push_str(&id.to_hex());
            buffer.push(' ');
            buffer.push_str(remote);
            buffer.push('\n');
        }
        file.write_all(buffer.as_bytes())
            .map_err(io_err("write the promise log"))?;
        file.sync_all().map_err(io_err("sync the promise log"))?;
        Ok(())
    }

    /// Record that promises were kept: the objects are here now.
    pub fn fulfilled(&self, ids: &[ObjectId]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(io_err("open the promise log"))?;
        let mut buffer = String::with_capacity(ids.len() * 68);
        for id in ids {
            buffer.push_str(&id.to_hex());
            buffer.push(' ');
            buffer.push_str(FORGOTTEN);
            buffer.push('\n');
        }
        file.write_all(buffer.as_bytes())
            .map_err(io_err("write the promise log"))?;
        file.sync_all().map_err(io_err("sync the promise log"))?;
        self.compact_if_worthwhile()?;
        Ok(())
    }

    /// Rewrite the log as just its live entries, when the dead ones outweigh
    /// them enough to be worth the write.
    pub fn compact_if_worthwhile(&self) -> Result<bool> {
        let log = self.read()?;
        if log.lines <= COMPACT_RATIO * log.entries.len().max(1) {
            return Ok(false);
        }
        self.rewrite(&log)?;
        Ok(true)
    }

    pub fn compact(&self) -> Result<()> {
        let log = self.read()?;
        self.rewrite(&log)
    }

    fn rewrite(&self, log: &PromiseLog) -> Result<()> {
        let temporary = self.path.with_extension("tmp");
        {
            let mut file =
                fs::File::create(&temporary).map_err(io_err("rewrite the promise log"))?;
            for (id, remote) in log.iter() {
                writeln!(file, "{} {remote}", id.to_hex())
                    .map_err(io_err("rewrite the promise log"))?;
            }
            file.sync_all().map_err(io_err("sync the promise log"))?;
        }
        fs::rename(&temporary, &self.path).map_err(io_err("publish the promise log"))?;
        Ok(())
    }
}

fn io_err(what: &'static str) -> impl Fn(std::io::Error) -> ObjectError {
    move |e| ObjectError::Corrupt(format!("failed to {what}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u8) -> ObjectId {
        ObjectId::from_blake3([n; 32])
    }

    fn promises(dir: &tempfile::TempDir) -> Promises {
        Promises::open(dir.path()).unwrap()
    }

    #[test]
    fn a_store_with_no_promises_has_none() {
        let dir = tempfile::tempdir().unwrap();
        let log = promises(&dir).read().unwrap();
        assert!(log.is_empty());
        assert_eq!(log.remote_for(&id(1)), None);
    }

    #[test]
    fn a_promise_says_who_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        promises.promise("origin", &[id(1), id(2)]).unwrap();
        promises.promise("backup", &[id(3)]).unwrap();

        let log = promises.read().unwrap();
        assert_eq!(log.remote_for(&id(1)), Some("origin"));
        assert_eq!(log.remote_for(&id(3)), Some("backup"));
        assert_eq!(log.remote_for(&id(9)), None);
        assert_eq!(log.by_remote("origin"), vec![id(1), id(2)]);
        assert_eq!(log.remotes()["origin"], 2);
    }

    #[test]
    fn a_promise_survives_reopening() {
        let dir = tempfile::tempdir().unwrap();
        promises(&dir).promise("origin", &[id(4)]).unwrap();
        assert_eq!(
            promises(&dir).read().unwrap().remote_for(&id(4)),
            Some("origin")
        );
    }

    #[test]
    fn a_kept_promise_is_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        promises.promise("origin", &[id(1), id(2)]).unwrap();
        promises.fulfilled(&[id(1)]).unwrap();

        let log = promises.read().unwrap();
        assert_eq!(log.remote_for(&id(1)), None);
        assert_eq!(log.remote_for(&id(2)), Some("origin"));
        assert_eq!(log.len(), 1);
    }

    /// The last word wins, so an object re-promised by a second remote is
    /// fetched from that one.
    #[test]
    fn the_latest_promise_is_the_one_that_counts() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        promises.promise("origin", &[id(1)]).unwrap();
        promises.promise("mirror", &[id(1)]).unwrap();
        assert_eq!(promises.read().unwrap().remote_for(&id(1)), Some("mirror"));
    }

    /// The log is append-only, so it grows even as promises are kept.
    /// Compaction keeps that from being unbounded, without ever changing what
    /// it says.
    #[test]
    fn compaction_shrinks_the_log_without_changing_it() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        let all: Vec<ObjectId> = (0..100).map(id).collect();
        promises.promise("origin", &all).unwrap();
        for id in &all[..90] {
            promises.fulfilled(std::slice::from_ref(id)).unwrap();
        }

        let before = promises.read().unwrap();
        let size_before = std::fs::metadata(&promises.path).unwrap().len();
        promises.compact().unwrap();
        let after = promises.read().unwrap();

        assert_eq!(after.len(), before.len());
        assert_eq!(after.len(), 10);
        for (id, remote) in before.iter() {
            assert_eq!(after.remote_for(id), Some(remote));
        }
        assert!(std::fs::metadata(&promises.path).unwrap().len() < size_before);
    }

    /// A remote name with a space in it would make the log ambiguous on the
    /// next read, so it is refused at the point it would be written.
    #[test]
    fn an_unusable_remote_name_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        assert!(promises.promise("two words", &[id(1)]).is_err());
        assert!(promises.promise("-", &[id(1)]).is_err());
        assert!(promises.promise("", &[id(1)]).is_err());
        assert!(promises.read().unwrap().is_empty());
    }

    #[test]
    fn a_damaged_line_is_skipped_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let promises = promises(&dir);
        promises.promise("origin", &[id(1)]).unwrap();
        // A crash mid-append leaves a partial line. Losing it costs a
        // re-fetch; refusing to open the store would cost far more.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&promises.path)
            .unwrap();
        use std::io::Write;
        write!(file, "not-a-line-at-all").unwrap();
        drop(file);

        let log = promises.read().unwrap();
        assert_eq!(log.remote_for(&id(1)), Some("origin"));
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn presence_tells_the_three_states_apart() {
        assert!(ObjectPresence::Local.is_local());
        assert!(!ObjectPresence::Local.is_a_problem());
        assert!(ObjectPresence::Promised {
            remote: "origin".into()
        }
        .is_promised());
        assert!(!ObjectPresence::Promised {
            remote: "origin".into()
        }
        .is_a_problem());
        assert!(ObjectPresence::Missing.is_a_problem());
    }
}
