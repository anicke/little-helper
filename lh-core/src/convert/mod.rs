//! Producing new files, and the two very different ways we do it.
//!
//! Principle 3 draws the line. FLAC → WAV is a lossless decode: deterministic and
//! bit-identical, so it runs in-process and the corpus tests pin our container output
//! byte-for-byte against `flac -d`. WAV → FLAC goes through the reference `flac` binary,
//! because the FLAC vendor string is a provenance record other traders inspect, and only
//! the real tool can write `reference libFLAC x.y.z` into it (Principle 2).
//!
//! Principle 1 governs both: output goes to a temp file beside the destination and is
//! renamed into place only after it has been checked. An interrupted run leaves the
//! original untouched and never a half-written file under the real name.
//!
//! There is no `Codec` trait yet. `format::probe` already dispatches on `AudioFormat` the
//! same way, and one implementor is not an abstraction; the trait in PLAN.md §3 earns its
//! place when SHN arrives and there is a second one to hold.

use crate::error::{Error, Result};
use crate::format::{self, wav::WavWriter};
use crate::model::AudioFormat;
use crate::output::TempOutput;
use crate::tools::{Agent, Provenance, Tool, ToolId, run_cancellable};
use std::ffi::OsString;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

/// How to drive the reference encoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodeOpts {
    /// `flac`'s `-0` … `-8`. Traders' standards ask for `-8`, and the extra time is
    /// nothing against the decades the file will sit in someone's archive.
    pub compression_level: u8,
    /// `flac --verify`: decode while encoding and compare. On by default, because the
    /// alternative is finding out later.
    pub verify: bool,
}

impl Default for EncodeOpts {
    fn default() -> Self {
        Self {
            compression_level: 8,
            verify: true,
        }
    }
}

/// One completed conversion, and the evidence that it was correct.
#[derive(Debug, Clone)]
pub struct Conversion {
    pub output: PathBuf,
    pub provenance: Provenance,
    /// MD5 of the audio, in FLAC's convention. Recomputed from what actually crossed the
    /// wire, not copied from a header.
    pub audio_md5: [u8; 16],
    /// Whether that value could be compared against one the source already carried.
    /// False when the source had nothing to compare against, which is a weaker result
    /// and should be reported as one.
    pub checked_against_source: bool,
}

/// Decode a FLAC to WAV, in-process.
///
/// The decoded audio is hashed as it is written and compared against the MD5 in the
/// source's STREAMINFO. A file that fails its own checksum does not produce a WAV: handing
/// someone audio we know to be wrong is worse than handing them nothing.
pub fn to_wav(src: &Path, dst: &Path, overwrite: bool) -> Result<Conversion> {
    to_wav_with_progress(src, dst, overwrite, &mut |_, _| true)
}

/// [`to_wav`], reporting (frames written, total frames) once per decoded block and
/// stopping early — with [`Error::Cancelled`], nothing renamed into place — the moment
/// `progress` returns `false`. `to_wav` is this with a `progress` that never says stop.
pub fn to_wav_with_progress(
    src: &Path,
    dst: &Path,
    overwrite: bool,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<Conversion> {
    let probed = format::probe(src)?;
    if probed.format != AudioFormat::Flac {
        return Err(Error::Unsupported {
            path: src.to_path_buf(),
            format: probed.format.name(),
            tool: "a decoder we do not have",
        });
    }
    let temp = TempOutput::stage(src, dst, overwrite)?;

    let file = File::create(temp.path()).map_err(|e| Error::io(temp.path(), e))?;
    let mut writer =
        WavWriter::new(BufWriter::new(file), &probed.stream_info).map_err(|e| Error::io(dst, e))?;
    let audio_md5 = format::flac::decode_to_wav(src, temp.path(), &mut writer, progress)?;
    writer.finish().map_err(|e| Error::io(dst, e))?;

    if let Some(stored) = probed.stream_info.audio_md5
        && stored != audio_md5
    {
        return Err(Error::malformed(
            src,
            format!(
                "decoded audio does not match the MD5 in its header (header {}, decoded {}); \
                 no WAV was written — run `lh verify` on it",
                hex::encode(stored),
                hex::encode(audio_md5)
            ),
        ));
    }

    Ok(Conversion {
        output: temp.commit()?,
        provenance: Provenance {
            operation: "FLAC → WAV".into(),
            agent: Agent::in_process(),
            input: src.to_path_buf(),
            output: dst.to_path_buf(),
        },
        audio_md5,
        checked_against_source: probed.stream_info.audio_md5.is_some(),
    })
}

/// Encode a WAV to FLAC with the reference `flac` binary.
///
/// After `flac` returns we read the MD5 it wrote into STREAMINFO and compare it against
/// the source's audio, in-process. That is an independent check of the encoder's own
/// `--verify`, and it is cheap: one header read against one pass over the WAV.
pub fn to_flac(
    src: &Path,
    dst: &Path,
    tool: &Tool,
    opts: &EncodeOpts,
    overwrite: bool,
) -> Result<Conversion> {
    to_flac_cancellable(src, dst, tool, opts, overwrite, &mut || true)
}

/// [`to_flac`], but `should_continue` is polled while `flac` is running and returning
/// `false` kills it mid-run instead of waiting for it to finish (docs/job-queue.md §8).
/// `flac` itself never learns it was asked to stop; the killed child's `.part` output is
/// cleaned up by `TempOutput` the same as any other cancelled or failed conversion.
///
/// There is no (done, total) here the way [`to_wav_with_progress`] has: `flac` only draws
/// its own percentage display when stderr is a terminal, confirmed empirically (see
/// docs/job-queue.md §8) — piped through `Command`, it prints nothing until it exits, so
/// there is no number to relay in between. `to_flac` is this with a `should_continue` that
/// never says stop.
pub fn to_flac_cancellable(
    src: &Path,
    dst: &Path,
    tool: &Tool,
    opts: &EncodeOpts,
    overwrite: bool,
    should_continue: &mut dyn FnMut() -> bool,
) -> Result<Conversion> {
    if tool.id != ToolId::Flac {
        return Err(Error::ToolUnusable {
            tool: tool.id.name(),
            path: tool.path.clone(),
            detail: "encoding FLAC needs the flac binary, not this one".into(),
        });
    }
    if opts.compression_level > 8 {
        return Err(Error::malformed(
            src,
            format!(
                "compression level {} is out of range; flac takes 0 to 8",
                opts.compression_level
            ),
        ));
    }
    let probed = format::probe(src)?;
    if probed.format != AudioFormat::Wav {
        return Err(Error::Unsupported {
            path: src.to_path_buf(),
            format: probed.format.name(),
            tool: "a decoder we do not have",
        });
    }
    let temp = TempOutput::stage(src, dst, overwrite)?;

    let mut argv: Vec<OsString> = vec![
        "--silent".into(),
        format!("--compression-level-{}", opts.compression_level).into(),
    ];
    if opts.verify {
        argv.push("--verify".into());
    }
    // `--` so a source file whose name begins with a dash is audio, not an option.
    argv.push("-o".into());
    argv.push(temp.path().into());
    argv.push("--".into());
    argv.push(src.into());

    let agent = run_cancellable(tool, &argv, should_continue)?;

    // What flac says it encoded, against what we read from the source ourselves.
    let source_md5 = format::audio_md5(src)?;
    // Read the header directly: the staged file is still under its `.part` name, which
    // `format::probe` would not recognize as FLAC.
    let (written, _) = format::flac::probe(temp.path())?;
    // 8-bit is the one depth where the two conventions differ — WAV unsigned, FLAC
    // signed — so the digests legitimately disagree and there is nothing to compare.
    let comparable = probed.stream_info.bits_per_sample != 8;
    match written.audio_md5 {
        Some(encoded) if !comparable || encoded == source_md5 => Ok(Conversion {
            output: temp.commit()?,
            provenance: Provenance {
                operation: "WAV → FLAC".into(),
                agent,
                input: src.to_path_buf(),
                output: dst.to_path_buf(),
            },
            audio_md5: encoded,
            checked_against_source: comparable,
        }),
        Some(encoded) => Err(Error::malformed(
            dst,
            format!(
                "the encoded FLAC does not contain the audio it was given \
                 (source {}, encoded {}); it was discarded",
                hex::encode(source_md5),
                hex::encode(encoded)
            ),
        )),
        None => Err(Error::malformed(
            dst,
            "the encoded FLAC carries no audio MD5, so it cannot be checked; it was discarded",
        )),
    }
}
