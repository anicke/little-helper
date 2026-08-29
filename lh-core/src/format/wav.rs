use crate::error::{Error, Result};
use crate::model::StreamInfo;
use md5::{Digest, Md5};
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::Path;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

#[derive(Debug, Clone)]
pub struct WavLayout {
    pub stream_info: StreamInfo,
    /// Byte offset of the `data` chunk payload.
    pub data_offset: u64,
    /// Length of the `data` chunk payload, excluding any RIFF pad byte.
    pub data_len: u64,
}

/// Walk the RIFF chunk list looking for `fmt ` and `data`. Chunks we do not recognize
/// (`LIST`, `INFO`, `id3 `, taper metadata) are skipped rather than treated as errors.
pub fn probe(path: &Path) -> Result<WavLayout> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut r = BufReader::new(file);

    let mut riff = [0u8; 12];
    r.read_exact(&mut riff).map_err(|e| Error::io(path, e))?;
    if &riff[0..4] != b"RIFF" || &riff[8..12] != b"WAVE" {
        return Err(Error::malformed(path, "not a RIFF/WAVE file"));
    }

    let mut fmt: Option<(u16, u8, u32, u8)> = None; // (tag, channels, rate, bits)
    let mut data: Option<(u64, u64)> = None;
    let mut pos = 12u64;

    loop {
        let mut hdr = [0u8; 8];
        match r.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(Error::io(path, e)),
        }
        let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        let body = pos + 8;

        match &id {
            b"fmt " => {
                if size < 16 {
                    return Err(Error::malformed(path, "fmt chunk shorter than 16 bytes"));
                }
                let mut f = vec![0u8; size as usize];
                r.read_exact(&mut f).map_err(|e| Error::io(path, e))?;
                let tag = u16::from_le_bytes([f[0], f[1]]);
                let channels = u16::from_le_bytes([f[2], f[3]]);
                let rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
                let bits = u16::from_le_bytes([f[14], f[15]]);
                if tag != WAVE_FORMAT_PCM && tag != WAVE_FORMAT_EXTENSIBLE {
                    return Err(Error::malformed(
                        path,
                        format!("unsupported WAV format tag 0x{tag:04X}; only PCM is supported"),
                    ));
                }
                if channels == 0 || channels > u8::MAX as u16 {
                    return Err(Error::malformed(
                        path,
                        format!("bad channel count {channels}"),
                    ));
                }
                if bits == 0 || bits > u8::MAX as u16 {
                    return Err(Error::malformed(path, format!("bad bit depth {bits}")));
                }
                fmt = Some((tag, channels as u8, rate, bits as u8));
            }
            b"data" => {
                data = Some((body, size));
                r.seek(SeekFrom::Start(body + size))
                    .map_err(|e| Error::io(path, e))?;
            }
            _ => {
                r.seek(SeekFrom::Start(body + size))
                    .map_err(|e| Error::io(path, e))?;
            }
        }

        // RIFF chunks are word-aligned: an odd-sized chunk is followed by a pad byte.
        pos = body + size + (size & 1);
        if data.is_some() && fmt.is_some() {
            // Keep walking only if we still need something; both found, stop early.
            break;
        }
        r.seek(SeekFrom::Start(pos))
            .map_err(|e| Error::io(path, e))?;
    }

    let (_, channels, sample_rate, bits_per_sample) =
        fmt.ok_or_else(|| Error::malformed(path, "no fmt chunk"))?;
    let (data_offset, data_len) = data.ok_or_else(|| Error::malformed(path, "no data chunk"))?;

    let bytes_per_frame = u64::from(channels) * u64::from(bits_per_sample).div_ceil(8);
    if bytes_per_frame == 0 {
        return Err(Error::malformed(path, "degenerate fmt chunk"));
    }

    Ok(WavLayout {
        stream_info: StreamInfo {
            sample_rate,
            channels,
            bits_per_sample,
            total_frames: Some(data_len / bytes_per_frame),
            audio_md5: None,
        },
        data_offset,
        data_len,
    })
}

/// MD5 of the `data` chunk payload only — the WAV header is excluded, which is what
/// makes this value survive a format change.
pub fn audio_md5(path: &Path) -> Result<[u8; 16]> {
    let layout = probe(path)?;
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut r = BufReader::new(file);
    r.seek(SeekFrom::Start(layout.data_offset))
        .map_err(|e| Error::io(path, e))?;

    let mut hasher = Md5::new();
    let mut remaining = layout.data_len;
    let mut buf = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        r.read_exact(&mut buf[..want])
            .map_err(|e| Error::io(path, e))?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    Ok(hasher.finalize().into())
}
