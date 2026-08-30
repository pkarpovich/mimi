mod layout;
mod ring;
mod tap;

use thiserror::Error;

use crate::activity::Devices;

pub use ring::{
    BlockRef, Consumer, DEFAULT_FRAMES_PER_BLOCK, DEFAULT_SLOTS, Drained, Producer, ring,
};
pub use tap::Tap;

/// TrackKind names the two tracks capture keeps apart until the writer folds them into one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Microphone,
    System,
}

/// CaptureConfig names the tap and the private aggregate device mimi builds for itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureConfig {
    pub name: String,
    pub aggregate_uid: String,
}

impl CaptureConfig {
    pub fn new(name: impl Into<String>, aggregate_uid: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            aggregate_uid: aggregate_uid.into(),
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CaptureError {
    #[error("capture was not started")]
    NotStarted,
    #[error("there is no default output device to build the aggregate on")]
    NoOutputDevice,
    #[error("creating the process tap failed with status {0}")]
    TapCreate(i32),
    #[error("creating the aggregate device failed with status {0}")]
    AggregateCreate(i32),
    #[error("creating the IO proc failed with status {0}")]
    IoProcCreate(i32),
    #[error("starting the aggregate device failed with status {0}")]
    DeviceStart(i32),
    #[error("the aggregate device reports no sample rate")]
    NoSampleRate,
}

/// Capture owns the tap and the aggregate device that deliver the two tracks.
pub trait Capture {
    fn start(&mut self, devices: &Devices, producer: Producer) -> Result<(), CaptureError>;
    fn stop(&mut self);
    fn rebuild(&mut self, devices: &Devices) -> Result<(), CaptureError>;
    fn sample_rate(&self) -> Option<f64>;
    fn tracks(&self) -> &[TrackKind];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cloned_producer_writes_into_the_same_ring() {
        let (producer, mut consumer) = ring(4, 8);
        let other = producer.clone();
        other.push(BlockRef {
            microphone: &[],
            system: &[0.5],
            frames: 1,
            host_time: 3,
            generation: 0,
        });

        let Drained { blocks, dropped } = consumer.drain();
        assert_eq!(dropped, 0);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].host_time, 3);
    }

    #[test]
    fn a_capture_config_keeps_the_names_it_was_built_from() {
        let CaptureConfig {
            name,
            aggregate_uid,
        } = CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate");
        assert_eq!(name, "mimi");
        assert_eq!(aggregate_uid, "dev.pkarpovich.mimi.aggregate");
    }
}
