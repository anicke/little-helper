use crate::error::{Error, Result};
use crate::model::StreamInfo;
use md5::{Digest, Md5};
use std::path::Path;

/// Read STREAMINFO and the vendor string. Header reads only — no decode, so this is
/// fast enough to run across a whole working set on load.
pub fn probe(path: &Path) -> Result<(StreamInfo, Option<String>)> {
    let tag = metaflac::Tag::read_from_path(path).map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })?;

    let si = tag
        .get_streaminfo()
        .ok_or_else(|| Error::malformed(path, "FLAC file has no STREAMINFO block"))?;

    // An all-zero MD5 is the spec's "not computed", not a real digest.
    let audio_md5 = if si.md5.len() == 16 && si.md5.iter().any(|b| *b != 0) {
        let mut out = [0u8; 16];
        out.copy_from_slice(&si.md5);
        Some(out)
    } else {
        None
    };

    let info = StreamInfo {
        sample_rate: si.sample_rate,
        channels: si.num_channels,
        bits_per_sample: si.bits_per_sample,
        total_frames: (si.total_samples > 0).then_some(si.total_samples),
        audio_md5,
    };

    let encoder = tag
        .vorbis_comments()
        .map(|vc| vc.vendor_string.clone())
        .filter(|v| !v.is_empty());

    Ok((info, encoder))
}

/// Decode the file and hash the raw interleaved samples, exactly as FLAC defines its
/// STREAMINFO MD5: signed, little-endian, `bits_per_sample` rounded up to whole bytes.
pub fn audio_md5(path: &Path) -> Result<[u8; 16]> {
    let mut reader = claxon::FlacReader::open(path).map_err(|source| Error::Flac {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes_per_sample = (reader.streaminfo().bits_per_sample as usize).div_ceil(8);

    let mut hasher = Md5::new();
    let mut buf = [0u8; 4];
    for sample in reader.samples() {
        let sample = sample.map_err(|source| Error::Flac {
            path: path.to_path_buf(),
            source,
        })?;
        buf.copy_from_slice(&sample.to_le_bytes());
        hasher.update(&buf[..bytes_per_sample]);
    }

    Ok(hasher.finalize().into())
}
