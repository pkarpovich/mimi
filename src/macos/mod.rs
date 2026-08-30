use std::ffi::c_void;
use std::mem::{self, MaybeUninit};
use std::ptr::{self, NonNull};

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};
use objc2_core_foundation::{CFRetained, CFString};

pub const SYSTEM_OBJECT: AudioObjectID = kAudioObjectSystemObject as AudioObjectID;

/// user_id is the uid launchd addresses this user's agent domain by.
pub fn user_id() -> u32 {
    unsafe { libc::getuid() }
}

/// Scalar marks the types a Core Audio property may be read into byte-for-byte; implementing it for
/// a type that is not a plain fixed-size value would let `read_scalar` build an invalid one.
pub trait Scalar: Copy {}

impl Scalar for u32 {}
impl Scalar for i32 {}
impl Scalar for f64 {}

/// read_scalar reads a fixed-size property from an audio object in the global scope.
pub fn read_scalar<T: Scalar>(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Option<T> {
    let address = global_address(selector);
    let mut value = MaybeUninit::<T>::uninit();
    let mut size = mem::size_of::<T>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return None;
    }
    if size as usize != mem::size_of::<T>() {
        return None;
    }
    Some(unsafe { value.assume_init() })
}

/// read_string reads a `CFString` property from an audio object in the global scope.
pub fn read_string(object: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<String> {
    let address = global_address(selector);
    let mut value: *const CFString = ptr::null();
    let mut size = mem::size_of::<*const CFString>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast::<c_void>(),
        )
    };
    if status != 0 {
        return None;
    }
    let value = NonNull::new(value.cast_mut())?;
    let value = unsafe { CFRetained::from_raw(value) };
    Some(value.to_string())
}

/// read_object_ids reads a property holding an array of audio object ids in the global scope.
pub fn read_object_ids(
    object: AudioObjectID,
    selector: AudioObjectPropertySelector,
) -> Option<Vec<AudioObjectID>> {
    let address = global_address(selector);
    let mut size = 0u32;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != 0 {
        return None;
    }
    let stride = mem::size_of::<AudioObjectID>();
    let mut ids = vec![0 as AudioObjectID; size as usize / stride];
    if ids.is_empty() {
        return Some(ids);
    }
    let mut size = (ids.len() * stride) as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            NonNull::from(&address),
            0,
            ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(ids.as_mut_slice()).cast::<c_void>(),
        )
    };
    if status != 0 {
        return None;
    }
    ids.truncate(size as usize / stride);
    Some(ids)
}

fn global_address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNKNOWN_SELECTOR: AudioObjectPropertySelector = 0x7a7a7a7a;

    #[test]
    fn unknown_scalar_property_yields_none() {
        assert_eq!(
            read_scalar::<u32>(SYSTEM_OBJECT, UNKNOWN_SELECTOR),
            None,
            "an unknown selector must not produce a value"
        );
    }

    #[test]
    fn unknown_string_property_yields_none() {
        assert_eq!(read_string(SYSTEM_OBJECT, UNKNOWN_SELECTOR), None);
    }

    #[test]
    fn unknown_object_list_property_yields_none() {
        assert_eq!(read_object_ids(SYSTEM_OBJECT, UNKNOWN_SELECTOR), None);
    }

    #[test]
    fn the_user_id_is_the_one_the_process_runs_as() {
        assert_eq!(user_id(), user_id(), "the uid does not change under us");
    }

    #[test]
    fn properties_of_an_invalid_object_yield_none() {
        let object: AudioObjectID = 0;
        assert_eq!(read_scalar::<i32>(object, UNKNOWN_SELECTOR), None);
        assert_eq!(read_string(object, UNKNOWN_SELECTOR), None);
        assert_eq!(read_object_ids(object, UNKNOWN_SELECTOR), None);
    }
}
