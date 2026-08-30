mod layout;
mod ring;
mod silence;
mod tap;

use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::activity::Devices;

const MAX_FORMATS: usize = 8;

pub use ring::{
    Block, BlockRef, Consumer, DEFAULT_FRAMES_PER_BLOCK, DEFAULT_SLOTS, Drained, Producer, ring,
};
pub use silence::{OPENING_WINDOW, Silence, Verdict};
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
    fn formats(&self) -> Formats;
}

/// Formats carries the delivered rate of each capture generation to the writer thread.
#[derive(Debug, Clone, Default)]
pub struct Formats(Arc<Mutex<Vec<Format>>>);

#[derive(Debug, Clone, Copy, PartialEq)]
struct Format {
    generation: u64,
    sample_rate: f64,
}

impl Formats {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Vec::with_capacity(MAX_FORMATS))))
    }

    pub fn publish(&self, generation: u64, sample_rate: f64) {
        let Self(published) = self;
        let Ok(mut published) = published.lock() else {
            return;
        };
        if published.len() == MAX_FORMATS {
            published.remove(0);
        }
        published.push(Format {
            generation,
            sample_rate,
        });
    }

    /// rate answers at which rate the frames of a generation were captured.
    pub fn rate(&self, generation: u64) -> Option<f64> {
        let Self(published) = self;
        let Ok(published) = published.lock() else {
            return None;
        };
        let mut rate = None;
        for format in published.iter() {
            let Format {
                generation: recorded,
                sample_rate,
            } = format;
            if *recorded == generation {
                rate = Some(*sample_rate);
            }
        }
        rate
    }
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
    fn a_published_generation_answers_with_the_rate_it_was_captured_at() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 24_000.0);
        assert_eq!(formats.rate(1), Some(48_000.0));
        assert_eq!(formats.rate(2), Some(24_000.0));
    }

    #[test]
    fn an_unpublished_generation_has_no_rate() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        assert_eq!(formats.rate(0), None);
        assert_eq!(formats.rate(2), None);
    }

    #[test]
    fn a_shared_handle_sees_what_the_other_published() {
        let formats = Formats::new();
        let other = formats.clone();
        other.publish(3, 16_000.0);
        assert_eq!(formats.rate(3), Some(16_000.0));
    }

    #[test]
    fn only_the_most_recent_generations_are_kept() {
        let formats = Formats::new();
        for generation in 1..=(MAX_FORMATS as u64 + 2) {
            formats.publish(generation, 1_000.0 * generation as f64);
        }
        assert_eq!(formats.rate(1), None);
        assert_eq!(formats.rate(2), None);
        assert_eq!(formats.rate(3), Some(3_000.0));
        assert_eq!(
            formats.rate(MAX_FORMATS as u64 + 2),
            Some(1_000.0 * (MAX_FORMATS as f64 + 2.0))
        );
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
