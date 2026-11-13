//! Bounded direct framework RPC framing; no command subprocesses or text shell protocol.
use std::io::{self, Read, Write};
/// Paired framework service/native receiver/host client marker.
pub const BUILD_ABI: &str = "DROIDLOOM_FRAMEWORK_CONTROL_ABI=1;";
/// Maximum JSON request, including a batch of launcher identities.
pub const MAX_REQUEST: usize = 16_384;
/// Maximum response, including a bounded icon batch.
pub const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// Launch an Android MAIN/LAUNCHER activity.
pub const LAUNCH: u16 = 1;
/// Persist built-in display density.
pub const DENSITY: u16 = 2;
/// Read authoritative launcher metadata.
pub const CATALOG: u16 = 3;
/// Render one or more explicitly selected launcher icons.
pub const ICONS: u16 = 4;
/// Read one installed package's version and storage.
pub const PACKAGE: u16 = 5;
/// Stop, uninstall, clear or open the Android settings of one package.
pub const PACKAGE_ACTION: u16 = 6;
/// Read the built-in display's density and the one it started with.
pub const DISPLAY: u16 = 7;
/// Ask MediaProvider to rescan shared-storage paths the host changed.
pub const MEDIA_SCAN: u16 = 8;
/// Tell the framework whether the host session is locked.
pub const HOST_LOCK: u16 = 9;
/// Give the framework Android's hidden credential, which only root keeps.
pub const CREDENTIAL: u16 = 10;
/// Answer a credential prompt the framework announced on `credential-events`.
pub const CREDENTIAL_ANSWER: u16 = 11;
/// Error bit in a response opcode.
pub const ERROR: u16 = 0x8000;

/// Write one length-bounded, versioned frame.
pub fn write(writer: &mut impl Write, opcode: u16, bytes: &[u8], limit: usize) -> io::Result<()> {
    if bytes.len() > limit || bytes.len() > u32::MAX as usize {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut header = [0; 12];
    header[..4].copy_from_slice(b"DLFC");
    header[4..6].copy_from_slice(&1_u16.to_be_bytes());
    header[6..8].copy_from_slice(&opcode.to_be_bytes());
    header[8..].copy_from_slice(&(bytes.len() as u32).to_be_bytes());
    writer.write_all(&header)?;
    writer.write_all(bytes)
}
/// Read one frame, rejecting unknown versions and oversized lengths before allocating.
pub fn read(reader: &mut impl Read, limit: usize) -> io::Result<(u16, Vec<u8>)> {
    let mut header = [0; 12];
    reader.read_exact(&mut header)?;
    let length = u32::from_be_bytes(header[8..12].try_into().unwrap()) as usize;
    if &header[..4] != b"DLFC" || header[4..6] != 1_u16.to_be_bytes() || length > limit {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let opcode = u16::from_be_bytes(header[6..8].try_into().unwrap());
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    Ok((opcode, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fragmented_frames_and_invalid_headers_are_bounded() {
        let mut encoded = Vec::new();
        write(&mut encoded, ICONS, b"{}", MAX_REQUEST).unwrap();
        let mut reader = encoded.as_slice();
        assert_eq!(
            read(&mut reader, MAX_REQUEST).unwrap(),
            (ICONS, b"{}".to_vec())
        );
        for length in 0..encoded.len() {
            assert!(read(&mut &encoded[..length], MAX_REQUEST).is_err());
        }
        for index in [0, 4, 5, 8] {
            let mut invalid = encoded.clone();
            invalid[index] = 255;
            assert!(read(&mut invalid.as_slice(), MAX_REQUEST).is_err());
        }
        assert!(write(&mut Vec::new(), LAUNCH, b"123", 2).is_err());
    }
}
