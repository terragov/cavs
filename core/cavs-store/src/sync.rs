//! How a write is made durable.
//!
//! Every place this crate reports a write as done first asks the operating
//! system to make it so, and the operating system offers more than one
//! meaning of "so". On macOS, `fsync(2)` hands the data to the drive and
//! returns; the drive may still hold it in a volatile cache, which is what
//! `F_FULLFSYNC` flushes — and `F_FULLFSYNC` is what Rust's `sync_all` and
//! `sync_data` issue there, at a cost of milliseconds per call on a laptop
//! SSD. Linux `fsync(2)` does both at once. Most database engines default to
//! `fsync(2)` (SQLite unless `PRAGMA fullfsync`, PostgreSQL unless
//! `wal_sync_method = fsync_writethrough`), and the difference decides what
//! a small transaction costs.
//!
//! The store does not know what it is asked to hold, so the choice is the
//! caller's: [`SyncMode`], set on a [`crate::GlobalStore`].

use std::fs::File;
use std::io;

/// What a sync point in the store waits for before reporting success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// The data is on stable media: `F_FULLFSYNC` on macOS (the drive's
    /// cache flushed), `fsync(2)` elsewhere. What a power cut cannot take.
    #[default]
    Full,
    /// `fsync(2)`: the data has reached the drive. On macOS the drive's own
    /// cache is not flushed, so a power cut can still lose it; a kernel panic
    /// or a killed process cannot. What most engines call durable by default.
    Fsync,
    /// Ordering only: writes issued before the barrier reach the drive
    /// before writes issued after it, and the call returns without waiting
    /// for either. `F_BARRIERFSYNC` on macOS, `fdatasync(2)` elsewhere.
    Barrier,
}

/// Sync `file` (or a directory handle) as `mode` says.
pub fn sync_file(file: &File, mode: SyncMode) -> io::Result<()> {
    match mode {
        SyncMode::Full => file.sync_all(),
        SyncMode::Fsync => plain_fsync(file),
        SyncMode::Barrier => barrier(file),
    }
}

#[cfg(unix)]
fn plain_fsync(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: fsync on a file descriptor this `File` owns and keeps open for
    // the duration of the call.
    let code = unsafe { libc::fsync(file.as_raw_fd()) };
    if code == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn plain_fsync(file: &File) -> io::Result<()> {
    file.sync_all()
}

#[cfg(target_os = "macos")]
fn barrier(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd as _;
    // SAFETY: as above; F_BARRIERFSYNC takes no argument.
    let code = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_BARRIERFSYNC) };
    if code == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
fn barrier(file: &File) -> io::Result<()> {
    file.sync_data()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn every_mode_syncs_a_file_and_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        for mode in [SyncMode::Full, SyncMode::Fsync, SyncMode::Barrier] {
            let path = dir.path().join(format!("{mode:?}"));
            let mut f = File::create(&path).unwrap();
            f.write_all(b"bytes").unwrap();
            sync_file(&f, mode).unwrap();
            sync_file(&File::open(dir.path()).unwrap(), mode).unwrap();
        }
    }
}
