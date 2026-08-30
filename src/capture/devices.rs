use std::time::Duration;

use crate::activity::Devices;

/// SETTLE is how long the hardware is given to arrive before capture is built on it again.
pub const SETTLE: Duration = Duration::from_millis(250);

/// ATTEMPTS is how many times a rebuild is tried before capture gives up and keeps the file.
pub const ATTEMPTS: u32 = 3;

/// Rebuild answers whether capture has to be built again for the devices in front of it now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rebuild {
    Required,
    NotRequired,
}

/// rebuild_needed compares the devices capture was built on with the ones current now.
pub fn rebuild_needed(built: &Devices, current: &Devices) -> Rebuild {
    let Devices {
        input,
        output,
        sample_rate,
    } = built;
    let Devices {
        input: current_input,
        output: current_output,
        sample_rate: current_rate,
    } = current;
    if input != current_input {
        return Rebuild::Required;
    }
    if output != current_output {
        return Rebuild::Required;
    }
    if sample_rate != current_rate {
        return Rebuild::Required;
    }
    Rebuild::NotRequired
}

/// with_retries settles, then builds, up to `ATTEMPTS` times, reporting the last failure.
pub fn with_retries<E>(
    mut settle: impl FnMut(Duration),
    mut build: impl FnMut() -> Result<(), E>,
) -> Result<(), E> {
    let mut failure = None;
    for _ in 0..ATTEMPTS {
        settle(SETTLE);
        let Err(error) = build() else {
            return Ok(());
        };
        failure = Some(error);
    }
    let Some(error) = failure else {
        return Ok(());
    };
    Err(error)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::activity::DeviceUid;

    fn devices(input: Option<&str>, output: Option<&str>, sample_rate: Option<f64>) -> Devices {
        Devices {
            input: input.map(DeviceUid::new),
            output: output.map(DeviceUid::new),
            sample_rate,
        }
    }

    fn airpods() -> Devices {
        devices(
            Some("20-F4-D4-60-E4-29:input"),
            Some("20-F4-D4-60-E4-29:output"),
            Some(24_000.0),
        )
    }

    #[test]
    fn unchanged_devices_at_an_unchanged_rate_keep_the_capture() {
        assert_eq!(rebuild_needed(&airpods(), &airpods()), Rebuild::NotRequired);
    }

    #[test]
    fn a_changed_output_device_forces_a_rebuild() {
        let current = devices(
            Some("20-F4-D4-60-E4-29:input"),
            Some("BuiltInSpeakerDevice"),
            Some(24_000.0),
        );
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);
    }

    #[test]
    fn a_changed_input_device_forces_a_rebuild() {
        let current = devices(
            Some("BuiltInMicrophoneDevice"),
            Some("20-F4-D4-60-E4-29:output"),
            Some(24_000.0),
        );
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);
    }

    #[test]
    fn both_devices_changing_forces_a_rebuild() {
        let current = devices(
            Some("BuiltInMicrophoneDevice"),
            Some("BuiltInSpeakerDevice"),
            Some(48_000.0),
        );
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);
    }

    #[test]
    fn a_device_disappearing_forces_a_rebuild() {
        let current = devices(None, Some("20-F4-D4-60-E4-29:output"), Some(24_000.0));
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);

        let current = devices(Some("20-F4-D4-60-E4-29:input"), None, Some(24_000.0));
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);
    }

    #[test]
    fn the_same_devices_at_a_changed_rate_force_a_rebuild() {
        let current = devices(
            Some("20-F4-D4-60-E4-29:input"),
            Some("20-F4-D4-60-E4-29:output"),
            Some(48_000.0),
        );
        assert_eq!(
            rebuild_needed(&airpods(), &current),
            Rebuild::Required,
            "AirPods entering headset mode change rate without changing UID"
        );

        let current = devices(
            Some("20-F4-D4-60-E4-29:input"),
            Some("20-F4-D4-60-E4-29:output"),
            None,
        );
        assert_eq!(rebuild_needed(&airpods(), &current), Rebuild::Required);
    }

    fn attempt(outcomes: Vec<Result<(), i32>>) -> (Result<(), i32>, usize, Vec<Duration>) {
        let outcomes = RefCell::new(outcomes.into_iter());
        let calls = RefCell::new(0);
        let mut settled = Vec::new();
        let result = with_retries(
            |delay| settled.push(delay),
            || {
                *calls.borrow_mut() += 1;
                match outcomes.borrow_mut().next() {
                    Some(outcome) => outcome,
                    None => Ok(()),
                }
            },
        );
        let calls = *calls.borrow();
        (result, calls, settled)
    }

    #[test]
    fn a_rebuild_that_succeeds_at_once_is_not_attempted_again() {
        let (result, calls, settled) = attempt(vec![Ok(())]);
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 1);
        assert_eq!(settled, vec![SETTLE]);
    }

    #[test]
    fn a_rebuild_that_succeeds_on_the_third_attempt_is_a_success() {
        let (result, calls, settled) = attempt(vec![Err(-1), Err(-2), Ok(())]);
        assert_eq!(result, Ok(()));
        assert_eq!(calls, 3);
        assert_eq!(settled, vec![SETTLE, SETTLE, SETTLE]);
    }

    #[test]
    fn a_rebuild_that_never_succeeds_reports_the_last_failure() {
        let (result, calls, settled) = attempt(vec![Err(-1), Err(-2), Err(-3)]);
        assert_eq!(result, Err(-3));
        assert_eq!(calls, ATTEMPTS as usize);
        assert_eq!(settled.len(), ATTEMPTS as usize);
    }
}
