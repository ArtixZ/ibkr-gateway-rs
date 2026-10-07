use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

pub fn nonce() -> Result<String> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("system entropy failed: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "runtime directory must be owned by you, not a symlink, and mode 0700"
    );
    Ok(())
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", nonce()?));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .context("create private temporary file")?;
    let result = (|| -> Result<()> {
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        if let Err(error) = fs::remove_file(&temporary) {
            eprintln!("temporary-file cleanup failed: {error}");
        }
    }
    result
}

pub fn read_private(path: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    let f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let meta = f.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0
            && meta.len() <= 65536,
        "state file must be private, regular, owned by you, and bounded"
    );
    let mut contents = Vec::new();
    f.take(65537).read_to_end(&mut contents)?;
    ensure!(contents.len() <= 65536, "state file too large");
    Ok(contents)
}

pub struct Lock(File);

#[derive(Debug)]
pub struct LockHeld;

impl std::fmt::Display for LockHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("instance is already supervised (ownership lock held)")
    }
}

impl std::error::Error for LockHeld {}

impl Lock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
            "lock must be a private file owned by you"
        );
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                return Err(LockHeld.into());
            }
            return Err(error).context("acquire instance ownership lock");
        }
        Ok(Self(file))
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub pid: i32,
    pub started_secs: u64,
    pub started_micros: u64,
}

#[cfg(target_os = "macos")]
impl Identity {
    pub fn executable(&self) -> Result<std::path::PathBuf> {
        ensure!(self.alive()?, "process identity changed");
        let mut buffer = vec![0_u8; 4096];
        let count = unsafe {
            libc::proc_pidpath(self.pid, buffer.as_mut_ptr().cast(), buffer.len() as u32)
        };
        ensure!(count > 0, "cannot inspect owned process executable");
        let length = buffer
            .iter()
            .position(|byte| *byte == 0)
            .context("invalid process executable path")?;
        use std::os::unix::ffi::OsStringExt;
        Ok(std::path::PathBuf::from(std::ffi::OsString::from_vec(
            buffer[..length].to_vec(),
        )))
    }

    pub fn read(pid: i32) -> Result<Option<Self>> {
        ensure!(pid > 1, "invalid child PID");
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of_val(&info);
        let count = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut libc::proc_bsdinfo).cast(),
                size as i32,
            )
        };
        if count == 0 {
            let error = std::io::Error::last_os_error();
            if matches!(error.raw_os_error(), Some(libc::ESRCH | libc::ENOENT)) {
                return Ok(None);
            }
            return Err(error).context("inspect process identity");
        }
        ensure!(count as usize == size, "incomplete process identity");
        if info.pbi_status == libc::SZOMB {
            return Ok(None);
        }
        ensure!(
            info.pbi_uid == unsafe { libc::geteuid() },
            "process belongs to another user"
        );
        Ok(Some(Self {
            pid,
            started_secs: info.pbi_start_tvsec,
            started_micros: info.pbi_start_tvusec,
        }))
    }

    pub fn alive(&self) -> Result<bool> {
        Ok(Self::read(self.pid)?.as_ref() == Some(self))
    }

    pub fn signal(&self, signal: i32) -> Result<()> {
        if !self.alive()? {
            return Ok(());
        }
        let rc = unsafe { libc::kill(self.pid, signal) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).context("signal owned Gateway process");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locks_are_per_instance_and_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let first = Lock::acquire(&dir.path().join("paper.lock")).unwrap();
        assert!(Lock::acquire(&dir.path().join("paper.lock")).is_err());
        let _live = Lock::acquire(&dir.path().join("live.lock")).unwrap();
        drop(first);
        assert!(Lock::acquire(&dir.path().join("paper.lock")).is_ok());
    }
    #[test]
    fn private_atomic_files_and_symlink_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();
        assert_eq!(read_private(&path).unwrap(), b"second");
        std::os::unix::fs::symlink(&path, dir.path().join("lock")).unwrap();
        assert!(Lock::acquire(&dir.path().join("lock")).is_err());
    }
    #[test]
    fn identity_detects_birth_time_mismatch() {
        let mut identity = Identity::read(std::process::id() as i32).unwrap().unwrap();
        assert!(identity.alive().unwrap());
        identity.started_micros += 1;
        assert!(!identity.alive().unwrap());
    }
    #[test]
    fn stopping_one_owned_process_does_not_stop_another() {
        let mut paper = std::process::Command::new("/bin/sleep")
            .arg("20")
            .spawn()
            .unwrap();
        let mut live = std::process::Command::new("/bin/sleep")
            .arg("20")
            .spawn()
            .unwrap();
        let identity = Identity::read(paper.id() as i32).unwrap().unwrap();
        let mut wrong_birth = identity.clone();
        wrong_birth.started_micros += 1;
        wrong_birth.signal(libc::SIGTERM).unwrap();
        assert!(paper.try_wait().unwrap().is_none());
        identity.signal(libc::SIGTERM).unwrap();
        paper.wait().unwrap();
        let live_running = live.try_wait().unwrap().is_none();
        live.kill().unwrap();
        live.wait().unwrap();
        assert!(live_running);
    }
}
