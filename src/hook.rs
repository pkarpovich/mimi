use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::macos;
use crate::session::Shutdown;

const SHELL: &str = "/bin/sh";
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Job is one completed recording the hook has to deliver, named by the two files it hands over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub sidecar: PathBuf,
    pub recording: PathBuf,
}

/// Outcome is what one run of the hook command did with a job.
#[derive(Debug)]
pub enum Outcome {
    Done,
    Failed(Failure),
}

/// Failure is why a run of the hook command left its job undelivered.
#[derive(Debug)]
pub enum Failure {
    Exited(i32),
    Signaled,
    TimedOut,
    NotStarted(io::Error),
}

/// Runner is what turns a job into an outcome; the daemon runs a shell command, the tests do not.
pub trait Runner {
    fn run(&self, job: &Job) -> Outcome;
}

/// ShellRunner hands a job to the configured command under `/bin/sh`, once, bounded by a timeout.
pub struct ShellRunner {
    command: String,
    timeout: Duration,
    cwd: PathBuf,
    shutdown: Shutdown,
}

impl ShellRunner {
    /// new builds the runner the daemon uses from the configured command and its output directory.
    pub fn new(command: String, timeout: Duration, cwd: PathBuf, shutdown: Shutdown) -> Self {
        Self {
            command,
            timeout,
            cwd,
            shutdown,
        }
    }
}

impl Runner for ShellRunner {
    fn run(&self, job: &Job) -> Outcome {
        let Self {
            command,
            timeout,
            cwd,
            shutdown,
        } = self;
        let Job { sidecar, recording } = job;

        let child = Command::new(SHELL)
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .env("MIMI_RECORDING", recording)
            .env("MIMI_SIDECAR", sidecar)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .process_group(0)
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(source) => return Outcome::Failed(Failure::NotStarted(source)),
        };

        let deadline = Instant::now() + *timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return exited(status),
                Ok(None) => {}
                Err(source) => {
                    kill(&mut child);
                    return Outcome::Failed(Failure::NotStarted(source));
                }
            }
            if shutdown.requested() {
                kill(&mut child);
                return Outcome::Failed(Failure::Signaled);
            }
            if Instant::now() >= deadline {
                kill(&mut child);
                return Outcome::Failed(Failure::TimedOut);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn exited(status: ExitStatus) -> Outcome {
    if status.success() {
        return Outcome::Done;
    }
    let Some(code) = status.code() else {
        return Outcome::Failed(Failure::Signaled);
    };
    Outcome::Failed(Failure::Exited(code))
}

fn kill(child: &mut Child) {
    macos::kill_process_group(child.id());
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mimi-hook-test-{}-{id}", std::process::id()));
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

    fn job(dir: &Path) -> Job {
        Job {
            sidecar: dir.join("2026-09-02T16-20-54-teams2.json"),
            recording: dir.join("2026-09-02T16-20-54-teams2.m4a"),
        }
    }

    fn runner(command: &str, timeout: Duration, cwd: &Path) -> ShellRunner {
        ShellRunner::new(
            command.to_owned(),
            timeout,
            cwd.to_path_buf(),
            Shutdown::new(),
        )
    }

    #[test]
    fn a_command_that_exits_zero_delivered_the_recording() {
        let dir = TempDir::new();
        let outcome = runner("exit 0", Duration::from_secs(5), dir.path()).run(&job(dir.path()));
        match outcome {
            Outcome::Done => {}
            Outcome::Failed(failure) => panic!("{failure:?}"),
        }
    }

    #[test]
    fn a_command_that_exits_nonzero_carries_its_status() {
        let dir = TempDir::new();
        let outcome = runner("exit 3", Duration::from_secs(5), dir.path()).run(&job(dir.path()));
        match outcome {
            Outcome::Failed(Failure::Exited(code)) => assert_eq!(code, 3),
            Outcome::Done
            | Outcome::Failed(Failure::Signaled)
            | Outcome::Failed(Failure::TimedOut)
            | Outcome::Failed(Failure::NotStarted(_)) => panic!("{outcome:?}"),
        }
    }

    #[test]
    fn a_command_that_outlives_its_timeout_is_killed_with_its_children() {
        let dir = TempDir::new();
        let started = Instant::now();
        let outcome = runner(
            "sleep 31337 & sleep 31337",
            Duration::from_millis(300),
            dir.path(),
        )
        .run(&job(dir.path()));
        let elapsed = started.elapsed();
        match outcome {
            Outcome::Failed(Failure::TimedOut) => {}
            Outcome::Done
            | Outcome::Failed(Failure::Exited(_))
            | Outcome::Failed(Failure::Signaled)
            | Outcome::Failed(Failure::NotStarted(_)) => panic!("{outcome:?}"),
        }
        assert!(
            elapsed < Duration::from_millis(300) + 2 * POLL_INTERVAL,
            "the deadline is enforced within one poll tick, not by waiting for the child: {elapsed:?}"
        );
        assert!(
            !survives("sleep 31337"),
            "the whole group goes, not just the shell that led it"
        );
    }

    fn survives(command: &str) -> bool {
        for _ in 0..20 {
            let found = Command::new("pgrep")
                .arg("-f")
                .arg(command)
                .output()
                .expect("pgrep");
            if !found.status.success() {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
        true
    }

    #[test]
    fn a_command_that_cannot_be_started_is_not_a_failed_run() {
        let dir = TempDir::new();
        let missing = dir.path().join("gone");
        let outcome = runner("exit 0", Duration::from_secs(5), &missing).run(&job(dir.path()));
        match &outcome {
            Outcome::Failed(Failure::NotStarted(source)) => {
                assert_eq!(source.kind(), io::ErrorKind::NotFound)
            }
            Outcome::Done
            | Outcome::Failed(Failure::Exited(_))
            | Outcome::Failed(Failure::Signaled)
            | Outcome::Failed(Failure::TimedOut) => panic!("{outcome:?}"),
        }
    }

    #[test]
    fn a_command_is_handed_both_paths_and_runs_in_the_output_directory() {
        let dir = TempDir::new();
        let job = job(dir.path());
        let outcome = runner(
            "printf '%s\\n%s\\n' \"$MIMI_RECORDING\" \"$MIMI_SIDECAR\" > probe",
            Duration::from_secs(5),
            dir.path(),
        )
        .run(&job);
        match outcome {
            Outcome::Done => {}
            Outcome::Failed(failure) => panic!("{failure:?}"),
        }

        let Job { sidecar, recording } = job;
        let probed = fs::read_to_string(dir.path().join("probe"))
            .expect("the command wrote its probe into the directory it was given");
        assert_eq!(
            probed,
            format!("{}\n{}\n", recording.display(), sidecar.display())
        );
    }

    #[test]
    fn a_raised_shutdown_does_not_wait_for_the_child() {
        let dir = TempDir::new();
        let shutdown = Shutdown::new();
        shutdown.request();
        let started = Instant::now();
        let outcome = ShellRunner::new(
            "sleep 30".to_owned(),
            Duration::from_secs(300),
            dir.path().to_path_buf(),
            shutdown,
        )
        .run(&job(dir.path()));
        let elapsed = started.elapsed();
        match outcome {
            Outcome::Failed(Failure::Signaled) => {}
            Outcome::Done
            | Outcome::Failed(Failure::Exited(_))
            | Outcome::Failed(Failure::TimedOut)
            | Outcome::Failed(Failure::NotStarted(_)) => panic!("{outcome:?}"),
        }
        assert!(
            elapsed < POLL_INTERVAL,
            "a stopping daemon must not wait out the hook timeout: {elapsed:?}"
        );
    }
}
