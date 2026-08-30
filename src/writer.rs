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

    let mut silence = Silence::new(OPENING_WINDOW);
    let mut encoding = Encoding::new(consumer.frames_per_block());
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
            let written = write_block(&file, block, &formats, &mut encoding, &mut silence);
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

/// Encoding is what the writer thread carries from one block to the next.
struct Encoding {
    client: Option<f64>,
    generation: Option<u64>,
    resampler: Resampler,
    stereo: Vec<f32>,
    resampled: Vec<f32>,
}

impl Encoding {
    fn new(frames_per_block: usize) -> Self {
        Self {
            client: None,
            generation: None,
            resampler: Resampler::new(),
            stereo: Vec::with_capacity(2 * frames_per_block),
            resampled: Vec::with_capacity(2 * frames_per_block),
        }
    }
}

fn write_block(
    file: &AacFile,
    block: &Block,
    formats: &Formats,
    encoding: &mut Encoding,
    silence: &mut Silence,
) -> Result<(), WriterError> {
    let Block {
        microphone,
        system,
        frames,
        host_time: _,
        generation,
    } = block;
    let Encoding {
        client,
        generation: written,
        resampler,
        stereo,
        resampled,
    } = encoding;

    if *written != Some(*generation) {
        silence.reset();
        resampler.reset();
    }
    *written = Some(*generation);

    let conversion = conversion(*client, formats.rate(*generation));
    match conversion {
        Conversion::Unknown => return Ok(()),
        Conversion::Establish(sample_rate) => {
            file.set_client_format(sample_rate)?;
            *client = Some(sample_rate);
        }
        Conversion::Direct => {}
        Conversion::Resample { from: _, to: _ } => {}
    }
    let Some(client) = *client else {
        return Ok(());
    };

    fold(microphone, system, *frames, stereo);
    let (samples, frames) = match conversion {
        Conversion::Unknown => return Ok(()),
        Conversion::Establish(_) | Conversion::Direct => (&*stereo, *frames),
        Conversion::Resample { from, to } => {
            let frames = resampler.resample(stereo, *frames, from, to, resampled);
            (&*resampled, frames)
        }
    };
    silence.feed(samples, block_duration(frames, client));
    file.write(samples, frames)
}

fn block_duration(frames: usize, sample_rate: f64) -> Duration {
    if sample_rate <= 0.0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(frames as f64 / sample_rate)
}

/// Conversion is what a block's captured rate needs before it can reach the open file.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Conversion {
    Establish(f64),
    Direct,
    Resample { from: f64, to: f64 },
    Unknown,
}

/// conversion decides how a block reaches a file whose client rate cannot be changed again.
fn conversion(client: Option<f64>, sample_rate: Option<f64>) -> Conversion {
    let Some(client) = client else {
        let Some(sample_rate) = sample_rate else {
            return Conversion::Unknown;
        };
        return Conversion::Establish(sample_rate);
    };
    let Some(sample_rate) = sample_rate else {
        return Conversion::Direct;
    };
    if sample_rate == client {
        return Conversion::Direct;
    }
    Conversion::Resample {
        from: sample_rate,
        to: client,
    }
}

// A rebuild that changes the delivered rate cannot re-apply the client format: measured on this
// machine, ExtAudioFile answers kExtAudioFileError_InvalidOperationOrder once writing has started,
// and every later write fails too. So the rate the first block established stands for the life of
// the file, and blocks captured at another rate are carried onto it here instead.
struct Resampler {
    previous: [f32; CHANNELS as usize],
    position: f64,
}

impl Resampler {
    fn new() -> Self {
        Self {
            previous: [0.0; CHANNELS as usize],
            position: 0.0,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    fn resample(
        &mut self,
        stereo: &[f32],
        frames: usize,
        from: f64,
        to: f64,
        out: &mut Vec<f32>,
    ) -> usize {
        out.clear();
        if frames == 0 || from <= 0.0 || to <= 0.0 {
            return 0;
        }
        let step = from / to;
        let last = frames as f64 - 1.0;
        let mut written = 0;
        while self.position < last {
            let index = self.position.floor();
            let fraction = (self.position - index) as f32;
            let index = index as isize;
            for channel in 0..CHANNELS as usize {
                let start = self.sample(stereo, index, channel);
                let end = self.sample(stereo, index + 1, channel);
                out.push(start + (end - start) * fraction);
            }
            written += 1;
            self.position += step;
        }
        self.position -= frames as f64;

        let mut previous = [0.0; CHANNELS as usize];
        for (channel, sample) in previous.iter_mut().enumerate() {
            *sample = self.sample(stereo, frames as isize - 1, channel);
        }
        self.previous = previous;
        written
    }

    fn sample(&self, stereo: &[f32], index: isize, channel: usize) -> f32 {
        let Self {
            previous,
            position: _,
        } = self;
        if index < 0 {
            return previous[channel];
        }
        let offset = index as usize * CHANNELS as usize + channel;
        if offset >= stereo.len() {
            return 0.0;
        }
        stereo[offset]
    }
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

    fn conversions(generations: &[u64], formats: &Formats) -> Vec<Conversion> {
        let mut client = None;
        let mut conversions = Vec::new();
        for generation in generations {
            let conversion = conversion(client, formats.rate(*generation));
            match conversion {
                Conversion::Establish(sample_rate) => client = Some(sample_rate),
                Conversion::Direct => {}
                Conversion::Resample { from: _, to: _ } => {}
                Conversion::Unknown => {}
            }
            conversions.push(conversion);
        }
        conversions
    }

    fn resampled(blocks: &[Vec<f32>], from: f64, to: f64) -> Vec<Vec<f32>> {
        let mut resampler = Resampler::new();
        let mut out = Vec::new();
        let mut results = Vec::new();
        for block in blocks {
            let frames = block.len() / 2;
            resampler.resample(block, frames, from, to, &mut out);
            results.push(out.clone());
        }
        results
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
    fn the_client_format_is_set_once_and_never_changed_again() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 48_000.0);
        assert_eq!(
            conversions(&[1, 1, 2, 2], &formats),
            vec![
                Conversion::Establish(48_000.0),
                Conversion::Direct,
                Conversion::Direct,
                Conversion::Direct,
            ]
        );
    }

    #[test]
    fn a_generation_captured_at_another_rate_is_resampled_onto_the_established_one() {
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 24_000.0);
        assert_eq!(
            conversions(&[1, 2, 1], &formats),
            vec![
                Conversion::Establish(48_000.0),
                Conversion::Resample {
                    from: 24_000.0,
                    to: 48_000.0
                },
                Conversion::Direct,
            ],
            "the open file keeps the rate its first block established"
        );
    }

    #[test]
    fn a_generation_without_a_published_rate_is_undecidable_until_one_was_established() {
        let formats = Formats::new();
        assert_eq!(conversion(None, formats.rate(1)), Conversion::Unknown);
        assert_eq!(
            conversion(Some(48_000.0), formats.rate(2)),
            Conversion::Direct
        );
    }

    #[test]
    fn halving_the_rate_halves_the_frames_and_keeps_the_samples_it_lands_on() {
        let block = vec![0.0, 0.0, 0.25, -0.25, 0.5, -0.5, 0.75, -0.75];
        let blocks = resampled(&[block], 48_000.0, 24_000.0);
        assert_eq!(blocks, vec![vec![0.0, 0.0, 0.5, -0.5]]);
    }

    #[test]
    fn doubling_the_rate_interpolates_between_the_frames_it_was_given() {
        let block = vec![0.0, 1.0, 1.0, 0.0];
        let blocks = resampled(&[block], 24_000.0, 48_000.0);
        assert_eq!(blocks, vec![vec![0.0, 1.0, 0.5, 0.5]]);
    }

    #[test]
    fn an_unchanged_rate_carries_the_frames_through_untouched() {
        let block = vec![0.1, -0.1, 0.2, -0.2, 0.3, -0.3, 0.4, -0.4];
        let blocks = resampled(std::slice::from_ref(&block), 24_000.0, 24_000.0);
        assert_eq!(blocks[0].len(), block.len() - 2);
        assert_eq!(blocks[0], block[..block.len() - 2]);
    }

    #[test]
    fn the_next_block_continues_where_the_previous_one_stopped() {
        let first = vec![0.0, 0.0, 1.0, 1.0, 2.0, 2.0];
        let second = vec![3.0, 3.0, 4.0, 4.0, 5.0, 5.0];
        let blocks = resampled(&[first, second], 30_000.0, 24_000.0);
        assert_eq!(blocks[0], vec![0.0, 0.0, 1.25, 1.25]);
        assert_eq!(
            blocks[1],
            vec![2.5, 2.5, 3.75, 3.75],
            "the fraction left over by a block is carried into the next one"
        );
    }

    #[test]
    fn a_block_of_no_frames_resamples_to_nothing() {
        assert_eq!(
            resampled(&[Vec::new()], 24_000.0, 48_000.0),
            vec![Vec::new()]
        );
        assert_eq!(
            resampled(&[vec![0.5, 0.5]], 0.0, 48_000.0),
            vec![Vec::new()]
        );
        assert_eq!(
            resampled(&[vec![0.5, 0.5]], 24_000.0, 0.0),
            vec![Vec::new()]
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
    fn a_rebuild_that_changes_the_delivered_rate_keeps_writing_the_same_file() {
        let file = TempFile::new();
        let formats = Formats::new();
        formats.publish(1, 48_000.0);
        formats.publish(2, 24_000.0);

        let (producer, consumer) = ring(64, 4096);
        let microphone = vec![0.25; 4096];
        let system = vec![-0.25; 4096];
        for round in 0..4 {
            producer.push(BlockRef {
                microphone: &microphone,
                system: &system,
                frames: 4096,
                host_time: round,
                generation: 1,
            });
        }
        for round in 4..8 {
            producer.push(BlockRef {
                microphone: &microphone,
                system: &system,
                frames: 4096,
                host_time: round,
                generation: 2,
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
        assert_eq!(
            error, None,
            "a rate change must be resampled, not re-applied to the open file"
        );
        assert_eq!(verdict, Verdict::AudioPresent);
        let written = fs::metadata(file.path()).expect("the writer created the file");
        assert!(written.len() > 0);
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
