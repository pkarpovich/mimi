use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use tracing::info;

use crate::session::Shutdown;

/// Identity tells one file on disk from another: the device it lives on and its inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    device: u64,
    inode: u64,
}

/// Executable is the running binary as it was found at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Executable {
    pub path: PathBuf,
    pub identity: Identity,
}

/// identity looks the file up without following links; `None` means nothing is there.
pub fn identity(path: &Path) -> Option<Identity> {
    let found = fs::symlink_metadata(path).ok()?;
    Some(Identity {
        device: found.dev(),
        inode: found.ino(),
    })
}

/// replaced says whether the file now at the path is no longer the one that was started.
pub fn replaced(original: Identity, current: Option<Identity>) -> bool {
    current != Some(original)
}

/// watch retires the daemon once its executable is swapped or removed, until `stop` closes.
pub fn watch(executable: Executable, interval: Duration, shutdown: Shutdown, stop: Receiver<()>) {
    let Executable {
        path,
        identity: original,
    } = executable;
    loop {
        match stop.recv_timeout(interval) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !replaced(original, identity(&path)) {
            continue;
        }
        info!(
            "{} was replaced; retiring after the session in progress, if any",
            path.display()
        );
        shutdown.retire();
        return;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    use super::*;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("mimi-executable-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            let Self(path) = self;
            path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_dir_all(path);
        }
    }

    fn swap_in_a_new_file(path: &Path) {
        let staged = path.with_extension("new");
        fs::write(&staged, b"new build").expect("write the replacement");
        fs::rename(&staged, path).expect("rename it over the original");
    }

    #[test]
    fn a_file_has_an_identity_and_a_missing_one_has_none() {
        let dir = TempDir::new();
        let binary = dir.path().join("mimi");
        fs::write(&binary, b"build").expect("write the binary");

        assert!(identity(&binary).is_some());
        assert_eq!(identity(&dir.path().join("absent")), None);
    }

    #[test]
    fn the_same_file_keeps_its_identity_and_a_swapped_one_does_not() {
        let dir = TempDir::new();
        let binary = dir.path().join("mimi");
        fs::write(&binary, b"build").expect("write the binary");
        let original = identity(&binary).expect("the original identity");

        assert!(!replaced(original, identity(&binary)));
        assert!(
            replaced(original, None),
            "a removed binary counts as replaced"
        );

        swap_in_a_new_file(&binary);
        assert!(
            replaced(original, identity(&binary)),
            "the way Homebrew swaps a bundle is a new inode at the same path"
        );
    }

    #[test]
    fn the_watch_retires_the_daemon_once_the_binary_is_swapped() {
        let dir = TempDir::new();
        let binary = dir.path().join("mimi");
        fs::write(&binary, b"build").expect("write the binary");
        let executable = Executable {
            path: binary.clone(),
            identity: identity(&binary).expect("the original identity"),
        };
        let shutdown = Shutdown::new();
        let (stop, stopped) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                watch(
                    executable,
                    Duration::from_millis(5),
                    shutdown.clone(),
                    stopped,
                )
            });
            thread::sleep(Duration::from_millis(30));
            assert!(!shutdown.retiring(), "an untouched binary is left alone");
            swap_in_a_new_file(&binary);
            let deadline = Instant::now() + Duration::from_secs(5);
            while !shutdown.retiring() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
        });

        assert!(shutdown.retiring());
        assert!(
            !shutdown.requested(),
            "a swapped binary asks for a retirement, never an immediate stop"
        );
        drop(stop);
    }

    #[test]
    fn closing_the_stop_channel_ends_the_watch_without_retiring() {
        let dir = TempDir::new();
        let binary = dir.path().join("mimi");
        fs::write(&binary, b"build").expect("write the binary");
        let executable = Executable {
            path: binary.clone(),
            identity: identity(&binary).expect("the original identity"),
        };
        let shutdown = Shutdown::new();
        let (stop, stopped) = mpsc::channel::<()>();

        let watcher = thread::spawn({
            let shutdown = shutdown.clone();
            move || watch(executable, Duration::from_millis(5), shutdown, stopped)
        });
        drop(stop);
        watcher.join().expect("the watch returns");

        assert!(!shutdown.retiring());
    }
}
