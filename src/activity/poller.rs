use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use objc2_core_audio::{
    AudioObjectID, AudioObjectPropertySelector, kAudioDevicePropertyDeviceUID,
    kAudioDevicePropertyNominalSampleRate, kAudioHardwarePropertyDefaultInputDevice,
    kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectUnknown,
};

use super::diff::diff;
use super::{
    ActivityEvent, ActivitySource, AudioProcess, DeviceSource, DeviceUid, Devices, audio_processes,
};
use crate::macos;

/// CoreAudio is the live source, reading the audio system and hiding mimi's own process.
pub struct CoreAudio {
    pid: i32,
}

impl CoreAudio {
    pub fn live() -> Self {
        Self {
            pid: std::process::id() as i32,
        }
    }
}

impl ActivitySource for CoreAudio {
    fn snapshot(&self) -> Vec<AudioProcess> {
        let Self { pid } = self;
        without_pid(audio_processes(), *pid)
    }
}

impl DeviceSource for CoreAudio {
    fn devices(&self) -> Devices {
        Devices {
            input: default_device_uid(kAudioHardwarePropertyDefaultInputDevice),
            output: default_device_uid(kAudioHardwarePropertyDefaultOutputDevice),
            sample_rate: default_input_rate(),
        }
    }
}

/// poll emits activity events every `interval` until `stop` receives a value or its sender drops.
pub fn poll<S>(source: &S, interval: Duration, stop: Receiver<()>, events: Sender<ActivityEvent>)
where
    S: ActivitySource + DeviceSource,
{
    let mut processes = Vec::new();
    let mut devices = source.devices();
    loop {
        let snapshot = source.snapshot();
        for event in diff(&processes, &snapshot) {
            let Ok(()) = events.send(event) else {
                return;
            };
        }
        processes = snapshot;

        let sampled = source.devices();
        if sampled != devices {
            devices = sampled.clone();
            let Ok(()) = events.send(ActivityEvent::DevicesChanged(sampled)) else {
                return;
            };
        }

        let Ok(()) = events.send(ActivityEvent::Tick) else {
            return;
        };

        match stop.recv_timeout(interval) {
            Ok(()) => return,
            Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

fn without_pid(processes: Vec<AudioProcess>, pid: i32) -> Vec<AudioProcess> {
    let mut kept = Vec::with_capacity(processes.len());
    for process in processes {
        if process.pid == pid {
            continue;
        }
        kept.push(process);
    }
    kept
}

fn default_device_uid(selector: AudioObjectPropertySelector) -> Option<DeviceUid> {
    let device = default_device(selector)?;
    let uid = macos::read_string(device, kAudioDevicePropertyDeviceUID)?;
    Some(DeviceUid::new(uid))
}

fn default_input_rate() -> Option<f64> {
    let device = default_device(kAudioHardwarePropertyDefaultInputDevice)?;
    macos::read_scalar::<f64>(device, kAudioDevicePropertyNominalSampleRate)
}

fn default_device(selector: AudioObjectPropertySelector) -> Option<AudioObjectID> {
    let device = macos::read_scalar::<AudioObjectID>(macos::SYSTEM_OBJECT, selector)?;
    if device == kAudioObjectUnknown {
        return None;
    }
    Some(device)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;
    use crate::activity::{BundleId, InputState, OutputState};

    struct Fake {
        processes: Vec<Vec<AudioProcess>>,
        devices: Vec<Devices>,
        snapshots: AtomicUsize,
        samples: AtomicUsize,
    }

    impl Fake {
        fn new(processes: Vec<Vec<AudioProcess>>, devices: Vec<Devices>) -> Self {
            Self {
                processes,
                devices,
                snapshots: AtomicUsize::new(0),
                samples: AtomicUsize::new(0),
            }
        }
    }

    impl ActivitySource for Fake {
        fn snapshot(&self) -> Vec<AudioProcess> {
            let index = self.snapshots.fetch_add(1, Ordering::Relaxed);
            let index = index.min(self.processes.len() - 1);
            self.processes[index].clone()
        }
    }

    impl DeviceSource for Fake {
        fn devices(&self) -> Devices {
            let index = self.samples.fetch_add(1, Ordering::Relaxed);
            let index = index.min(self.devices.len() - 1);
            self.devices[index].clone()
        }
    }

    fn process(object: AudioObjectID, pid: i32, input: InputState) -> AudioProcess {
        AudioProcess {
            object,
            bundle_id: Some(BundleId::new("us.zoom.xos")),
            pid,
            input,
            output: OutputState::Idle,
        }
    }

    fn devices(uid: &str, sample_rate: f64) -> Devices {
        Devices {
            input: Some(DeviceUid::new(uid)),
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(sample_rate),
        }
    }

    fn collect(source: &Fake, stop: Receiver<()>) -> Vec<ActivityEvent> {
        let (sender, incoming) = mpsc::channel();
        poll(source, Duration::from_millis(1), stop, sender);
        let mut events = Vec::new();
        for event in incoming {
            events.push(event);
        }
        events
    }

    #[test]
    fn own_pid_is_filtered_out_of_a_snapshot() {
        let mine = std::process::id() as i32;
        let processes = vec![
            process(1, mine, InputState::Running),
            process(2, 4242, InputState::Running),
        ];
        let kept = without_pid(processes, mine);
        assert_eq!(kept, vec![process(2, 4242, InputState::Running)]);
    }

    #[test]
    fn the_live_source_never_reports_the_daemon_itself() {
        let source = CoreAudio::live();
        let mine = std::process::id() as i32;
        for process in source.snapshot() {
            assert_ne!(process.pid, mine, "mimi must not observe its own process");
        }
    }

    #[test]
    fn one_pass_emits_the_synthesized_take_and_a_tick() {
        let (stop, stopped) = mpsc::channel();
        drop(stop);
        let source = Fake::new(
            vec![vec![process(1, 4242, InputState::Running)]],
            vec![devices("BuiltInMicrophoneDevice", 48_000.0)],
        );
        assert_eq!(
            collect(&source, stopped),
            vec![
                ActivityEvent::InputTaken(process(1, 4242, InputState::Running)),
                ActivityEvent::Tick,
            ]
        );
    }

    #[test]
    fn a_changed_rate_on_the_same_devices_emits_a_devices_changed_event() {
        let (stop, stopped) = mpsc::channel();
        drop(stop);
        let source = Fake::new(
            vec![Vec::new()],
            vec![
                devices("20-F4-D4-60-E4-29:input", 48_000.0),
                devices("20-F4-D4-60-E4-29:input", 24_000.0),
            ],
        );
        assert_eq!(
            collect(&source, stopped),
            vec![
                ActivityEvent::DevicesChanged(devices("20-F4-D4-60-E4-29:input", 24_000.0)),
                ActivityEvent::Tick,
            ]
        );
    }

    #[test]
    fn unchanged_devices_emit_only_a_tick() {
        let (stop, stopped) = mpsc::channel();
        drop(stop);
        let source = Fake::new(
            vec![Vec::new()],
            vec![devices("BuiltInMicrophoneDevice", 48_000.0)],
        );
        assert_eq!(collect(&source, stopped), vec![ActivityEvent::Tick]);
    }

    #[test]
    fn a_stop_signal_ends_the_loop() {
        let (stop, stopped) = mpsc::channel();
        stop.send(()).expect("request a stop");
        let source = Fake::new(
            vec![Vec::new()],
            vec![devices("BuiltInMicrophoneDevice", 48_000.0)],
        );
        assert_eq!(collect(&source, stopped), vec![ActivityEvent::Tick]);
        assert_eq!(source.snapshots.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn several_passes_diff_against_the_previous_snapshot() {
        let (stop, stopped) = mpsc::channel();
        let source = Fake::new(
            vec![
                vec![process(1, 4242, InputState::Idle)],
                vec![process(1, 4242, InputState::Running)],
                vec![process(1, 4242, InputState::Idle)],
            ],
            vec![devices("BuiltInMicrophoneDevice", 48_000.0)],
        );
        let (sender, incoming) = mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| poll(&source, Duration::from_millis(1), stopped, sender));
            let mut events = Vec::new();
            for event in &incoming {
                match event {
                    ActivityEvent::Tick => {}
                    ActivityEvent::InputTaken(_)
                    | ActivityEvent::InputReleased(_)
                    | ActivityEvent::DevicesChanged(_) => events.push(event),
                }
                if events.len() == 2 {
                    break;
                }
            }
            stop.send(()).expect("request a stop");
            assert_eq!(
                events,
                vec![
                    ActivityEvent::InputTaken(process(1, 4242, InputState::Running)),
                    ActivityEvent::InputReleased(process(1, 4242, InputState::Idle)),
                ]
            );
        });
    }
}
