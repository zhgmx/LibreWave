//! Exclusive, process-scoped lock for installation and runtime ownership changes.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

pub struct OperationLock(File);
impl OperationLock {
    /// Acquire a private lock without following a symbolic link.
    /// # Errors
    /// Returns an error for unsafe metadata or a concurrent lifecycle operation.
    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let metadata = file.metadata()?;
        // SAFETY: geteuid has no arguments or borrowed storage.
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0
        {
            return Err(io::Error::other(
                "lifecycle state directory has unsafe ownership or permissions",
            ));
        }
        // SAFETY: the owned file descriptor remains open for this lock's lifetime.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "another LibreWave lifecycle operation is running",
            ));
        }
        Ok(Self(file))
    }
}
impl Drop for OperationLock {
    fn drop(&mut self) {
        // SAFETY: this descriptor belongs to this lock and is still open.
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_lock_fails_and_process_owned_release_is_reusable() {
        let path = std::env::temp_dir().join(format!("librewave-lock-test-{}", std::process::id()));
        std::fs::create_dir(&path).expect("test directory");
        let first = OperationLock::acquire(&path).expect("first lock");
        assert!(OperationLock::acquire(&path).is_err());
        drop(first);
        drop(OperationLock::acquire(&path).expect("released lock"));
        std::fs::remove_dir(path).expect("remove test lock");
    }
}
