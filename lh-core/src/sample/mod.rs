//! Short MP3 clips of a track, for a show's listing (docs/sample.md).
//!
//! The clip is cut in-process: a FLAC is decoded up to the end of the clip, or a WAV's
//! range is read, and the excerpt is written to a temporary WAV. The MP3 itself comes from
//! a reference encoder (Principle 3): `lame`, or `ffmpeg` when `lame` is absent, recorded
//! in the [`Provenance`] like every other tool run (Principle 2).
//!
//! Sites cap a sample's size, so the size is part of the contract. For CBR it is known
//! before encoding, and a clip that cannot fit is refused up front. For VBR it is only
//! estimated, so the encoded file is measured and discarded if it is over (Principle 1).

use crate::error::{Error, Result};
use crate::format::{self, wav::WavWriter};
use crate::model::{AudioFormat, StreamInfo};
use crate::output::TempOutput;
use crate::tag::{self, Field};
use crate::tools::{Provenance, Registry, Tool, ToolId, run_cancellable};
use std::ffi::OsString;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// 1 MB, the cap sites commonly put on a sample. Decimal, so it also fits a limit that
/// meant 1 MiB.
pub const DEFAULT_MAX_BYTES: u64 = 1_000_000;

/// The longest default clip. Long enough to hear the recording; a limit that allows more
/// is not a reason to take more.
pub const DEFAULT_MAX_SECS: f64 = 30.0;

/// Frames, the ID3 tag and the encoder's delay and padding: what a CBR file carries on top
/// of `bitrate × length`. Generous, so the up-front check never passes a clip that the
/// measured one would then reject.
const OVERHEAD_BYTES: u64 = 6 * 1024;

/// How the MP3 is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Constant bitrate, in kbps. The size is known before encoding.
    Cbr(u16),
    /// LAME's `-V` quality, 0 (best) to 9. The size is only estimated.
    Vbr(u8),
}

impl Mode {
    /// Every mode the screens offer, best first.
    pub const PRESETS: [Mode; 7] = [
        Mode::Cbr(320),
        Mode::Cbr(256),
        Mode::Cbr(192),
        Mode::Cbr(160),
        Mode::Cbr(128),
        Mode::Vbr(0),
        Mode::Vbr(2),
    ];

    /// 30 s of it fits 1 MB.
    pub const DEFAULT: Mode = Mode::Cbr(256);

    /// `320`, `v0`: what `--mode` takes.
    pub fn parse(s: &str) -> Option<Mode> {
        let s = s.trim().to_ascii_lowercase();
        let s = s.strip_suffix("kbps").or(s.strip_suffix('k')).unwrap_or(&s);
        if let Some(q) = s.strip_prefix('v') {
            let q: u8 = q.parse().ok()?;
            return (q <= 9).then_some(Mode::Vbr(q));
        }
        let kbps: u16 = s.parse().ok()?;
        // The MPEG-1 Layer III bitrates at and above 32 kbps.
        const RATES: [u16; 14] = [
            32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ];
        RATES.contains(&kbps).then_some(Mode::Cbr(kbps))
    }

    pub fn label(self) -> String {
        match self {
            Mode::Cbr(kbps) => format!("{kbps} kbps CBR"),
            Mode::Vbr(q) => format!("V{q} VBR (~{} kbps)", self.nominal_kbps()),
        }
    }

    /// Whether [`estimate_bytes`] is a bound rather than a guess.
    pub fn is_exact(self) -> bool {
        matches!(self, Mode::Cbr(_))
    }

    /// The bitrate a size is estimated from: exact for CBR, LAME's typical average for
    /// VBR on music.
    fn nominal_kbps(self) -> u32 {
        match self {
            Mode::Cbr(kbps) => u32::from(kbps),
            Mode::Vbr(q) => match q {
                0 => 245,
                1 => 225,
                2 => 190,
                3 => 175,
                4 => 165,
                5 => 130,
                6 => 115,
                7 => 100,
                8 => 85,
                _ => 65,
            },
        }
    }

    fn lame_args(self) -> Vec<OsString> {
        match self {
            Mode::Cbr(kbps) => vec!["-b".into(), kbps.to_string().into(), "--cbr".into()],
            Mode::Vbr(q) => vec!["-V".into(), q.to_string().into()],
        }
    }

    fn ffmpeg_args(self) -> Vec<OsString> {
        match self {
            Mode::Cbr(kbps) => vec!["-b:a".into(), format!("{kbps}k").into()],
            Mode::Vbr(q) => vec!["-q:a".into(), q.to_string().into()],
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// The size a clip of `secs` should come out at.
pub fn estimate_bytes(mode: Mode, secs: f64) -> u64 {
    let audio = secs.max(0.0) * f64::from(mode.nominal_kbps()) * 1000.0 / 8.0;
    audio.ceil() as u64 + OVERHEAD_BYTES
}

/// The longest clip whose estimate stays under `max_bytes`, in whole seconds.
pub fn longest_fitting(mode: Mode, max_bytes: u64) -> f64 {
    let room = max_bytes.saturating_sub(OVERHEAD_BYTES) as f64;
    (room * 8.0 / 1000.0 / f64::from(mode.nominal_kbps())).floor()
}

/// Where in the track the clip is, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Clip {
    pub start: f64,
    pub length: f64,
}

impl Clip {
    /// The longest clip that fits `max_bytes`, capped at [`DEFAULT_MAX_SECS`] and the
    /// track itself, centred in the track (docs/sample.md §1).
    pub fn default_for(track_secs: Option<f64>, mode: Mode, max_bytes: u64) -> Clip {
        let length = longest_fitting(mode, max_bytes).clamp(1.0, DEFAULT_MAX_SECS);
        Clip::centred(track_secs, length)
    }

    /// A clip of `length` (or the whole track, if that is shorter) centred in the track.
    /// From the start when the track's length is unknown.
    pub fn centred(track_secs: Option<f64>, length: f64) -> Clip {
        match track_secs {
            Some(track) => {
                let length = length.min(track.floor()).max(1.0);
                Clip {
                    start: ((track - length) / 2.0).max(0.0).floor(),
                    length,
                }
            }
            None => Clip { start: 0.0, length },
        }
    }

    pub fn end(&self) -> f64 {
        self.start + self.length
    }
}

/// The encoder: `lame` if it is found, else `ffmpeg` (docs/sample.md §3). When neither
/// is, the error names both, so the person knows either will do. A `lame` named through
/// `LH_LAME` is the whole search, the same as the registry's own rule: falling back to
/// `ffmpeg` when the person asked for a particular `lame` would make the provenance lie.
pub fn find_encoder() -> Result<Tool> {
    let lame = Registry::discover_one(ToolId::Lame);
    let lame_error = match lame.require(ToolId::Lame) {
        Ok(tool) => return Ok(tool.clone()),
        Err(e) => e,
    };
    if std::env::var_os(ToolId::Lame.env_var()).is_some() {
        return Err(lame_error);
    }
    let ffmpeg = Registry::discover_one(ToolId::Ffmpeg);
    match (ffmpeg.require(ToolId::Ffmpeg), lame_error) {
        (Ok(tool), _) => Ok(tool.clone()),
        (
            Err(Error::ToolNotFound { searched, .. }),
            Error::ToolNotFound {
                searched: lame_searched,
                ..
            },
        ) => Err(Error::ToolNotFound {
            tool: "lame (or ffmpeg)",
            purpose: ToolId::Lame.purpose(),
            searched: format!("{lame_searched}; {searched}"),
        }),
        // A lame or ffmpeg that is there but unusable is the more useful thing to report.
        (Err(_), lame_error @ Error::ToolUnusable { .. }) => Err(lame_error),
        (Err(ffmpeg_error), _) => Err(ffmpeg_error),
    }
}

/// `<stem>.sample.mp3` beside the show folder the track is in — not inside it, where it
/// would end up in the checksums and the torrent (docs/sample.md §1).
pub fn default_output(src: &Path) -> Result<PathBuf> {
    let no_file_name = || Error::malformed(src, "has no file name to work from");
    let abs = src.canonicalize().map_err(|e| Error::io(src, e))?;
    let folder = abs.parent().ok_or_else(no_file_name)?;
    let dir = folder.parent().unwrap_or(folder);
    let mut name = abs.file_stem().ok_or_else(no_file_name)?.to_os_string();
    name.push(".sample.mp3");
    Ok(dir.join(name))
}

/// A written sample, and what made it.
#[derive(Debug, Clone)]
pub struct Sample {
    pub output: PathBuf,
    pub bytes: u64,
    pub clip: Clip,
    pub provenance: Provenance,
}

/// What [`encode`] is asked to make.
#[derive(Debug, Clone)]
pub struct Request {
    pub clip: Clip,
    pub mode: Mode,
    /// Refuse, or discard, a clip over this many bytes. `None` for no limit.
    pub max_bytes: Option<u64>,
    pub overwrite: bool,
}

/// Cut `request.clip` out of `src` and encode it to `dst` with `encoder`.
///
/// `progress` is called with (frames cut, frames to cut) while the excerpt is being cut,
/// then with `(0, 0)` — "no count available" — while the encoder runs, the same as
/// [`to_flac`](crate::convert::to_flac). Returning `false` stops either phase with
/// [`Error::Cancelled`], and nothing is left under `dst`.
pub fn encode(
    src: &Path,
    dst: &Path,
    encoder: &Tool,
    request: &Request,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<Sample> {
    let Request {
        clip,
        mode,
        max_bytes,
        overwrite,
    } = *request;
    if !(clip.start >= 0.0 && clip.length > 0.0) {
        return Err(Error::malformed(
            src,
            "a clip needs a start of 0 or more and a length above 0",
        ));
    }
    if let Some(max) = max_bytes
        && mode.is_exact()
        && estimate_bytes(mode, clip.length) > max
    {
        return Err(Error::SampleTooLarge {
            path: dst.to_path_buf(),
            detail: format!(
                "{} s at {mode} comes to about {}, over the {} limit; {} s would fit",
                clip.length,
                format_size(estimate_bytes(mode, clip.length)),
                format_size(max),
                longest_fitting(mode, max)
            ),
        });
    }

    let probed = format::probe(src)?;
    let si = &probed.stream_info;
    let start = (clip.start * f64::from(si.sample_rate)).round() as u64;
    let mut end = start + (clip.length * f64::from(si.sample_rate)).round() as u64;
    if let Some(total) = si.total_frames {
        if start >= total {
            return Err(Error::malformed(
                src,
                format!(
                    "the clip starts at {} but the track is only {} long",
                    format_time(clip.start),
                    format_time(total as f64 / f64::from(si.sample_rate))
                ),
            ));
        }
        end = end.min(total);
    }

    let temp = TempOutput::stage(src, dst, overwrite)?;
    let scratch = tempfile::tempdir().map_err(|e| Error::io(std::env::temp_dir(), e))?;
    let excerpt = scratch.path().join("excerpt.wav");
    let cut = cut_excerpt(src, probed.format, si, start, end, &excerpt, progress)?;
    if cut == 0 {
        return Err(Error::malformed(src, "the clip holds no audio"));
    }

    let tags = if probed.format == AudioFormat::Flac {
        tag::read(src).unwrap_or_default()
    } else {
        tag::Tags::default()
    };
    let argv = encoder_argv(encoder, mode, &tags, &excerpt, temp.path())?;
    let agent = run_cancellable(encoder, &argv, &mut || progress(0, 0))?;

    let bytes = std::fs::metadata(temp.path())
        .map_err(|e| Error::io(temp.path(), e))?
        .len();
    if let Some(max) = max_bytes
        && bytes > max
    {
        return Err(Error::SampleTooLarge {
            path: dst.to_path_buf(),
            detail: format!(
                "came out at {}, over the {} limit, so it was discarded; \
                 shorten it or pick a lower bitrate",
                format_size(bytes),
                format_size(max)
            ),
        });
    }

    let length = cut as f64 / f64::from(si.sample_rate);
    Ok(Sample {
        output: temp.commit()?,
        bytes,
        clip: Clip {
            start: clip.start,
            length,
        },
        provenance: Provenance {
            operation: format!(
                "{} → MP3 sample ({} to {}, {mode})",
                probed.format,
                format_time(clip.start),
                format_time(clip.start + length)
            ),
            agent,
            input: src.to_path_buf(),
            output: dst.to_path_buf(),
        },
    })
}

/// Writes frames `start..end` of `src` to a WAV at `out`, returning how many it wrote —
/// fewer than asked when the track ends first and did not say how long it was.
fn cut_excerpt(
    src: &Path,
    format: AudioFormat,
    si: &StreamInfo,
    start: u64,
    end: u64,
    out: &Path,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<u64> {
    let file = File::create(out).map_err(|e| Error::io(out, e))?;
    let mut writer = WavWriter::new(BufWriter::new(file), si).map_err(|e| Error::io(out, e))?;
    let channels = usize::from(si.channels);
    let total = u32::try_from(end).unwrap_or(u32::MAX);
    let mut written = 0u64;

    match format {
        AudioFormat::Flac => {
            // claxon cannot seek, so everything before the clip is decoded and dropped.
            let mut reader = claxon::FlacReader::open(src).map_err(|source| Error::Flac {
                path: src.to_path_buf(),
                source,
            })?;
            let mut blocks = reader.blocks();
            let mut buf = Vec::new();
            let mut interleaved: Vec<i32> = Vec::new();
            let mut pos = 0u64;
            while pos < end {
                let block = match blocks.read_next_or_eof(buf) {
                    Ok(Some(block)) => block,
                    Ok(None) => break,
                    Err(source) => {
                        return Err(Error::Flac {
                            path: src.to_path_buf(),
                            source,
                        });
                    }
                };
                let len = u64::from(block.duration());
                let from = start.saturating_sub(pos).min(len) as u32;
                let to = end.saturating_sub(pos).min(len) as u32;
                interleaved.clear();
                for i in from..to {
                    for ch in 0..block.channels() {
                        interleaved.push(block.sample(ch, i));
                    }
                }
                writer
                    .write_samples(&interleaved)
                    .map_err(|e| Error::io(out, e))?;
                written += u64::from(to - from);
                pos += len;
                if !progress(u32::try_from(pos.min(end)).unwrap_or(u32::MAX), total) {
                    return Err(Error::Cancelled);
                }
                buf = block.into_buffer();
            }
        }
        AudioFormat::Wav => {
            let layout = format::wav::probe(src)?;
            let frame_bytes = u64::from(si.bytes_per_frame());
            let available = layout.data_len / frame_bytes;
            let end = end.min(available);
            let width = usize::from(si.bits_per_sample).div_ceil(8);
            let mut samples: Vec<i32> = Vec::new();
            let mut done = start;
            format::wav::read_range(
                src,
                layout.data_offset + start * frame_bytes,
                end.saturating_sub(start) * frame_bytes,
                |bytes| {
                    samples.clear();
                    samples.extend(bytes.chunks_exact(width).map(|b| match width {
                        1 => i32::from(b[0]) - 128,
                        2 => i32::from(i16::from_le_bytes([b[0], b[1]])),
                        // Sign-extended from the top byte.
                        _ => i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 8,
                    }));
                    writer
                        .write_samples(&samples)
                        .map_err(|e| Error::io(out, e))?;
                    done += (bytes.len() / (width * channels)) as u64;
                    if progress(u32::try_from(done).unwrap_or(u32::MAX), total) {
                        Ok(())
                    } else {
                        Err(Error::Cancelled)
                    }
                },
            )?;
            written = end.saturating_sub(start);
        }
        other => {
            return Err(Error::Unsupported {
                path: src.to_path_buf(),
                format: other.name(),
                tool: other.required_tool(),
            });
        }
    }

    writer.finish().map_err(|e| Error::io(out, e))?;
    Ok(written)
}

fn encoder_argv(
    encoder: &Tool,
    mode: Mode,
    tags: &tag::Tags,
    input: &Path,
    output: &Path,
) -> Result<Vec<OsString>> {
    let fields = [Field::Title, Field::Artist, Field::Album];
    let mut argv: Vec<OsString> = Vec::new();
    match encoder.id {
        ToolId::Lame => {
            // Without it, lame writes UTF-8 argv bytes into a UTF-16 frame, and a
            // non-ASCII title reads back as noise.
            argv.push("--silent".into());
            argv.push("--id3v2-utf16".into());
            argv.extend(mode.lame_args());
            for (field, flag) in fields.into_iter().zip(["--tt", "--ta", "--tl"]) {
                if let Some(value) = tags.get(field).filter(|v| !v.is_empty()) {
                    argv.push(flag.into());
                    argv.push(value.into());
                }
            }
            argv.push(input.into());
            argv.push(output.into());
        }
        ToolId::Ffmpeg => {
            for arg in ["-nostdin", "-hide_banner", "-loglevel", "error", "-y", "-i"] {
                argv.push(arg.into());
            }
            argv.push(input.into());
            for arg in ["-vn", "-codec:a", "libmp3lame"] {
                argv.push(arg.into());
            }
            argv.extend(mode.ffmpeg_args());
            for (field, key) in fields.into_iter().zip(["title", "artist", "album"]) {
                if let Some(value) = tags.get(field).filter(|v| !v.is_empty()) {
                    argv.push("-metadata".into());
                    argv.push(format!("{key}={value}").into());
                }
            }
            // The staged name ends in `.part`, so the container has to be named.
            argv.push("-f".into());
            argv.push("mp3".into());
            argv.push(output.into());
        }
        other => {
            return Err(Error::ToolUnusable {
                tool: other.name(),
                path: encoder.path.clone(),
                detail: "encoding MP3 needs lame or ffmpeg, not this one".into(),
            });
        }
    }
    Ok(argv)
}

/// `90`, `1:30`, `1:02:03`, `1:30.5`, in seconds.
pub fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut secs = 0.0;
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    for (i, part) in parts.iter().enumerate() {
        let last = i == parts.len() - 1;
        let value: f64 = if last {
            part.parse().ok()?
        } else {
            f64::from(part.parse::<u32>().ok()?)
        };
        if !value.is_finite() || value < 0.0 || (i > 0 && value >= 60.0) {
            return None;
        }
        secs = secs * 60.0 + value;
    }
    Some(secs)
}

/// `1:30`, or `1:02:03` past the hour. Whole seconds, rounded down.
pub fn format_time(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// `1M`, `900K`, `1MiB`, `1000000`, in bytes. `K`/`M` are decimal, `Ki`/`Mi` binary.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.strip_suffix('b').unwrap_or(&s);
    let (number, unit) = match s.find(|c: char| !c.is_ascii_digit() && c != '.') {
        Some(i) => s.split_at(i),
        None => (s, ""),
    };
    let scale: f64 = match unit.trim() {
        "" => 1.0,
        "k" => 1e3,
        "m" => 1e6,
        "ki" => 1024.0,
        "mi" => 1024.0 * 1024.0,
        _ => return None,
    };
    let n: f64 = number.parse().ok()?;
    (n.is_finite() && n > 0.0).then(|| (n * scale).round() as u64)
}

/// `942 KB`, decimal like the limit it is compared against.
pub fn format_size(bytes: u64) -> String {
    if bytes >= 10_000_000 {
        format!("{:.1} MB", bytes as f64 / 1e6)
    } else {
        format!("{} KB", bytes.div_ceil(1000))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_parse_and_format() {
        assert_eq!(parse_time("90"), Some(90.0));
        assert_eq!(parse_time("1:30"), Some(90.0));
        assert_eq!(parse_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_time("0:30.5"), Some(30.5));
        assert_eq!(parse_time("1:75"), None);
        assert_eq!(parse_time(""), None);
        assert_eq!(parse_time("a"), None);
        assert_eq!(format_time(90.9), "1:30");
        assert_eq!(format_time(3723.0), "1:02:03");
    }

    #[test]
    fn sizes_parse_decimal_and_binary() {
        assert_eq!(parse_size("1M"), Some(1_000_000));
        assert_eq!(parse_size("1MB"), Some(1_000_000));
        assert_eq!(parse_size("1MiB"), Some(1_048_576));
        assert_eq!(parse_size("900k"), Some(900_000));
        assert_eq!(parse_size("1.5M"), Some(1_500_000));
        assert_eq!(parse_size("12345"), Some(12_345));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("1G"), None);
    }

    #[test]
    fn modes_parse() {
        assert_eq!(Mode::parse("320"), Some(Mode::Cbr(320)));
        assert_eq!(Mode::parse("256k"), Some(Mode::Cbr(256)));
        assert_eq!(Mode::parse("V0"), Some(Mode::Vbr(0)));
        assert_eq!(Mode::parse("300"), None);
        assert_eq!(Mode::parse("v10"), None);
    }

    #[test]
    fn the_longest_fitting_clip_is_under_the_limit() {
        for mode in Mode::PRESETS {
            let secs = longest_fitting(mode, DEFAULT_MAX_BYTES);
            assert!(estimate_bytes(mode, secs) <= DEFAULT_MAX_BYTES, "{mode}");
            assert!(
                estimate_bytes(mode, secs + 1.0) > DEFAULT_MAX_BYTES,
                "{mode}"
            );
        }
        // The default mode's reason for being the default.
        assert!(longest_fitting(Mode::DEFAULT, DEFAULT_MAX_BYTES) >= DEFAULT_MAX_SECS);
    }

    #[test]
    fn the_default_clip_is_centred_and_capped() {
        let clip = Clip::default_for(Some(300.0), Mode::DEFAULT, DEFAULT_MAX_BYTES);
        assert_eq!(
            clip,
            Clip {
                start: 135.0,
                length: 30.0
            }
        );
        assert_eq!(
            Clip::centred(Some(300.0), 40.0),
            Clip {
                start: 130.0,
                length: 40.0
            }
        );
        let short = Clip::default_for(Some(12.5), Mode::DEFAULT, DEFAULT_MAX_BYTES);
        assert_eq!(
            short,
            Clip {
                start: 0.0,
                length: 12.0
            }
        );
        let tight = Clip::default_for(Some(300.0), Mode::Cbr(320), DEFAULT_MAX_BYTES);
        assert_eq!(
            tight.length,
            longest_fitting(Mode::Cbr(320), DEFAULT_MAX_BYTES)
        );
    }
}
