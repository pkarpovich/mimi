use std::path::{Path, PathBuf};

use crate::macos::audiofile::{File, Kind};

const PACKETS_PER_BATCH: u32 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum RemuxError {
    #[error("{} could not be opened: OS status {status}", path.display())]
    Open { path: PathBuf, status: i32 },
    #[error("{} could not be created: OS status {status}", path.display())]
    Create { path: PathBuf, status: i32 },
    #[error("the source format could not be read: OS status {0}")]
    Format(i32),
    #[error("the decoder configuration could not be carried over: OS status {0}")]
    Cookie(i32),
    #[error("reading packets failed at {packet}: OS status {status}")]
    Read { packet: i64, status: i32 },
    #[error("writing packets failed at {packet}: OS status {status}")]
    Write { packet: i64, status: i32 },
    #[error("the source carries no audio")]
    Empty,
}

/// Remuxed reports what a finished remux produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remuxed {
    pub path: PathBuf,
    pub packets: u64,
}

/// to_m4a rewrites an ADTS recording into an m4a beside it, carrying the packets over untouched.
///
/// ADTS is what the recording is written as, because its frames are self-synchronising and a file
/// left by a crash still plays. It carries no duration, so every reader estimates one - and on an
/// hour-long file macOS estimates 30 hours. m4a states the duration instead.
pub fn to_m4a(source: &Path, destination: &Path) -> Result<Remuxed, RemuxError> {
    let input = File::open(source, Kind::Adts).map_err(|status| RemuxError::Open {
        path: source.to_path_buf(),
        status,
    })?;
    let format = input.format().map_err(RemuxError::Format)?;
    let packet_count = input.packet_count().map_err(RemuxError::Format)?;
    if packet_count == 0 {
        return Err(RemuxError::Empty);
    }
    let packet_size = input
        .packet_size_upper_bound()
        .map_err(RemuxError::Format)?;

    let output =
        File::create(destination, Kind::M4a, &format).map_err(|status| RemuxError::Create {
            path: destination.to_path_buf(),
            status,
        })?;
    if let Some(cookie) = input.magic_cookie() {
        output
            .set_magic_cookie(&cookie)
            .map_err(RemuxError::Cookie)?;
    }

    let buffer_size = packet_size.max(1) * PACKETS_PER_BATCH;
    let mut written = 0u64;
    let mut at = 0i64;
    loop {
        let packets = input
            .read_packets(at, PACKETS_PER_BATCH, buffer_size)
            .map_err(|status| RemuxError::Read { packet: at, status })?;
        if packets.count == 0 {
            break;
        }
        let count = output
            .write_packets(at, &packets)
            .map_err(|status| RemuxError::Write { packet: at, status })?;
        written += u64::from(count);
        at += i64::from(count);
        if count < packets.count {
            break;
        }
    }

    Ok(Remuxed {
        path: destination.to_path_buf(),
        packets: written,
    })
}

/// destination_for names the m4a that replaces a recording, keeping everything but the extension.
pub fn destination_for(source: &Path) -> PathBuf {
    source.with_extension("m4a")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_destination_keeps_the_name_and_changes_the_extension() {
        assert_eq!(
            destination_for(Path::new("/r/2026-08-31T10-01-23-thebrowser.aac")),
            Path::new("/r/2026-08-31T10-01-23-thebrowser.m4a")
        );
    }

    #[test]
    fn a_partial_file_becomes_an_m4a_too() {
        assert_eq!(
            destination_for(Path::new("/r/session.aac.partial")),
            Path::new("/r/session.aac.m4a")
        );
    }

    #[test]
    fn a_missing_source_is_an_open_error_rather_than_a_panic() {
        let missing = std::env::temp_dir().join("mimi-remux-missing.aac");
        let _ = std::fs::remove_file(&missing);
        let destination = destination_for(&missing);
        let error = to_m4a(&missing, &destination).expect_err("a missing source cannot be remuxed");
        match error {
            RemuxError::Open { .. } => {}
            other => panic!("expected an open error, got {other:?}"),
        }
    }
}
