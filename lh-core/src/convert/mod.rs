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

impl Conversion {
    /// [`move_to_originals`] for this conversion's source — but only when the output was
    /// checked against it. An unchecked conversion (8-bit WAV, a source with no MD5) is a
    /// weaker result, and its source stays where it is rather than looking set aside for
    /// the same reason a checked one is.
    pub fn move_source_to_originals(&self) -> Result<PathBuf> {
        if !self.checked_against_source {
            return Err(Error::malformed(
                &self.provenance.input,
                "not checked against its output, so left in place",
            ));
        }
        move_to_originals(&self.provenance.input)
    }
}

/// Decode a FLAC to WAV, in-process.
///
/// The decoded audio is hashed as it is written and compared against the MD5 in the
/// source's STREAMINFO. A file that fails its own checksum does not produce a WAV: handing
/// someone audio we know to be wrong is worse than handing them nothing.
///
/// `progress` is called with (frames written, total frames) once per decoded block; a
/// one-shot caller passes `&mut |_, _| true`. Returning `false` stops early — with
/// [`Error::Cancelled`], nothing renamed into place.
pub fn to_wav(
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
///
/// `progress` is polled while `flac` is running (docs/job-queue.md §8) and called with
/// `(0, 0)` — "no count available": `flac` only draws its own percentage display when
/// stderr is a terminal, confirmed empirically (see docs/job-queue.md §8), and piped
/// through `Command` it prints nothing until it exits, so there is no real number to relay
/// in between. Every existing display already copes: a gauge renders `total == 0` as an
/// empty bar. Returning `false` kills `flac` mid-run instead of waiting for it to finish;
/// `flac` itself never learns it was asked to stop, and the killed child's `.part` output
/// is cleaned up by [`TempOutput`] the same as any other cancelled or failed conversion. A
/// one-shot caller passes `&mut |_, _| true`.
pub fn to_flac(
    src: &Path,
    dst: &Path,
    tool: &Tool,
    opts: &EncodeOpts,
    overwrite: bool,
    progress: &mut dyn FnMut(u32, u32) -> bool,
) -> Result<Conversion> {
    let (temp, provenance, audio_md5, checked_against_source) =
        encode_flac_staged(src, dst, tool, opts, overwrite, &mut || progress(0, 0))?;
    Ok(Conversion {
        output: temp.commit()?,
        provenance,
        audio_md5,
        checked_against_source,
    })
}

/// [`to_flac`], stopping short of the commit: the encoded, checked output is left staged
/// under [`TempOutput`] rather than renamed into place.
///
/// Repair (docs/sbe-repair.md §4 step 5f) needs exactly this — two files encoded and
/// verified independently, but renamed into place together or not at all, which is a
/// commit `to_flac` cannot defer once it has made it. Everything up to the decision to
/// commit is identical, so it lives here once and `to_flac` is the thin wrapper that always
/// commits immediately. Takes the plain `should_continue` shape rather than `to_flac`'s
/// `progress`, since [`execute_fix`](crate::analysis::execute_fix) — its other caller — has
/// no progress reporting of its own (out of scope for docs/architecture-cleanup.md A3; see
/// docs/sbe-repair.md).
pub(crate) fn encode_flac_staged(
    src: &Path,
    dst: &Path,
    tool: &Tool,
    opts: &EncodeOpts,
    overwrite: bool,
    should_continue: &mut dyn FnMut() -> bool,
) -> Result<(TempOutput, Provenance, [u8; 16], bool)> {
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
        Some(encoded) if !comparable || encoded == source_md5 => Ok((
            temp,
            Provenance {
                operation: "WAV → FLAC".into(),
                agent,
                input: src.to_path_buf(),
                output: dst.to_path_buf(),
            },
            encoded,
            comparable,
        )),
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

/// The folder, inside a show's own folder, that converted sources are moved into.
pub const ORIGINALS_DIR: &str = "_original";

/// Move a source that has just converted cleanly into [`ORIGINALS_DIR`] beside it, so every
/// later step on the show folder (rename, tag, checksum) sees only the converted files.
/// Moved, never deleted (Principle 1): the person clears it out once satisfied. Refuses
/// with [`Error::OutputExists`] rather than replace a file already there. Returns where
/// the source now is.
pub fn move_to_originals(src: &Path) -> Result<PathBuf> {
    let dir = src
        .parent()
        .ok_or_else(|| Error::malformed(src, "has no file name to work from"))?
        .join(ORIGINALS_DIR);
    move_aside(src, &dir)
}

/// [`move_to_originals`] into any folder `dir`, created if need be: `src` keeps its name,
/// and a file already there by that name is refused with [`Error::OutputExists`].
pub fn move_aside(src: &Path, dir: &Path) -> Result<PathBuf> {
    let name = src
        .file_name()
        .ok_or_else(|| Error::malformed(src, "has no file name to work from"))?;
    std::fs::create_dir_all(dir).map_err(|e| Error::io(dir, e))?;
    let dst = dir.join(name);
    // `symlink_metadata` so a dangling symlink still counts as something in the way.
    if dst.symlink_metadata().is_ok() {
        return Err(Error::OutputExists { path: dst });
    }
    std::fs::rename(src, &dst).map_err(|e| Error::io(src, e))?;
    Ok(dst)
}

/// Same stem, new extension, beside `src` unless told otherwise. Shared by `lh-cli` and
/// `lh-gui` (Principle 4) so both front ends name outputs the same way.
///
/// Built as an `OsString` rather than through `with_extension`, which would eat everything
/// after the last dot of a name like `gd77-05-08.d1t01.flac` — and non-UTF-8 names are in
/// the fixture corpus for a reason.
pub fn destination(src: &Path, extension: &str, out_dir: Option<&Path>) -> Result<PathBuf> {
    let no_file_name = || Error::malformed(src, "has no file name to work from");
    let dir = out_dir
        .map(Path::to_path_buf)
        .or_else(|| src.parent().map(Path::to_path_buf))
        .ok_or_else(no_file_name)?;
    let mut name = src.file_stem().ok_or_else(no_file_name)?.to_os_string();
    name.push(".");
    name.push(extension);
    Ok(dir.join(name))
}
