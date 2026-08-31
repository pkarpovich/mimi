use std::ffi::c_void;
use std::path::Path;

use objc2_audio_toolbox::{
    AudioFileClose, AudioFileCreateWithURL, AudioFileGetProperty, AudioFileGetPropertyInfo,
    AudioFileID, AudioFileOpenURL, AudioFilePermissions, AudioFileSetProperty, AudioFileTypeID,
    kAudioFileAAC_ADTSType, kAudioFileM4AType, kAudioFilePropertyAudioDataPacketCount,
    kAudioFilePropertyDataFormat, kAudioFilePropertyMagicCookieData,
    kAudioFilePropertyPacketSizeUpperBound,
};
use objc2_core_audio_types::{AudioStreamBasicDescription, AudioStreamPacketDescription};
use objc2_core_foundation::{CFRetained, CFURL};

// The packet-level calls objc2-audio-toolbox does not generate. Reading and writing whole packets is
// what keeps the remux lossless: ExtAudioFile would decode and re-encode instead.
unsafe extern "C-unwind" {
    fn AudioFileReadPacketData(
        file: AudioFileID,
        use_cache: bool,
        io_num_bytes: *mut u32,
        out_packet_descriptions: *mut AudioStreamPacketDescription,
        starting_packet: i64,
        io_num_packets: *mut u32,
        out_buffer: *mut c_void,
    ) -> i32;

    fn AudioFileWritePackets(
        file: AudioFileID,
        use_cache: bool,
        in_num_bytes: u32,
        in_packet_descriptions: *const AudioStreamPacketDescription,
        starting_packet: i64,
        io_num_packets: *mut u32,
        in_buffer: *const c_void,
    ) -> i32;
}

/// Kind is the container a file is opened or created as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Adts,
    M4a,
}

impl Kind {
    fn type_id(self) -> AudioFileTypeID {
        match self {
            Kind::Adts => kAudioFileAAC_ADTSType,
            Kind::M4a => kAudioFileM4AType,
        }
    }
}

/// Packets is one batch read out of a file: the bytes, and where each packet starts within them.
#[derive(Debug)]
pub struct Packets {
    pub bytes: Vec<u8>,
    pub descriptions: Vec<AudioStreamPacketDescription>,
    pub count: u32,
}

/// File is an open audio file, closed when it is dropped.
#[derive(Debug)]
pub struct File {
    id: AudioFileID,
}

impl File {
    pub fn open(path: &Path, kind: Kind) -> Result<Self, i32> {
        let url = url_for(path);
        let mut id: AudioFileID = std::ptr::null_mut();
        let status = unsafe {
            AudioFileOpenURL(
                &url,
                AudioFilePermissions::ReadPermission,
                kind.type_id(),
                std::ptr::NonNull::from(&mut id),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(Self { id })
    }

    pub fn create(
        path: &Path,
        kind: Kind,
        format: &AudioStreamBasicDescription,
    ) -> Result<Self, i32> {
        let url = url_for(path);
        let mut format = *format;
        let mut id: AudioFileID = std::ptr::null_mut();
        let status = unsafe {
            AudioFileCreateWithURL(
                &url,
                kind.type_id(),
                std::ptr::NonNull::from(&mut format),
                objc2_audio_toolbox::AudioFileFlags::EraseFile,
                std::ptr::NonNull::from(&mut id),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(Self { id })
    }

    pub fn format(&self) -> Result<AudioStreamBasicDescription, i32> {
        let mut value: AudioStreamBasicDescription = unsafe { std::mem::zeroed() };
        let mut size = size_of::<AudioStreamBasicDescription>() as u32;
        let status = unsafe {
            AudioFileGetProperty(
                self.id,
                kAudioFilePropertyDataFormat,
                std::ptr::NonNull::from(&mut size),
                std::ptr::NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(value)
    }

    pub fn packet_count(&self) -> Result<u64, i32> {
        let mut value: u64 = 0;
        let mut size = size_of::<u64>() as u32;
        let status = unsafe {
            AudioFileGetProperty(
                self.id,
                kAudioFilePropertyAudioDataPacketCount,
                std::ptr::NonNull::from(&mut size),
                std::ptr::NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(value)
    }

    pub fn packet_size_upper_bound(&self) -> Result<u32, i32> {
        let mut value: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        let status = unsafe {
            AudioFileGetProperty(
                self.id,
                kAudioFilePropertyPacketSizeUpperBound,
                std::ptr::NonNull::from(&mut size),
                std::ptr::NonNull::from(&mut value).cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(value)
    }

    /// magic_cookie is the decoder configuration the destination file needs to describe its own audio.
    pub fn magic_cookie(&self) -> Option<Vec<u8>> {
        let mut size = 0u32;
        let status = unsafe {
            AudioFileGetPropertyInfo(
                self.id,
                kAudioFilePropertyMagicCookieData,
                &mut size,
                std::ptr::null_mut(),
            )
        };
        if status != 0 || size == 0 {
            return None;
        }
        let mut cookie = vec![0u8; size as usize];
        let status = unsafe {
            AudioFileGetProperty(
                self.id,
                kAudioFilePropertyMagicCookieData,
                std::ptr::NonNull::from(&mut size),
                std::ptr::NonNull::new(cookie.as_mut_ptr().cast::<c_void>())
                    .expect("cookie buffer"),
            )
        };
        if status != 0 {
            return None;
        }
        cookie.truncate(size as usize);
        Some(cookie)
    }

    pub fn set_magic_cookie(&self, cookie: &[u8]) -> Result<(), i32> {
        let status = unsafe {
            AudioFileSetProperty(
                self.id,
                kAudioFilePropertyMagicCookieData,
                cookie.len() as u32,
                std::ptr::NonNull::new(cookie.as_ptr() as *mut c_void)
                    .expect("cookie is not empty"),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(())
    }

    pub fn read_packets(&self, from: i64, want: u32, buffer_size: u32) -> Result<Packets, i32> {
        let mut bytes = vec![0u8; buffer_size as usize];
        let mut descriptions =
            vec![unsafe { std::mem::zeroed::<AudioStreamPacketDescription>() }; want as usize];
        let mut num_bytes = buffer_size;
        let mut num_packets = want;
        let status = unsafe {
            AudioFileReadPacketData(
                self.id,
                false,
                &mut num_bytes,
                descriptions.as_mut_ptr(),
                from,
                &mut num_packets,
                bytes.as_mut_ptr().cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        bytes.truncate(num_bytes as usize);
        descriptions.truncate(num_packets as usize);
        Ok(Packets {
            bytes,
            descriptions,
            count: num_packets,
        })
    }

    pub fn write_packets(&self, at: i64, packets: &Packets) -> Result<u32, i32> {
        let Packets {
            bytes,
            descriptions,
            count,
        } = packets;
        let mut num_packets = *count;
        let status = unsafe {
            AudioFileWritePackets(
                self.id,
                false,
                bytes.len() as u32,
                descriptions.as_ptr(),
                at,
                &mut num_packets,
                bytes.as_ptr().cast::<c_void>(),
            )
        };
        if status != 0 {
            return Err(status);
        }
        Ok(num_packets)
    }
}

impl Drop for File {
    fn drop(&mut self) {
        unsafe { AudioFileClose(self.id) };
    }
}

fn url_for(path: &Path) -> CFRetained<CFURL> {
    let text = path.to_string_lossy();
    let bytes = text.as_bytes();
    let url = unsafe {
        CFURL::from_file_system_representation(None, bytes.as_ptr(), bytes.len() as isize, false)
    };
    url.expect("a file url for the recording path")
}
