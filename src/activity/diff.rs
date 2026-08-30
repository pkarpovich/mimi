use objc2_core_audio::AudioObjectID;

use super::{ActivityEvent, AudioProcess, InputState};

/// diff turns two consecutive snapshots into the input takes and releases between them.
pub fn diff(previous: &[AudioProcess], current: &[AudioProcess]) -> Vec<ActivityEvent> {
    let mut events = Vec::new();
    for process in current {
        let before = find(previous, process.object);
        let Some(before) = before else {
            match process.input {
                InputState::Running => events.push(ActivityEvent::InputTaken(process.clone())),
                InputState::Idle => {}
            }
            continue;
        };
        match (before.input, process.input) {
            (InputState::Idle, InputState::Running) => {
                events.push(ActivityEvent::InputTaken(process.clone()));
            }
            (InputState::Running, InputState::Idle) => {
                events.push(ActivityEvent::InputReleased(process.clone()));
            }
            (InputState::Idle, InputState::Idle) | (InputState::Running, InputState::Running) => {}
        }
    }
    for process in previous {
        if find(current, process.object).is_some() {
            continue;
        }
        match process.input {
            InputState::Running => events.push(ActivityEvent::InputReleased(process.clone())),
            InputState::Idle => {}
        }
    }
    events
}

fn find(processes: &[AudioProcess], object: AudioObjectID) -> Option<&AudioProcess> {
    let mut found = None;
    for process in processes {
        if process.object == object {
            found = Some(process);
            break;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{BundleId, OutputState};

    fn process(object: AudioObjectID, input: InputState) -> AudioProcess {
        AudioProcess {
            object,
            bundle_id: Some(BundleId::new("company.thebrowser.browser.helper")),
            pid: 900 + object as i32,
            input,
            output: OutputState::Idle,
        }
    }

    #[test]
    fn an_unchanged_snapshot_produces_no_events() {
        let snapshot = vec![
            process(1, InputState::Running),
            process(2, InputState::Idle),
        ];
        assert_eq!(diff(&snapshot, &snapshot), Vec::new());
    }

    #[test]
    fn taking_the_input_yields_a_take() {
        let previous = vec![process(1, InputState::Idle)];
        let current = vec![process(1, InputState::Running)];
        assert_eq!(
            diff(&previous, &current),
            vec![ActivityEvent::InputTaken(process(1, InputState::Running))]
        );
    }

    #[test]
    fn releasing_the_input_yields_a_release() {
        let previous = vec![process(1, InputState::Running)];
        let current = vec![process(1, InputState::Idle)];
        assert_eq!(
            diff(&previous, &current),
            vec![ActivityEvent::InputReleased(process(1, InputState::Idle))]
        );
    }

    #[test]
    fn a_process_appearing_while_holding_the_input_yields_a_take() {
        let current = vec![process(7, InputState::Running)];
        assert_eq!(
            diff(&[], &current),
            vec![ActivityEvent::InputTaken(process(7, InputState::Running))]
        );
    }

    #[test]
    fn a_process_appearing_without_the_input_yields_nothing() {
        let current = vec![process(7, InputState::Idle)];
        assert_eq!(diff(&[], &current), Vec::new());
    }

    #[test]
    fn a_process_disappearing_while_holding_the_input_yields_a_release() {
        let previous = vec![process(7, InputState::Running)];
        assert_eq!(
            diff(&previous, &[]),
            vec![ActivityEvent::InputReleased(process(
                7,
                InputState::Running
            ))]
        );
    }

    #[test]
    fn a_process_disappearing_without_the_input_yields_nothing() {
        let previous = vec![process(7, InputState::Idle)];
        assert_eq!(diff(&previous, &[]), Vec::new());
    }

    #[test]
    fn processes_are_matched_by_audio_object_id() {
        let previous = vec![process(1, InputState::Running)];
        let current = vec![process(2, InputState::Running)];
        assert_eq!(
            diff(&previous, &current),
            vec![
                ActivityEvent::InputTaken(process(2, InputState::Running)),
                ActivityEvent::InputReleased(process(1, InputState::Running)),
            ]
        );
    }

    #[test]
    fn two_processes_taking_the_input_yield_two_takes() {
        let previous = vec![process(1, InputState::Idle), process(2, InputState::Idle)];
        let current = vec![
            process(1, InputState::Running),
            process(2, InputState::Running),
        ];
        assert_eq!(
            diff(&previous, &current),
            vec![
                ActivityEvent::InputTaken(process(1, InputState::Running)),
                ActivityEvent::InputTaken(process(2, InputState::Running)),
            ]
        );
    }
}
