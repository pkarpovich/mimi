use std::cell::RefCell;
use std::ffi::CStr;
use std::ptr::NonNull;

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

use super::{BlockRef, Capture, CaptureConfig, CaptureError, Producer, TrackKind};
use crate::activity::{DeviceUid, Devices};
use crate::macos;

const AGGREGATE_IS_PRIVATE: i32 = 1;
const TAP_AUTO_START: i32 = 0;
const DRIFT_COMPENSATION_ON: i32 = 1;

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

        let Some(sample_rate) =
            macos::read_scalar::<f64>(aggregate, kAudioDevicePropertyNominalSampleRate)
        else {
            return Err(CaptureError::NoSampleRate);
        };
        self.sample_rate = Some(sample_rate);

        self.generation += 1;
        let io_proc = create_io_proc(aggregate, producer, self.generation)?;
        self.io_proc = io_proc;

        let status = unsafe { AudioDeviceStart(aggregate, io_proc) };
        if status != 0 {
            return Err(CaptureError::DeviceStart(status));
        }
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
        self.producer = Some(producer);
        self.build(devices)
    }

    fn stop(&mut self) {
        self.teardown();
        self.producer = None;
    }

    fn rebuild(&mut self, devices: &Devices) -> Result<(), CaptureError> {
        self.teardown();
        self.build(devices)
    }

    fn sample_rate(&self) -> Option<f64> {
        self.sample_rate
    }

    fn tracks(&self) -> &[TrackKind] {
        &self.tracks
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.teardown();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Io {
    Running,
    Stopped,
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
    if let Some(input) = input {
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
    let scratch = RefCell::new(Vec::<f32>::with_capacity(producer.frames_per_block()));
    let block = RcBlock::new(
        move |_now: NonNull<AudioTimeStamp>,
              input: NonNull<AudioBufferList>,
              input_time: NonNull<AudioTimeStamp>,
              _output: NonNull<AudioBufferList>,
              _output_time: NonNull<AudioTimeStamp>| {
            let Ok(mut system) = scratch.try_borrow_mut() else {
                return;
            };
            let frames = unsafe { copy_system_track(input, &mut system) };
            let host_time = unsafe { input_time.as_ref() }.mHostTime;
            producer.push(BlockRef {
                microphone: &[],
                system: system.as_slice(),
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

unsafe fn copy_system_track(input: NonNull<AudioBufferList>, samples: &mut Vec<f32>) -> usize {
    samples.clear();
    let list = unsafe { input.as_ref() };
    let count = list.mNumberBuffers as usize;
    if count == 0 {
        return 0;
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.mBuffers.as_ptr(), count) };
    let AudioBuffer {
        mNumberChannels: channels,
        mDataByteSize: bytes,
        mData: data,
    } = buffers[count - 1];
    let channels = channels as usize;
    if channels == 0 {
        return 0;
    }
    let Some(data) = NonNull::new(data.cast::<f32>()) else {
        return 0;
    };
    let frames = bytes as usize / size_of::<f32>() / channels;
    let frames = frames.min(samples.capacity());
    let data = unsafe { std::slice::from_raw_parts(data.as_ptr(), frames * channels) };
    for frame in 0..frames {
        samples.push(data[frame * channels]);
    }
    frames
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
    }
}
