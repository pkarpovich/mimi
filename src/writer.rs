use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use objc2_audio_toolbox::{
    AudioConverterRef, AudioConverterSetProperty, AudioFileFlags, ExtAudioFileCreateWithURL,
    ExtAudioFileDispose, ExtAudioFileGetProperty, ExtAudioFileRef, ExtAudioFileSetProperty,
    ExtAudioFileWrite, kAudioConverterEncodeBitRate, kAudioFileAAC_ADTSType,
    kExtAudioFileProperty_AudioConverter, kExtAudioFileProperty_ClientDataFormat,
    kExtAudioFileProperty_ConverterConfig,
};
use objc2_core_audio_types::{
    AudioBuffer, AudioBufferList, AudioStreamBasicDescription, kAudioFormatFlagsNativeFloatPacked,
    kAudioFormatLinearPCM, kAudioFormatMPEG4AAC,
};
use objc2_core_foundation::CFURL;
use thiserror::Error;

use crate::capture::{Block, Consumer, Drained, Formats, OPENING_WINDOW, Silence, Verdict};

const CHANNELS: u32 = 2;
const AAC_FRAMES_PER_PACKET: u32 = 1024;
const DRAIN_INTERVAL: Duration = Duration::from_millis(20);

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WriterError {
    #[error("{0} is not a usable file url")]
    Path(String),
    #[error("creating the audio file failed with status {0}")]
    Create(i32),
    #[error("setting the client format failed with status {0}")]
    ClientFormat(i32),
    #[error("reading the audio converter failed with status {0}")]
    Converter(i32),
    #[error("setting the encoder bit rate failed with status {0}")]
    BitRate(i32),
    #[error("committing the converter configuration failed with status {0}")]
    ConverterConfig(i32),
    #[error("writing to the audio file failed with status {0}")]
    Write(i32),
}

/// WriterSettings is the file a session writes and the encoder it is written through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterSettings {
    pub path: PathBuf,
    pub sample_rate: u32,
    pub bit_rate: u32,
}

/// Errors is the slot a writer-thread failure lands in, read once the thread has joined.
#[derive(Debug, Clone, Default)]
pub struct Errors(Arc<Mutex<Option<WriterError>>>);

impl Errors {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    pub fn take(&self) -> Option<WriterError> {
        let Self(slot) = self;
        let Ok(mut slot) = slot.lock() else {
            return None;
        };
        slot.take()
    }

    fn set(&self, error: WriterError) {
        let Self(slot) = self;
        let Ok(mut slot) = slot.lock() else {
            return;
        };
        if slot.is_some() {
            return;
        }
        *slot = Some(error);
    }
}

/// Finished is what the writer thread leaves behind: the failure it hit and its silence verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    pub error: Option<WriterError>,
    pub verdict: Verdict,
}

/// Writer is the run loop's handle on the thread that owns the open file.
pub struct Writer {
    stop: Sender<()>,
    thread: JoinHandle<Verdict>,
    errors: Errors,
}

impl Writer {
    /// finish stops the writer thread, waits for it, and reports what it failed on and heard.
    pub fn finish(self) -> Finished {
        let Self {
            stop,
            thread,
            errors,
        } = self;
        drop(stop);
        let verdict = match thread.join() {
            Ok(verdict) => verdict,
            Err(_) => Verdict::Undecided,
        };
        Finished {
            error: errors.take(),
            verdict,
        }
    }
}

/// spawn starts the thread that drains the ring into an ADTS AAC file it alone owns.
pub fn spawn(settings: WriterSettings, consumer: Consumer, formats: Formats) -> Writer {
    let errors = Errors::new();
    let reported = errors.clone();
    let (stop, stopping) = mpsc::channel();
    let thread = thread::spawn(move || run(settings, consumer, formats, reported, stopping));
    Writer {
        stop,
        thread,
        errors,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stopping {
    Yes,
    No,
}

fn run(
    settings: WriterSettings,
    mut consumer: Consumer,
    formats: Formats,
    errors: Errors,
    stopping: Receiver<()>,
) -> Verdict {
    let WriterSettings {
        path,
        sample_rate,
        bit_rate,
    } = settings;
    let file = match AacFile::create(&path, sample_rate, bit_rate) {
        Ok(file) => file,
        Err(error) => {
            errors.set(error);
            return Verdict::Undecided;
        }
    };

    let mut applied = None;
    let mut silence = Silence::new(OPENING_WINDOW);
    let mut stereo = Vec::with_capacity(2 * consumer.frames_per_block());
    loop {
        let stop = match stopping.try_recv() {
            Ok(()) => Stopping::Yes,
            Err(TryRecvError::Disconnected) => Stopping::Yes,
            Err(TryRecvError::Empty) => Stopping::No,
        };

        let Drained { blocks, dropped } = consumer.drain();
        if dropped > 0 {
            eprintln!("mimi: the writer fell behind and lost {dropped} blocks");
        }
        for block in blocks {
            let written = write_block(
                &file,
                block,
                &formats,
                &mut applied,
                &mut stereo,
                &mut silence,
            );
            let Err(error) = written else {
                continue;
            };
            errors.set(error);
            return silence.verdict();
        }

        match stop {
            Stopping::Yes => return silence.verdict(),
            Stopping::No => thread::sleep(DRAIN_INTERVAL),
        }
    }
}

/// Applied is the client format the open file currently carries.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Applied {
    generation: u64,
    sample_rate: f64,
}

fn write_block(
    file: &AacFile,
    block: &Block,
    formats: &Formats,
    applied: &mut Option<Applied>,
    stereo: &mut Vec<f32>,
    silence: &mut Silence,
) -> Result<(), WriterError> {
    let Block {
        microphone,
        system,
        frames,
        host_time: _,
        generation,
    } = block;
    let current = applied.map(
        |Applied {
             generation,
             sample_rate: _,
         }| generation,
    );
    match client_format(current, *generation, formats.rate(*generation)) {
        ClientFormat::Unknown => return Ok(()),
        ClientFormat::Keep => {}
        ClientFormat::Apply(sample_rate) => {
            file.set_client_format(sample_rate)?;
            *applied = Some(Applied {
                generation: *generation,
                sample_rate,
            });
            silence.reset();
        }
    }
    let Some(Applied {
        generation: _,
        sample_rate,
    }) = *applied
    else {
        return Ok(());
    };
    fold(microphone, system, *frames, stereo);
    silence.feed(stereo, block_duration(*frames, sample_rate));
    file.write(stereo, *frames)
}

fn block_duration(frames: usize, sample_rate: f64) -> Duration {
    if sample_rate <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(frames as f64 / sample_rate)
}

/// ClientFormat is what a block needs done to the open file before it can be written.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ClientFormat {
    Apply(f64),
    Keep,
    Unknown,
}

fn client_format(applied: Option<u64>, generation: u64, sample_rate: Option<f64>) -> ClientFormat {
    let unchanged = match applied {
        Some(applied) => applied == generation,
        None => false,
    };
    if unchanged {
        return ClientFormat::Keep;
    }
    let Some(sample_rate) = sample_rate else {
        return match applied {
            Some(_) => ClientFormat::Keep,
            None => ClientFormat::Unknown,
        };
    };
    ClientFormat::Apply(sample_rate)
}

/// fold interleaves the two tracks into stereo frames, the microphone left and the system right.
fn fold(microphone: &[f32], system: &[f32], frames: usize, stereo: &mut Vec<f32>) {
    stereo.clear();
    let microphone = Track::new(microphone, frames);
    let system = Track::new(system, frames);
    for frame in 0..frames {
        stereo.push(microphone.sample(frame));
        stereo.push(system.sample(frame));
    }
}

struct Track<'a> {
    samples: &'a [f32],
    channels: usize,
}

impl<'a> Track<'a> {
    fn new(samples: &'a [f32], frames: usize) -> Self {
        if frames == 0 || samples.is_empty() {
            return Self {
                samples: &[],
                channels: 0,
            };
        }
        Self {
            samples,
            channels: (samples.len() / frames).max(1),
        }
    }

    fn sample(&self, frame: usize) -> f32 {
        let Self { samples, channels } = self;
        let channels = *channels;
        if channels == 0 {
            return 0.0;
        }
        let start = frame * channels;
        if start + channels > samples.len() {
            return 0.0;
        }
        let mut total = 0.0;
        for channel in 0..channels {
            total += samples[start + channel];
        }
        total / channels as f32
    }
}

struct AacFile {
    file: ExtAudioFileRef,
    bit_rate: u32,
}

impl AacFile {
    fn create(path: &Path, sample_rate: u32, bit_rate: u32) -> Result<Self, WriterError> {
        let Some(url) = CFURL::from_file_path(path) else {
            return Err(WriterError::Path(path.display().to_string()));
        };
        let mut format = file_format(sample_rate);
        let mut file: ExtAudioFileRef = ptr::null_mut();
        let status = unsafe {
            ExtAudioFileCreateWithURL(
                &url,
                kAudioFileAAC_ADTSType,
                NonNull::from(&mut format),
                ptr::null(),
                AudioFileFlags::EraseFile.0,
                NonNull::from(&mut file),
            )
        };
        if status != 0 {
            return Err(WriterError::Create(status));
        }
        Ok(Self { file, bit_rate })
    }

    fn set_client_format(&self, sample_rate: f64) -> Result<(), WriterError> {
        let mut format = client_stream_format(sample_rate);
        let status = unsafe {
            ExtAudioFileSetProperty(
                self.file,
                kExtAudioFileProperty_ClientDataFormat,
                size_of::<AudioStreamBasicDescription>() as u32,
                NonNull::from(&mut format).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(WriterError::ClientFormat(status));
        }
        self.set_bit_rate()
    }

    fn set_bit_rate(&self) -> Result<(), WriterError> {
        let mut converter: AudioConverterRef = ptr::null_mut();
        let mut size = size_of::<AudioConverterRef>() as u32;
        let status = unsafe {
            ExtAudioFileGetProperty(
                self.file,
                kExtAudioFileProperty_AudioConverter,
                NonNull::from(&mut size),
                NonNull::from(&mut converter).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(WriterError::Converter(status));
        }
        let Some(converter) = NonNull::new(converter) else {
            return Err(WriterError::Converter(0));
        };

        let mut bit_rate = self.bit_rate;
        let status = unsafe {
            AudioConverterSetProperty(
                converter.as_ptr(),
                kAudioConverterEncodeBitRate,
                size_of::<u32>() as u32,
                NonNull::from(&mut bit_rate).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(WriterError::BitRate(status));
        }

        let mut commit: *const c_void = ptr::null();
        let status = unsafe {
            ExtAudioFileSetProperty(
                self.file,
                kExtAudioFileProperty_ConverterConfig,
                size_of::<*const c_void>() as u32,
                NonNull::from(&mut commit).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(WriterError::ConverterConfig(status));
        }
        Ok(())
    }

    fn write(&self, stereo: &[f32], frames: usize) -> Result<(), WriterError> {
        if frames == 0 {
            return Ok(());
        }
        let mut list = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: CHANNELS,
                mDataByteSize: size_of_val(stereo) as u32,
                mData: stereo.as_ptr().cast::<c_void>().cast_mut(),
            }],
        };
        let status =
            unsafe { ExtAudioFileWrite(self.file, frames as u32, NonNull::from(&mut list)) };
        if status != 0 {
            return Err(WriterError::Write(status));
        }
        Ok(())
    }
}

impl Drop for AacFile {
    fn drop(&mut self) {
        unsafe { ExtAudioFileDispose(self.file) };
    }
}

fn file_format(sample_rate: u32) -> AudioStreamBasicDescription {
    AudioStreamBasicDescription {
        mSampleRate: sample_rate.into(),
        mFormatID: kAudioFormatMPEG4AAC,
        mFormatFlags: 0,
        mBytesPerPacket: 0,
        mFramesPerPacket: AAC_FRAMES_PER_PACKET,
        mBytesPerFrame: 0,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: 0,
        mReserved: 0,
    }
}

fn client_stream_format(sample_rate: f64) -> AudioStreamBasicDescription {
    let bytes_per_frame = CHANNELS * size_of::<f32>() as u32;
    AudioStreamBasicDescription {
        mSampleRate: sample_rate,
        mFormatID: kAudioFormatLinearPCM,
        mFormatFlags: kAudioFormatFlagsNativeFloatPacked,
        mBytesPerPacket: bytes_per_frame,
        mFramesPerPacket: 1,
        mBytesPerFrame: bytes_per_frame,
        mChannelsPerFrame: CHANNELS,
        mBitsPerChannel: 8 * size_of::<f32>() as u32,
        mReserved: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::capture::{BlockRef, ring};

    static NEXT_FILE: AtomicU32 = AtomicU32::new(0);

    struct TempFile(PathBuf);

    impl TempFile {
        fn new() -> Self {
            let id = NEXT_FILE.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("mimi-writer-test-{}-{id}.aac", std::process::id()));
            Self(path)
        }

        fn path(&self) -> &Path {
            let Self(path) = self;
            path
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let Self(path) = self;
            let _ = fs::remove_file(path);
        }
    }

    fn folded(microphone: &[f32], system: &[f32], frames: usize) -> Vec<f32> {
        let mut stereo = Vec::new();
        fold(microphone, system, frames, &mut stereo);
        stereo
    }

    fn actions(generations: &[u64], formats: &Formats) -> Vec<ClientFormat> {
        let mut applied = None;
        let mut actions = Vec::new();
        for generation in generations {
            let action = client_format(applied, *generation, formats.rate(*generation));
            match action {
                ClientFormat::Apply(_) => applied = Some(*generation),
                ClientFormat::Keep => {}
                ClientFormat::Unknown => {}
            }
            actions.push(action);
        }
        actions
    }

    #[test]
    fn both_tracks_interleave_with_the_microphone_on_the_left() {
        assert_eq!(
            folded(&[0.1, 0.2, 0.3], &[-0.1, -0.2, -0.3], 3),
            vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3]
        );
    }

    #[test]
    fn an_absent_microphone_leaves_silence_rather_than_a_copy_of_the_system_track() {
        let stereo = folded(&[], &[0.5, 0.6], 2);
        assert_eq!(stereo, vec![0.0, 0.5, 0.0, 0.6]);
        assert_ne!(
            stereo,
            vec![0.5, 0.5, 0.6, 0.6],
            "an absent microphone must not be filled with the system track"
        );
    }

    #[test]
    fn a_short_track_is_padded_with_silence_rather_than_truncating_the_other() {
        assert_eq!(
            folded(&[0.1, 0.2], &[0.5, 0.6, 0.7], 3),
            vec![0.1, 0.5, 0.2, 0.6, 0.0, 0.7]
        );
        assert_eq!(
            folded(&[0.1, 0.2, 0.3], &[0.5], 3),
            vec![0.1, 0.5, 0.2, 0.0, 0.3, 0.0]
        );
    }

    #[test]
    fn a_multi_channel_system_track_is_averaged_into_the_right_channel() {
        assert_eq!(
            folded(&[0.1, 0.2], &[1.0, 0.0, 0.5, 0.5], 2),
            vec![0.1, 0.5, 0.2, 0.5]
        );
    }

    #[test]
    fn a_multi_channel_microphone_track_is_averaged_into_the_left_channel() {
        assert_eq!(
            folded(&[1.0, 0.0, -1.0, 1.0], &[0.5, 0.5], 2),
            vec![0.5, 0.5, 0.0, 0.5]
        );
    }

    #[test]
    fn a_block_of_no_frames_folds_to_nothing() {
        assert!(folded(&[], &[], 0).is_empty());
        assert!(folded(&[0.1], &[0.2], 0).is_empty());
    }

    #[test]
    fn an_unchanged_generation_keeps_the_client_format() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        assert_eq!(
            client_format(Some(1), 1, formats.rate(1)),
            ClientFormat::Keep
        );
    }

    #[test]
    fn a_changed_generation_applies_the_client_format_exactly_once() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 24_000.0);
        assert_eq!(
            actions(&[1, 1, 1, 2, 2, 2], &formats),
            vec![
                ClientFormat::Apply(48_000.0),
                ClientFormat::Keep,
                ClientFormat::Keep,
                ClientFormat::Apply(24_000.0),
                ClientFormat::Keep,
                ClientFormat::Keep,
            ]
        );
    }

    #[test]
    fn frames_captured_at_the_old_rate_are_written_at_the_old_rate() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 24_000.0);
        assert_eq!(
            actions(&[1, 2, 1], &formats),
            vec![
                ClientFormat::Apply(48_000.0),
                ClientFormat::Apply(24_000.0),
                ClientFormat::Apply(48_000.0),
            ],
            "a block queued at the old rate re-applies that rate rather than the current one"
        );
    }

    #[test]
    fn a_generation_without_a_published_rate_is_undecidable_until_one_was_applied() {
        let formats = Formats::new();
        assert_eq!(
            client_format(None, 1, formats.rate(1)),
            ClientFormat::Unknown
        );
        assert_eq!(
            client_format(Some(1), 2, formats.rate(2)),
            ClientFormat::Keep
        );
    }

    #[test]
    fn a_clean_run_leaves_the_error_slot_empty_and_the_file_written() {
        let file = TempFile::new();
        let formats = Formats::new();
        formats.publish(1, 48_000.0);

        let (producer, consumer) = ring(16, 2048);
        let microphone = vec![0.25; 2048];
        let system = vec![-0.25; 2048];
        for round in 0..8 {
            producer.push(BlockRef {
                microphone: &microphone,
                system: &system,
                frames: 2048,
                host_time: round,
                generation: 1,
            });
        }

        let writer = spawn(
            WriterSettings {
                path: file.path().to_path_buf(),
                sample_rate: 24_000,
                bit_rate: 96_000,
            },
            consumer,
            formats,
        );
        let Finished { error, verdict } = writer.finish();
        assert_eq!(error, None);
        assert_eq!(
            verdict,
            Verdict::AudioPresent,
            "blocks carrying non-zero samples must not be reported as silence"
        );

        let written = fs::metadata(file.path()).expect("the writer created the file");
        assert!(
            written.len() > 0,
            "a clean run must leave encoded audio behind"
        );
    }

    #[test]
    fn a_run_of_all_zero_blocks_reports_silence() {
        let file = TempFile::new();
        let formats = Formats::new();
        formats.publish(1, 24_000.0);

        let (producer, consumer) = ring(64, 4096);
        let zeros = vec![0.0; 4096];
        for round in 0..18 {
            producer.push(BlockRef {
                microphone: &zeros,
                system: &zeros,
                frames: 4096,
                host_time: round,
                generation: 1,
            });
        }

        let writer = spawn(
            WriterSettings {
                path: file.path().to_path_buf(),
                sample_rate: 24_000,
                bit_rate: 96_000,
            },
            consumer,
            formats,
        );
        let Finished { error, verdict } = writer.finish();
        assert_eq!(error, None);
        assert_eq!(verdict, Verdict::Silent);
    }

    #[test]
    fn a_rebuild_restarts_the_silence_window_on_the_open_file() {
        let file = TempFile::new();
        let formats = Formats::new();
        formats.publish(1, 24_000.0);
        formats.publish(2, 24_000.0);

        let (producer, consumer) = ring(64, 4096);
        let zeros = vec![0.0; 4096];
        for round in 0..18 {
            producer.push(BlockRef {
                microphone: &zeros,
                system: &zeros,
                frames: 4096,
                host_time: round,
                generation: 1,
            });
        }
        let audible = vec![0.25; 4096];
        producer.push(BlockRef {
            microphone: &audible,
            system: &zeros,
            frames: 4096,
            host_time: 18,
            generation: 2,
        });

        let writer = spawn(
            WriterSettings {
                path: file.path().to_path_buf(),
                sample_rate: 24_000,
                bit_rate: 96_000,
            },
            consumer,
            formats,
        );
        let Finished { error, verdict } = writer.finish();
        assert_eq!(error, None);
        assert_eq!(
            verdict,
            Verdict::AudioPresent,
            "the window a rebuild opened must judge the new capture, not the silent one before it"
        );
    }

    #[test]
    fn a_block_covers_its_frames_at_the_rate_it_was_captured() {
        assert_eq!(block_duration(24_000, 24_000.0), Duration::from_secs(1));
        assert_eq!(
            block_duration(512, 48_000.0),
            Duration::from_secs_f64(512.0 / 48_000.0)
        );
        assert_eq!(block_duration(512, 0.0), Duration::ZERO);
        assert_eq!(block_duration(0, 24_000.0), Duration::ZERO);
    }

    #[test]
    fn a_file_that_cannot_be_created_populates_the_error_slot() {
        let (_producer, consumer) = ring(4, 8);
        let writer = spawn(
            WriterSettings {
                path: PathBuf::from("/mimi-writer-test-missing-directory/out.aac"),
                sample_rate: 24_000,
                bit_rate: 96_000,
            },
            consumer,
            Formats::new(),
        );
        let Finished { error, verdict } = writer.finish();
        assert_eq!(verdict, Verdict::Undecided);
        let error = error.expect("an unwritable path is reported");
        match error {
            WriterError::Create(status) => assert_ne!(status, 0),
            WriterError::Path(_)
            | WriterError::ClientFormat(_)
            | WriterError::Converter(_)
            | WriterError::BitRate(_)
            | WriterError::ConverterConfig(_)
            | WriterError::Write(_) => panic!("expected a create error, got {error}"),
        }
    }

    #[test]
    fn the_error_slot_keeps_the_first_failure_it_was_given() {
        let errors = Errors::new();
        assert_eq!(errors.take(), None);
        errors.set(WriterError::Write(-1));
        errors.set(WriterError::Create(-2));
        assert_eq!(errors.take(), Some(WriterError::Write(-1)));
        assert_eq!(errors.take(), None);
    }
}
