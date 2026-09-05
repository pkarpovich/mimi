use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, SecondsFormat};
use serde::Deserialize;
use thiserror::Error;
use tracing::{error, info, warn};

use crate::macos;
use crate::remux::M4A_EXTENSION;
use crate::session::Shutdown;
use crate::sink::{
    AUDIO_EXTENSION, OnComplete, RECORDINGS_DIR_MODE, SIDECAR_EXTENSION, write_private,
};

const SHELL: &str = "/bin/sh";
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const ON_COMPLETE_FIELD: &str = "on_complete";
const TEMPORARY_SUFFIX: &str = ".tmp";
const OTHERS_WRITE: u32 = 0o022;
const STICKY: u32 = 0o1000;
const ROOT: u32 = 0;

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
#[derive(Debug, Error)]
pub enum Failure {
    #[error("it exited {0}")]
    Exited(i32),
    #[error("a signal ended it")]
    Signaled,
    #[error("it outlived its timeout")]
    TimedOut,
    #[error("it could not be started: {0}")]
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

/// Exposure is why an `output_dir` is not the daemon user's alone.
#[derive(Debug, Error)]
pub enum Exposure {
    #[error("{path} could not be created: {source}")]
    Uncreated { path: PathBuf, source: io::Error },
    #[error("{0} belongs to another user")]
    Owned(PathBuf),
    #[error("{0} is writable by users other than its owner")]
    Writable(PathBuf),
    #[error("{0} hands write access to others through an extended ACL")]
    Granted(PathBuf),
    #[error("{path} could not be looked at: {source}")]
    Unknown { path: PathBuf, source: io::Error },
}

/// claim_private creates the output directory if it is not there yet, accepts it only when `user`
/// is the only one who can put files in it or swap it for another, and answers with the directory
/// it judged.
///
/// Creating it here rather than leaving it to the first meeting is what makes the check mean
/// something: a directory that does not exist yet is one another user can still create, and
/// `session::start_session` would then accept theirs instead of making its own.
///
/// The answer is the resolved path because every link on the way to it was followed to reach the
/// directory that was judged, and a link is somebody else's to repoint afterwards.
pub fn claim_private(dir: &Path, user: u32) -> Result<PathBuf, Exposure> {
    let created = fs::DirBuilder::new()
        .recursive(true)
        .mode(RECORDINGS_DIR_MODE)
        .create(dir);
    if let Err(source) = created {
        return Err(Exposure::Uncreated {
            path: dir.to_path_buf(),
            source,
        });
    }
    let dir = match fs::canonicalize(dir) {
        Ok(dir) => dir,
        Err(source) => {
            return Err(Exposure::Unknown {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    private(&dir, user)?;
    for ancestor in dir.ancestors().skip(1) {
        stable(ancestor, user)?;
    }
    Ok(dir)
}

fn private(dir: &Path, user: u32) -> Result<(), Exposure> {
    let found = match fs::symlink_metadata(dir) {
        Ok(found) => found,
        Err(source) => {
            return Err(Exposure::Unknown {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if found.uid() != user {
        return Err(Exposure::Owned(dir.to_path_buf()));
    }
    if found.mode() & OTHERS_WRITE != 0 {
        return Err(Exposure::Writable(dir.to_path_buf()));
    }
    granted(dir)
}

fn stable(dir: &Path, user: u32) -> Result<(), Exposure> {
    let found = match fs::symlink_metadata(dir) {
        Ok(found) => found,
        Err(source) => {
            return Err(Exposure::Unknown {
                path: dir.to_path_buf(),
                source,
            });
        }
    };
    if found.uid() != user && found.uid() != ROOT {
        return Err(Exposure::Owned(dir.to_path_buf()));
    }
    let mode = found.mode();
    if mode & STICKY == 0 && mode & OTHERS_WRITE != 0 {
        return Err(Exposure::Writable(dir.to_path_buf()));
    }
    granted(dir)
}

fn granted(dir: &Path) -> Result<(), Exposure> {
    match macos::acl_grants_write(dir) {
        Ok(macos::Grant::Nothing) => Ok(()),
        Ok(macos::Grant::Write) => Err(Exposure::Granted(dir.to_path_buf())),
        Err(source) => Err(Exposure::Unknown {
            path: dir.to_path_buf(),
            source,
        }),
    }
}

#[derive(Deserialize)]
struct Ledger {
    file: Option<String>,
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
    let Some(file) = file else {
        warn!("{} is pending but names no recording", sidecar.display());
        return None;
    };
    let Some(recording) = beside(sidecar, &file) else {
        warn!(
            "{} is pending but {file} is not the recording it was written for",
            sidecar.display()
        );
        return None;
    };
    let found = match fs::symlink_metadata(&recording) {
        Ok(found) => found,
        Err(source) => {
            warn!(
                "{} is pending but {} could not be looked at: {source}",
                sidecar.display(),
                recording.display()
            );
            return None;
        }
    };
    if !found.is_file() {
        warn!(
            "{} is pending but {} is not a recording mimi wrote",
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

fn beside(sidecar: &Path, file: &str) -> Option<PathBuf> {
    let mut components = Path::new(file).components();
    let (Some(Component::Normal(name)), None) = (components.next(), components.next()) else {
        return None;
    };
    let named = Path::new(name);
    if named.file_stem() != sidecar.file_stem() {
        return None;
    }
    let extension = named.extension()?;
    let mut audio = false;
    for candidate in [M4A_EXTENSION, AUDIO_EXTENSION] {
        if extension == candidate {
            audio = true;
            break;
        }
    }
    if !audio {
        return None;
    }
    Some(sidecar.with_file_name(name))
}

/// scan is every delivery the recordings in `dir` still owe the hook, oldest recording first.
pub fn scan(dir: &Path) -> Vec<Job> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Vec::new(),
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

/// HookSettings is everything the hook thread needs to deliver the recordings it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookSettings {
    pub command: String,
    pub timeout: Duration,
    pub output_dir: PathBuf,
    pub retry_base: Duration,
    pub retry_cap: Duration,
}

/// Hook is the running hook thread.
pub struct Hook {
    thread: JoinHandle<()>,
}

impl Hook {
    /// join waits for the hook thread to leave the run it is in and stop.
    pub fn join(self) {
        let Self { thread } = self;
        match thread.join() {
            Ok(()) => {}
            Err(_) => error!("the completion hook thread panicked"),
        }
    }
}

/// spawn starts the hook thread on the recordings already pending in `output_dir` and the ones still to come.
pub fn spawn(settings: HookSettings, jobs: Receiver<PathBuf>, shutdown: Shutdown) -> Hook {
    let HookSettings {
        command,
        timeout,
        output_dir,
        retry_base: _,
        retry_cap: _,
    } = settings.clone();
    let runner = ShellRunner::new(command, timeout, output_dir, shutdown.clone());
    let thread = thread::spawn(move || run(settings, jobs, shutdown, runner, Instant::now));
    Hook { thread }
}

struct Pending {
    job: Job,
    attempts: u32,
    due: Instant,
}

enum Feed {
    Open,
    Closed,
}

enum Settled {
    Delivered,
    Undelivered,
}

fn run(
    settings: HookSettings,
    jobs: Receiver<PathBuf>,
    shutdown: Shutdown,
    runner: impl Runner,
    clock: impl Fn() -> Instant,
) {
    let HookSettings {
        command: _,
        timeout: _,
        output_dir,
        retry_base,
        retry_cap,
    } = settings;

    let mut queue = Vec::new();
    for job in scan(&output_dir) {
        queue.push(Pending {
            job,
            attempts: 0,
            due: clock(),
        });
    }

    while !shutdown.requested() {
        let now = clock();
        match drain(&jobs, &mut queue, now) {
            Feed::Open => {}
            Feed::Closed => return,
        }
        let Some(index) = next_due(&queue, now) else {
            match wait(&jobs, &mut queue, now) {
                Feed::Open => continue,
                Feed::Closed => return,
            }
        };

        let Pending {
            job,
            attempts,
            due: _,
        } = &queue[index];
        let job = job.clone();
        let attempts = attempts + 1;
        let outcome = runner.run(&job);
        match settle(&job, outcome) {
            Settled::Delivered => {
                queue.remove(index);
            }
            Settled::Undelivered => {
                let due = clock() + retry_after(attempts, retry_base, retry_cap);
                queue[index] = Pending { job, attempts, due };
            }
        }
    }
}

fn drain(jobs: &Receiver<PathBuf>, queue: &mut Vec<Pending>, now: Instant) -> Feed {
    loop {
        match jobs.try_recv() {
            Ok(sidecar) => enqueue(queue, &sidecar, now),
            Err(TryRecvError::Empty) => return Feed::Open,
            Err(TryRecvError::Disconnected) => return Feed::Closed,
        }
    }
}

fn wait(jobs: &Receiver<PathBuf>, queue: &mut Vec<Pending>, now: Instant) -> Feed {
    let mut waiting = POLL_INTERVAL;
    for pending in queue.iter() {
        let Pending {
            job: _,
            attempts: _,
            due,
        } = pending;
        waiting = waiting.min(due.saturating_duration_since(now));
    }
    match jobs.recv_timeout(waiting) {
        Ok(sidecar) => {
            enqueue(queue, &sidecar, now);
            Feed::Open
        }
        Err(RecvTimeoutError::Timeout) => Feed::Open,
        Err(RecvTimeoutError::Disconnected) => Feed::Closed,
    }
}

fn enqueue(queue: &mut Vec<Pending>, sidecar: &Path, now: Instant) {
    for pending in queue.iter() {
        let Pending {
            job,
            attempts: _,
            due: _,
        } = pending;
        let Job {
            sidecar: queued,
            recording: _,
        } = job;
        if queued == sidecar {
            return;
        }
    }
    let Some(job) = job_for(sidecar) else {
        return;
    };
    queue.push(Pending {
        job,
        attempts: 0,
        due: now,
    });
}

fn next_due(queue: &[Pending], now: Instant) -> Option<usize> {
    let mut next = None;
    let mut earliest = now;
    for (index, pending) in queue.iter().enumerate() {
        let Pending {
            job: _,
            attempts: _,
            due,
        } = pending;
        if *due > now {
            continue;
        }
        let earlier = match next {
            None => true,
            Some(_) => *due < earliest,
        };
        if earlier {
            next = Some(index);
            earliest = *due;
        }
    }
    next
}

fn settle(job: &Job, outcome: Outcome) -> Settled {
    let Job { sidecar, recording } = job;
    match outcome {
        Outcome::Done => match mark_done(sidecar, Local::now()) {
            Ok(()) => {
                info!("the completion hook delivered {}", recording.display());
                Settled::Delivered
            }
            Err(error) => {
                if gone(&error) {
                    info!(
                        "the completion hook delivered {}; its sidecar is gone, so there is nothing left to mark",
                        recording.display()
                    );
                    return Settled::Delivered;
                }
                warn!(
                    "the completion hook delivered {} but the sidecar still says pending: {error}",
                    recording.display()
                );
                Settled::Undelivered
            }
        },
        Outcome::Failed(failure) => {
            warn!(
                "the completion hook left {} undelivered: {failure}",
                recording.display()
            );
            Settled::Undelivered
        }
    }
}

fn gone(error: &HookError) -> bool {
    match error {
        HookError::Read { path: _, source } => source.kind() == io::ErrorKind::NotFound,
        HookError::NotAnObject(_)
        | HookError::Describe { path: _, source: _ }
        | HookError::Write { path: _, source: _ } => false,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex, mpsc};

    use chrono::TimeZone;

    use super::*;
    use crate::activity::BundleId;
    use crate::capture::Verdict;
    use crate::sink::{LocalFolder, Recording, Sink, file_stem};
    use crate::writer::Written;

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
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&path);
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
            if found.status.success() {
                thread::sleep(Duration::from_millis(50));
                continue;
            }
            assert_eq!(
                found.status.code(),
                Some(1),
                "pgrep itself failed, so it says nothing about the process group"
            );
            return false;
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
    fn a_recording_that_finished_before_the_hook_existed_is_not_the_hooks_business() {
        let dir = TempDir::new();
        let dir = dir.path();
        let sidecar = dir.join("2026-08-30T14-32-05-thebrowser.json");
        fs::write(
            &sidecar,
            serde_json::to_vec_pretty(&serde_json::json!({
                "started_at": "2026-08-30T14:32:05+02:00",
                "ended_at": "2026-08-30T15:02:35+02:00",
                "duration_seconds": 1830,
                "bundle_id": "company.thebrowser.browser.helper",
                "sample_rate": 24_000,
                "channels": 2,
                "device_changes": 0,
                "failed_device_changes": 0,
                "silent": false,
                "write_failed": false,
            }))
            .expect("describe the sidecar"),
        )
        .expect("write the sidecar");
        write_recording(dir, "2026-08-30T14-32-05-thebrowser");

        assert_eq!(
            job_for(&sidecar),
            None,
            "enabling the hook publishes nothing retroactively"
        );
        assert_eq!(scan(dir), Vec::new());
    }

    #[test]
    fn a_sidecar_naming_anything_but_a_sibling_is_not_a_job() {
        let dir = TempDir::new();
        let outside = dir.path().join("outside");
        let inside = outside.join("recordings");
        fs::create_dir_all(&inside).expect("create the output directory");
        write_recording(&outside, "2026-09-02T09-00-00-planted");

        let absolute = outside.join("2026-09-02T09-00-00-planted.m4a");
        let absolute = absolute.to_str().expect("a utf-8 path");
        let names = [
            "../2026-09-02T09-00-00-planted.m4a",
            "./2026-09-02T09-00-00-planted.m4a",
            "..",
            "",
            absolute,
        ];
        for name in names {
            let sidecar = inside.join("2026-09-02T09-00-00-planted.json");
            fs::write(
                &sidecar,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "file": name,
                    "on_complete": {"state": "pending"},
                }))
                .expect("describe the sidecar"),
            )
            .expect("write the sidecar");
            assert_eq!(
                job_for(&sidecar),
                None,
                "{name} names something other than a recording beside the sidecar"
            );
            assert_eq!(scan(&inside), Vec::new());
        }
    }

    #[test]
    fn a_sidecar_naming_a_symlink_beside_it_is_not_a_job() {
        let dir = TempDir::new();
        let outside = dir.path().join("outside");
        let inside = outside.join("recordings");
        fs::create_dir_all(&inside).expect("create the output directory");
        write_recording(&outside, "planted");
        std::os::unix::fs::symlink(
            outside.join("planted.m4a"),
            inside.join("2026-09-02T09-00-00-symlinked.m4a"),
        )
        .expect("plant the symlink");
        fs::create_dir(inside.join("2026-09-02T09-00-00-directory.m4a"))
            .expect("plant the directory");

        let stems = [
            "2026-09-02T09-00-00-symlinked",
            "2026-09-02T09-00-00-directory",
        ];
        for stem in stems {
            let sidecar = inside.join(format!("{stem}.{SIDECAR_EXTENSION}"));
            fs::write(
                &sidecar,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "file": format!("{stem}.m4a"),
                    "on_complete": {"state": "pending"},
                }))
                .expect("describe the sidecar"),
            )
            .expect("write the sidecar");
            assert_eq!(
                job_for(&sidecar),
                None,
                "{stem}.m4a is a way out of the directory, not a recording mimi wrote"
            );
            assert_eq!(scan(&inside), Vec::new());
        }
    }

    #[test]
    fn a_sidecar_naming_a_recording_that_is_not_its_own_is_not_a_job() {
        let dir = TempDir::new();
        let dir = dir.path();
        write_sidecar(dir, "2026-09-02T09-00-00-early", None);
        write_recording(dir, "2026-09-02T09-00-00-early");

        let planted = dir.join(format!("planted.{SIDECAR_EXTENSION}"));
        fs::write(
            &planted,
            serde_json::to_vec_pretty(&serde_json::json!({
                "file": "2026-09-02T09-00-00-early.m4a",
                "on_complete": {"state": "pending"},
            }))
            .expect("describe the sidecar"),
        )
        .expect("write the sidecar");

        assert_eq!(
            job_for(&planted),
            None,
            "a sidecar delivers the recording it was written for, not one it points at"
        );
        assert_eq!(scan(dir), Vec::new());
    }

    #[test]
    fn a_sidecar_naming_something_that_is_not_audio_is_not_a_job() {
        let dir = TempDir::new();
        let dir = dir.path();
        let stem = "2026-09-02T09-00-00-early";
        fs::write(dir.join(format!("{stem}.txt")), b"notes").expect("write the intruder");

        let names = [format!("{stem}.txt"), stem.to_owned()];
        for name in names {
            let sidecar = dir.join(format!("{stem}.{SIDECAR_EXTENSION}"));
            fs::write(
                &sidecar,
                serde_json::to_vec_pretty(&serde_json::json!({
                    "file": name,
                    "on_complete": {"state": "pending"},
                }))
                .expect("describe the sidecar"),
            )
            .expect("write the sidecar");
            assert_eq!(
                job_for(&sidecar),
                None,
                "{name} is not a recording mimi wrote"
            );
            assert_eq!(scan(dir), Vec::new());
        }
    }

    #[test]
    fn the_output_directory_mimi_creates_is_the_hooks_to_deliver_from() {
        let dir = TempDir::new();
        let mimis = fs::Permissions::from_mode(RECORDINGS_DIR_MODE);
        fs::set_permissions(dir.path(), mimis).expect("create the directory the way mimi does");
        claim_private(dir.path(), macos::user_id()).expect("mimi's own directory");
    }

    #[test]
    fn an_output_directory_that_is_not_there_yet_is_created_before_it_is_judged() {
        let dir = TempDir::new();
        let gone = dir.path().join("gone");
        claim_private(&gone, macos::user_id()).expect("the hook makes its own directory");
        let found = fs::metadata(&gone).expect("the directory the hook made");
        assert_eq!(
            found.mode() & 0o777,
            RECORDINGS_DIR_MODE,
            "a directory left for somebody else to create is a directory somebody else can own"
        );
        assert_eq!(found.uid(), macos::user_id());
    }

    #[test]
    fn an_output_directory_another_user_can_write_is_refused() {
        let dir = TempDir::new();
        let modes = [0o777, 0o770, 0o702];
        for mode in modes {
            let opened = fs::Permissions::from_mode(mode);
            fs::set_permissions(dir.path(), opened).expect("open the output directory");
            let exposure = claim_private(dir.path(), macos::user_id()).expect_err(
                "a planted sidecar in a shared directory is a delivery mimi never made",
            );
            match exposure {
                Exposure::Writable(path) => assert_eq!(path, resolved(dir.path())),
                Exposure::Uncreated { path: _, source: _ }
                | Exposure::Owned(_)
                | Exposure::Granted(_)
                | Exposure::Unknown { path: _, source: _ } => {
                    panic!("{exposure}")
                }
            }
        }
    }

    #[test]
    fn an_output_directory_another_user_owns_is_refused() {
        let dir = TempDir::new();
        let exposure = claim_private(dir.path(), macos::user_id() + 1)
            .expect_err("its owner decides what the command is handed, not mimi");
        match exposure {
            Exposure::Owned(path) => assert_eq!(path, resolved(dir.path())),
            Exposure::Uncreated { path: _, source: _ }
            | Exposure::Writable(_)
            | Exposure::Granted(_)
            | Exposure::Unknown { path: _, source: _ } => {
                panic!("{exposure}")
            }
        }
    }

    #[test]
    fn an_output_directory_an_acl_opens_to_others_is_refused() {
        let grants = ["everyone allow write", "everyone allow delete_child"];
        for grant in grants {
            let dir = TempDir::new();
            allow(dir.path(), grant);
            let exposure = claim_private(dir.path(), macos::user_id()).expect_err(
                "an ACL hands out what the mode bits say nobody has, and the mode bits are what a planted sidecar needs",
            );
            match exposure {
                Exposure::Granted(path) => assert_eq!(path, resolved(dir.path())),
                Exposure::Uncreated { path: _, source: _ }
                | Exposure::Owned(_)
                | Exposure::Writable(_)
                | Exposure::Unknown { path: _, source: _ } => {
                    panic!("{exposure}")
                }
            }
        }
    }

    #[test]
    fn an_output_directory_an_acl_only_denies_on_is_the_hooks_to_deliver_from() {
        let dir = TempDir::new();
        allow(dir.path(), "everyone deny delete");
        claim_private(dir.path(), macos::user_id()).expect(
            "the ACL every home directory carries takes access away, it does not hand it out",
        );
    }

    #[test]
    fn an_output_directory_another_user_can_swap_is_refused() {
        let dir = TempDir::new();
        let holding = dir.path().join("holding");
        let inside = holding.join("recordings");
        fs::create_dir_all(&inside).expect("create the output directory");
        let opened = fs::Permissions::from_mode(0o777);
        fs::set_permissions(&holding, opened).expect("open the directory that holds it");

        let exposure = claim_private(&inside, macos::user_id()).expect_err(
            "a directory another user can rename away is one they can put their own in place of",
        );
        match exposure {
            Exposure::Writable(path) => assert_eq!(path, resolved(&holding)),
            Exposure::Uncreated { path: _, source: _ }
            | Exposure::Owned(_)
            | Exposure::Granted(_)
            | Exposure::Unknown { path: _, source: _ } => {
                panic!("{exposure}")
            }
        }
    }

    #[test]
    fn an_output_directory_an_acl_lets_another_user_swap_is_refused() {
        let dir = TempDir::new();
        let holding = dir.path().join("holding");
        let inside = holding.join("recordings");
        fs::create_dir_all(&inside).expect("create the output directory");
        allow(&holding, "everyone allow delete_child");

        let exposure = claim_private(&inside, macos::user_id())
            .expect_err("an ACL hands out the swap the mode bits say nobody can make");
        match exposure {
            Exposure::Granted(path) => assert_eq!(path, resolved(&holding)),
            Exposure::Uncreated { path: _, source: _ }
            | Exposure::Owned(_)
            | Exposure::Writable(_)
            | Exposure::Unknown { path: _, source: _ } => {
                panic!("{exposure}")
            }
        }
    }

    #[test]
    fn an_output_directory_held_by_a_sticky_shared_one_is_the_hooks_to_deliver_from() {
        let dir = TempDir::new();
        let holding = dir.path().join("holding");
        let inside = holding.join("recordings");
        fs::create_dir_all(&inside).expect("create the output directory");
        let shared = fs::Permissions::from_mode(0o1777);
        fs::set_permissions(&holding, shared).expect("open the directory that holds it");

        claim_private(&inside, macos::user_id())
            .expect("a sticky directory hands out no rename of somebody else's directory");
    }

    #[test]
    fn an_output_directory_reached_through_a_link_is_claimed_where_the_link_lands() {
        let dir = TempDir::new();
        let landing = dir.path().join("landing");
        fs::create_dir(&landing).expect("the directory the link points at");
        let mimis = fs::Permissions::from_mode(RECORDINGS_DIR_MODE);
        fs::set_permissions(&landing, mimis).expect("create the directory the way mimi does");
        let link = dir.path().join("link");
        symlink(&landing, &link).expect("plant the link");
        let claimed = claim_private(&link, macos::user_id()).expect("the link lands in mimi's own");
        assert_eq!(
            claimed,
            resolved(&landing),
            "a link judged and then followed again is a link somebody else can repoint in between"
        );
    }

    fn resolved(dir: &Path) -> PathBuf {
        dir.canonicalize().expect("the directory behind the path")
    }

    fn allow(dir: &Path, entry: &str) {
        let set = Command::new("/bin/chmod")
            .arg("+a")
            .arg(entry)
            .arg(dir)
            .status()
            .expect("chmod +a");
        assert!(set.success(), "chmod +a {entry} failed");
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

    fn keys(described: &serde_json::Value) -> Vec<String> {
        let mut keys = Vec::new();
        for (key, _) in described.as_object().expect("an object") {
            keys.push(key.clone());
        }
        keys
    }

    #[test]
    fn a_sidecar_the_sink_wrote_is_the_job_the_hook_delivers() {
        let dir = TempDir::new();
        let stem = file_stem(delivered_at(), "thebrowser");
        let partial = dir.path().join(format!("{stem}.aac.partial"));
        fs::write(&partial, b"adts").expect("write the partial file");

        let (sender, sidecars) = mpsc::channel();
        LocalFolder::new(Some(sender))
            .accept(Recording {
                partial,
                started_at: delivered_at(),
                ended_at: delivered_at() + chrono::Duration::seconds(1830),
                bundle_id: BundleId::new("company.thebrowser.browser.helper"),
                label: "thebrowser".to_owned(),
                sample_rate: 24_000,
                channels: 2,
                device_changes: 1,
                failed_device_changes: 0,
                verdict: Verdict::AudioPresent,
                written: Written::Whole,
            })
            .expect("accept the recording");

        let sidecar = sidecars
            .try_recv()
            .expect("the sink handed the sidecar over");
        assert_eq!(
            scan(dir.path()),
            vec![Job {
                sidecar: sidecar.clone(),
                recording: dir.path().join(format!("{stem}.aac")),
            }],
            "the hook picks up exactly what the sink wrote"
        );

        let before = fs::read(&sidecar).expect("the sidecar");
        let before: serde_json::Value =
            serde_json::from_slice(&before).expect("valid sidecar json");
        mark_done(&sidecar, delivered_at()).expect("mark the sidecar done");
        let after = fs::read(&sidecar).expect("the sidecar");
        let after: serde_json::Value = serde_json::from_slice(&after).expect("valid sidecar json");

        assert_eq!(
            keys(&after),
            keys(&before),
            "a delivery leaves the sidecar's shape alone"
        );
        for (key, value) in before.as_object().expect("an object") {
            if key == ON_COMPLETE_FIELD {
                continue;
            }
            assert_eq!(
                after.get(key),
                Some(value),
                "{key} did not survive the delivery"
            );
        }
        assert_eq!(
            after[ON_COMPLETE_FIELD],
            serde_json::json!({
                "state": "done",
                "at": delivered_at().to_rfc3339_opts(SecondsFormat::Secs, false),
            })
        );
        assert_eq!(job_for(&sidecar), None);
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

    const RETRY_BASE: Duration = Duration::from_secs(60);
    const RETRY_CAP: Duration = Duration::from_secs(1800);

    #[derive(Clone)]
    struct FakeRunner {
        ran: Arc<Mutex<Vec<Job>>>,
        outcomes: Arc<Mutex<VecDeque<Outcome>>>,
    }

    impl FakeRunner {
        fn new(outcomes: Vec<Outcome>) -> Self {
            Self {
                ran: Arc::new(Mutex::new(Vec::new())),
                outcomes: Arc::new(Mutex::new(VecDeque::from(outcomes))),
            }
        }

        fn ran(&self) -> Vec<Job> {
            let Self { ran, outcomes: _ } = self;
            ran.lock().expect("the recorded runs").clone()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, job: &Job) -> Outcome {
            let Self { ran, outcomes } = self;
            ran.lock().expect("the recorded runs").push(job.clone());
            let mut outcomes = outcomes.lock().expect("the scripted outcomes");
            let Some(outcome) = outcomes.pop_front() else {
                return Outcome::Done;
            };
            outcome
        }
    }

    struct VanishingRunner {
        ran: Arc<AtomicU32>,
    }

    impl Runner for VanishingRunner {
        fn run(&self, job: &Job) -> Outcome {
            let Self { ran } = self;
            let Job {
                sidecar,
                recording: _,
            } = job;
            ran.fetch_add(1, Ordering::SeqCst);
            fs::remove_file(sidecar).expect("take the sidecar away");
            Outcome::Done
        }
    }

    #[derive(Clone)]
    struct FakeClock {
        base: Instant,
        offset: Arc<Mutex<Duration>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                base: Instant::now(),
                offset: Arc::new(Mutex::new(Duration::ZERO)),
            }
        }

        fn now(&self) -> Instant {
            let Self { base, offset } = self;
            *base + *offset.lock().expect("the fake clock")
        }

        fn advance(&self, by: Duration) {
            let Self { base: _, offset } = self;
            let mut offset = offset.lock().expect("the fake clock");
            *offset += by;
        }
    }

    fn settings(dir: &Path) -> HookSettings {
        HookSettings {
            command: "exit 0".to_owned(),
            timeout: Duration::from_secs(5),
            output_dir: dir.to_path_buf(),
            retry_base: RETRY_BASE,
            retry_cap: RETRY_CAP,
        }
    }

    fn eventually(what: &str, condition: impl Fn() -> bool) {
        for _ in 0..500 {
            if condition() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}");
    }

    fn delivered(sidecar: &Path) -> bool {
        let described = fs::read(sidecar).expect("the sidecar");
        let Ledger {
            file: _,
            on_complete,
        } = serde_json::from_slice(&described).expect("valid sidecar json");
        match on_complete {
            Some(OnComplete::Done { at: _ }) => true,
            Some(OnComplete::Pending) | None => false,
        }
    }

    fn recorded(dir: &Path, stem: &str) -> PathBuf {
        let sidecar = write_sidecar(dir, stem, pending());
        write_recording(dir, stem);
        sidecar
    }

    #[test]
    fn a_sidecar_arriving_on_the_channel_is_delivered_once() {
        let dir = TempDir::new();
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(Vec::new());
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        thread::sleep(POLL_INTERVAL);
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        sender.send(sidecar.clone()).expect("hand over the sidecar");

        eventually("the recording was never delivered", || delivered(&sidecar));
        drop(sender);
        thread.join().expect("the hook thread");
        assert_eq!(ran.ran().len(), 1, "a delivered recording is run once");
    }

    #[test]
    fn a_run_that_took_its_sidecar_away_is_never_run_again() {
        let dir = TempDir::new();
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let (sender, receiver) = mpsc::channel();
        let ran = Arc::new(AtomicU32::new(0));
        let runner = VanishingRunner {
            ran: Arc::clone(&ran),
        };
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ticking = clock.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || !sidecar.exists());
        clock.advance(RETRY_CAP);
        thread::sleep(2 * POLL_INTERVAL);
        assert_eq!(
            ran.load(Ordering::SeqCst),
            1,
            "a sidecar that is gone leaves nothing to deliver again"
        );

        drop(sender);
        thread.join().expect("the hook thread");
    }

    #[test]
    fn a_failed_run_is_retried_with_a_doubling_wait() {
        let dir = TempDir::new();
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(vec![
            Outcome::Failed(Failure::Exited(1)),
            Outcome::Failed(Failure::TimedOut),
        ]);
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || {
            ran.ran().len() == 1
        });
        thread::sleep(2 * POLL_INTERVAL);
        assert_eq!(
            ran.ran().len(),
            1,
            "a failed hook waits out its backoff before it is run again"
        );
        assert!(!delivered(&sidecar));

        clock.advance(RETRY_BASE);
        eventually("the first retry never came", || ran.ran().len() == 2);
        thread::sleep(2 * POLL_INTERVAL);
        assert_eq!(
            ran.ran().len(),
            2,
            "the second wait is longer than the first"
        );
        assert!(!delivered(&sidecar));

        clock.advance(2 * RETRY_BASE);
        eventually("the recording was never delivered", || delivered(&sidecar));
        drop(sender);
        thread.join().expect("the hook thread");
        assert_eq!(ran.ran().len(), 3);
    }

    #[test]
    fn the_recordings_found_at_start_are_delivered_in_name_order_before_a_live_one() {
        let dir = TempDir::new();
        let early = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let late = recorded(dir.path(), "2026-09-02T11-00-00-late");
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(Vec::new());
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the recordings found at start were never run", || {
            ran.ran().len() == 2
        });
        let live = recorded(dir.path(), "2026-09-02T08-00-00-live");
        sender.send(live.clone()).expect("hand over the sidecar");

        eventually("the live recording was never delivered", || {
            delivered(&live)
        });
        drop(sender);
        thread.join().expect("the hook thread");

        let mut sidecars = Vec::new();
        for Job {
            sidecar,
            recording: _,
        } in ran.ran()
        {
            sidecars.push(sidecar);
        }
        assert_eq!(sidecars, vec![early, late, live]);
    }

    #[test]
    fn a_recording_waiting_out_its_backoff_does_not_hold_up_a_newer_one() {
        let dir = TempDir::new();
        let stuck = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(vec![Outcome::Failed(Failure::Exited(1))]);
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let raised = shutdown.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || {
            ran.ran().len() == 1
        });

        let live = recorded(dir.path(), "2026-09-02T11-00-00-late");
        sender.send(live.clone()).expect("hand over the sidecar");

        eventually(
            "a newer recording waited out somebody else's backoff",
            || delivered(&live),
        );
        assert!(
            !delivered(&stuck),
            "the failing recording is still owed, and its wait was not cut short"
        );
        raised.request();
        thread.join().expect("the hook thread");
        drop(sender);
    }

    #[test]
    fn a_sidecar_that_is_already_queued_is_not_queued_again() {
        let dir = TempDir::new();
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(vec![Outcome::Failed(Failure::Exited(1))]);
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let raised = shutdown.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || {
            ran.ran().len() == 1
        });
        sender.send(sidecar.clone()).expect("hand over the sidecar");
        thread::sleep(2 * POLL_INTERVAL);

        assert_eq!(
            ran.ran().len(),
            1,
            "a recording the scan already queued must not be run a second time when it arrives on the channel"
        );
        assert!(!delivered(&sidecar));
        raised.request();
        thread.join().expect("the hook thread");
        drop(sender);
    }

    #[test]
    fn a_raised_shutdown_leaves_an_undelivered_recording_pending() {
        let dir = TempDir::new();
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(vec![Outcome::Failed(Failure::Signaled)]);
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let raised = shutdown.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || {
            ran.ran().len() == 1
        });
        raised.request();
        thread.join().expect("the hook thread");

        assert!(
            !delivered(&sidecar),
            "a recording the hook never delivered is still pending on disk"
        );
        drop(sender);
    }

    #[test]
    fn a_disconnected_channel_ends_the_thread() {
        let dir = TempDir::new();
        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(Vec::new());
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ticking = clock.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        drop(sender);
        thread.join().expect("the hook thread");
    }

    #[test]
    fn a_delivery_that_cannot_be_recorded_is_retried() {
        let dir = TempDir::new();
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        let sealed = fs::Permissions::from_mode(0o500);
        fs::set_permissions(dir.path(), sealed).expect("seal the output directory");

        let (sender, receiver) = mpsc::channel();
        let runner = FakeRunner::new(Vec::new());
        let clock = FakeClock::new();
        let shutdown = Shutdown::new();

        let ran = runner.clone();
        let ticking = clock.clone();
        let raised = shutdown.clone();
        let settings = settings(dir.path());
        let thread =
            thread::spawn(move || run(settings, receiver, shutdown, runner, move || ticking.now()));

        eventually("the pending recording was never run", || {
            ran.ran().len() == 1
        });
        assert!(!delivered(&sidecar));

        clock.advance(RETRY_BASE);
        eventually("a delivery mimi could not record was dropped", || {
            ran.ran().len() == 2
        });
        raised.request();
        thread.join().expect("the hook thread");
        assert!(
            !delivered(&sidecar),
            "a delivery mimi could not record must not look delivered"
        );

        let opened = fs::Permissions::from_mode(0o700);
        fs::set_permissions(dir.path(), opened).expect("open the output directory");
        drop(sender);
    }

    #[test]
    fn a_spawned_hook_runs_the_configured_command() {
        let dir = TempDir::new();
        let (sender, receiver) = mpsc::channel();
        let shutdown = Shutdown::new();
        let hook = spawn(
            HookSettings {
                command: "printf '%s\\n' \"$MIMI_SIDECAR\" > delivered".to_owned(),
                timeout: Duration::from_secs(5),
                output_dir: dir.path().to_path_buf(),
                retry_base: RETRY_BASE,
                retry_cap: RETRY_CAP,
            },
            receiver,
            shutdown,
        );

        thread::sleep(POLL_INTERVAL);
        let sidecar = recorded(dir.path(), "2026-09-02T09-00-00-early");
        sender.send(sidecar.clone()).expect("hand over the sidecar");

        eventually("the recording was never delivered", || delivered(&sidecar));
        drop(sender);
        hook.join();

        let probed = fs::read_to_string(dir.path().join("delivered")).expect("the probe");
        assert_eq!(probed, format!("{}\n", sidecar.display()));
    }
}
