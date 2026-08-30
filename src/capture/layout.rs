/// Buffer is one entry of a delivered buffer list: its channel count and its interleaved samples.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Buffer<'a> {
    pub channels: usize,
    pub samples: &'a [f32],
}

impl Buffer<'static> {
    pub const NONE: Self = Self {
        channels: 0,
        samples: &[],
    };
}

/// Tracks is the preallocated destination the IOProc interprets each callback into.
#[derive(Debug)]
pub struct Tracks {
    microphone: Vec<f32>,
    system: Vec<f32>,
    frames_per_block: usize,
}

impl Tracks {
    pub fn new(frames_per_block: usize) -> Self {
        Self {
            microphone: Vec::with_capacity(frames_per_block),
            system: Vec::with_capacity(frames_per_block),
            frames_per_block,
        }
    }

    pub fn microphone(&self) -> &[f32] {
        &self.microphone
    }

    pub fn system(&self) -> &[f32] {
        &self.system
    }
}

/// interpret splits the delivered buffers into the two tracks, one buffer per track.
pub fn interpret(buffers: &[Buffer<'_>], tracks: &mut Tracks) -> usize {
    let Tracks {
        microphone,
        system,
        frames_per_block,
    } = tracks;
    microphone.clear();
    system.clear();

    let (microphone_source, system_source) = match buffers {
        [] => (None, None),
        [system] => (None, carrying(*system)),
        [microphone, system, ..] => (carrying(*microphone), carrying(*system)),
    };

    let frames = match (microphone_source, system_source) {
        (None, None) => return 0,
        (Some(microphone), None) => frames_of(microphone),
        (None, Some(system)) => frames_of(system),
        (Some(microphone), Some(system)) => frames_of(microphone).min(frames_of(system)),
    };
    let frames = frames.min(*frames_per_block);

    if let Some(source) = microphone_source {
        mix_down(source, frames, microphone);
    }
    if let Some(source) = system_source {
        mix_down(source, frames, system);
    }
    frames
}

fn carrying(buffer: Buffer<'_>) -> Option<Buffer<'_>> {
    let Buffer { channels, samples } = buffer;
    if channels == 0 || samples.is_empty() {
        return None;
    }
    Some(buffer)
}

fn frames_of(buffer: Buffer<'_>) -> usize {
    let Buffer { channels, samples } = buffer;
    samples.len() / channels
}

fn mix_down(buffer: Buffer<'_>, frames: usize, track: &mut Vec<f32>) {
    let Buffer { channels, samples } = buffer;
    for frame in 0..frames {
        let mut total = 0.0;
        for channel in 0..channels {
            total += samples[frame * channels + channel];
        }
        track.push(total / channels as f32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_buffers_are_two_tracks_with_the_microphone_first() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[
                Buffer {
                    channels: 1,
                    samples: &[0.1, 0.2, 0.3],
                },
                Buffer {
                    channels: 2,
                    samples: &[1.0, 0.0, 0.5, 0.5, -1.0, 1.0],
                },
            ],
            &mut tracks,
        );

        assert_eq!(frames, 3);
        assert_eq!(tracks.microphone(), &[0.1, 0.2, 0.3]);
        assert_eq!(tracks.system(), &[0.5, 0.5, 0.0]);
    }

    #[test]
    fn the_two_buffer_case_does_not_concatenate_the_sources() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[
                Buffer {
                    channels: 1,
                    samples: &[0.25, 0.25, 0.25],
                },
                Buffer {
                    channels: 1,
                    samples: &[0.75, 0.75, 0.75],
                },
            ],
            &mut tracks,
        );

        assert_eq!(frames, 3);
        assert_eq!(tracks.microphone(), &[0.25, 0.25, 0.25]);
        assert_eq!(tracks.system(), &[0.75, 0.75, 0.75]);
        assert_ne!(
            tracks.system(),
            &[0.25, 0.25, 0.25, 0.75, 0.75, 0.75],
            "reading channels within a buffer instead of one buffer per track concatenates them"
        );
    }

    #[test]
    fn a_single_buffer_is_system_audio_without_a_microphone() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[Buffer {
                channels: 2,
                samples: &[1.0, 0.0, -0.5, 0.5],
            }],
            &mut tracks,
        );

        assert_eq!(frames, 2);
        assert!(tracks.microphone().is_empty());
        assert_eq!(tracks.system(), &[0.5, 0.0]);
    }

    #[test]
    fn disagreeing_frame_counts_truncate_to_the_shorter_buffer() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[
                Buffer {
                    channels: 1,
                    samples: &[0.1, 0.2],
                },
                Buffer {
                    channels: 1,
                    samples: &[0.7, 0.8, 0.9, 1.0],
                },
            ],
            &mut tracks,
        );

        assert_eq!(frames, 2);
        assert_eq!(tracks.microphone(), &[0.1, 0.2]);
        assert_eq!(tracks.system(), &[0.7, 0.8]);
    }

    #[test]
    fn an_empty_buffer_list_yields_nothing() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(&[], &mut tracks);

        assert_eq!(frames, 0);
        assert!(tracks.microphone().is_empty());
        assert!(tracks.system().is_empty());
    }

    #[test]
    fn a_buffer_without_data_is_skipped() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[
                Buffer::NONE,
                Buffer {
                    channels: 2,
                    samples: &[1.0, 0.0, -0.5, 0.5],
                },
            ],
            &mut tracks,
        );

        assert_eq!(frames, 2);
        assert!(
            tracks.microphone().is_empty(),
            "a skipped buffer must not become an empty microphone track paired with the tap"
        );
        assert_eq!(tracks.system(), &[0.5, 0.0]);
    }

    #[test]
    fn an_empty_tap_buffer_leaves_the_microphone_on_its_own_track() {
        let mut tracks = Tracks::new(512);
        let frames = interpret(
            &[
                Buffer {
                    channels: 1,
                    samples: &[0.1, 0.2, 0.3],
                },
                Buffer::NONE,
            ],
            &mut tracks,
        );

        assert_eq!(frames, 3);
        assert_eq!(
            tracks.microphone(),
            &[0.1, 0.2, 0.3],
            "buffer 0 is the microphone whatever the tap buffer carries"
        );
        assert!(tracks.system().is_empty());
    }

    #[test]
    fn a_block_longer_than_the_destination_is_truncated_to_it() {
        let mut tracks = Tracks::new(2);
        let frames = interpret(
            &[Buffer {
                channels: 1,
                samples: &[0.1, 0.2, 0.3, 0.4],
            }],
            &mut tracks,
        );

        assert_eq!(frames, 2);
        assert_eq!(tracks.system(), &[0.1, 0.2]);
    }

    #[test]
    fn a_second_callback_replaces_the_first_rather_than_appending_to_it() {
        let mut tracks = Tracks::new(512);
        interpret(
            &[Buffer {
                channels: 1,
                samples: &[0.1, 0.2, 0.3],
            }],
            &mut tracks,
        );
        let frames = interpret(
            &[Buffer {
                channels: 1,
                samples: &[0.9],
            }],
            &mut tracks,
        );

        assert_eq!(frames, 1);
        assert_eq!(tracks.system(), &[0.9]);
    }
}
