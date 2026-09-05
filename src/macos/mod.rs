pub mod audiofile;

use std::ffi::{CString, c_char, c_int, c_void};
use std::io;
use std::mem::{self, MaybeUninit};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
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

/// kill_process_group ends every process in the group led by `pid`, leader included.
///
/// The result is ignored: a group that is already gone is the expected outcome, not a failure.
pub fn kill_process_group(pid: u32) {
    unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
}

const ACL_TYPE_EXTENDED: u32 = 0x0000_0100;
const ACL_FIRST_ENTRY: c_int = 0;
const ACL_NEXT_ENTRY: c_int = -1;
const ACL_EXTENDED_ALLOW: u32 = 1;
const ACL_WRITE_GRANTS: u64 = (1 << 2) | (1 << 5) | (1 << 6) | (1 << 12) | (1 << 13);

unsafe extern "C" {
    fn acl_get_file(path: *const c_char, acl_type: u32) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
    fn acl_get_tag_type(entry: *mut c_void, tag: *mut u32) -> c_int;
    fn acl_get_permset_mask_np(entry: *mut c_void, mask: *mut u64) -> c_int;
    fn acl_free(obj: *mut c_void) -> c_int;
}

/// Grant is what an extended ACL hands out on top of the mode bits, which do not show it.
#[derive(Debug, PartialEq, Eq)]
pub enum Grant {
    Write,
    Nothing,
}

/// acl_grants_write reports whether the extended ACL on `path` allows anyone to write into it.
///
/// A path with no extended ACL is the common case and reports `Nothing`: `acl_get_file` answers
/// that with a null and `ENOENT` rather than with an empty list.
pub fn acl_grants_write(path: &Path) -> Result<Grant, io::Error> {
    let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    };
    let acl = unsafe { acl_get_file(path.as_ptr(), ACL_TYPE_EXTENDED) };
    if acl.is_null() {
        let failure = io::Error::last_os_error();
        if failure.kind() == io::ErrorKind::NotFound {
            return Ok(Grant::Nothing);
        }
        return Err(failure);
    }
    let granted = granted_by(acl);
    unsafe { acl_free(acl) };
    granted
}

fn granted_by(acl: *mut c_void) -> Result<Grant, io::Error> {
    let mut entry: *mut c_void = ptr::null_mut();
    let mut id = ACL_FIRST_ENTRY;
    while unsafe { acl_get_entry(acl, id, &mut entry) } == 0 {
        id = ACL_NEXT_ENTRY;
        let mut tag: u32 = 0;
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if tag != ACL_EXTENDED_ALLOW {
            continue;
        }
        let mut granted: u64 = 0;
        if unsafe { acl_get_permset_mask_np(entry, &mut granted) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if granted & ACL_WRITE_GRANTS != 0 {
            return Ok(Grant::Write);
        }
    }
    Ok(Grant::Nothing)
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
    use std::process::Command;

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
        let reported = Command::new("id").arg("-u").output().expect("id -u");
        let reported = String::from_utf8(reported.stdout).expect("a numeric uid");
        assert_eq!(
            user_id().to_string(),
            reported.trim(),
            "the launchd domain is built from this uid"
        );
    }

    #[test]
    fn properties_of_an_invalid_object_yield_none() {
        let object: AudioObjectID = 0;
        assert_eq!(read_scalar::<i32>(object, UNKNOWN_SELECTOR), None);
        assert_eq!(read_string(object, UNKNOWN_SELECTOR), None);
        assert_eq!(read_object_ids(object, UNKNOWN_SELECTOR), None);
    }
}

/// Lock answers whether an exclusive advisory lock could be taken without waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    Taken,
    HeldByAnother,
    Failed(i32),
}

/// try_lock takes an exclusive advisory lock on an open file, never waiting for the holder.
///
/// The lock lives on the open file description, so the kernel releases it even on a kill that runs
/// no cleanup.
pub fn try_lock(file: &std::fs::File) -> Lock {
    use std::os::unix::io::AsRawFd;
    let taken = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if taken == 0 {
        return Lock::Taken;
    }
    let error = std::io::Error::last_os_error();
    let Some(code) = error.raw_os_error() else {
        return Lock::Failed(0);
    };
    if code == libc::EWOULDBLOCK {
        return Lock::HeldByAnother;
    }
    Lock::Failed(code)
}
