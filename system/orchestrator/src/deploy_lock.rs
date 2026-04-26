use anyhow::Result;
use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;

#[derive(Debug)]
pub struct DeployLock {
    _file: File,
}

impl DeployLock {
    /// Acquire an exclusive non-blocking flock on the lock file for `service`.
    /// Returns `Err` immediately if another deploy is already in progress.
    pub fn try_acquire(service: &str) -> Result<Self> {
        if service.is_empty()
            || !service
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
        {
            anyhow::bail!("invalid service name for lock: {:?}", service);
        }
        let path = crate::config::deploy_lock_path(service);
        Self::try_acquire_path(&path)
    }

    pub(crate) fn try_acquire_path(path: &str) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .open(path)
            .map_err(|e| anyhow::anyhow!("failed to open lock file {}: {}", path, e))?;

        // LOCK_EX | LOCK_NB: exclusive lock, return immediately if unavailable.
        // The OS releases the lock automatically when `file` is dropped (fd closed).
        let ret = unsafe {
            libc::flock(
                file.as_raw_fd(),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };

        if ret == 0 {
            Ok(DeployLock { _file: file })
        } else {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock {
                anyhow::bail!("deploy already in progress")
            } else {
                Err(err.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp_lock_path(dir: &TempDir, name: &str) -> String {
        dir.path().join(name).to_str().unwrap().to_string()
    }

    #[test]
    fn uncontested_acquire_succeeds() {
        let dir = TempDir::new().unwrap();
        let path = tmp_lock_path(&dir, "test.lock");
        assert!(DeployLock::try_acquire_path(&path).is_ok());
    }

    #[test]
    fn contested_acquire_fails_immediately() {
        let dir = TempDir::new().unwrap();
        let path = tmp_lock_path(&dir, "test.lock");

        let _lock1 = DeployLock::try_acquire_path(&path).unwrap();
        let lock2 = DeployLock::try_acquire_path(&path);

        assert!(lock2.is_err());
        let msg = lock2.unwrap_err().to_string();
        assert!(
            msg.contains("already in progress"),
            "expected 'already in progress', got: {msg}"
        );
    }

    #[test]
    fn try_acquire_rejects_path_traversal() {
        for bad in &["../etc/passwd", "foo/bar", "", "..", ".hidden", "foo.yml"] {
            let result = DeployLock::try_acquire(bad);
            assert!(
                result.is_err(),
                "expected error for service name {bad:?}, got Ok"
            );
        }
    }

    #[test]
    fn lock_released_after_drop() {
        let dir = TempDir::new().unwrap();
        let path = tmp_lock_path(&dir, "test.lock");

        {
            let _lock = DeployLock::try_acquire_path(&path).unwrap();
        } // drops here — flock released when File is closed

        // Must be re-acquirable
        assert!(DeployLock::try_acquire_path(&path).is_ok());
    }
}
