use std::fs;
use std::io;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, SecondsFormat};
use serde::Deserialize;
use thiserror::Error;
use tracing::warn;

use crate::macos;
use crate::session::Shutdown;
use crate::sink::{OnComplete, SIDECAR_EXTENSION, write_private};

const SHELL: &str = "/bin/sh";
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const ON_COMPLETE_FIELD: &str = "on_complete";
const TEMPORARY_SUFFIX: &str = ".tmp";

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

#[derive(Debug, Error)]
pub enum HookError {
    #[error("reading {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{0} is not a sidecar object")]
    NotAnObject(PathBuf),
    #[error("describing {path}: {source}")]
    Describe {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("writing {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}

#[derive(Deserialize)]
struct Ledger {
    file: String,
    on_complete: Option<OnComplete>,
}

/// job_for is the delivery a sidecar still owes the hook, or nothing when it owes none.
pub fn job_for(sidecar: &Path) -> Option<Job> {
    let described = match fs::read(sidecar) {
        Ok(described) => described,
        Err(source) => {
            warn!("{} could not be read: {source}", sidecar.display());
            return None;
        }
    };
    let Ledger { file, on_complete } = match serde_json::from_slice(&described) {
        Ok(ledger) => ledger,
        Err(source) => {
            warn!(
                "{} is not a sidecar mimi wrote: {source}",
                sidecar.display()
            );
            return None;
        }
    };
    match on_complete {
        Some(OnComplete::Pending) => {}
        Some(OnComplete::Done { at: _ }) | None => return None,
    }
    let recording = sidecar.with_file_name(file);
    if !recording.exists() {
        warn!(
            "{} is pending but {} is gone",
            sidecar.display(),
            recording.display()
        );
        return None;
    }
    Some(Job {
        sidecar: sidecar.to_path_buf(),
        recording,
    })
}

/// scan is every delivery the recordings in `dir` still owe the hook, oldest recording first.
pub fn scan(dir: &Path) -> Vec<Job> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) => {
            warn!(
                "{} could not be read for pending recordings: {source}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut sidecars = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(extension) = path.extension() else {
            continue;
        };
        if extension != SIDECAR_EXTENSION {
            continue;
        }
        sidecars.push(path);
    }
    sidecars.sort();

    let mut jobs = Vec::new();
    for sidecar in sidecars {
        let Some(job) = job_for(&sidecar) else {
            continue;
        };
        jobs.push(job);
    }
    jobs
}

/// mark_done records in the sidecar that the hook delivered its recording, and when.
pub fn mark_done(sidecar: &Path, at: DateTime<Local>) -> Result<(), HookError> {
    let described = match fs::read(sidecar) {
        Ok(described) => described,
        Err(source) => {
            return Err(HookError::Read {
                path: sidecar.to_path_buf(),
                source,
            });
        }
    };
    let described = match serde_json::from_slice(&described) {
        Ok(described) => described,
        Err(source) => {
            return Err(HookError::Describe {
                path: sidecar.to_path_buf(),
                source,
            });
        }
    };
    let serde_json::Value::Object(mut described) = described else {
        return Err(HookError::NotAnObject(sidecar.to_path_buf()));
    };

    let state = OnComplete::Done {
        at: at.to_rfc3339_opts(SecondsFormat::Secs, false),
    };
    let state = match serde_json::to_value(state) {
        Ok(state) => state,
        Err(source) => {
            return Err(HookError::Describe {
                path: sidecar.to_path_buf(),
                source,
            });
        }
    };
    described.insert(ON_COMPLETE_FIELD.to_owned(), state);
    let described = match serde_json::to_vec_pretty(&described) {
        Ok(described) => described,
        Err(source) => {
            return Err(HookError::Describe {
                path: sidecar.to_path_buf(),
                source,
            });
        }
    };

    let mut temporary = sidecar.as_os_str().to_owned();
    temporary.push(TEMPORARY_SUFFIX);
    let temporary = PathBuf::from(temporary);
    if let Err(source) = write_private(&temporary, &described) {
        return Err(HookError::Write {
            path: temporary,
            source,
        });
    }
    if let Err(source) = fs::rename(&temporary, sidecar) {
        let _ = fs::remove_file(&temporary);
        return Err(HookError::Write {
            path: sidecar.to_path_buf(),
            source,
        });
    }
    Ok(())
}

/// retry_after is how long a job waits after its `attempts`-th run left it undelivered.
pub fn retry_after(attempts: u32, base: Duration, cap: Duration) -> Duration {
    let mut wait = base;
    for _ in 1..attempts {
        if wait >= cap {
            return cap;
        }
        wait *= 2;
    }
    wait.min(cap)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::TimeZone;

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

    fn delivered_at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 9, 2, 16, 21, 3)
            .single()
            .expect("a local timestamp")
    }

    fn write_sidecar(dir: &Path, stem: &str, on_complete: Option<serde_json::Value>) -> PathBuf {
        let mut described = serde_json::json!({
            "file": format!("{stem}.m4a"),
            "label": "teams2",
            "duration_seconds": 1830,
            "silent": false,
        });
        if let Some(state) = on_complete {
            described["on_complete"] = state;
        }
        let path = dir.join(format!("{stem}.{SIDECAR_EXTENSION}"));
        fs::write(
            &path,
            serde_json::to_vec_pretty(&described).expect("describe the sidecar"),
        )
        .expect("write the sidecar");
        path
    }

    fn write_recording(dir: &Path, stem: &str) {
        fs::write(dir.join(format!("{stem}.m4a")), b"m4a").expect("write the recording");
    }

    fn pending() -> Option<serde_json::Value> {
        Some(serde_json::json!({"state": "pending"}))
    }

    #[test]
    fn a_scan_finds_the_pending_recordings_oldest_first() {
        let dir = TempDir::new();
        let dir = dir.path();

        write_sidecar(dir, "2026-09-02T11-00-00-late", pending());
        write_recording(dir, "2026-09-02T11-00-00-late");
        write_sidecar(dir, "2026-09-02T09-00-00-early", pending());
        write_recording(dir, "2026-09-02T09-00-00-early");

        write_sidecar(
            dir,
            "2026-09-02T10-00-00-done",
            Some(serde_json::json!({"state": "done", "at": "2026-09-02T16:21:03+02:00"})),
        );
        write_recording(dir, "2026-09-02T10-00-00-done");
        write_sidecar(dir, "2026-09-02T10-30-00-unhooked", None);
        write_recording(dir, "2026-09-02T10-30-00-unhooked");
        write_sidecar(dir, "2026-09-02T10-45-00-deleted", pending());
        fs::write(dir.join("notes.json"), b"not json at all").expect("write the intruder");

        assert_eq!(
            scan(dir),
            vec![
                Job {
                    sidecar: dir.join("2026-09-02T09-00-00-early.json"),
                    recording: dir.join("2026-09-02T09-00-00-early.m4a"),
                },
                Job {
                    sidecar: dir.join("2026-09-02T11-00-00-late.json"),
                    recording: dir.join("2026-09-02T11-00-00-late.m4a"),
                },
            ],
            "only a pending sidecar whose recording is still there is the hook's business"
        );
    }

    #[test]
    fn a_scan_of_a_directory_that_is_not_there_finds_nothing() {
        let dir = TempDir::new();
        assert_eq!(scan(&dir.path().join("gone")), Vec::new());
    }

    #[test]
    fn a_marked_sidecar_carries_the_delivery_and_every_field_it_had() {
        let dir = TempDir::new();
        let sidecar = write_sidecar(dir.path(), "2026-09-02T09-00-00-early", pending());
        write_recording(dir.path(), "2026-09-02T09-00-00-early");

        mark_done(&sidecar, delivered_at()).expect("mark the sidecar done");

        let described = fs::read_to_string(&sidecar).expect("the sidecar");
        let described: serde_json::Value =
            serde_json::from_str(&described).expect("valid sidecar json");
        assert_eq!(
            described["on_complete"],
            serde_json::json!({
                "state": "done",
                "at": delivered_at().to_rfc3339_opts(SecondsFormat::Secs, false),
            })
        );
        assert_eq!(described["file"], "2026-09-02T09-00-00-early.m4a");
        assert_eq!(described["label"], "teams2");
        assert_eq!(described["duration_seconds"], 1830);
        assert_eq!(described["silent"], false);
        assert_eq!(
            described.as_object().expect("an object").len(),
            5,
            "a delivery rewrites one field and leaves the rest of the recording described"
        );

        let mode = fs::metadata(&sidecar)
            .expect("the sidecar")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "the rewrite must not widen a sidecar the sink wrote privately"
        );
        assert!(
            !dir.path()
                .join("2026-09-02T09-00-00-early.json.tmp")
                .exists(),
            "the rename takes the temporary file with it"
        );
        assert_eq!(
            job_for(&sidecar),
            None,
            "a delivered recording is never handed to the command again"
        );
    }

    #[test]
    fn a_sidecar_that_is_not_an_object_cannot_be_marked() {
        let dir = TempDir::new();
        let sidecar = dir.path().join("2026-09-02T09-00-00-early.json");
        fs::write(&sidecar, b"[1, 2, 3]").expect("write the sidecar");

        let failure = mark_done(&sidecar, delivered_at()).expect_err("an array is not a sidecar");
        match failure {
            HookError::NotAnObject(path) => assert_eq!(path, sidecar),
            HookError::Read { path: _, source: _ }
            | HookError::Describe { path: _, source: _ }
            | HookError::Write { path: _, source: _ } => panic!("{failure}"),
        }
    }

    #[test]
    fn a_sidecar_that_is_gone_cannot_be_marked() {
        let dir = TempDir::new();
        let sidecar = dir.path().join("2026-09-02T09-00-00-early.json");

        let failure = mark_done(&sidecar, delivered_at()).expect_err("nothing to rewrite");
        match failure {
            HookError::Read { path, source: _ } => assert_eq!(path, sidecar),
            HookError::NotAnObject(_)
            | HookError::Describe { path: _, source: _ }
            | HookError::Write { path: _, source: _ } => panic!("{failure}"),
        }
    }

    #[test]
    fn a_retry_doubles_its_wait_until_the_cap() {
        let base = Duration::from_secs(60);
        let cap = Duration::from_secs(1800);
        assert_eq!(retry_after(1, base, cap), base);
        assert_eq!(retry_after(2, base, cap), Duration::from_secs(120));
        assert_eq!(retry_after(3, base, cap), Duration::from_secs(240));
        assert_eq!(retry_after(6, base, cap), cap);
        assert_eq!(
            retry_after(600, base, cap),
            cap,
            "a hook that never succeeds waits the cap, not forever"
        );
    }
}
