// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Per-zone advisory config lock (`flock(2)`), held for the whole of
//! create/delete/update/reprovision so two vmadm operations never race a
//! zone's config. An OS-level advisory lock (not an `O_EXCL` lockfile) so
//! a crashed holder's lock is released by the kernel — no stale-lockfile
//! footgun. This is the only `unsafe` in the crate, isolated here.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd as _;
use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::{Result, VmadmError};

/// Default lock directory (the SmartOS `zoneConfigLockpath` lives under
/// `/var/run`).
pub const LOCK_DIR: &str = "/var/run";

/// A held zone lock; releases on drop (the kernel drops the `flock` when
/// the file descriptor closes).
#[derive(Debug)]
pub struct ZoneLock {
    _file: File,
    path: PathBuf,
}

impl ZoneLock {
    /// Block until the lock for `uuid` is held (under [`LOCK_DIR`]).
    pub async fn acquire(uuid: Uuid) -> Result<Self> {
        Self::acquire_in(Path::new(LOCK_DIR), uuid).await
    }

    /// Block until the lock for `uuid` under `dir` is held. The blocking
    /// `flock` runs on a blocking thread so it never stalls the runtime.
    pub async fn acquire_in(dir: &Path, uuid: Uuid) -> Result<Self> {
        let path = lock_path(dir, uuid);
        let p = path.clone();
        tokio::task::spawn_blocking(move || flock(&p, false))
            .await
            .map_err(|e| VmadmError::Payload(format!("lock task join: {e}")))?
            .map(|file| ZoneLock { _file: file, path })
    }

    /// Try to take the lock without blocking; `Ok(None)` when another
    /// holder has it. Used by tests and by callers that prefer to fail
    /// fast rather than queue.
    pub async fn try_acquire_in(dir: &Path, uuid: Uuid) -> Result<Option<Self>> {
        let path = lock_path(dir, uuid);
        let p = path.clone();
        let res = tokio::task::spawn_blocking(move || flock(&p, true))
            .await
            .map_err(|e| VmadmError::Payload(format!("lock task join: {e}")))?;
        match res {
            Ok(file) => Ok(Some(ZoneLock { _file: file, path })),
            Err(VmadmError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::WouldBlock =>
            {
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// The lockfile path being held (for diagnostics).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn lock_path(dir: &Path, uuid: Uuid) -> PathBuf {
    dir.join(format!("vm.{uuid}.config.lockfile"))
}

/// Open the lockfile and take an exclusive `flock`. `nonblocking` adds
/// `LOCK_NB` (returns `WouldBlock` when contended).
fn flock(path: &Path, nonblocking: bool) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| VmadmError::Io {
            path: path.to_path_buf(),
            source: e,
        })?;
    let mut op = libc::LOCK_EX;
    if nonblocking {
        op |= libc::LOCK_NB;
    }
    // SAFETY: `file` owns a valid fd for the duration of the call; flock
    // takes only the fd and the operation flags.
    let rc = unsafe { libc::flock(file.as_raw_fd(), op) };
    if rc != 0 {
        return Err(VmadmError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::last_os_error(),
        });
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        "11111111-1111-1111-1111-111111111111".parse().unwrap()
    }

    #[tokio::test]
    async fn acquire_then_release_then_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let lock = ZoneLock::acquire_in(dir.path(), uuid()).await.unwrap();
        assert!(
            lock.path()
                .ends_with(format!("vm.{}.config.lockfile", uuid()))
        );
        drop(lock);
        // Releasing lets a fresh acquire succeed.
        let _again = ZoneLock::acquire_in(dir.path(), uuid()).await.unwrap();
    }

    #[tokio::test]
    async fn try_acquire_conflicts_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let held = ZoneLock::try_acquire_in(dir.path(), uuid())
            .await
            .unwrap()
            .expect("first acquire");
        // A second non-blocking attempt sees it held.
        let second = ZoneLock::try_acquire_in(dir.path(), uuid()).await.unwrap();
        assert!(second.is_none());
        drop(held);
        // After release it's available again.
        let third = ZoneLock::try_acquire_in(dir.path(), uuid()).await.unwrap();
        assert!(third.is_some());
    }
}
