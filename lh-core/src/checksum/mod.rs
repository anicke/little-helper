//! The three checksum formats traders exchange, and why all three exist:
//!
//! | Kind | Hashes                                    | Survives            | Breaks on              |
//! |------|-------------------------------------------|---------------------|------------------------|
//! | FFP  | unencoded audio, read from FLAC STREAMINFO | re-encode, retag    | audio change           |
//! | MD5  | file bytes                                | copy, move          | any re-encode or retag |
//! | ST5  | audio data only, WAV header excluded       | format change       | audio change           |

pub mod file;

use crate::error::{Error, Result};
use crate::format;
use md5::{Digest, Md5};
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

pub use file::{ChecksumFile, Entry};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumKind {
    Ffp,
    Md5,
    St5,
}

impl ChecksumKind {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Ffp => "ffp",
            Self::Md5 => "md5",
            Self::St5 => "st5",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ffp => "FFP",
            Self::Md5 => "MD5",
            Self::St5 => "ST5",
        }
    }
}

/// FFP: the MD5 already stored in FLAC's STREAMINFO. A header read — no decode.
pub fn ffp(path: &Path) -> Result<[u8; 16]> {
    let probed = format::probe(path)?;
    probed
        .stream_info
        .audio_md5
        .ok_or_else(|| Error::malformed(path, "no audio MD5 in STREAMINFO; cannot produce an FFP"))
}

/// MD5 of the file's bytes.
pub fn md5(path: &Path) -> Result<[u8; 16]> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut r = BufReader::new(file);
    let mut hasher = Md5::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

/// ST5: MD5 of the audio data alone. Computed by decoding, so it is a genuine
/// independent check of the FFP rather than a restatement of it.
pub fn st5(path: &Path) -> Result<[u8; 16]> {
    format::audio_md5(path)
}

pub fn compute(kind: ChecksumKind, path: &Path) -> Result<[u8; 16]> {
    match kind {
        ChecksumKind::Ffp => ffp(path),
        ChecksumKind::Md5 => md5(path),
        ChecksumKind::St5 => st5(path),
    }
}
