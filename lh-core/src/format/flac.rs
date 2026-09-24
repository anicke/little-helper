use crate::error::{Error, Result};
use crate::format::wav::WavWriter;
use crate::model::StreamInfo;
use md5::{Digest, Md5};
use std::io::{Seek, Write};
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

/// Decode a FLAC fully into memory: every sample, interleaved, alongside the
/// [`StreamInfo`] read straight from the stream.
///
/// Repair (docs/sbe-repair.md §4 step 5a) needs a whole file as one buffer so it can move
/// frames across a boundary; that is the one case in this codebase where holding an
/// entire show's track in memory is the point, rather than something [`decode_to_wav`]'s
/// block-by-block streaming is specifically written to avoid.
pub fn decode_to_samples(path: &Path) -> Result<(StreamInfo, Vec<i32>)> {
    let mut reader = claxon::FlacReader::open(path).map_err(|source| Error::Flac {
        path: path.to_path_buf(),
        source,
    })?;
    let si = reader.streaminfo();
    let info = StreamInfo {
        sample_rate: si.sample_rate,
        channels: si.channels as u8,
        bits_per_sample: si.bits_per_sample as u8,
        total_frames: si.samples,
        audio_md5: None,
    };

    let mut samples = Vec::new();
    for sample in reader.samples() {
        samples.push(sample.map_err(|source| Error::Flac {
            path: path.to_path_buf(),
            source,
        })?);
    }
    Ok((info, samples))
}

/// MD5 of several buffers of interleaved samples, concatenated as if they were one
/// continuous PCM stream — FLAC's own convention: signed, little-endian, `bits_per_sample`
/// rounded up to whole bytes.
///
/// This is the whole-set invariant repair checks itself against (docs/sbe-repair.md §1):
/// decode the set before a fix and after, concatenated, and the two digests must match —
/// moving frames across a boundary must never add or drop a sample from the set as a
/// whole, only reassign which file it belongs to.
pub fn concatenated_pcm_md5(buffers: &[&[i32]], bits_per_sample: u8) -> [u8; 16] {
    let bytes_per_sample = (bits_per_sample as usize).div_ceil(8);
    let mut hasher = Md5::new();
    // Packed a chunk at a time: one `update` per 2-byte sample costs more than the hashing.
    let mut bytes = Vec::with_capacity(64 * 1024 * bytes_per_sample);
    for buf in buffers {
        for chunk in buf.chunks(64 * 1024) {
            bytes.clear();
            for &sample in chunk {
                bytes.extend_from_slice(&sample.to_le_bytes()[..bytes_per_sample]);
            }
            hasher.update(&bytes);
        }
    }
    hasher.finalize().into()
}

/// Decode a FLAC into `out`, returning the MD5 of the audio we actually produced.
///
/// Lossless decoding is deterministic and bit-identical, so this is the in-process path
/// (Principle 3): the reference decoder and ours cannot disagree about the samples, only
/// about the container, and the corpus tests pin the container too.
///
/// The MD5 follows FLAC's own convention — signed, little-endian, `bits_per_sample`
/// rounded up to whole bytes — so the caller can compare it against STREAMINFO without
/// a second pass over the file.
/// `dst` is used only to attribute write errors to the file they happened on.
///
/// `progress` is called once per decoded block with (frames written so far, total frames
/// — `0` if the source's STREAMINFO does not declare one) and returns whether to keep
/// going, the same `FnMut(u32, u32) -> bool` shape `torrent::hash_pieces` uses for pieces
/// (docs/job-queue.md §8). Returning `false` stops the decode and yields
/// [`Error::Cancelled`]; nothing has been renamed into place yet at that point, so the
/// caller's `TempOutput` cleans up the partial file as it always does.
pub fn decode_to_wav<W: Write + Seek>(
    path: &Path,
    dst: &Path,
    out: &mut WavWriter<W>,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<[u8; 16]> {
    let mut reader = claxon::FlacReader::open(path).map_err(|source| Error::Flac {
        path: path.to_path_buf(),
        source,
    })?;
    let bytes_per_sample = (reader.streaminfo().bits_per_sample as usize).div_ceil(8);
    let total_frames = reader
        .streaminfo()
        .samples
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);

    let mut hasher = Md5::new();
    let mut frames = reader.blocks();
    let mut block_buf = Vec::new();
    let mut interleaved: Vec<i32> = Vec::new();
    let mut digest_buf: Vec<u8> = Vec::new();
    let mut done_frames: u32 = 0;

    loop {
        let block = match frames.read_next_or_eof(block_buf) {
            Ok(Some(block)) => block,
            Ok(None) => break,
            Err(source) => {
                return Err(Error::Flac {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };

        interleaved.clear();
        digest_buf.clear();
        for i in 0..block.duration() {
            for ch in 0..block.channels() {
                let sample = block.sample(ch, i);
                interleaved.push(sample);
                digest_buf.extend_from_slice(&sample.to_le_bytes()[..bytes_per_sample]);
            }
        }
        hasher.update(&digest_buf);
        out.write_samples(&interleaved)
            .map_err(|e| Error::io(dst, e))?;

        done_frames = done_frames.saturating_add(block.duration());
        if !progress(done_frames, total_frames) {
            return Err(Error::Cancelled);
        }

        block_buf = block.into_buffer();
    }

    Ok(hasher.finalize().into())
}
