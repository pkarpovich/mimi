use std::fs;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{SendError, Sender};

use chrono::{DateTime, Local, SecondsFormat};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{info, warn};

use crate::activity::BundleId;
use crate::capture::Verdict;
use crate::remux;
use crate::writer::Written;

const AUDIO_EXTENSION: &str = "aac";
const PARTIAL_EXTENSION: &str = "partial";

/// SIDECAR_EXTENSION is what tells a recording's metadata apart from the recording itself.
pub const SIDECAR_EXTENSION: &str = "json";

/// RECORDING_MODE keeps a meeting readable by the user who recorded it and nobody else.
pub const RECORDING_MODE: u32 = 0o600;

/// RECORDINGS_DIR_MODE is the mode `output_dir` is created with when it does not exist yet.
pub const RECORDINGS_DIR_MODE: u32 = 0o700;

/// Recording is the finished session a sink accepts, with every field its sidecar carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub partial: PathBuf,
    pub started_at: DateTime<Local>,
    pub ended_at: DateTime<Local>,
    pub bundle_id: BundleId,
    pub label: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub device_changes: u32,
    pub failed_device_changes: u32,
    pub verdict: Verdict,
    pub written: Written,
}

#[derive(Debug, Error)]
pub enum SinkError {
    #[error("{0} is not an in-progress recording")]
    NotPartial(PathBuf),
    #[error("renaming {path}: {source}")]
    Rename { path: PathBuf, source: io::Error },
    #[error("describing {path}: {source}")]
    Describe {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("writing {path}: {source}")]
    Sidecar { path: PathBuf, source: io::Error },
}

/// Sink is what a finished recording is handed to; v1 has one, the local folder.
pub trait Sink {
    fn accept(&self, recording: Recording) -> Result<(), SinkError>;
}

/// LocalFolder completes a recording where it was written, next to its own bytes.
pub struct LocalFolder(Option<Sender<PathBuf>>);

impl LocalFolder {
    /// new builds the local sink, publishing every sidecar it writes when a hook is configured.
    pub fn new(hook: Option<Sender<PathBuf>>) -> Self {
        Self(hook)
    }
}

impl Sink for LocalFolder {
    fn accept(&self, recording: Recording) -> Result<(), SinkError> {
        let Self(hook) = self;
        let Recording {
            partial,
            started_at: _,
            ended_at: _,
            bundle_id: _,
            label: _,
            sample_rate: _,
            channels: _,
            device_changes: _,
            failed_device_changes: _,
            verdict: _,
            written: _,
        } = &recording;

        let Some(completed) = completed_path(partial) else {
            return Err(SinkError::NotPartial(partial.clone()));
        };
        if let Err(source) = fs::rename(partial, &completed) {
            return Err(SinkError::Rename {
                path: partial.clone(),
                source,
            });
        }

        let completed = match remux::to_m4a(&completed, &remux::destination_for(&completed)) {
            Ok(remux::Remuxed { path, packets }) => {
                info!("{} carries {packets} packets", path.display());
                if let Err(source) = fs::remove_file(&completed) {
                    warn!("{} could not be removed: {source}", completed.display());
                }
                path
            }
            Err(error) => {
                warn!("{} was kept as it is: {error}", completed.display());
                completed
            }
        };

        let on_complete = hook.as_ref().map(|_| OnComplete::Pending);
        let described =
            match serde_json::to_vec_pretty(&sidecar(&recording, &completed, on_complete)) {
                Ok(described) => described,
                Err(source) => {
                    return Err(SinkError::Describe {
                        path: completed,
                        source,
                    });
                }
            };
        let completed = completed.with_extension(SIDECAR_EXTENSION);
        if let Err(source) = write_private(&completed, &described) {
            return Err(SinkError::Sidecar {
                path: completed,
                source,
            });
        }

        let Some(hook) = hook else {
            return Ok(());
        };
        if let Err(SendError(sidecar)) = hook.send(completed) {
            warn!("{} did not reach the completion hook", sidecar.display());
        }
        Ok(())
    }
}

/// OnComplete is how far the completion hook has got with the recording beside it.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum OnComplete {
    Pending,
    Done { at: String },
}

#[derive(Debug, Serialize, PartialEq)]
struct Sidecar {
    file: String,
    started_at: String,
    ended_at: String,
    duration_seconds: i64,
    bundle_id: String,
    label: String,
    sample_rate: u32,
    channels: u32,
    device_changes: u32,
    failed_device_changes: u32,
    silent: bool,
    write_failed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_complete: Option<OnComplete>,
}

fn sidecar(recording: &Recording, completed: &Path, on_complete: Option<OnComplete>) -> Sidecar {
    let Recording {
        partial: _,
        started_at,
        ended_at,
        bundle_id,
        label,
        sample_rate,
        channels,
        device_changes,
        failed_device_changes,
        verdict,
        written,
    } = recording;
    let silent = match verdict {
        Verdict::Silent => true,
        Verdict::AudioPresent => false,
        Verdict::Undecided => false,
    };
    let write_failed = match written {
        Written::Failed => true,
        Written::Whole => false,
    };
    let file = match completed.file_name() {
        Some(file) => file.to_string_lossy().into_owned(),
        None => String::new(),
    };
    Sidecar {
        file,
        started_at: started_at.to_rfc3339_opts(SecondsFormat::Secs, false),
        ended_at: ended_at.to_rfc3339_opts(SecondsFormat::Secs, false),
        duration_seconds: ended_at
            .signed_duration_since(started_at)
            .num_seconds()
            .max(0),
        bundle_id: bundle_id.as_str().to_owned(),
        label: label.clone(),
        sample_rate: *sample_rate,
        channels: *channels,
        device_changes: *device_changes,
        failed_device_changes: *failed_device_changes,
        silent,
        write_failed,
        on_complete,
    }
}

/// file_stem is the name a recording's audio file and its sidecar share.
pub fn file_stem(started_at: DateTime<Local>, label: &str) -> String {
    format!("{}-{label}", started_at.format("%Y-%m-%dT%H-%M-%S"))
}

/// reserve creates the in-progress file for the first stem in `dir` no other recording has taken.
pub fn reserve(dir: &Path, stem: String) -> Result<PathBuf, io::Error> {
    let mut candidate = stem.clone();
    let mut suffix: u32 = 2;
    loop {
        // The name is claimed by creating the file, never by looking first: a second recorder
        // walking the same stems in the same second must not be handed a name this one is about to
        // write. The completed siblings are then checked *after* the claim exists, because a
        // recorder that finished in the meantime renamed its own `.partial` away - looking before
        // the claim leaves a window where this reservation lands on a finished recording and the
        // rename at the end of the session overwrites it.
        let path = partial_path(dir, &candidate);
        let created = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(RECORDING_MODE)
            .open(&path);
        match created {
            Ok(_) => {
                if !finished(dir, &candidate) {
                    return Ok(path);
                }
                let _ = fs::remove_file(&path);
            }
            Err(failure) if failure.kind() == io::ErrorKind::AlreadyExists => {}
            Err(failure) => return Err(failure),
        }
        candidate = format!("{stem}-{suffix}");
        suffix += 1;
    }
}

fn partial_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.{AUDIO_EXTENSION}.{PARTIAL_EXTENSION}"))
}

/// write_private replaces `path` with `contents`, readable by the user who recorded it and nobody else.
pub fn write_private(path: &Path, contents: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(RECORDING_MODE)
        .open(path)?;
    file.write_all(contents)
}

fn finished(dir: &Path, stem: &str) -> bool {
    let names = [
        format!("{stem}.{AUDIO_EXTENSION}"),
        format!("{stem}.{SIDECAR_EXTENSION}"),
    ];
    let mut found = false;
    for name in names {
        if dir.join(name).exists() {
            found = true;
            break;
        }
    }
    found
}

fn completed_path(partial: &Path) -> Option<PathBuf> {
    let name = partial.file_name()?.to_str()?;
    let name = name.strip_suffix(&format!(".{PARTIAL_EXTENSION}"))?;
    if name.is_empty() {
        return None;
    }
    Some(partial.with_file_name(name))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;

    use chrono::TimeZone;

    use super::*;
    use crate::capture::{BlockRef, Formats, ring};
    use crate::writer::{self, Finished, WriterSettings};

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mimi-sink-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> &Path {
            let Self(path) = self;
            path
        }

        fn touch(&self, name: &str) {
            fs::write(self.path().join(name), b"x").expect("touch");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_dir_all(path);
        }
    }

    fn started_at() -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 8, 30, 14, 32, 5)
            .single()
            .expect("a local timestamp")
    }

    fn encode(path: &Path) {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        let (producer, consumer) = ring(16, 2048);
        let samples = vec![0.25; 2048];
        for round in 0..8 {
            producer.push(BlockRef {
                microphone: &samples,
                system: &samples,
                frames: 2048,
                host_time: round,
                generation: 1,
            });
        }
        let writer = writer::spawn(
            WriterSettings {
                path: path.to_path_buf(),
                sample_rate: 24_000,
                bit_rate: 96_000,
            },
            consumer,
            formats,
        );
        let Finished { error, verdict: _ } = writer.finish();
        assert_eq!(error, None, "the test needs a real recording to remux");
    }

    fn recording(partial: PathBuf) -> Recording {
        Recording {
            partial,
            started_at: started_at(),
            ended_at: started_at() + chrono::Duration::seconds(1830),
            bundle_id: BundleId::new("company.thebrowser.browser.helper"),
            label: "thebrowser".to_owned(),
            sample_rate: 24_000,
            channels: 2,
            device_changes: 1,
            failed_device_changes: 0,
            verdict: Verdict::AudioPresent,
            written: Written::Whole,
        }
    }

    #[test]
    fn a_file_stem_carries_the_local_start_time_and_the_label() {
        assert_eq!(
            file_stem(started_at(), "thebrowser"),
            "2026-08-30T14-32-05-thebrowser"
        );
    }

    #[test]
    fn an_unused_stem_reserves_the_file_it_was_asked_for() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        let reserved = reserve(dir.path(), stem.clone()).expect("reserve the stem");
        assert_eq!(reserved, partial_path(dir.path(), &stem));
        assert!(
            reserved.exists(),
            "the reservation is on disk, not just a name"
        );
    }

    #[test]
    fn a_taken_stem_gets_a_numeric_suffix_instead_of_overwriting() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        dir.touch(&format!("{stem}.aac"));
        assert_eq!(
            reserve(dir.path(), stem.clone()).expect("suffix past the audio file"),
            partial_path(dir.path(), &format!("{stem}-2"))
        );

        dir.touch(&format!("{stem}-3.json"));
        assert_eq!(
            reserve(dir.path(), stem.clone()).expect("suffix past the sidecar"),
            partial_path(dir.path(), &format!("{stem}-4"))
        );
    }

    #[test]
    fn a_completed_recording_survives_the_walk_past_its_stem() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        dir.touch(&format!("{stem}.aac"));
        dir.touch(&format!("{stem}.json"));

        let reserved = reserve(dir.path(), stem.clone()).expect("reserve past the completed pair");

        assert_eq!(reserved, partial_path(dir.path(), &format!("{stem}-2")));
        assert_eq!(
            fs::read(dir.path().join(format!("{stem}.aac"))).expect("the completed file"),
            b"x",
            "claiming a stem to test it must leave a finished recording untouched"
        );
        assert!(
            !partial_path(dir.path(), &stem).exists(),
            "the claim that landed on a finished recording is given back"
        );
    }

    #[test]
    fn a_reservation_is_not_handed_out_twice() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        let first = reserve(dir.path(), stem.clone()).expect("the first reservation");
        let second = reserve(dir.path(), stem.clone()).expect("the second reservation");
        assert_eq!(first, partial_path(dir.path(), &stem));
        assert_eq!(
            second,
            partial_path(dir.path(), &format!("{stem}-2")),
            "a stem another recorder is already writing is taken"
        );
    }

    #[test]
    fn a_reserved_recording_is_readable_only_by_its_owner() {
        let dir = TempDir::new();
        let reserved =
            reserve(dir.path(), file_stem(started_at(), "zoom")).expect("reserve the stem");
        let mode = fs::metadata(&reserved)
            .expect("the reserved file")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a meeting must not be readable by group or other"
        );
    }

    #[test]
    fn a_partial_path_lives_in_the_output_directory() {
        assert_eq!(
            partial_path(Path::new("/tmp/mimi"), "2026-08-30T14-32-05-zoom"),
            PathBuf::from("/tmp/mimi/2026-08-30T14-32-05-zoom.aac.partial")
        );
    }

    #[test]
    fn a_sidecar_carries_every_documented_field() {
        let described = serde_json::to_value(sidecar(
            &recording(PathBuf::from("/tmp/x.aac.partial")),
            Path::new("/tmp/2026-08-30T14-32-05-thebrowser.m4a"),
            None,
        ))
        .expect("serialize the sidecar");
        assert_eq!(described["file"], "2026-08-30T14-32-05-thebrowser.m4a");
        assert_eq!(described["label"], "thebrowser");
        assert_eq!(
            described["started_at"],
            started_at().to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(
            described["ended_at"],
            (started_at() + chrono::Duration::seconds(1830))
                .to_rfc3339_opts(SecondsFormat::Secs, false)
        );
        assert_eq!(described["duration_seconds"], 1830);
        assert_eq!(described["bundle_id"], "company.thebrowser.browser.helper");
        assert_eq!(described["sample_rate"], 24_000);
        assert_eq!(described["channels"], 2);
        assert_eq!(described["device_changes"], 1);
        assert_eq!(described["failed_device_changes"], 0);
        assert_eq!(described["silent"], false);
        assert_eq!(described["write_failed"], false);
        assert_eq!(described.get("on_complete"), None);
        assert_eq!(
            described.as_object().expect("an object").len(),
            12,
            "the sidecar carries no field the plan does not document"
        );
    }

    #[test]
    fn a_sidecar_written_under_a_hook_starts_out_pending() {
        let described = serde_json::to_value(sidecar(
            &recording(PathBuf::from("/tmp/x.aac.partial")),
            Path::new("/tmp/2026-08-30T14-32-05-thebrowser.m4a"),
            Some(OnComplete::Pending),
        ))
        .expect("serialize the sidecar");
        assert_eq!(
            described["on_complete"],
            serde_json::json!({"state": "pending"})
        );
        assert_eq!(
            described.as_object().expect("an object").len(),
            13,
            "the hook adds one field and nothing else"
        );
    }

    #[test]
    fn a_delivery_state_survives_a_round_trip_through_json() {
        for state in [
            OnComplete::Pending,
            OnComplete::Done {
                at: "2026-09-02T16:21:03+02:00".to_owned(),
            },
        ] {
            let described = serde_json::to_string(&state).expect("serialize the state");
            assert_eq!(
                serde_json::from_str::<OnComplete>(&described).expect("read the state back"),
                state
            );
        }
    }

    #[test]
    fn a_silent_verdict_marks_the_sidecar_and_an_undecided_one_does_not() {
        let mut recording = recording(PathBuf::from("/tmp/x.aac.partial"));
        recording.verdict = Verdict::Silent;
        assert!(sidecar(&recording, Path::new("/tmp/x.m4a"), None).silent);
        recording.verdict = Verdict::Undecided;
        assert!(!sidecar(&recording, Path::new("/tmp/x.m4a"), None).silent);
    }

    #[test]
    fn a_writer_that_gave_up_marks_the_sidecar() {
        let mut recording = recording(PathBuf::from("/tmp/x.aac.partial"));
        recording.written = Written::Failed;
        assert!(
            sidecar(&recording, Path::new("/tmp/x.m4a"), None).write_failed,
            "a file the writer abandoned part way must not look complete"
        );
    }

    #[test]
    fn accepting_a_recording_renames_it_in_place_and_writes_the_sidecar_beside_it() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "thebrowser");
        let partial = partial_path(dir.path(), &stem);
        fs::write(&partial, b"adts").expect("write the partial file");

        LocalFolder::new(None)
            .accept(recording(partial.clone()))
            .expect("accept the recording");

        assert!(!partial.exists(), "the .partial suffix is gone");
        let completed = dir.path().join(format!("{stem}.aac"));
        assert_eq!(fs::read(&completed).expect("the completed file"), b"adts");

        let sidecar = dir.path().join(format!("{stem}.json"));
        let described = fs::read_to_string(&sidecar).expect("the sidecar beside it");
        let described: serde_json::Value =
            serde_json::from_str(&described).expect("valid sidecar json");
        assert_eq!(described["duration_seconds"], 1830);
        assert_eq!(described["device_changes"], 1);
        assert_eq!(
            described["file"],
            format!("{stem}.aac"),
            "a recording the remux left alone is named as the aac it stayed"
        );

        let mode = fs::metadata(&sidecar)
            .expect("the sidecar")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "a sidecar names the meeting application and must not be readable by group or other"
        );
    }

    #[test]
    fn a_remuxed_recording_is_named_by_the_m4a_that_replaced_it() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "thebrowser");
        let partial = partial_path(dir.path(), &stem);
        encode(&partial);

        LocalFolder::new(None)
            .accept(recording(partial))
            .expect("accept the recording");

        let described = fs::read_to_string(dir.path().join(format!("{stem}.json")))
            .expect("the sidecar beside it");
        let described: serde_json::Value =
            serde_json::from_str(&described).expect("valid sidecar json");
        assert_eq!(described["file"], format!("{stem}.m4a"));
        assert!(
            dir.path().join(format!("{stem}.m4a")).exists(),
            "the sidecar must name a file that is on disk"
        );
    }

    #[test]
    fn a_hooked_sink_marks_the_sidecar_pending_and_hands_over_its_path() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "thebrowser");
        let partial = partial_path(dir.path(), &stem);
        fs::write(&partial, b"adts").expect("write the partial file");

        let (hook, delivered) = mpsc::channel();
        LocalFolder::new(Some(hook))
            .accept(recording(partial))
            .expect("accept the recording");

        let sidecar = dir.path().join(format!("{stem}.json"));
        assert_eq!(
            delivered
                .try_recv()
                .expect("the hook was handed the sidecar"),
            sidecar
        );
        let described = fs::read_to_string(&sidecar).expect("the sidecar beside it");
        let described: serde_json::Value =
            serde_json::from_str(&described).expect("valid sidecar json");
        assert_eq!(
            described["on_complete"],
            serde_json::json!({"state": "pending"}),
            "a recording is pending from the moment its sidecar exists"
        );
    }

    #[test]
    fn a_hook_that_is_gone_does_not_fail_the_recording() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "thebrowser");
        let partial = partial_path(dir.path(), &stem);
        fs::write(&partial, b"adts").expect("write the partial file");

        let (hook, delivered) = mpsc::channel();
        drop(delivered);
        LocalFolder::new(Some(hook))
            .accept(recording(partial))
            .expect("a hook thread that ended must not lose the recording");

        assert!(dir.path().join(format!("{stem}.json")).exists());
    }

    #[test]
    fn a_path_without_the_partial_suffix_is_refused() {
        let dir = TempDir::new();
        let completed = dir.path().join("2026-08-30T14-32-05-zoom.aac");
        fs::write(&completed, b"adts").expect("write the file");
        let failure = LocalFolder::new(None)
            .accept(recording(completed.clone()))
            .expect_err("a completed file is not an in-progress recording");
        match failure {
            SinkError::NotPartial(path) => assert_eq!(path, completed),
            SinkError::Rename { path: _, source: _ }
            | SinkError::Describe { path: _, source: _ }
            | SinkError::Sidecar { path: _, source: _ } => panic!("{failure}"),
        }
    }

    #[test]
    fn a_missing_partial_file_is_reported_as_a_rename_failure() {
        let dir = TempDir::new();
        let partial = partial_path(dir.path(), "2026-08-30T14-32-05-zoom");
        let failure = LocalFolder::new(None)
            .accept(recording(partial.clone()))
            .expect_err("nothing to rename");
        match failure {
            SinkError::Rename { path, source: _ } => assert_eq!(path, partial),
            SinkError::NotPartial(_)
            | SinkError::Describe { path: _, source: _ }
            | SinkError::Sidecar { path: _, source: _ } => panic!("{failure}"),
        }
    }
}
