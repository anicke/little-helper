use crate::error::{Error, Result};
use crate::model::StreamInfo;
use md5::{Digest, Md5};
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom, Write};
use std::path::Path;

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// `KSDATAFORMAT_SUBTYPE_PCM`, the only subtype we write.
const SUBTYPE_PCM: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Channel masks for `WAVE_FORMAT_EXTENSIBLE`, indexed by channel count - 1. These are
/// the values `flac -d` actually writes, read out of its output rather than inferred from
/// a specification — matching the reference decoder is the requirement.
const CHANNEL_MASKS: [u32; 8] = [0x4, 0x3, 0x7, 0x33, 0x37, 0x3F, 0x70F, 0x63F];

/// WAV is a 32-bit format: every length field is a `u32`.
const MAX_WAV_DATA: u64 = u32::MAX as u64;

#[derive(Debug, Clone)]
pub(crate) struct WavLayout {
    pub stream_info: StreamInfo,
    /// Byte offset of the `data` chunk payload.
    pub data_offset: u64,
    /// Length of the `data` chunk payload, excluding any RIFF pad byte.
    pub data_len: u64,
}

/// Walk the RIFF chunk list looking for `fmt ` and `data`. Chunks we do not recognize
/// (`LIST`, `INFO`, `id3 `, taper metadata) are skipped rather than treated as errors.
pub(crate) fn probe(path: &Path) -> Result<WavLayout> {
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

/// Streams PCM into a canonical RIFF/WAVE file.
///
/// The target is byte-for-byte what `flac -d` writes, because that is the file traders
/// already have: legacy 16-byte `fmt ` below 17 bits and 3 channels, and
/// `WAVE_FORMAT_EXTENSIBLE` above. The corpus tests pin that equality.
pub struct WavWriter<W: Write + Seek> {
    inner: W,
    bits_per_sample: u8,
    header_len: u64,
    data_len: u64,
    scratch: Vec<u8>,
}

impl<W: Write + Seek> WavWriter<W> {
    /// Writes the header straight away, with placeholder lengths that [`finish`] patches.
    /// We cannot know the length up front: a FLAC that does not declare its own total
    /// sample count is a real thing, and buffering a whole show in memory is not an option.
    ///
    /// [`finish`]: WavWriter::finish
    pub fn new(mut inner: W, si: &StreamInfo) -> io::Result<Self> {
        if si.channels == 0 || si.sample_rate == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot write a WAV with no channels or no sample rate",
            ));
        }
        if !matches!(si.bits_per_sample, 8 | 16 | 24) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "cannot write a {}-bit WAV; only 8, 16 and 24 bits are supported",
                    si.bits_per_sample
                ),
            ));
        }

        let extensible = si.bits_per_sample > 16 || si.channels > 2;
        let block_align = si.bytes_per_frame() as u16;
        let byte_rate = si.sample_rate * si.bytes_per_frame();
        let fmt_len: u32 = if extensible { 40 } else { 16 };

        let mut header = Vec::with_capacity(12 + 8 + fmt_len as usize + 8);
        header.extend_from_slice(b"RIFF");
        header.extend_from_slice(&0u32.to_le_bytes()); // patched by finish
        header.extend_from_slice(b"WAVE");
        header.extend_from_slice(b"fmt ");
        header.extend_from_slice(&fmt_len.to_le_bytes());
        header.extend_from_slice(
            &if extensible {
                WAVE_FORMAT_EXTENSIBLE
            } else {
                WAVE_FORMAT_PCM
            }
            .to_le_bytes(),
        );
        header.extend_from_slice(&u16::from(si.channels).to_le_bytes());
        header.extend_from_slice(&si.sample_rate.to_le_bytes());
        header.extend_from_slice(&byte_rate.to_le_bytes());
        header.extend_from_slice(&block_align.to_le_bytes());
        header.extend_from_slice(&u16::from(si.bits_per_sample).to_le_bytes());
        if extensible {
            header.extend_from_slice(&22u16.to_le_bytes()); // cbSize
            header.extend_from_slice(&u16::from(si.bits_per_sample).to_le_bytes()); // valid bits
            header.extend_from_slice(&channel_mask(si.channels).to_le_bytes());
            header.extend_from_slice(&SUBTYPE_PCM);
        }
        header.extend_from_slice(b"data");
        header.extend_from_slice(&0u32.to_le_bytes()); // patched by finish

        inner.write_all(&header)?;
        Ok(Self {
            inner,
            bits_per_sample: si.bits_per_sample,
            header_len: header.len() as u64,
            data_len: 0,
            scratch: Vec::new(),
        })
    }

    /// Append interleaved samples, as the decoder hands them over.
    ///
    /// 8-bit is the one place WAV and FLAC disagree: WAV stores it unsigned, FLAC signed,
    /// so the bias is applied here and nowhere else.
    pub fn write_samples(&mut self, samples: &[i32]) -> io::Result<()> {
        let width = usize::from(self.bits_per_sample) / 8;
        self.scratch.clear();
        self.scratch.reserve(samples.len() * width);
        for &s in samples {
            match self.bits_per_sample {
                8 => self.scratch.push((s + 128) as u8),
                16 => self.scratch.extend_from_slice(&(s as i16).to_le_bytes()),
                _ => self.scratch.extend_from_slice(&s.to_le_bytes()[..3]),
            }
        }
        let written = self.scratch.len() as u64;
        if self.data_len + written > MAX_WAV_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "audio exceeds the 4 GiB a WAV file can address",
            ));
        }
        self.inner.write_all(&self.scratch)?;
        self.data_len += written;
        Ok(())
    }

    /// Patch the two length fields and return the `data` payload length.
    pub fn finish(mut self) -> io::Result<u64> {
        // RIFF chunks are word-aligned. `flac` counts the pad byte in the RIFF size;
        // so do we.
        if self.data_len % 2 == 1 {
            self.inner.write_all(&[0])?;
        }
        let padded = self.data_len + (self.data_len & 1);
        let riff_len = self.header_len - 8 + padded;
        if riff_len > MAX_WAV_DATA {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "audio exceeds the 4 GiB a WAV file can address",
            ));
        }

        self.inner.seek(SeekFrom::Start(4))?;
        self.inner.write_all(&(riff_len as u32).to_le_bytes())?;
        self.inner.seek(SeekFrom::Start(self.header_len - 4))?;
        self.inner
            .write_all(&(self.data_len as u32).to_le_bytes())?;
        self.inner.flush()?;
        Ok(self.data_len)
    }
}

fn channel_mask(channels: u8) -> u32 {
    CHANNEL_MASKS
        .get(usize::from(channels) - 1)
        .copied()
        .unwrap_or(0)
}

/// A WAV's chunks in file order: every one but `data` held verbatim, `data` only by where its
/// payload sits (in `layout`). What SBE repair on WAV keeps of a file while it replaces the
/// audio — `LIST`/`INFO`, `id3 ` and taper chunks included, which [`probe`] skips.
#[derive(Debug, Clone)]
pub(crate) struct WavChunks {
    pub layout: WavLayout,
    chunks: Vec<Chunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Chunk {
    Data,
    Other { id: [u8; 4], body: Vec<u8> },
}

impl WavChunks {
    /// True when both files carry the same chunks, in the same order, byte for byte, apart
    /// from the `data` payload.
    pub fn same_chunks_besides_data(&self, other: &WavChunks) -> bool {
        self.chunks == other.chunks
    }
}

/// Every chunk of the WAV at `path`, in order. Refuses a file with more than one `data`
/// chunk, or a chunk that claims to run past the end of the file.
pub(crate) fn read_chunks(path: &Path) -> Result<WavChunks> {
    let layout = probe(path)?;
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let file_len = file.metadata().map_err(|e| Error::io(path, e))?.len();
    let mut r = BufReader::new(file);
    r.seek(SeekFrom::Start(12))
        .map_err(|e| Error::io(path, e))?;

    let mut chunks = Vec::new();
    let mut pos = 12u64;
    // A trailer too short to be a chunk header is not a chunk; `probe` ignores it too.
    while pos + 8 <= file_len {
        let mut hdr = [0u8; 8];
        r.read_exact(&mut hdr).map_err(|e| Error::io(path, e))?;
        let id = [hdr[0], hdr[1], hdr[2], hdr[3]];
        let size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        let body = pos + 8;
        if body + size > file_len {
            return Err(Error::malformed(
                path,
                format!(
                    "the '{}' chunk runs past the end of the file",
                    String::from_utf8_lossy(&id)
                ),
            ));
        }
        if &id == b"data" {
            if chunks.contains(&Chunk::Data) {
                return Err(Error::malformed(path, "more than one data chunk"));
            }
            chunks.push(Chunk::Data);
        } else {
            let mut bytes = vec![0u8; size as usize];
            r.read_exact(&mut bytes).map_err(|e| Error::io(path, e))?;
            chunks.push(Chunk::Other { id, body: bytes });
        }
        pos = body + size + (size & 1);
        r.seek(SeekFrom::Start(pos))
            .map_err(|e| Error::io(path, e))?;
    }
    Ok(WavChunks { layout, chunks })
}

/// Write a WAV to `path` with `like`'s chunks, in `like`'s order, around a `data` chunk of
/// `data_len` bytes that `fill` writes. Fails if `fill` writes any other number of bytes.
pub(crate) fn write_chunks(
    path: &Path,
    like: &WavChunks,
    data_len: u64,
    fill: impl FnOnce(&mut dyn Write) -> Result<()>,
) -> Result<()> {
    let body_len = |c: &Chunk| match c {
        Chunk::Data => data_len,
        Chunk::Other { body, .. } => body.len() as u64,
    };
    let riff_len: u64 = 4 + like
        .chunks
        .iter()
        .map(|c| 8 + body_len(c) + (body_len(c) & 1))
        .sum::<u64>();
    if data_len > MAX_WAV_DATA || riff_len > MAX_WAV_DATA {
        return Err(Error::malformed(
            path,
            "audio exceeds the 4 GiB a WAV file can address",
        ));
    }

    let file = File::create(path).map_err(|e| Error::io(path, e))?;
    let mut w = io::BufWriter::new(file);
    let io_err = |e| Error::io(path, e);
    w.write_all(b"RIFF").map_err(io_err)?;
    w.write_all(&(riff_len as u32).to_le_bytes())
        .map_err(io_err)?;
    w.write_all(b"WAVE").map_err(io_err)?;
    let mut fill = Some(fill);
    for c in &like.chunks {
        let len = body_len(c);
        match c {
            Chunk::Data => {
                w.write_all(b"data").map_err(io_err)?;
                w.write_all(&(len as u32).to_le_bytes()).map_err(io_err)?;
                (fill.take().expect("one data chunk"))(&mut w)?;
            }
            Chunk::Other { id, body } => {
                w.write_all(id).map_err(io_err)?;
                w.write_all(&(len as u32).to_le_bytes()).map_err(io_err)?;
                w.write_all(body).map_err(io_err)?;
            }
        }
        if len & 1 == 1 {
            w.write_all(&[0]).map_err(io_err)?;
        }
    }
    let file = w
        .into_inner()
        .map_err(|e| Error::io(path, e.into_error()))?;
    file.sync_all().map_err(io_err)?;
    let written = file.metadata().map_err(io_err)?.len();
    if written != 8 + riff_len {
        return Err(Error::malformed(
            path,
            format!(
                "wrote {written} bytes where the chunks add up to {}; the data chunk was not \
                 {data_len} bytes",
                8 + riff_len
            ),
        ));
    }
    Ok(())
}

/// Feed `len` bytes of `path`'s file, starting `offset` bytes in, to `f` a block at a time.
pub(crate) fn read_range(
    path: &Path,
    offset: u64,
    len: u64,
    mut f: impl FnMut(&[u8]) -> Result<()>,
) -> Result<()> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Error::io(path, e))?;
    let mut remaining = len;
    let mut buf = vec![0u8; 256 * 1024];
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        file.read_exact(&mut buf[..want])
            .map_err(|e| Error::io(path, e))?;
        f(&buf[..want])?;
        remaining -= want as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    }

    fn cdda_fmt() -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&44_100u32.to_le_bytes());
        f.extend_from_slice(&(44_100u32 * 4).to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f
    }

    fn riff(chunks: &[Vec<u8>]) -> Vec<u8> {
        let body: Vec<u8> = chunks.concat();
        let mut w = b"RIFF".to_vec();
        w.extend_from_slice(&(4 + body.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVE");
        w.extend_from_slice(&body);
        w
    }

    /// Every chunk but `data` comes back out exactly as it went in, odd-sized ones and
    /// their pad byte included, around whatever new `data` is written.
    #[test]
    fn chunks_round_trip_around_new_data() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.wav");
        let list = chunk(b"LIST", b"INFOIART\x05\0\0\0band\0");
        std::fs::write(
            &src,
            riff(&[
                chunk(b"fmt ", &cdda_fmt()),
                list.clone(),
                chunk(b"data", &[1; 8]),
                chunk(b"id3 ", b"xyz"),
            ]),
        )
        .unwrap();

        let chunks = read_chunks(&src).unwrap();
        assert_eq!(chunks.layout.data_len, 8);
        let dst = dir.path().join("b.wav");
        write_chunks(&dst, &chunks, 12, |w| {
            w.write_all(&[2; 12]).map_err(|e| Error::io(&dst, e))
        })
        .unwrap();

        assert_eq!(
            std::fs::read(&dst).unwrap(),
            riff(&[
                chunk(b"fmt ", &cdda_fmt()),
                list,
                chunk(b"data", &[2; 12]),
                chunk(b"id3 ", b"xyz")
            ])
        );
        assert!(read_chunks(&dst).unwrap().same_chunks_besides_data(&chunks));
    }

    #[test]
    fn a_short_fill_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.wav");
        std::fs::write(
            &src,
            riff(&[chunk(b"fmt ", &cdda_fmt()), chunk(b"data", &[1; 8])]),
        )
        .unwrap();
        let chunks = read_chunks(&src).unwrap();
        let dst = dir.path().join("b.wav");
        let err = write_chunks(&dst, &chunks, 12, |w| {
            w.write_all(&[2; 8]).map_err(|e| Error::io(&dst, e))
        })
        .unwrap_err();
        assert!(err.to_string().contains("was not 12 bytes"), "{err}");
    }

    #[test]
    fn a_second_data_chunk_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.wav");
        std::fs::write(
            &src,
            riff(&[
                chunk(b"fmt ", &cdda_fmt()),
                chunk(b"data", &[1; 4]),
                chunk(b"data", &[1; 4]),
            ]),
        )
        .unwrap();
        assert!(
            read_chunks(&src)
                .unwrap_err()
                .to_string()
                .contains("more than one")
        );
    }
}
