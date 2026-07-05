use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;

pub struct InstanceGuard {
    file: File,
}

impl InstanceGuard {
    pub fn acquire() -> io::Result<Option<Self>> {
        let dir = crate::config::app_support_dir();
        Self::acquire_in(&dir)
    }

    fn acquire_in(dir: &std::path::Path) -> io::Result<Option<Self>> {
        crate::config::create_private_dir(dir)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.join("instance.lock"))?;
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            Ok(Some(Self { file }))
        } else {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Ok(None)
            } else {
                Err(err)
            }
        }
    }
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_allows_only_one_live_guard() {
        let dir =
            std::env::temp_dir().join(format!("rustquit-instance-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let first = InstanceGuard::acquire_in(&dir).unwrap().unwrap();
        assert!(InstanceGuard::acquire_in(&dir).unwrap().is_none());
        drop(first);
        assert!(InstanceGuard::acquire_in(&dir).unwrap().is_some());

        std::fs::remove_dir_all(dir).unwrap();
    }
}
