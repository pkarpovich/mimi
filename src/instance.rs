use std::fs::{self, File};
use std::path::{Path, PathBuf};

use crate::macos::{self, Lock};

#[derive(Debug, thiserror::Error)]
pub enum InstanceError {
    #[error("{} could not be created: {source}", path.display())]
    Directory {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} could not be opened: {source}", path.display())]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{} could not be locked: OS error {code}", path.display())]
    Lock { path: PathBuf, code: i32 },
}

/// Claim is whether this process may run, or another mimi already holds the machine.
#[derive(Debug)]
pub enum Claim {
    Held(Guard),
    AlreadyRunning,
}

/// Guard holds the lock for as long as it is alive.
#[derive(Debug)]
pub struct Guard {
    _file: File,
}

/// lock_path is where the daemon keeps the lock that admits one instance per user.
pub fn lock_path(home: &Path) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("mimi")
        .join("instance.lock")
}

/// claim takes the single-instance lock, reporting whether another mimi already holds it.
pub fn claim(path: &Path) -> Result<Claim, InstanceError> {
    let Some(parent) = path.parent() else {
        return Err(InstanceError::Open {
            path: path.to_path_buf(),
            source: std::io::Error::from(std::io::ErrorKind::InvalidInput),
        });
    };
    if let Err(source) = fs::create_dir_all(parent) {
        return Err(InstanceError::Directory {
            path: parent.to_path_buf(),
            source,
        });
    }

    let file = File::options()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path);
    let file = match file {
        Ok(file) => file,
        Err(source) => {
            return Err(InstanceError::Open {
                path: path.to_path_buf(),
                source,
            });
        }
    };

    match macos::try_lock(&file) {
        Lock::Taken => Ok(Claim::Held(Guard { _file: file })),
        Lock::HeldByAnother => Ok(Claim::AlreadyRunning),
        Lock::Failed(code) => Err(InstanceError::Lock {
            path: path.to_path_buf(),
            code,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mimi-instance-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_lock_lives_under_the_given_home() {
        let path = lock_path(Path::new("/Users/tester"));
        assert_eq!(
            path,
            Path::new("/Users/tester/Library/Application Support/mimi/instance.lock")
        );
    }

    #[test]
    fn the_first_claim_is_granted() {
        let dir = temp_dir("first");
        let path = dir.join("instance.lock");
        let claim = claim(&path).expect("claim");
        match claim {
            Claim::Held(_) => {}
            Claim::AlreadyRunning => panic!("an unheld lock was reported as held"),
        }
    }

    #[test]
    fn a_second_claim_while_the_first_is_alive_is_refused() {
        let dir = temp_dir("second");
        let path = dir.join("instance.lock");
        let first = claim(&path).expect("first claim");
        match first {
            Claim::Held(_) => {}
            Claim::AlreadyRunning => panic!("the first claim was refused"),
        }
        match claim(&path).expect("second claim") {
            Claim::AlreadyRunning => {}
            Claim::Held(_) => panic!("two instances were admitted at once"),
        }
    }

    #[test]
    fn the_lock_is_released_when_the_guard_is_dropped() {
        let dir = temp_dir("released");
        let path = dir.join("instance.lock");
        let first = claim(&path).expect("first claim");
        drop(first);
        match claim(&path).expect("second claim") {
            Claim::Held(_) => {}
            Claim::AlreadyRunning => panic!("the lock outlived its guard"),
        }
    }

    #[test]
    fn the_parent_directory_is_created_when_it_is_missing() {
        let dir = temp_dir("nested");
        let path = dir.join("deeper").join("still").join("instance.lock");
        let _claim = claim(&path).expect("claim");
        assert!(path.exists());
    }
}
