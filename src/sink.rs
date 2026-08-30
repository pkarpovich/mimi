use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, SecondsFormat};
use serde::Serialize;
use thiserror::Error;

use crate::activity::BundleId;
use crate::capture::Verdict;
use crate::writer::Written;

const AUDIO_EXTENSION: &str = "aac";
const PARTIAL_EXTENSION: &str = "partial";
const SIDECAR_EXTENSION: &str = "json";

/// Recording is the finished session a sink accepts, with every field its sidecar carries.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub partial: PathBuf,
    pub started_at: DateTime<Local>,
    pub ended_at: DateTime<Local>,
    pub bundle_id: BundleId,
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
pub struct LocalFolder;

impl Sink for LocalFolder {
    fn accept(&self, recording: Recording) -> Result<(), SinkError> {
        let sidecar = sidecar(&recording);
        let Recording {
            partial,
            started_at: _,
            ended_at: _,
            bundle_id: _,
            sample_rate: _,
            channels: _,
            device_changes: _,
            failed_device_changes: _,
            verdict: _,
            written: _,
        } = recording;

        let Some(completed) = completed_path(&partial) else {
            return Err(SinkError::NotPartial(partial));
        };
        if let Err(source) = fs::rename(&partial, &completed) {
            return Err(SinkError::Rename {
                path: partial,
                source,
            });
        }

        let described = match serde_json::to_vec_pretty(&sidecar) {
            Ok(described) => described,
            Err(source) => {
                return Err(SinkError::Describe {
                    path: completed,
                    source,
                });
            }
        };
        let completed = completed.with_extension(SIDECAR_EXTENSION);
        if let Err(source) = fs::write(&completed, described) {
            return Err(SinkError::Sidecar {
                path: completed,
                source,
            });
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, PartialEq)]
struct Sidecar {
    started_at: String,
    ended_at: String,
    duration_seconds: i64,
    bundle_id: String,
    sample_rate: u32,
    channels: u32,
    device_changes: u32,
    failed_device_changes: u32,
    silent: bool,
    write_failed: bool,
}

fn sidecar(recording: &Recording) -> Sidecar {
    let Recording {
        partial: _,
        started_at,
        ended_at,
        bundle_id,
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
    Sidecar {
        started_at: started_at.to_rfc3339_opts(SecondsFormat::Secs, false),
        ended_at: ended_at.to_rfc3339_opts(SecondsFormat::Secs, false),
        duration_seconds: ended_at
            .signed_duration_since(started_at)
            .num_seconds()
            .max(0),
        bundle_id: bundle_id.as_str().to_owned(),
        sample_rate: *sample_rate,
        channels: *channels,
        device_changes: *device_changes,
        failed_device_changes: *failed_device_changes,
        silent,
        write_failed,
    }
}

/// file_stem is the name a recording's audio file and its sidecar share.
pub fn file_stem(started_at: DateTime<Local>, label: &str) -> String {
    format!("{}-{label}", started_at.format("%Y-%m-%dT%H-%M-%S"))
}

/// unused_stem answers with the first stem in `dir` that no other recording has taken.
pub fn unused_stem(dir: &Path, stem: String) -> String {
    let mut candidate = stem.clone();
    let mut suffix: u32 = 2;
    while taken(dir, &candidate) {
        candidate = format!("{stem}-{suffix}");
        suffix += 1;
    }
    candidate
}

/// partial_path is where a session writes while it is still recording.
pub fn partial_path(dir: &Path, stem: &str) -> PathBuf {
    dir.join(format!("{stem}.{AUDIO_EXTENSION}.{PARTIAL_EXTENSION}"))
}

fn taken(dir: &Path, stem: &str) -> bool {
    let names = [
        format!("{stem}.{AUDIO_EXTENSION}"),
        format!("{stem}.{AUDIO_EXTENSION}.{PARTIAL_EXTENSION}"),
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
    use std::sync::atomic::{AtomicU32, Ordering};

    use chrono::TimeZone;

    use super::*;

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

    fn recording(partial: PathBuf) -> Recording {
        Recording {
            partial,
            started_at: started_at(),
            ended_at: started_at() + chrono::Duration::seconds(1830),
            bundle_id: BundleId::new("company.thebrowser.browser.helper"),
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
    fn an_unused_stem_is_returned_unchanged() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        assert_eq!(unused_stem(dir.path(), stem.clone()), stem);
    }

    #[test]
    fn a_taken_stem_gets_a_numeric_suffix_instead_of_overwriting() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "zoom");
        dir.touch(&format!("{stem}.aac"));
        assert_eq!(unused_stem(dir.path(), stem.clone()), format!("{stem}-2"));

        dir.touch(&format!("{stem}-2.json"));
        assert_eq!(unused_stem(dir.path(), stem.clone()), format!("{stem}-3"));

        dir.touch(&format!("{stem}-3.aac.partial"));
        assert_eq!(unused_stem(dir.path(), stem.clone()), format!("{stem}-4"));
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
        let described =
            serde_json::to_value(sidecar(&recording(PathBuf::from("/tmp/x.aac.partial"))))
                .expect("serialize the sidecar");
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
        assert_eq!(
            described.as_object().expect("an object").len(),
            10,
            "the sidecar carries no field the plan does not document"
        );
    }

    #[test]
    fn a_silent_verdict_marks_the_sidecar_and_an_undecided_one_does_not() {
        let mut recording = recording(PathBuf::from("/tmp/x.aac.partial"));
        recording.verdict = Verdict::Silent;
        assert!(sidecar(&recording).silent);
        recording.verdict = Verdict::Undecided;
        assert!(!sidecar(&recording).silent);
    }

    #[test]
    fn a_writer_that_gave_up_marks_the_sidecar() {
        let mut recording = recording(PathBuf::from("/tmp/x.aac.partial"));
        recording.written = Written::Failed;
        assert!(
            sidecar(&recording).write_failed,
            "a file the writer abandoned part way must not look complete"
        );
    }

    #[test]
    fn accepting_a_recording_renames_it_in_place_and_writes_the_sidecar_beside_it() {
        let dir = TempDir::new();
        let stem = file_stem(started_at(), "thebrowser");
        let partial = partial_path(dir.path(), &stem);
        fs::write(&partial, b"adts").expect("write the partial file");

        LocalFolder
            .accept(recording(partial.clone()))
            .expect("accept the recording");

        assert!(!partial.exists(), "the .partial suffix is gone");
        let completed = dir.path().join(format!("{stem}.aac"));
        assert_eq!(fs::read(&completed).expect("the completed file"), b"adts");

        let described = fs::read_to_string(dir.path().join(format!("{stem}.json")))
            .expect("the sidecar beside it");
        let described: serde_json::Value =
            serde_json::from_str(&described).expect("valid sidecar json");
        assert_eq!(described["duration_seconds"], 1830);
        assert_eq!(described["device_changes"], 1);
    }

    #[test]
    fn a_path_without_the_partial_suffix_is_refused() {
        let dir = TempDir::new();
        let completed = dir.path().join("2026-08-30T14-32-05-zoom.aac");
        fs::write(&completed, b"adts").expect("write the file");
        let failure = LocalFolder
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
        let failure = LocalFolder
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
