mod decide;

use tracing::error;

use crate::activity::Devices;
use crate::capture::Capture;

pub use decide::{Decider, SessionCommand, SessionStart};

/// devices_changed forwards a device change to capture, which rebuilds only if it has to.
pub fn devices_changed(capture: &mut impl Capture, devices: &Devices) {
    let Err(failure) = capture.rebuild(devices) else {
        return;
    };
    error!("capture did not rebuild on the current devices: {failure}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::DeviceUid;
    use crate::capture::{CaptureError, Formats, Producer, Rebuilds, TrackKind};

    struct FakeCapture {
        rebuilt: Vec<Devices>,
        stops: usize,
        failure: Option<CaptureError>,
    }

    impl FakeCapture {
        fn new(failure: Option<CaptureError>) -> Self {
            Self {
                rebuilt: Vec::new(),
                stops: 0,
                failure,
            }
        }
    }

    impl Capture for FakeCapture {
        fn start(&mut self, _devices: &Devices, _producer: Producer) -> Result<(), CaptureError> {
            Ok(())
        }

        fn stop(&mut self) {
            self.stops += 1;
        }

        fn rebuild(&mut self, devices: &Devices) -> Result<(), CaptureError> {
            self.rebuilt.push(devices.clone());
            let Some(failure) = self.failure else {
                return Ok(());
            };
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
            Rebuilds {
                succeeded: self.rebuilt.len() as u32,
                failed: 0,
            }
        }
    }

    fn devices(input: &str) -> Devices {
        Devices {
            input: Some(DeviceUid::new(input)),
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(48_000.0),
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
}
