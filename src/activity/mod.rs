use objc2_core_audio::{
    AudioObjectID, AudioObjectPropertySelector, kAudioHardwarePropertyProcessObjectList,
    kAudioProcessPropertyBundleID, kAudioProcessPropertyIsRunningInput,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};

use crate::macos;

/// BundleId is the bundle identifier a Core Audio process object reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleId(String);

impl BundleId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        let Self(value) = self;
        value
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputState {
    Running,
    Idle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    Running,
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioProcess {
    pub object: AudioObjectID,
    pub bundle_id: Option<BundleId>,
    pub pid: i32,
    pub input: InputState,
    pub output: OutputState,
}

/// audio_processes lists the processes Core Audio currently knows about.
pub fn audio_processes() -> Vec<AudioProcess> {
    let Some(objects) = macos::read_object_ids(
        macos::SYSTEM_OBJECT,
        kAudioHardwarePropertyProcessObjectList,
    ) else {
        return Vec::new();
    };
    let mut processes = Vec::with_capacity(objects.len());
    for object in objects {
        let Some(pid) = macos::read_scalar::<i32>(object, kAudioProcessPropertyPID) else {
            continue;
        };
        let bundle_id = bundle_id(macos::read_string(object, kAudioProcessPropertyBundleID));
        processes.push(AudioProcess {
            object,
            bundle_id,
            pid,
            input: input_state(object),
            output: output_state(object),
        });
    }
    processes
}

fn bundle_id(value: Option<String>) -> Option<BundleId> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    Some(BundleId::new(value))
}

fn input_state(object: AudioObjectID) -> InputState {
    if running_flag(object, kAudioProcessPropertyIsRunningInput) == 0 {
        return InputState::Idle;
    }
    InputState::Running
}

fn output_state(object: AudioObjectID) -> OutputState {
    if running_flag(object, kAudioProcessPropertyIsRunningOutput) == 0 {
        return OutputState::Idle;
    }
    OutputState::Running
}

fn running_flag(object: AudioObjectID, selector: AudioObjectPropertySelector) -> u32 {
    let Some(flag) = macos::read_scalar::<u32>(object, selector) else {
        return 0;
    };
    flag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(object: AudioObjectID, bundle: Option<&str>, input: InputState) -> AudioProcess {
        AudioProcess {
            object,
            bundle_id: bundle.map(BundleId::new),
            pid: 4242,
            input,
            output: OutputState::Idle,
        }
    }

    #[test]
    fn bundle_id_keeps_its_value() {
        let bundle = BundleId::new("company.thebrowser.browser.helper");
        assert_eq!(bundle.as_str(), "company.thebrowser.browser.helper");
    }

    #[test]
    fn a_process_keeps_the_fields_it_was_built_from() {
        let AudioProcess {
            object,
            bundle_id,
            pid,
            input,
            output,
        } = process(17, Some("us.zoom.xos"), InputState::Running);
        assert_eq!(object, 17);
        assert_eq!(bundle_id, Some(BundleId::new("us.zoom.xos")));
        assert_eq!(pid, 4242);
        assert_eq!(input, InputState::Running);
        assert_eq!(output, OutputState::Idle);
    }

    #[test]
    fn a_process_without_a_bundle_id_keeps_none() {
        let AudioProcess {
            object: _,
            bundle_id,
            pid: _,
            input: _,
            output: _,
        } = process(17, None, InputState::Idle);
        assert_eq!(bundle_id, None);
    }

    #[test]
    fn an_unreadable_or_empty_bundle_id_becomes_none() {
        assert_eq!(bundle_id(None), None);
        assert_eq!(bundle_id(Some(String::new())), None);
        assert_eq!(
            bundle_id(Some(String::from("us.zoom.xos"))),
            Some(BundleId::new("us.zoom.xos"))
        );
    }

    #[test]
    fn identical_processes_are_equal() {
        assert_eq!(
            process(17, Some("us.zoom.xos"), InputState::Running),
            process(17, Some("us.zoom.xos"), InputState::Running)
        );
    }

    #[test]
    fn a_changed_input_state_breaks_equality() {
        assert_ne!(
            process(17, Some("us.zoom.xos"), InputState::Running),
            process(17, Some("us.zoom.xos"), InputState::Idle)
        );
    }

    #[test]
    fn a_changed_object_or_bundle_id_breaks_equality() {
        let running = InputState::Running;
        assert_ne!(
            process(17, Some("us.zoom.xos"), running),
            process(18, Some("us.zoom.xos"), running)
        );
        assert_ne!(
            process(17, Some("us.zoom.xos"), running),
            process(17, Some("com.google.Chrome"), running)
        );
        assert_ne!(
            process(17, Some("us.zoom.xos"), running),
            process(17, None, running)
        );
    }

    #[test]
    fn a_changed_pid_breaks_equality() {
        let mut other = process(17, Some("us.zoom.xos"), InputState::Running);
        other.pid = 99;
        assert_ne!(process(17, Some("us.zoom.xos"), InputState::Running), other);
    }

    #[test]
    fn a_changed_output_state_breaks_equality() {
        let mut other = process(17, Some("us.zoom.xos"), InputState::Running);
        other.output = OutputState::Running;
        assert_ne!(process(17, Some("us.zoom.xos"), InputState::Running), other);
    }
}
