use crate::error::Result;
use crate::format;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verification {
    /// Decoded cleanly and the audio matches the MD5 the file carries.
    Ok,
    /// Decoded cleanly, but the audio is not what the file claims it is.
    Md5Mismatch {
        stored: [u8; 16],
        computed: [u8; 16],
    },
    /// Decoded cleanly; the file carries no MD5 to check against.
    NoStoredMd5 { computed: [u8; 16] },
}

impl Verification {
    pub fn is_ok(&self) -> bool {
        matches!(self, Verification::Ok)
    }
}

/// Decode the whole file and compare against the MD5 stored in its header.
///
/// A decode error surfaces as `Err` — that is a broken file, not a failed comparison,
/// and the two deserve different words in the report.
pub fn verify(path: &Path) -> Result<Verification> {
    let probed = format::probe(path)?;
    let computed = format::audio_md5(path)?;
    Ok(match probed.stream_info.audio_md5 {
        Some(stored) if stored == computed => Verification::Ok,
        Some(stored) => Verification::Md5Mismatch { stored, computed },
        None => Verification::NoStoredMd5 { computed },
    })
}
