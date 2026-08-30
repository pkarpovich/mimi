mod decide;

use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use signal_hook::consts::{SIGINT, SIGTERM};
use signal_hook::flag;
use tracing::{error, info, warn};

use crate::activity::{ActivityEvent, BundleId, Devices};
use crate::capture::{self, Capture, DEFAULT_FRAMES_PER_BLOCK, DEFAULT_SLOTS, Rebuilds, Verdict};
use crate::config::BundlePrefix;
use crate::sink::{self, Recording, Sink};
use crate::writer::{self, CHANNELS, Finished, Writer, WriterSettings};

const IDLE_TICK: Duration = Duration::from_millis(200);

pub use decide::{Decider, SessionCommand, SessionStart};

/// Settings is what the run loop needs from configuration to record a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub output_dir: PathBuf,
    pub prefixes: Vec<BundlePrefix>,
    pub sample_rate: u32,
    pub bit_rate: u32,
    pub grace: Duration,
}

/// Shutdown is the flag the signal handlers raise and the run loop reads between events.
#[derive(Debug, Clone, Default)]
pub struct Shutdown(Arc<AtomicBool>);

impl Shutdown {
    pub fn new() -> Self {
        Self(Arc::new(AtomicBool::new(false)))
    }

    /// install asks SIGINT and SIGTERM to raise this flag instead of ending the process.
    pub fn install(&self) -> Result<(), io::Error> {
        let Self(raised) = self;
        flag::register(SIGINT, Arc::clone(raised))?;
        flag::register(SIGTERM, Arc::clone(raised))?;
        Ok(())
    }

    pub fn requested(&self) -> bool {
        let Self(raised) = self;
        raised.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn request(&self) {
        let Self(raised) = self;
        raised.store(true, Ordering::Relaxed);
    }
}

struct Session {
    partial: PathBuf,
    started_at: DateTime<Local>,
    bundle_id: BundleId,
    writer: Writer,
}

struct Output {
    dir: PathBuf,
    sample_rate: u32,
    bit_rate: u32,
}

/// run drives sessions until the events end or a shutdown is requested.
pub fn run(
    settings: Settings,
    devices: Devices,
    capture: &mut impl Capture,
    sink: &impl Sink,
    events: Receiver<ActivityEvent>,
    shutdown: &Shutdown,
) {
    let Settings {
        output_dir,
        prefixes,
        sample_rate,
        bit_rate,
        grace,
    } = settings;
    let output = Output {
        dir: output_dir,
        sample_rate,
        bit_rate,
    };
    let mut decider = Decider::new(prefixes, grace);
    let mut devices = devices;
    let mut session = None;

    while !shutdown.requested() {
        let event = match events.recv_timeout(IDLE_TICK) {
            Ok(event) => event,
            Err(RecvTimeoutError::Timeout) => ActivityEvent::Tick,
            Err(RecvTimeoutError::Disconnected) => break,
        };

        for command in decider.observe(&event, Instant::now()) {
            match command {
                SessionCommand::Start(start) => {
                    session = start_session(capture, &devices, &output, start);
                    match &session {
                        Some(_) => {}
                        None => decider.abandon(),
                    }
                }
                SessionCommand::Stop => finish_session(capture, sink, &output, session.take()),
            }
        }

        match event {
            ActivityEvent::InputTaken(_) | ActivityEvent::InputReleased(_) => {}
            ActivityEvent::Tick => {}
            ActivityEvent::DevicesChanged(sampled) => {
                devices = sampled;
                match &session {
                    None => {}
                    Some(_) => devices_changed(capture, &devices),
                }
            }
        }
    }

    finish_session(capture, sink, &output, session.take());
}

fn start_session(
    capture: &mut impl Capture,
    devices: &Devices,
    output: &Output,
    start: SessionStart,
) -> Option<Session> {
    let SessionStart { bundle_id, label } = start;
    let Output {
        dir,
        sample_rate,
        bit_rate,
    } = output;
    if let Err(failure) = fs::create_dir_all(dir) {
        error!("{} is not usable: {failure}", dir.display());
        return None;
    }

    let started_at = Local::now();
    let stem = sink::unused_stem(dir, sink::file_stem(started_at, &label));
    let partial = sink::partial_path(dir, &stem);

    let (producer, consumer) = capture::ring(DEFAULT_SLOTS, DEFAULT_FRAMES_PER_BLOCK);
    if let Err(failure) = capture.start(devices, producer) {
        error!("capture did not start: {failure}");
        return None;
    }
    let writer = writer::spawn(
        WriterSettings {
            path: partial.clone(),
            sample_rate: *sample_rate,
            bit_rate: *bit_rate,
        },
        consumer,
        capture.formats(),
    );
    info!(
        "session started by {} at {:?} Hz on {:?} into {}",
        bundle_id.as_str(),
        capture.sample_rate(),
        capture.tracks(),
        partial.display()
    );
    Some(Session {
        partial,
        started_at,
        bundle_id,
        writer,
    })
}

fn finish_session(
    capture: &mut impl Capture,
    sink: &impl Sink,
    output: &Output,
    session: Option<Session>,
) {
    let Some(session) = session else {
        return;
    };
    let Session {
        partial,
        started_at,
        bundle_id,
        writer,
    } = session;
    let Output {
        dir: _,
        sample_rate,
        bit_rate: _,
    } = output;

    capture.stop();
    let Rebuilds { succeeded, failed } = capture.rebuilds();
    let Finished { error, verdict } = writer.finish();
    if let Some(failure) = error {
        error!("writing {} failed: {failure}", partial.display());
    }
    match verdict {
        Verdict::AudioPresent => info!("{} opened with audio present", partial.display()),
        Verdict::Undecided => info!("{} was too short to judge for silence", partial.display()),
        Verdict::Silent => warn!("{} opened on digital silence", partial.display()),
    }

    let recording = Recording {
        partial,
        started_at,
        ended_at: Local::now(),
        bundle_id,
        sample_rate: *sample_rate,
        channels: CHANNELS,
        device_changes: succeeded,
        failed_device_changes: failed,
        verdict,
    };
    let Err(failure) = sink.accept(recording) else {
        info!("session ended after {succeeded} rebuilds and {failed} exhausted ones");
        return;
    };
    error!("the recording was not completed: {failure}");
}

/// devices_changed forwards a device change to capture, which rebuilds only if it has to.
pub fn devices_changed(capture: &mut impl Capture, devices: &Devices) {
    let Devices {
        input,
        output,
        sample_rate,
    } = devices;
    let Err(failure) = capture.rebuild(devices) else {
        info!("the devices changed to input {input:?}, output {output:?}, {sample_rate:?} Hz");
        return;
    };
    error!("capture did not rebuild on the current devices: {failure}");
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU32, AtomicUsize};
    use std::sync::mpsc;
    use std::thread;

    use super::*;
    use crate::activity::poller::poll;
    use crate::activity::{
        ActivitySource, AudioProcess, DeviceSource, DeviceUid, InputState, OutputState,
    };
    use crate::capture::{CaptureError, Formats, Producer, TrackKind};
    use crate::sink::SinkError;

    static NEXT_DIR: AtomicU32 = AtomicU32::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let id = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("mimi-session-test-{}-{id}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }

        fn path(&self) -> PathBuf {
            let Self(path) = self;
            path.clone()
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_dir_all(path);
        }
    }

    struct FakeCapture {
        rebuilt: Vec<Devices>,
        starts: usize,
        stops: usize,
        rebuilds: Rebuilds,
        failure: Option<CaptureError>,
        start_failure: Option<CaptureError>,
    }

    impl FakeCapture {
        fn new(failure: Option<CaptureError>) -> Self {
            Self {
                rebuilt: Vec::new(),
                starts: 0,
                stops: 0,
                rebuilds: Rebuilds::default(),
                failure,
                start_failure: None,
            }
        }

        fn failing_to_start(failure: CaptureError) -> Self {
            let mut capture = Self::new(None);
            capture.start_failure = Some(failure);
            capture
        }
    }

    impl Capture for FakeCapture {
        fn start(&mut self, _devices: &Devices, _producer: Producer) -> Result<(), CaptureError> {
            self.starts += 1;
            self.rebuilds = Rebuilds::default();
            let Some(failure) = self.start_failure else {
                return Ok(());
            };
            Err(failure)
        }

        fn stop(&mut self) {
            self.stops += 1;
        }

        fn rebuild(&mut self, devices: &Devices) -> Result<(), CaptureError> {
            self.rebuilt.push(devices.clone());
            let Some(failure) = self.failure else {
                self.rebuilds.succeeded += 1;
                return Ok(());
            };
            self.rebuilds.failed += 1;
            Err(failure)
        }

        fn sample_rate(&self) -> Option<f64> {
            None
        }

        fn tracks(&self) -> &[TrackKind] {
            &[]
        }

        fn formats(&self) -> Formats {
            Formats::new()
        }

        fn rebuilds(&self) -> Rebuilds {
            self.rebuilds
        }
    }

    #[derive(Default)]
    struct FakeSink(Mutex<Vec<Recording>>);

    impl FakeSink {
        fn accepted(&self) -> Vec<Recording> {
            let Self(accepted) = self;
            accepted.lock().expect("the accepted recordings").clone()
        }
    }

    impl Sink for FakeSink {
        fn accept(&self, recording: Recording) -> Result<(), SinkError> {
            let Self(accepted) = self;
            accepted
                .lock()
                .expect("the accepted recordings")
                .push(recording);
            Ok(())
        }
    }

    struct FakeActivity {
        processes: Vec<Vec<AudioProcess>>,
        devices: Vec<Devices>,
        snapshots: AtomicUsize,
        samples: AtomicUsize,
    }

    impl ActivitySource for FakeActivity {
        fn snapshot(&self) -> Vec<AudioProcess> {
            let index = self.snapshots.fetch_add(1, Ordering::Relaxed);
            let index = index.min(self.processes.len() - 1);
            self.processes[index].clone()
        }
    }

    impl DeviceSource for FakeActivity {
        fn devices(&self) -> Devices {
            let index = self.samples.fetch_add(1, Ordering::Relaxed);
            let index = index.min(self.devices.len() - 1);
            self.devices[index].clone()
        }
    }

    fn recording_in_progress(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        let mut found = false;
        for entry in entries {
            let Ok(entry) = entry else {
                continue;
            };
            if entry.path().extension() == Some(OsStr::new("partial")) {
                found = true;
                break;
            }
        }
        found
    }

    fn devices(input: &str) -> Devices {
        Devices {
            input: Some(DeviceUid::new(input)),
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(48_000.0),
        }
    }

    fn process(input: InputState) -> AudioProcess {
        AudioProcess {
            object: 11,
            bundle_id: Some(BundleId::new("company.thebrowser.browser.helper")),
            pid: 4242,
            input,
            output: OutputState::Idle,
        }
    }

    fn settings(dir: PathBuf, grace: Duration) -> Settings {
        Settings {
            output_dir: dir,
            prefixes: vec![BundlePrefix::new("company.thebrowser.")],
            sample_rate: 24_000,
            bit_rate: 96_000,
            grace,
        }
    }

    #[test]
    fn a_device_change_reaches_capture_as_a_rebuild() {
        let mut capture = FakeCapture::new(None);
        devices_changed(&mut capture, &devices("BuiltInMicrophoneDevice"));
        assert_eq!(capture.rebuilt, vec![devices("BuiltInMicrophoneDevice")]);
        assert_eq!(capture.stops, 0, "a device change does not end the session");
    }

    #[test]
    fn a_rebuild_that_failed_leaves_the_session_recording() {
        let mut capture = FakeCapture::new(Some(CaptureError::NoOutputDevice));
        devices_changed(&mut capture, &devices("20-F4-D4-60-E4-29:input"));
        assert_eq!(capture.rebuilt.len(), 1);
        assert_eq!(
            capture.stops, 0,
            "an exhausted rebuild keeps the file rather than ending the session"
        );
    }

    #[test]
    fn a_shutdown_request_is_visible_to_the_run_loop() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.requested());
        let other = shutdown.clone();
        other.request();
        assert!(shutdown.requested());
    }

    #[test]
    fn one_meeting_produces_one_recording_across_a_device_change() {
        let dir = TempDir::new();
        let source = FakeActivity {
            processes: vec![
                vec![process(InputState::Running)],
                vec![process(InputState::Running)],
                vec![process(InputState::Idle)],
            ],
            devices: vec![
                devices("BuiltInMicrophoneDevice"),
                devices("BuiltInMicrophoneDevice"),
                devices("20-F4-D4-60-E4-29:input"),
            ],
            snapshots: AtomicUsize::new(0),
            samples: AtomicUsize::new(0),
        };
        let mut capture = FakeCapture::new(None);
        let sink = FakeSink::default();
        let shutdown = Shutdown::new();
        let (events, incoming) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| poll(&source, Duration::from_millis(5), stopped, events));
            scope.spawn(|| {
                run(
                    settings(dir.path(), Duration::from_millis(150)),
                    devices("BuiltInMicrophoneDevice"),
                    &mut capture,
                    &sink,
                    incoming,
                    &shutdown,
                );
            });

            let deadline = Instant::now() + Duration::from_secs(10);
            while sink.accepted().is_empty() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            shutdown.request();
            stop.send(()).expect("stop the poller");
        });

        let accepted = sink.accepted();
        assert_eq!(accepted.len(), 1, "one meeting is one recording");
        let Recording {
            partial,
            started_at,
            ended_at,
            bundle_id,
            sample_rate,
            channels,
            device_changes,
            failed_device_changes,
            verdict,
        } = &accepted[0];
        assert!(
            partial.starts_with(dir.path()),
            "{} is written where it will live",
            partial.display()
        );
        assert!(partial.exists(), "the in-progress file is on disk");
        assert!(ended_at >= started_at);
        assert_eq!(
            bundle_id,
            &BundleId::new("company.thebrowser.browser.helper")
        );
        assert_eq!(*sample_rate, 24_000);
        assert_eq!(*channels, 2);
        assert_eq!(*device_changes, 1);
        assert_eq!(*failed_device_changes, 0);
        assert_eq!(*verdict, Verdict::Undecided);

        assert_eq!(capture.starts, 1);
        assert_eq!(capture.stops, 1);
        assert_eq!(
            capture.rebuilt,
            vec![devices("20-F4-D4-60-E4-29:input")],
            "the devices-changed event reached capture"
        );
    }

    #[test]
    fn a_shutdown_closes_the_recording_in_progress_through_the_session_end_path() {
        let dir = TempDir::new();
        let source = FakeActivity {
            processes: vec![vec![process(InputState::Running)]],
            devices: vec![devices("BuiltInMicrophoneDevice")],
            snapshots: AtomicUsize::new(0),
            samples: AtomicUsize::new(0),
        };
        let mut capture = FakeCapture::new(None);
        let sink = FakeSink::default();
        let shutdown = Shutdown::new();
        let (events, incoming) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| poll(&source, Duration::from_millis(5), stopped, events));
            scope.spawn(|| {
                run(
                    settings(dir.path(), Duration::from_secs(3600)),
                    devices("BuiltInMicrophoneDevice"),
                    &mut capture,
                    &sink,
                    incoming,
                    &shutdown,
                );
            });

            let deadline = Instant::now() + Duration::from_secs(10);
            while !recording_in_progress(&dir.path()) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            shutdown.request();
            stop.send(()).expect("stop the poller");
        });

        assert_eq!(
            sink.accepted().len(),
            1,
            "the meeting still in progress is handed to the sink"
        );
        assert_eq!(capture.stops, 1);
    }

    #[test]
    fn an_exhausted_rebuild_keeps_the_recording_and_is_reported_in_the_sidecar() {
        let dir = TempDir::new();
        let mut capture = FakeCapture::new(Some(CaptureError::NoOutputDevice));
        let sink = FakeSink::default();
        let shutdown = Shutdown::new();
        let (events, incoming) = mpsc::channel();

        events
            .send(ActivityEvent::InputTaken(process(InputState::Running)))
            .expect("send the take");
        events
            .send(ActivityEvent::DevicesChanged(devices(
                "20-F4-D4-60-E4-29:input",
            )))
            .expect("send the device change");
        drop(events);

        run(
            settings(dir.path(), Duration::from_millis(10)),
            devices("BuiltInMicrophoneDevice"),
            &mut capture,
            &sink,
            incoming,
            &shutdown,
        );

        let accepted = sink.accepted();
        assert_eq!(accepted.len(), 1, "an exhausted rebuild keeps the file");
        assert_eq!(accepted[0].device_changes, 0);
        assert_eq!(
            accepted[0].failed_device_changes, 1,
            "a capture that never came back must say so in the sidecar"
        );
    }

    #[test]
    fn a_capture_that_does_not_start_records_nothing_and_is_attempted_again() {
        let dir = TempDir::new();
        let mut capture = FakeCapture::failing_to_start(CaptureError::NoOutputDevice);
        let sink = FakeSink::default();
        let shutdown = Shutdown::new();
        let (events, incoming) = mpsc::channel();

        events
            .send(ActivityEvent::InputTaken(process(InputState::Running)))
            .expect("send the take");
        events
            .send(ActivityEvent::InputTaken(AudioProcess {
                object: 12,
                bundle_id: Some(BundleId::new("company.thebrowser.browser.helper")),
                pid: 4243,
                input: InputState::Running,
                output: OutputState::Idle,
            }))
            .expect("send the second take");
        drop(events);

        run(
            settings(dir.path(), Duration::from_millis(10)),
            devices("BuiltInMicrophoneDevice"),
            &mut capture,
            &sink,
            incoming,
            &shutdown,
        );

        assert_eq!(
            capture.starts, 2,
            "the next holder of the microphone must be given another attempt"
        );
        assert_eq!(sink.accepted().len(), 0, "nothing was recorded");
        assert!(
            !recording_in_progress(&dir.path()),
            "a capture that never started leaves no file behind"
        );
    }

    #[test]
    fn a_meeting_that_never_starts_hands_nothing_to_the_sink() {
        let dir = TempDir::new();
        let mut capture = FakeCapture::new(None);
        let sink = FakeSink::default();
        let shutdown = Shutdown::new();
        let (events, incoming) = mpsc::channel();

        events
            .send(ActivityEvent::InputTaken(AudioProcess {
                object: 3,
                bundle_id: Some(BundleId::new("com.apple.CoreSpeech")),
                pid: 77,
                input: InputState::Running,
                output: OutputState::Idle,
            }))
            .expect("send the event");
        drop(events);

        run(
            settings(dir.path(), Duration::from_millis(10)),
            devices("BuiltInMicrophoneDevice"),
            &mut capture,
            &sink,
            incoming,
            &shutdown,
        );

        assert_eq!(capture.starts, 0);
        assert_eq!(sink.accepted().len(), 0);
    }
}
