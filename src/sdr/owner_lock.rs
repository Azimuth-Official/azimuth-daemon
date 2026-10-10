//! Cross-process ownership of the dongle: an exclusive OS advisory lock on a
//! well-known file, held by the OWNER daemon for as long as it owns the dongle
//! and released by the OS when its process exits. This makes "who owns the
//! RTL-SDR" a real fact two daemons can both read, so a restarted owner and a
//! parked peer never wait on each other. Where no lock can be taken
//! (Android, other platforms, file not openable) it reports `Unavailable` and
//! the daemon behaves as it does without a lock.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::dongle::LockProbe;

/// The lock this process holds while it owns the dongle.
static HELD: Mutex<Option<File>> = Mutex::new(None);

/// Well-known lock path shared by every user of the machine.
pub fn lock_path() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
        Some(base.join("Azimuth").join("sdr-owner.lock"))
    }
    #[cfg(all(unix, not(target_os = "android")))]
    {
        Some(PathBuf::from("/tmp/azimuth-sdr-owner.lock"))
    }
    #[cfg(not(any(windows, all(unix, not(target_os = "android")))))]
    {
        None
    }
}

fn open_lock_file(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut o = std::fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o666);
    }
    o.open(path)
}

/// Try to take (or keep) the lock at `path` in `slot`. Already holding it is `Acquired`.
pub fn probe_and_hold_at(path: &Path, slot: &Mutex<Option<File>>) -> LockProbe {
    let mut g = slot.lock().unwrap_or_else(|p| p.into_inner());
    if g.is_some() {
        return LockProbe::Acquired;
    }
    let Ok(f) = open_lock_file(path) else {
        return LockProbe::Unavailable;
    };
    match f.try_lock() {
        Ok(()) => {
            *g = Some(f);
            LockProbe::Acquired
        }
        Err(std::fs::TryLockError::WouldBlock) => LockProbe::HeldElsewhere,
        Err(_) => LockProbe::Unavailable,
    }
}

/// Take (or keep) this process's ownership lock.
pub fn acquire() -> LockProbe {
    match lock_path() {
        Some(p) => probe_and_hold_at(&p, &HELD),
        None => LockProbe::Unavailable,
    }
}

/// Drop the lock (a daemon that tried to claim but did not end up owning).
pub fn release() {
    *HELD.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("azimuth-owner-lock-test-{}-{name}", std::process::id()))
    }

    #[test]
    fn second_holder_is_refused_until_the_first_drops() {
        let path = tmp("a");
        let (s1, s2) = (Mutex::new(None), Mutex::new(None));
        assert_eq!(probe_and_hold_at(&path, &s1), LockProbe::Acquired);
        assert_eq!(probe_and_hold_at(&path, &s1), LockProbe::Acquired, "already held is idempotent");
        assert_eq!(probe_and_hold_at(&path, &s2), LockProbe::HeldElsewhere, "separate open file description is refused in-process");
        *s1.lock().unwrap() = None;
        assert_eq!(probe_and_hold_at(&path, &s2), LockProbe::Acquired, "free once the holder drops");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unopenable_path_is_unavailable() {
        let path = tmp("nodir").join("x").join("y.lock");
        // Parent is created best-effort; a path under a regular file cannot be.
        let file = tmp("plainfile");
        std::fs::write(&file, b"x").unwrap();
        let bad = file.join("sub.lock");
        assert_eq!(probe_and_hold_at(&bad, &Mutex::new(None)), LockProbe::Unavailable);
        let _ = (path, std::fs::remove_file(&file));
    }
}
