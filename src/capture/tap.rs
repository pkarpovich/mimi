use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::NonNull;
use std::thread;

use block2::RcBlock;
use objc2::AllocAnyThread;
use objc2::rc::Retained;
use objc2_core_audio::{
    AudioDeviceCreateIOProcIDWithBlock, AudioDeviceDestroyIOProcID, AudioDeviceIOProcID,
    AudioDeviceStart, AudioDeviceStop, AudioHardwareCreateAggregateDevice,
    AudioHardwareCreateProcessTap, AudioHardwareDestroyAggregateDevice,
    AudioHardwareDestroyProcessTap, AudioObjectID, CATapDescription,
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceMainSubDeviceKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceSubDeviceListKey,
    kAudioAggregateDeviceTapAutoStartKey, kAudioAggregateDeviceTapListKey,
    kAudioAggregateDeviceUIDKey, kAudioDevicePropertyNominalSampleRate,
    kAudioSubDeviceDriftCompensationKey, kAudioSubDeviceUIDKey, kAudioSubTapUIDKey,
};
use objc2_core_audio_types::{AudioBuffer, AudioBufferList, AudioTimeStamp};
use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_foundation::{NSArray, NSNumber, NSString};

use super::devices::{self, Io, Rebuild};
use super::layout::{self, Buffer, Tracks};
use super::{
    BlockRef, Capture, CaptureConfig, CaptureError, Formats, Producer, Rebuilds, TrackKind,
};
use crate::activity::{DeviceUid, Devices};
use crate::macos;

const AGGREGATE_IS_PRIVATE: i32 = 1;
const TAP_AUTO_START: i32 = 0;
const DRIFT_COMPENSATION_ON: i32 = 1;
const MAX_BUFFERS: usize = 8;
const RATE_READS: u32 = 24;
const RATE_READ_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

/// Tap is the live capture: a global process tap inside a private aggregate device.
pub struct Tap {
    config: CaptureConfig,
    producer: Option<Producer>,
    tap: Option<AudioObjectID>,
    aggregate: Option<AudioObjectID>,
    io_proc: AudioDeviceIOProcID,
    io: Io,
    sample_rate: Option<f64>,
    tracks: Vec<TrackKind>,
    generation: u64,
    formats: Formats,
    built: Option<Devices>,
    rebuilds: Rebuilds,
}

impl Tap {
    pub fn new(config: CaptureConfig) -> Self {
        Self {
            config,
            producer: None,
            tap: None,
            aggregate: None,
            io_proc: None,
            io: Io::Stopped,
            sample_rate: None,
            tracks: Vec::new(),
            generation: 0,
            formats: Formats::new(),
            built: None,
            rebuilds: Rebuilds::default(),
        }
    }

    fn build(&mut self, devices: &Devices) -> Result<(), CaptureError> {
        let Err(error) = self.try_build(devices) else {
            return Ok(());
        };
        self.teardown();
        Err(error)
    }

    fn try_build(&mut self, devices: &Devices) -> Result<(), CaptureError> {
        let Devices {
            input,
            output,
            sample_rate: _,
        } = devices;
        let Some(output) = output else {
            return Err(CaptureError::NoOutputDevice);
        };
        let Some(producer) = self.producer.clone() else {
            return Err(CaptureError::NotStarted);
        };

        let description = tap_description(&self.config.name);
        let tap = create_tap(&description)?;
        self.tap = Some(tap);

        let aggregate = aggregate_description(
            &self.config,
            output,
            input.as_ref(),
            &tap_uuid(&description),
        );
        let aggregate = create_aggregate(&aggregate)?;
        self.aggregate = Some(aggregate);

        self.generation += 1;
        let io_proc = create_io_proc(aggregate, producer, self.generation)?;
        self.io_proc = io_proc;

        let status = unsafe { AudioDeviceStart(aggregate, io_proc) };
        if status != 0 {
            return Err(CaptureError::DeviceStart(status));
        }

        let Some(sample_rate) = settled_rate(aggregate) else {
            return Err(CaptureError::NoSampleRate);
        };
        self.sample_rate = Some(sample_rate);
        self.formats.publish(self.generation, sample_rate);
        self.io = Io::Running;
        self.tracks = tracks(devices);
        Ok(())
    }

    fn teardown(&mut self) {
        let tap = self.tap;
        let aggregate = self.aggregate;
        let io_proc = self.io_proc;
        for step in teardown_steps(self.resources()) {
            match step {
                TeardownStep::StopDevice => stop_device(aggregate, io_proc),
                TeardownStep::DestroyIoProc => destroy_io_proc(aggregate, io_proc),
                TeardownStep::DestroyAggregate => destroy_aggregate(aggregate),
                TeardownStep::DestroyTap => destroy_tap(tap),
            }
        }
        self.tap = None;
        self.aggregate = None;
        self.io_proc = None;
        self.io = Io::Stopped;
        self.sample_rate = None;
        self.tracks = Vec::new();
    }

    fn resources(&self) -> Resources {
        Resources {
            tap: presence(self.tap),
            aggregate: presence(self.aggregate),
            io_proc: presence(self.io_proc),
            io: self.io,
        }
    }
}

impl Capture for Tap {
    fn start(&mut self, devices: &Devices, producer: Producer) -> Result<(), CaptureError> {
        self.teardown();
        self.built = None;
        self.rebuilds = Rebuilds::default();
        self.producer = Some(producer);
        self.build(devices)?;
        self.built = Some(devices.clone());
        Ok(())
    }

    fn stop(&mut self) {
        self.teardown();
        self.producer = None;
        self.built = None;
    }

    fn rebuild(&mut self, devices: &Devices) -> Result<(), CaptureError> {
        let Some(built) = self.built.clone() else {
            return Err(CaptureError::NotStarted);
        };
        let io = self.io;
        match devices::rebuild_needed(io, &built, devices) {
            Rebuild::NotRequired => return Ok(()),
            Rebuild::Required => {}
        }

        self.teardown();
        let rebuilt = devices::with_retries(thread::sleep, || self.build(devices));
        let Err(error) = rebuilt else {
            self.built = Some(devices.clone());
            self.rebuilds.succeeded += 1;
            return Ok(());
        };
        // The run loop retries a rebuild that exhausted its attempts every RECOVERY seconds, and
        // each retry enters with capture already down. The failure it is retrying was counted when
        // the attempts first ran out; counting it again would report one device change as hundreds.
        match io {
            Io::Running => self.rebuilds.failed += 1,
            Io::Stopped => {}
        }
        Err(error)
    }

    fn sample_rate(&self) -> Option<f64> {
        self.sample_rate
    }

    fn tracks(&self) -> &[TrackKind] {
        &self.tracks
    }

    fn formats(&self) -> Formats {
        self.formats.clone()
    }

    fn rebuilds(&self) -> Rebuilds {
        self.rebuilds
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Present,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resources {
    tap: Presence,
    aggregate: Presence,
    io_proc: Presence,
    io: Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TeardownStep {
    StopDevice,
    DestroyIoProc,
    DestroyAggregate,
    DestroyTap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriftCompensation {
    On,
    Off,
}

fn teardown_steps(resources: Resources) -> Vec<TeardownStep> {
    let Resources {
        tap,
        aggregate,
        io_proc,
        io,
    } = resources;
    let mut steps = Vec::new();
    match (io_proc, io) {
        (Presence::Present, Io::Running) => steps.push(TeardownStep::StopDevice),
        (Presence::Present, Io::Stopped) => {}
        (Presence::Absent, Io::Running) => {}
        (Presence::Absent, Io::Stopped) => {}
    }
    match io_proc {
        Presence::Present => steps.push(TeardownStep::DestroyIoProc),
        Presence::Absent => {}
    }
    match aggregate {
        Presence::Present => steps.push(TeardownStep::DestroyAggregate),
        Presence::Absent => {}
    }
    match tap {
        Presence::Present => steps.push(TeardownStep::DestroyTap),
        Presence::Absent => {}
    }
    steps
}

fn presence<T>(value: Option<T>) -> Presence {
    match value {
        Some(_) => Presence::Present,
        None => Presence::Absent,
    }
}

/// Settle answers whether two consecutive readings of the delivered rate agree.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Settle {
    Settled(f64),
    Changing(f64),
}

/// settle compares a reading with the one before it, so a rate still moving is not taken as final.
pub fn settle(previous: Option<f64>, current: f64) -> Settle {
    let Some(previous) = previous else {
        return Settle::Changing(current);
    };
    if previous == current {
        return Settle::Settled(current);
    }
    Settle::Changing(current)
}

fn settled_rate(aggregate: AudioObjectID) -> Option<f64> {
    let mut previous = None;
    for _ in 0..RATE_READS {
        let Some(current) =
            macos::read_scalar::<f64>(aggregate, kAudioDevicePropertyNominalSampleRate)
        else {
            return previous;
        };
        if !current.is_finite() || current <= 0.0 {
            return previous;
        }
        match settle(previous, current) {
            Settle::Settled(rate) => return Some(rate),
            Settle::Changing(rate) => previous = Some(rate),
        }
        std::thread::sleep(RATE_READ_INTERVAL);
    }
    previous
}

fn tracks(devices: &Devices) -> Vec<TrackKind> {
    let Devices {
        input,
        output: _,
        sample_rate: _,
    } = devices;
    let mut tracks = Vec::new();
    if input.is_some() {
        tracks.push(TrackKind::Microphone);
    }
    tracks.push(TrackKind::System);
    tracks
}

fn aggregate_description(
    config: &CaptureConfig,
    output: &DeviceUid,
    input: Option<&DeviceUid>,
    tap_uuid: &str,
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let CaptureConfig {
        name,
        aggregate_uid,
    } = config;

    let mut sub_devices = Vec::new();
    sub_devices.push(sub_device_entry(output, DriftCompensation::Off));
    // A composition that repeats one UID produces no microphone buffer at all, so a duplex device
    // that is both the default input and the default output is listed once, as the main sub-device.
    if let Some(input) = input
        && input != output
    {
        sub_devices.push(sub_device_entry(input, DriftCompensation::On));
    }
    let mut entries = Vec::with_capacity(sub_devices.len());
    for entry in &sub_devices {
        entries.push(&**entry);
    }
    let sub_devices = CFArray::from_objects(&entries);

    let tap_entry = tap_entry(tap_uuid);
    let taps = CFArray::from_objects(&[&*tap_entry]);

    let name = CFString::from_str(name);
    let uid = CFString::from_str(aggregate_uid);
    let main = CFString::from_str(output.as_str());
    let private = CFNumber::new_i32(AGGREGATE_IS_PRIVATE);
    let auto_start = CFNumber::new_i32(TAP_AUTO_START);

    let keys = [
        key(kAudioAggregateDeviceNameKey),
        key(kAudioAggregateDeviceUIDKey),
        key(kAudioAggregateDeviceMainSubDeviceKey),
        key(kAudioAggregateDeviceIsPrivateKey),
        key(kAudioAggregateDeviceTapAutoStartKey),
        key(kAudioAggregateDeviceSubDeviceListKey),
        key(kAudioAggregateDeviceTapListKey),
    ];
    let mut named = Vec::with_capacity(keys.len());
    for key in &keys {
        named.push(&**key);
    }
    let values: [&CFType; 7] = [
        &name,
        &uid,
        &main,
        &private,
        &auto_start,
        &sub_devices,
        &taps,
    ];
    CFDictionary::from_slices(&named, &values)
}

fn sub_device_entry(
    uid: &DeviceUid,
    drift: DriftCompensation,
) -> CFRetained<CFDictionary<CFString, CFType>> {
    let uid_key = key(kAudioSubDeviceUIDKey);
    let uid = CFString::from_str(uid.as_str());
    match drift {
        DriftCompensation::Off => CFDictionary::from_slices(&[&uid_key], &[&*uid]),
        DriftCompensation::On => {
            let drift_key = key(kAudioSubDeviceDriftCompensationKey);
            let drift = CFNumber::new_i32(DRIFT_COMPENSATION_ON);
            CFDictionary::from_slices(&[&uid_key, &drift_key], &[&*uid, &*drift])
        }
    }
}

fn tap_entry(uuid: &str) -> CFRetained<CFDictionary<CFString, CFType>> {
    let uid_key = key(kAudioSubTapUIDKey);
    let uuid = CFString::from_str(uuid);
    CFDictionary::from_slices(&[&uid_key], &[&*uuid])
}

fn key(name: &CStr) -> CFRetained<CFString> {
    CFString::from_str(&String::from_utf8_lossy(name.to_bytes()))
}

fn tap_description(name: &str) -> Retained<CATapDescription> {
    let excluded = NSArray::<NSNumber>::new();
    let description = unsafe {
        CATapDescription::initStereoGlobalTapButExcludeProcesses(
            CATapDescription::alloc(),
            &excluded,
        )
    };
    unsafe {
        description.setName(&NSString::from_str(name));
        description.setPrivate(true);
    }
    description
}

fn tap_uuid(description: &CATapDescription) -> String {
    unsafe { description.UUID() }.UUIDString().to_string()
}

fn create_tap(description: &CATapDescription) -> Result<AudioObjectID, CaptureError> {
    let mut tap: AudioObjectID = 0;
    let status = unsafe { AudioHardwareCreateProcessTap(Some(description), &mut tap) };
    if status != 0 {
        return Err(CaptureError::TapCreate(status));
    }
    Ok(tap)
}

fn create_aggregate(
    description: &CFDictionary<CFString, CFType>,
) -> Result<AudioObjectID, CaptureError> {
    let mut device: AudioObjectID = 0;
    let status = unsafe {
        AudioHardwareCreateAggregateDevice(description.as_opaque(), NonNull::from(&mut device))
    };
    if status != 0 {
        return Err(CaptureError::AggregateCreate(status));
    }
    Ok(device)
}

fn create_io_proc(
    device: AudioObjectID,
    producer: Producer,
    generation: u64,
) -> Result<AudioDeviceIOProcID, CaptureError> {
    let scratch = RefCell::new(Tracks::new(producer.frames_per_block()));
    let block = RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              input_time: NonNull<AudioTimeStamp>,
              _output: NonNull<AudioBufferList>,
              _output_time: NonNull<AudioTimeStamp>| {
            let Ok(mut tracks) = scratch.try_borrow_mut() else {
                return;
            };
            let frames = unsafe { interpret_buffer_list(input, &mut tracks) };
            let host_time = unsafe { input_time.as_ref() }.mHostTime;
            producer.push(BlockRef {
                microphone: tracks.microphone(),
                system: tracks.system(),
                frames,
                host_time,
                generation,
            });
        },
    );
    let mut io_proc: AudioDeviceIOProcID = None;
    let status = unsafe {
        AudioDeviceCreateIOProcIDWithBlock(
            NonNull::from(&mut io_proc),
            device,
            None,
            RcBlock::as_ptr(&block),
        )
    };
    if status != 0 {
        return Err(CaptureError::IoProcCreate(status));
    }
    let Some(_) = io_proc else {
        return Err(CaptureError::IoProcCreate(status));
    };
    Ok(io_proc)
}

unsafe fn interpret_buffer_list(input: NonNull<AudioBufferList>, tracks: &mut Tracks) -> usize {
    let list = unsafe { input.as_ref() };
    let count = (list.mNumberBuffers as usize).min(MAX_BUFFERS);
    if count == 0 {
        return layout::interpret(&[], tracks);
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), count) };
    let mut views = [Buffer::NONE; MAX_BUFFERS];
    for (index, buffer) in buffers.iter().enumerate() {
        let AudioBuffer {
            mNumberChannels: channels,
            mDataByteSize: bytes,
            mData: data,
        } = *buffer;
        let channels = channels as usize;
        if channels == 0 {
            continue;
        }
        let Some(data) = NonNull::new(data.cast::<f32>()) else {
            continue;
        };
        let samples = bytes as usize / size_of::<f32>();
        views[index] = Buffer {
            channels,
            samples: unsafe { std::slice::from_raw_parts(data.as_ptr(), samples) },
        };
    }
    layout::interpret(&views[..count], tracks)
}

fn stop_device(device: Option<AudioObjectID>, io_proc: AudioDeviceIOProcID) {
    let Some(device) = device else {
        return;
    };
    unsafe { AudioDeviceStop(device, io_proc) };
}

fn destroy_io_proc(device: Option<AudioObjectID>, io_proc: AudioDeviceIOProcID) {
    let Some(device) = device else {
        return;
    };
    unsafe { AudioDeviceDestroyIOProcID(device, io_proc) };
}

fn destroy_aggregate(device: Option<AudioObjectID>) {
    let Some(device) = device else {
        return;
    };
    unsafe { AudioHardwareDestroyAggregateDevice(device) };
}

fn destroy_tap(tap: Option<AudioObjectID>) {
    let Some(tap) = tap else {
        return;
    };
    unsafe { AudioHardwareDestroyProcessTap(tap) };
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::Type;

    use super::*;

    #[test]
    fn a_first_reading_is_never_taken_as_settled() {
        assert_eq!(settle(None, 48000.0), Settle::Changing(48000.0));
    }

    #[test]
    fn two_equal_readings_settle_on_that_rate() {
        assert_eq!(settle(Some(24000.0), 24000.0), Settle::Settled(24000.0));
    }

    #[test]
    fn a_rate_still_moving_carries_the_newer_reading_forward() {
        assert_eq!(settle(Some(48000.0), 24000.0), Settle::Changing(24000.0));
    }

    #[test]
    fn a_rate_that_moves_back_is_still_unsettled() {
        assert_eq!(settle(Some(24000.0), 48000.0), Settle::Changing(48000.0));
    }

    fn description() -> CFRetained<CFDictionary<CFString, CFType>> {
        aggregate_description(
            &CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"),
            &DeviceUid::new("BuiltInSpeakerDevice"),
            Some(&DeviceUid::new("BuiltInMicrophoneDevice")),
            "6E1F0A2C-0000-4000-8000-0123456789AB",
        )
    }

    fn value(
        dictionary: &CFDictionary<CFString, CFType>,
        name: &CStr,
    ) -> Option<CFRetained<CFType>> {
        dictionary.get(&key(name))
    }

    fn string(dictionary: &CFDictionary<CFString, CFType>, name: &CStr) -> Option<String> {
        let value = value(dictionary, name)?;
        let Ok(value) = value.downcast::<CFString>() else {
            return None;
        };
        Some(value.to_string())
    }

    fn number(dictionary: &CFDictionary<CFString, CFType>, name: &CStr) -> Option<i32> {
        let value = value(dictionary, name)?;
        let Ok(value) = value.downcast::<CFNumber>() else {
            return None;
        };
        value.as_i32()
    }

    fn array(dictionary: &CFDictionary<CFString, CFType>, name: &CStr) -> Vec<CFRetained<CFType>> {
        let Some(value) = value(dictionary, name) else {
            return Vec::new();
        };
        let Ok(value) = value.downcast::<CFArray>() else {
            return Vec::new();
        };
        unsafe { value.cast_unchecked::<CFType>() }.to_vec()
    }

    fn entry(value: &CFType) -> CFRetained<CFDictionary<CFString, CFType>> {
        let value = value
            .retain()
            .downcast::<CFDictionary>()
            .expect("a sub-device or tap entry is a dictionary");
        unsafe { CFRetained::cast_unchecked::<CFDictionary<CFString, CFType>>(value) }
    }

    #[test]
    fn the_aggregate_description_carries_exactly_the_documented_keys() {
        let description = description();
        let (keys, _) = description.to_vecs();
        let mut names = Vec::new();
        for name in &keys {
            names.push(name.to_string());
        }
        names.sort();
        assert_eq!(
            names,
            vec![
                "master".to_string(),
                "name".to_string(),
                "private".to_string(),
                "subdevices".to_string(),
                "tapautostart".to_string(),
                "taps".to_string(),
                "uid".to_string(),
            ]
        );
    }

    #[test]
    fn the_aggregate_description_names_the_default_output_as_the_main_sub_device() {
        let description = description();
        assert_eq!(
            string(&description, kAudioAggregateDeviceNameKey),
            Some("mimi".to_string())
        );
        assert_eq!(
            string(&description, kAudioAggregateDeviceUIDKey),
            Some("dev.pkarpovich.mimi.aggregate".to_string())
        );
        assert_eq!(
            string(&description, kAudioAggregateDeviceMainSubDeviceKey),
            Some("BuiltInSpeakerDevice".to_string())
        );
    }

    #[test]
    fn the_aggregate_is_private_and_does_not_wait_for_another_client() {
        let description = description();
        assert_eq!(
            number(&description, kAudioAggregateDeviceIsPrivateKey),
            Some(1)
        );
        assert_eq!(
            number(&description, kAudioAggregateDeviceTapAutoStartKey),
            Some(0),
            "tapautostart must be 0, otherwise nothing arrives until somebody else plays audio"
        );
    }

    #[test]
    fn the_sub_device_list_holds_dictionaries_and_drifts_the_microphone() {
        let description = description();
        let entries = array(&description, kAudioAggregateDeviceSubDeviceListKey);
        assert_eq!(entries.len(), 2);

        let output = entry(&entries[0]);
        assert_eq!(
            string(&output, kAudioSubDeviceUIDKey),
            Some("BuiltInSpeakerDevice".to_string())
        );
        assert_eq!(number(&output, kAudioSubDeviceDriftCompensationKey), None);

        let input = entry(&entries[1]);
        assert_eq!(
            string(&input, kAudioSubDeviceUIDKey),
            Some("BuiltInMicrophoneDevice".to_string())
        );
        assert_eq!(number(&input, kAudioSubDeviceDriftCompensationKey), Some(1));
    }

    #[test]
    fn the_tap_list_holds_a_dictionary_rather_than_a_bare_uuid() {
        let description = description();
        let entries = array(&description, kAudioAggregateDeviceTapListKey);
        assert_eq!(entries.len(), 1);
        let tap = entry(&entries[0]);
        assert_eq!(
            string(&tap, kAudioSubTapUIDKey),
            Some("6E1F0A2C-0000-4000-8000-0123456789AB".to_string())
        );
    }

    #[test]
    fn a_missing_input_device_leaves_the_output_alone_in_the_sub_device_list() {
        let description = aggregate_description(
            &CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"),
            &DeviceUid::new("BuiltInSpeakerDevice"),
            None,
            "6E1F0A2C-0000-4000-8000-0123456789AB",
        );
        let entries = array(&description, kAudioAggregateDeviceSubDeviceListKey);
        assert_eq!(entries.len(), 1);
        assert_eq!(
            string(&entry(&entries[0]), kAudioSubDeviceUIDKey),
            Some("BuiltInSpeakerDevice".to_string())
        );
    }

    #[test]
    fn a_device_that_is_both_default_input_and_default_output_is_listed_once() {
        let description = aggregate_description(
            &CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"),
            &DeviceUid::new("RODE-NT-USB-Plus"),
            Some(&DeviceUid::new("RODE-NT-USB-Plus")),
            "6E1F0A2C-0000-4000-8000-0123456789AB",
        );
        let entries = array(&description, kAudioAggregateDeviceSubDeviceListKey);
        assert_eq!(
            entries.len(),
            1,
            "a repeated UID yields a composition with no microphone buffer at all"
        );
        let entry = entry(&entries[0]);
        assert_eq!(
            string(&entry, kAudioSubDeviceUIDKey),
            Some("RODE-NT-USB-Plus".to_string())
        );
        assert_eq!(number(&entry, kAudioSubDeviceDriftCompensationKey), None);
    }

    #[test]
    fn a_running_capture_tears_down_in_four_steps() {
        assert_eq!(
            teardown_steps(Resources {
                tap: Presence::Present,
                aggregate: Presence::Present,
                io_proc: Presence::Present,
                io: Io::Running,
            }),
            vec![
                TeardownStep::StopDevice,
                TeardownStep::DestroyIoProc,
                TeardownStep::DestroyAggregate,
                TeardownStep::DestroyTap,
            ]
        );
    }

    #[test]
    fn a_capture_that_never_started_is_not_stopped() {
        assert_eq!(
            teardown_steps(Resources {
                tap: Presence::Present,
                aggregate: Presence::Present,
                io_proc: Presence::Present,
                io: Io::Stopped,
            }),
            vec![
                TeardownStep::DestroyIoProc,
                TeardownStep::DestroyAggregate,
                TeardownStep::DestroyTap,
            ]
        );
    }

    #[test]
    fn a_partial_build_tears_down_only_what_exists() {
        assert_eq!(
            teardown_steps(Resources {
                tap: Presence::Present,
                aggregate: Presence::Absent,
                io_proc: Presence::Absent,
                io: Io::Stopped,
            }),
            vec![TeardownStep::DestroyTap]
        );
        assert_eq!(
            teardown_steps(Resources {
                tap: Presence::Absent,
                aggregate: Presence::Absent,
                io_proc: Presence::Absent,
                io: Io::Stopped,
            }),
            Vec::new()
        );
    }

    #[test]
    fn the_microphone_track_comes_first_when_an_input_device_exists() {
        let devices = Devices {
            input: Some(DeviceUid::new("BuiltInMicrophoneDevice")),
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(48_000.0),
        };
        assert_eq!(
            tracks(&devices),
            vec![TrackKind::Microphone, TrackKind::System]
        );
    }

    #[test]
    fn without_an_input_device_only_the_system_track_is_delivered() {
        let devices = Devices {
            input: None,
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(48_000.0),
        };
        assert_eq!(tracks(&devices), vec![TrackKind::System]);
    }

    #[test]
    fn building_without_a_default_output_device_fails() {
        let mut capture = Tap::new(CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"));
        let devices = Devices {
            input: None,
            output: None,
            sample_rate: None,
        };
        let (producer, _consumer) = crate::capture::ring(2, 8);
        assert_eq!(
            capture.start(&devices, producer),
            Err(CaptureError::NoOutputDevice)
        );
        assert_eq!(capture.sample_rate(), None);
        assert_eq!(capture.tracks(), &[]);
        assert_eq!(
            capture.formats().rate(1),
            None,
            "a build that never read a rate must not publish one for the writer"
        );
    }

    #[test]
    fn rebuilding_before_a_start_reports_that_capture_is_not_running() {
        let mut capture = Tap::new(CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"));
        let devices = Devices {
            input: None,
            output: Some(DeviceUid::new("BuiltInSpeakerDevice")),
            sample_rate: Some(48_000.0),
        };
        assert_eq!(capture.rebuild(&devices), Err(CaptureError::NotStarted));
        assert_eq!(capture.rebuilds(), Rebuilds::default());
    }

    #[test]
    fn a_build_that_failed_leaves_no_baseline_to_rebuild_against() {
        let mut capture = Tap::new(CaptureConfig::new("mimi", "dev.pkarpovich.mimi.aggregate"));
        let devices = Devices {
            input: None,
            output: None,
            sample_rate: None,
        };
        let (producer, _consumer) = crate::capture::ring(2, 8);
        assert_eq!(
            capture.start(&devices, producer),
            Err(CaptureError::NoOutputDevice)
        );
        assert_eq!(
            capture.rebuild(&devices),
            Err(CaptureError::NotStarted),
            "a baseline is kept only after a start that actually built capture"
        );
        let Rebuilds { succeeded, failed } = capture.rebuilds();
        assert_eq!(succeeded, 0);
        assert_eq!(failed, 0);
    }
}
