//! Planning and executing an SBE repair (docs/sbe-repair.md).
//!
//! R1 is [`plan_fix`]: pure arithmetic over each file's declared frame count — the same
//! header-only read [`crate::analysis::sbe`] already does, so a plan can be shown to a user
//! with no decode.
//!
//! R2/R3 is [`execute_fix`]: decode, shift (chained left to right across as many files as
//! the plan has), pad the tail when asked, re-encode, restore tags, and commit every file
//! atomically. [`execute_single_boundary`] is the R2-shaped entry point for the common case
//! of one boundary between two adjacent files, built on top of it.
//!
//! R5 is the same two entry points on a set of WAVs: the shift is bytes of each file's
//! `data` chunk moved to its neighbour, with no decode, no encode and no tags to carry
//! (docs/sbe-repair.md §10). [`execute_fix`] and [`fix_in_place`] pick the path by format.

use crate::convert::{self, EncodeOpts};
use crate::error::{Error, Result};
use crate::format::{
    self,
    wav::{self, WavWriter},
};
use crate::model::{AudioFile, AudioFormat, FRAMES_PER_SECTOR, StreamInfo};
use crate::output::{self, TempOutput};
use crate::tag;
use crate::tools::{Agent, Provenance, Tool};
use md5::{Digest, Md5};
use rayon::prelude::*;
use std::fs::File;
use std::io::{BufWriter, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// shntool's `-b`/`-f`/`-u`: which way a boundary's remainder frames move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryDirection {
    /// `-b`, the default: round the earlier file down to the previous sector, handing its
    /// excess frames to the file after it.
    Backward,
    /// `-f`: round the earlier file up to the next sector, borrowing frames from the file
    /// after it.
    Forward,
    /// `-u`: whichever of the two moves fewer frames. A tie goes to `Backward`.
    Nearest,
}

/// What happens to the last file in the set, which no boundary can fix by borrowing —
/// there is no file after it to hand a remainder to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailPolicy {
    /// Leave it. If still misaligned, that is reported, not hidden.
    Report,
    /// Add silence to reach the next sector boundary — the one operation here that adds
    /// samples instead of reassigning them.
    Pad,
}

/// One boundary between adjacent files, and what crosses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryFix {
    /// Index into the ordered set; the boundary sits between `index` and `index + 1`.
    pub index: usize,
    /// Frames moved. Positive: taken from the end of `index`, prepended to `index + 1`.
    /// Negative: taken from the start of `index + 1`, appended to `index`. Zero: already
    /// aligned, listed so a caller can show "no change" rather than a gap.
    pub shifted_frames: i64,
}

/// The whole set's outcome, computed before anything is written.
#[derive(Debug, Clone)]
pub struct FixPlan {
    pub boundaries: Vec<BoundaryFix>,
    /// `Some` only when the tail needed padding and [`TailPolicy::Pad`] was given.
    pub tail_padding_frames: Option<u64>,
    /// True when every file in the set would report [`crate::analysis::Sbe::Aligned`] after this
    /// plan is applied (a padded tail counts as aligned by construction).
    pub fully_fixed: bool,
}

impl FixPlan {
    /// Frames file `i` gains from (positive) or lends to (negative) the file before it.
    pub fn shifted_in(&self, i: usize) -> i64 {
        i.checked_sub(1)
            .map_or(0, |prev| self.boundaries[prev].shifted_frames)
    }

    /// Frames file `i` hands to (positive) or borrows from (negative) the file after it.
    pub fn shifted_out(&self, i: usize) -> i64 {
        self.boundaries.get(i).map_or(0, |b| b.shifted_frames)
    }

    /// Whether applying the plan changes file `i` at all: frames cross one of its edges,
    /// or it is the tail and gets padded.
    pub fn touches(&self, i: usize) -> bool {
        self.shifted_in(i) != 0
            || self.shifted_out(i) != 0
            || (i == self.boundaries.len() && self.tail_padding_frames.is_some())
    }
}

/// Compute a [`FixPlan`] for `files`, taken in the order given — filename order, the same
/// order [`crate::scan::scan`] already returns.
///
/// Refuses the set if any file is not CD audio, or its length is not stated in its header:
/// repair has nothing to align for a file with no sector concept, the same case
/// [`crate::analysis::sbe`] reports as `NotApplicable`.
pub fn plan_fix(
    files: &[AudioFile],
    direction: BoundaryDirection,
    tail: TailPolicy,
) -> Result<FixPlan> {
    if files.is_empty() {
        return Err(Error::malformed(
            "<empty set>",
            "no files to plan a fix for",
        ));
    }
    let sector = FRAMES_PER_SECTOR as i64;

    // Running frame count per file, adjusted as boundaries to its left are decided —
    // fixing boundary N changes how many frames file N+1 starts with, which changes what
    // boundary N+1 needs (docs/sbe-repair.md §1).
    let mut current: Vec<i64> = Vec::with_capacity(files.len());
    for f in files {
        if !f.stream_info.is_cdda() {
            return Err(Error::malformed(
                &f.path,
                "not CD audio (44.1 kHz / 16-bit / stereo); sbe fix has nothing to align",
            ));
        }
        let Some(frames) = f.stream_info.total_frames else {
            return Err(Error::malformed(
                &f.path,
                "frame count unknown; sbe fix has nothing to align",
            ));
        };
        current.push(frames as i64);
    }

    let mut boundaries = Vec::with_capacity(files.len().saturating_sub(1));
    for i in 0..current.len().saturating_sub(1) {
        let remainder = current[i].rem_euclid(sector);
        let shifted = if remainder == 0 {
            0
        } else {
            match direction {
                BoundaryDirection::Backward => remainder,
                BoundaryDirection::Forward => -(sector - remainder),
                BoundaryDirection::Nearest => {
                    let forward = sector - remainder;
                    if forward < remainder {
                        -forward
                    } else {
                        remainder
                    }
                }
            }
        };
        if shifted < 0 && current[i + 1] + shifted < 0 {
            return Err(Error::malformed(
                &files[i + 1].path,
                format!(
                    "too short to lend {} frames to the previous file's boundary fix",
                    -shifted
                ),
            ));
        }
        current[i] -= shifted;
        current[i + 1] += shifted;
        boundaries.push(BoundaryFix {
            index: i,
            shifted_frames: shifted,
        });
    }

    let tail_frames = *current.last().expect("checked non-empty above");
    let tail_remainder = tail_frames.rem_euclid(sector) as u64;
    let (tail_padding_frames, fully_fixed) = match (tail_remainder, tail) {
        (0, _) => (None, true),
        (r, TailPolicy::Pad) => (Some(FRAMES_PER_SECTOR - r), true),
        (_, TailPolicy::Report) => (None, false),
    };

    Ok(FixPlan {
        boundaries,
        tail_padding_frames,
        fully_fixed,
    })
}

/// One committed repair.
#[derive(Debug, Clone)]
pub struct Fixed {
    pub path: PathBuf,
    /// Frames gained from the previous file's tail in this call. Always `0` here:
    /// [`execute_single_boundary`] only ever sees one boundary, so the earlier file of the
    /// pair has no "previous" and the later file's `shifted_out` is likewise always `0`.
    /// Chaining several boundaries (R3) is what makes both fields carry real values.
    pub shifted_in: i64,
    /// Frames given to the next file's head in this call.
    pub shifted_out: i64,
    pub audio_md5: [u8; 16],
    pub provenance: Provenance,
}

/// What repair encodes with — the same knobs [`convert::to_flac`] takes, bundled once
/// because every boundary in a fix shares one reference binary and one set of options.
/// A set of WAVs is never encoded, so `flac` and `opts` only matter for FLAC, and `flac`
/// may be `None` for WAV; `overwrite` applies to both.
pub struct RepairEncode<'a> {
    pub flac: Option<&'a Tool>,
    pub opts: &'a EncodeOpts,
    pub overwrite: bool,
}

/// Where one file of a fix has got to, reported to [`execute_fix`]'s and [`fix_in_place`]'s
/// `on_step` as each starts — the whole run takes minutes on a real show, and a caller
/// showing one row per file wants to move each row along as it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixStep {
    /// Its audio is being read into memory (FLAC), or its chunks read (WAV).
    Decoding,
    /// Its shifted audio is being encoded by the reference `flac` binary (FLAC), or
    /// written (WAV).
    Encoding,
    /// Its output is being decoded (FLAC) or read (WAV) again for the round-trip check.
    Checking,
    /// The round-trip check passed for it and every file it trades frames with; it waits
    /// only on the final commit.
    Checked,
}

impl FixStep {
    /// Lowercase, for a status line or a table cell.
    pub fn label(self) -> &'static str {
        match self {
            FixStep::Decoding => "decoding",
            FixStep::Encoding => "encoding",
            FixStep::Checking => "checking",
            FixStep::Checked => "checked",
        }
    }
}

/// The one format a set of files can be fixed as: every file FLAC, or every file WAV. A set
/// mixing the two is refused (convert first), and so is any other format — there is no
/// repair path for it.
pub fn set_format(files: &[AudioFile]) -> Result<AudioFormat> {
    let first = files
        .first()
        .ok_or_else(|| Error::malformed("<set>", "no files to fix"))?;
    if !matches!(first.format, AudioFormat::Flac | AudioFormat::Wav) {
        return Err(Error::malformed(
            &first.path,
            format!(
                "sbe fix can only execute against FLAC or WAV, and this is {}",
                first.format
            ),
        ));
    }
    if let Some(f) = files.iter().find(|f| f.format != first.format) {
        return Err(Error::malformed(
            &f.path,
            format!(
                "is {} but {} is {}; sbe fix takes a set of one format — convert first",
                f.format,
                first.file_name(),
                first.format
            ),
        ));
    }
    Ok(first.format)
}

/// Execute a whole [`FixPlan`] over an ordered set of files (R3): apply every boundary
/// shift left to right, chaining as each one changes what the next file starts with, add
/// the tail's silence when [`FixPlan::tail_padding_frames`] asks for it, re-encode every
/// file through the reference `flac` binary, restore each file's original Vorbis comments,
/// verify the round-trip PCM invariant across the *whole* set (docs/sbe-repair.md §1, §5,
/// excluding a padded tail's added silence from the comparison), and commit every output
/// atomically — all of it or none ([`output::commit_all`]).
///
/// `files` and `dsts` must be the same length, in the same order `plan` was computed for by
/// [`plan_fix`] — this does not re-derive the plan, it only executes the one given. `dsts`
/// are never any `files[i].path` itself: repair produces new files the same way `convert`
/// does (Principle 1). A set of exactly one file is legal — no boundaries, tail padding
/// only. `on_step` hears each file's index into `files` as it reaches each [`FixStep`] —
/// from several threads at once, since files are decoded, encoded and checked in parallel.
///
/// A set of WAVs (R5) takes the same steps with the decode and encode taken out: each
/// output's `data` is copied straight out of the sources' `data` chunks, cut at the planned
/// lengths, around the file's own other chunks kept verbatim; the invariant is the MD5 of
/// every `data` chunk laid end to end. See [`set_format`] for what a set may hold.
pub fn execute_fix(
    files: &[AudioFile],
    plan: &FixPlan,
    dsts: &[PathBuf],
    encode: &RepairEncode,
    on_step: &(dyn Fn(usize, FixStep) + Sync),
) -> Result<Vec<Fixed>> {
    if files.is_empty() {
        return Err(Error::malformed("<set>", "no files to fix"));
    }
    if files.len() != dsts.len() {
        return Err(Error::malformed(
            "<set>",
            "files and destinations must be the same length",
        ));
    }
    if plan.boundaries.len() != files.len() - 1 {
        return Err(Error::malformed(
            "<set>",
            "this plan was not computed for this file set",
        ));
    }
    for f in files {
        if !f.stream_info.is_cdda() {
            return Err(Error::malformed(
                &f.path,
                "not CD audio (44.1 kHz / 16-bit / stereo); sbe fix has nothing to align",
            ));
        }
    }
    // Its own pool rather than rayon's global one: a caller running this as a job on a
    // `Queue` of one worker would otherwise have every `par_iter` below run on that one
    // worker's pool, one file at a time.
    let format = set_format(files)?;
    let flac = match format {
        AudioFormat::Flac => Some(encode.flac.ok_or_else(|| {
            Error::malformed(
                &files[0].path,
                "fixing FLAC needs the reference flac binary",
            )
        })?),
        _ => None,
    };
    let pool = rayon::ThreadPoolBuilder::new()
        .build()
        .map_err(|e| Error::malformed("<set>", format!("starting worker threads: {e}")))?;
    pool.install(|| match flac {
        Some(flac) => execute_flac_fix(files, plan, dsts, flac, encode, on_step),
        None => execute_wav_fix(files, plan, dsts, encode.overwrite, on_step),
    })
}

fn execute_flac_fix(
    files: &[AudioFile],
    plan: &FixPlan,
    dsts: &[PathBuf],
    flac: &Tool,
    encode: &RepairEncode,
    on_step: &(dyn Fn(usize, FixStep) + Sync),
) -> Result<Vec<Fixed>> {
    let channels = files[0].stream_info.channels as usize;
    let bits_per_sample = files[0].stream_info.bits_per_sample;

    // §4 step 5c: tags are read before anything else touches any file.
    let tags = files
        .iter()
        .map(|f| tag::read_comment_block(&f.path))
        .collect::<Result<Vec<_>>>()?;

    // §4 step 5a.
    let mut buffers = files
        .par_iter()
        .enumerate()
        .map(|(i, f)| {
            on_step(i, FixStep::Decoding);
            Ok(format::flac::decode_to_samples(&f.path)?.1)
        })
        .collect::<Result<Vec<Vec<i32>>>>()?;

    // The whole-set invariant's "before": every file's audio, concatenated, as it is now —
    // untouched by anything below.
    let before_md5 = {
        let refs: Vec<&[i32]> = buffers.iter().map(Vec::as_slice).collect();
        format::flac::concatenated_pcm_md5(&refs, bits_per_sample)
    };

    // §4 step 5b, left to right — fixing boundary i changes how many frames file i+1
    // starts with, which is exactly what boundary i+1's own shift already accounts for
    // (plan_fix computed it that way), so applying them in order is what makes this correct.
    for b in &plan.boundaries {
        let i = b.index;
        if b.shifted_frames > 0 && b.shifted_frames as usize * channels > buffers[i].len() {
            return Err(Error::malformed(
                &files[i].path,
                format!(
                    "cannot shift {} frames off a file whose decoded audio is shorter than \
                     its header claims",
                    b.shifted_frames
                ),
            ));
        }
        if b.shifted_frames < 0 && (-b.shifted_frames) as usize * channels > buffers[i + 1].len() {
            return Err(Error::malformed(
                &files[i + 1].path,
                format!(
                    "cannot borrow {} frames from a file whose decoded audio is shorter than \
                     its header claims",
                    -b.shifted_frames
                ),
            ));
        }
        let (left, right) = buffers.split_at_mut(i + 1);
        move_boundary(&mut left[i], &mut right[0], b.shifted_frames, channels);
    }

    // The one operation here that adds samples instead of reassigning them (§1).
    if let Some(pad) = plan.tail_padding_frames {
        let last = buffers.last_mut().expect("checked non-empty above");
        last.extend(std::iter::repeat_n(0i32, pad as usize * channels));
    }

    // §4 step 5d: re-encode every buffer through the reference binary. Staged, not yet
    // committed.
    // Each file is its own `flac` process, so they run side by side.
    let staged = (0..files.len())
        .into_par_iter()
        .map(|i| {
            on_step(i, FixStep::Encoding);
            let scratch = write_scratch_wav(&dsts[i], &buffers[i], &files[i].stream_info)?;
            let (temp, prov, _, _) = convert::encode_flac_staged(
                scratch.path(),
                &dsts[i],
                flac,
                encode.opts,
                encode.overwrite,
                &mut || true,
            )?;
            // §4 step 5e: restore tags on the staged file, still before commit.
            tag::restore_comment_block(temp.path(), &tags[i])?;
            Ok((temp, prov))
        })
        .collect::<Result<Vec<_>>>()?;
    // The shifted audio is all on disk now; don't hold it through the check as well.
    drop(buffers);
    let (temps, provenance): (Vec<_>, Vec<_>) = staged.into_iter().unzip();

    // §4 step 6 / §5: decode what was actually staged and compare against the "before" —
    // this is the whole correctness argument, and it catches an encode-side mistake that
    // aligning-per-`sbe()` alone would not: wrong output that happens to land on a sector.
    let mut checked = temps
        .par_iter()
        .enumerate()
        .map(|(i, t)| {
            on_step(i, FixStep::Checking);
            Ok(format::flac::decode_to_samples(t.path())?.1)
        })
        .collect::<Result<Vec<Vec<i32>>>>()?;
    // Each output's own audio MD5, from the audio just decoded out of it — padding and all,
    // so before the tail's silence is cut off for the invariant below.
    let audio_md5s: Vec<[u8; 16]> = checked
        .par_iter()
        .map(|c| format::flac::concatenated_pcm_md5(&[c], bits_per_sample))
        .collect();
    if let Some(pad) = plan.tail_padding_frames {
        let last = checked.last_mut().expect("checked non-empty above");
        let cut = pad as usize * channels;
        last.truncate(last.len().saturating_sub(cut));
    }
    let after_md5 = {
        let refs: Vec<&[i32]> = checked.iter().map(Vec::as_slice).collect();
        format::flac::concatenated_pcm_md5(&refs, bits_per_sample)
    };
    if after_md5 != before_md5 {
        return Err(Error::malformed(
            &dsts[0],
            format!(
                "round-trip audio MD5 mismatch after repair (before {}, after {}); \
                 nothing was committed",
                hex::encode(before_md5),
                hex::encode(after_md5)
            ),
        ));
    }
    for i in 0..files.len() {
        on_step(i, FixStep::Checked);
    }

    // §4 step 7: all or nothing.
    let committed = output::commit_all(temps)?;

    Ok((0..files.len())
        .map(|i| Fixed {
            path: committed[i].clone(),
            shifted_in: plan.shifted_in(i),
            shifted_out: plan.shifted_out(i),
            audio_md5: audio_md5s[i],
            provenance: provenance[i].clone(),
        })
        .collect())
}

/// [`execute_fix`] for exactly one boundary between two files — the R2 case, kept as its
/// own entry point because "one boundary between two adjacent files" is the common shape
/// callers reach for without building a whole-set [`FixPlan`] by hand.
///
/// `a` and `b` must be adjacent files from a [`plan_fix`]'d set — CD audio, `shifted_frames`
/// no larger than what either file actually has to give. `dst_a`/`dst_b` are never `a.path`
/// /`b.path` themselves (Principle 1).
pub fn execute_single_boundary(
    a: &AudioFile,
    b: &AudioFile,
    shifted_frames: i64,
    dst_a: &Path,
    dst_b: &Path,
    encode: &RepairEncode,
) -> Result<(Fixed, Fixed)> {
    let plan = FixPlan {
        boundaries: vec![BoundaryFix {
            index: 0,
            shifted_frames,
        }],
        tail_padding_frames: None,
        fully_fixed: false,
    };
    let files = [a.clone(), b.clone()];
    let dsts = [dst_a.to_path_buf(), dst_b.to_path_buf()];
    let mut fixed = execute_fix(&files, &plan, &dsts, encode, &|_, _| {})?;
    let fixed_b = fixed.pop().expect("execute_fix returns one Fixed per file");
    let fixed_a = fixed.pop().expect("execute_fix returns one Fixed per file");
    Ok((fixed_a, fixed_b))
}

/// Where [`fix_in_place`] sets replaced files aside, inside [`convert::ORIGINALS_DIR`]: a
/// folder of its own, so a WAV fixed in place can still be converted to FLAC with its
/// sources moved to `_original/` afterwards, without the two steps wanting the same name.
pub const SBE_FIX_ORIGINALS_DIR: &str = "sbe-fix";

/// What [`fix_in_place`] did with one file of the set.
#[derive(Debug, Clone)]
pub enum InPlace {
    /// Rewritten under its own name; the file it replaced now sits at `original`, inside
    /// `_original/sbe-fix/` ([`SBE_FIX_ORIGINALS_DIR`]).
    Replaced {
        fixed: Box<Fixed>,
        original: PathBuf,
    },
    /// The plan moves nothing across either of its edges and pads nothing, so it was left
    /// exactly as it was — not re-encoded, not moved.
    Unchanged { path: PathBuf },
}

/// [`execute_fix`] for a show folder that should end up holding the fixed files under their
/// own names, the way `convert --move-sources` leaves a folder holding its FLACs: every file
/// the plan changes is replaced, and the file it replaced moves into `_original/sbe-fix/`
/// beside it ([`SBE_FIX_ORIGINALS_DIR`]; Principle 1: moved, never deleted). A file the plan
/// leaves alone is not touched at all.
///
/// The fixes are encoded, tag-restored and checked against the round-trip invariant into a
/// hidden staging folder inside the show folder first, exactly as [`execute_fix`] does for
/// any destination, so nothing in the folder changes until every changed file has a checked
/// replacement. Only then are the originals moved aside and the replacements renamed in —
/// all of them, or, if one fails, none: the ones already swapped are swapped back. Refuses
/// up front, before any encode, if `_original/sbe-fix/` already holds a file by any of the changed
/// files' names.
///
/// Every file in `files` must sit in the same folder. `on_step` is [`execute_fix`]'s, with
/// indices into the whole of `files`; a file the plan leaves alone never reaches a step.
pub fn fix_in_place(
    files: &[AudioFile],
    plan: &FixPlan,
    encode: &RepairEncode,
    on_step: &(dyn Fn(usize, FixStep) + Sync),
) -> Result<Vec<InPlace>> {
    if files.is_empty() {
        return Err(Error::malformed("<set>", "no files to fix"));
    }
    if plan.boundaries.len() != files.len() - 1 {
        return Err(Error::malformed(
            "<set>",
            "this plan was not computed for this file set",
        ));
    }
    set_format(files)?;
    let dir = files[0]
        .path
        .parent()
        .ok_or_else(|| Error::malformed(&files[0].path, "has no folder"))?;
    if let Some(f) = files.iter().find(|f| f.path.parent() != Some(dir)) {
        return Err(Error::malformed(
            &f.path,
            "not in the same folder as the rest of the set",
        ));
    }

    let originals = dir.join(convert::ORIGINALS_DIR).join(SBE_FIX_ORIGINALS_DIR);
    for f in (0..files.len())
        .filter(|&i| plan.touches(i))
        .map(|i| &files[i])
    {
        let aside = originals.join(f.path.file_name().unwrap_or_default());
        if aside.symlink_metadata().is_ok() {
            return Err(Error::OutputExists { path: aside });
        }
    }

    // Split the set at every boundary that moves nothing: no frames cross it, so the files
    // on either side are independent. Each piece the plan touches is its own `execute_fix`,
    // and a file the plan leaves alone is never decoded or re-encoded. A touched piece of
    // one file is only ever the padded tail.
    let mut pieces = Vec::new();
    let mut start = 0;
    for (i, b) in plan.boundaries.iter().enumerate() {
        if b.shifted_frames == 0 {
            pieces.push(start..=i);
            start = i + 1;
        }
    }
    pieces.push(start..=files.len() - 1);

    let staging = tempfile::Builder::new()
        .prefix(".lh-sbe-fix-")
        .tempdir_in(dir)
        .map_err(|e| Error::io(dir, e))?;
    let mut fixed: Vec<Option<Fixed>> = vec![None; files.len()];
    for piece in pieces.into_iter().filter(|p| plan.touches(*p.start())) {
        let (start, end) = (*piece.start(), *piece.end());
        let sub = FixPlan {
            boundaries: plan.boundaries[start..end]
                .iter()
                .map(|b| BoundaryFix {
                    index: b.index - start,
                    shifted_frames: b.shifted_frames,
                })
                .collect(),
            tail_padding_frames: plan.tail_padding_frames.filter(|_| end == files.len() - 1),
            fully_fixed: plan.fully_fixed,
        };
        let dsts: Vec<PathBuf> = files[piece.clone()]
            .iter()
            .map(|f| staging.path().join(f.path.file_name().unwrap_or_default()))
            .collect();
        // The piece's own edges are zero-shift boundaries, so what `execute_fix` reports
        // for each file is already what the whole plan says.
        let run = execute_fix(&files[piece.clone()], &sub, &dsts, encode, &|k, step| {
            on_step(start + k, step)
        })?;
        for (slot, f) in fixed[piece].iter_mut().zip(run) {
            *slot = Some(f);
        }
    }

    // Swap every checked replacement in, or none.
    let mut done: Vec<InPlace> = Vec::with_capacity(files.len());
    for (f, fx) in files.iter().zip(fixed) {
        let Some(mut fx) = fx else {
            done.push(InPlace::Unchanged {
                path: f.path.clone(),
            });
            continue;
        };
        let swapped = convert::move_aside(&f.path, &originals).and_then(|original| {
            std::fs::rename(&fx.path, &f.path)
                .map(|()| original.clone())
                .map_err(|e| {
                    let _ = std::fs::rename(&original, &f.path);
                    Error::io(&f.path, e)
                })
        });
        match swapped {
            Ok(original) => {
                fx.path = f.path.clone();
                done.push(InPlace::Replaced {
                    fixed: Box::new(fx),
                    original,
                });
            }
            Err(e) => {
                for d in done.into_iter().rev() {
                    if let InPlace::Replaced { fixed, original } = d {
                        let _ = std::fs::remove_file(&fixed.path);
                        let _ = std::fs::rename(&original, &fixed.path);
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(done)
}

/// [`execute_fix`] on a set of WAVs. Nothing is decoded: a frame is `bytes_per_frame` bytes
/// of a `data` chunk, so the fixed set's `data`, laid end to end, is the original set's cut
/// at the new lengths — plus the tail's silence, which is zero bytes.
fn execute_wav_fix(
    files: &[AudioFile],
    plan: &FixPlan,
    dsts: &[PathBuf],
    overwrite: bool,
    on_step: &(dyn Fn(usize, FixStep) + Sync),
) -> Result<Vec<Fixed>> {
    let bytes_per_frame = u64::from(files[0].stream_info.bytes_per_frame());
    let sources = files
        .par_iter()
        .enumerate()
        .map(|(i, f)| {
            on_step(i, FixStep::Decoding);
            let chunks = wav::read_chunks(&f.path)?;
            if chunks.layout.data_len % bytes_per_frame != 0 {
                return Err(Error::malformed(
                    &f.path,
                    format!(
                        "its data chunk ({} bytes) is not a whole number of {bytes_per_frame}-byte \
                         frames",
                        chunks.layout.data_len
                    ),
                ));
            }
            Ok(chunks)
        })
        .collect::<Result<Vec<_>>>()?;

    let src_lens: Vec<u64> = sources.iter().map(|c| c.layout.data_len).collect();
    let mut new_lens = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let frames =
            (src_lens[i] / bytes_per_frame) as i64 + plan.shifted_in(i) - plan.shifted_out(i);
        if frames < 0 {
            return Err(Error::malformed(
                &f.path,
                "cannot shift more frames across its edges than its data chunk holds; its \
                 header claims more audio than it has",
            ));
        }
        new_lens.push(frames as u64 * bytes_per_frame);
    }
    let ranges = data_ranges(&src_lens, &new_lens);
    let pad_len = plan.tail_padding_frames.unwrap_or(0) * bytes_per_frame;
    let last = files.len() - 1;

    // The invariant's "before", hashed straight from the source files and independently of
    // `data_ranges` — the arithmetic the check is there to catch a mistake in — while the
    // outputs are written beside it.
    let (before_md5, staged) = rayon::join(
        || -> Result<[u8; 16]> {
            let mut hasher = Md5::new();
            for (f, c) in files.iter().zip(&sources) {
                wav::read_range(&f.path, c.layout.data_offset, c.layout.data_len, |b| {
                    hasher.update(b);
                    Ok(())
                })?;
            }
            Ok(hasher.finalize().into())
        },
        || {
            (0..files.len())
                .into_par_iter()
                .map(|i| {
                    on_step(i, FixStep::Encoding);
                    let temp = TempOutput::stage(&files[i].path, &dsts[i], overwrite)?;
                    let pad = if i == last { pad_len } else { 0 };
                    wav::write_chunks(temp.path(), &sources[i], new_lens[i] + pad, |w| {
                        for (j, range) in &ranges[i] {
                            let c = &sources[*j].layout;
                            wav::read_range(
                                &files[*j].path,
                                c.data_offset + range.start,
                                range.end - range.start,
                                |b| w.write_all(b).map_err(|e| Error::io(temp.path(), e)),
                            )?;
                        }
                        std::io::copy(&mut std::io::repeat(0).take(pad), w)
                            .map_err(|e| Error::io(temp.path(), e))?;
                        Ok(())
                    })?;
                    Ok(temp)
                })
                .collect::<Result<Vec<_>>>()
        },
    );
    let (before_md5, temps) = (before_md5?, staged?);

    // The check: read back what was staged — its chunks, and its `data` into both its own
    // MD5 and the whole set's, the tail's silence left out of the latter.
    let mut after = Md5::new();
    let mut audio_md5s = Vec::with_capacity(files.len());
    for (i, t) in temps.iter().enumerate() {
        on_step(i, FixStep::Checking);
        let staged = wav::read_chunks(t.path())?;
        if !staged.same_chunks_besides_data(&sources[i]) {
            return Err(Error::malformed(
                &dsts[i],
                "its chunks besides data differ from the original's after repair; nothing \
                 was committed",
            ));
        }
        let pad = if i == last { pad_len } else { 0 };
        let keep = staged.layout.data_len.saturating_sub(pad);
        let mut own = Md5::new();
        let mut seen = 0u64;
        wav::read_range(
            t.path(),
            staged.layout.data_offset,
            staged.layout.data_len,
            |b| {
                own.update(b);
                let into_set = (keep.saturating_sub(seen) as usize).min(b.len());
                after.update(&b[..into_set]);
                seen += b.len() as u64;
                Ok(())
            },
        )?;
        audio_md5s.push(<[u8; 16]>::from(own.finalize()));
    }
    let after_md5: [u8; 16] = after.finalize().into();
    if after_md5 != before_md5 {
        return Err(Error::malformed(
            &dsts[0],
            format!(
                "round-trip audio MD5 mismatch after repair (before {}, after {}); \
                 nothing was committed",
                hex::encode(before_md5),
                hex::encode(after_md5)
            ),
        ));
    }
    for i in 0..files.len() {
        on_step(i, FixStep::Checked);
    }

    let committed = output::commit_all(temps)?;
    Ok((0..files.len())
        .map(|i| Fixed {
            path: committed[i].clone(),
            shifted_in: plan.shifted_in(i),
            shifted_out: plan.shifted_out(i),
            audio_md5: audio_md5s[i],
            provenance: Provenance {
                operation: "SBE fix (WAV)".into(),
                agent: Agent::in_process(),
                input: files[i].path.clone(),
                output: committed[i].clone(),
            },
        })
        .collect())
}

/// Where each output's `data` comes from: byte ranges of the sources' `data`, as
/// `(source index, range)`, in order. Laid end to end, output after output, they are every
/// source's `data` laid end to end — cut at `new_lens` instead of `src_lens`. Both must add
/// up to the same total; a range may reach past the next source when a short file hands on
/// frames it was itself handed.
fn data_ranges(src_lens: &[u64], new_lens: &[u64]) -> Vec<Vec<(usize, Range<u64>)>> {
    debug_assert_eq!(src_lens.iter().sum::<u64>(), new_lens.iter().sum::<u64>());
    let (mut j, mut at) = (0usize, 0u64);
    new_lens
        .iter()
        .map(|&len| {
            let mut out = Vec::new();
            let mut need = len;
            while need > 0 {
                let left = src_lens[j] - at;
                if left == 0 {
                    (j, at) = (j + 1, 0);
                    continue;
                }
                let take = need.min(left);
                out.push((j, at..at + take));
                at += take;
                need -= take;
            }
            out
        })
        .collect()
}

/// Move `shifted_frames` (see [`BoundaryFix`]'s sign convention) between two interleaved
/// sample buffers, `channels` values per frame. Callers check feasibility first — this
/// assumes it.
fn move_boundary(a: &mut Vec<i32>, b: &mut Vec<i32>, shifted_frames: i64, channels: usize) {
    match shifted_frames.cmp(&0) {
        std::cmp::Ordering::Equal => {}
        std::cmp::Ordering::Greater => {
            let n = shifted_frames as usize * channels;
            let tail = a.split_off(a.len() - n);
            b.splice(0..0, tail);
        }
        std::cmp::Ordering::Less => {
            let n = (-shifted_frames) as usize * channels;
            let head: Vec<i32> = b.drain(..n).collect();
            a.extend(head);
        }
    }
}

/// A WAV written purely to hand the reference `flac` binary something to encode — never a
/// destination itself, so it is removed once it has been read, success or failure.
struct ScratchWav(PathBuf);

impl ScratchWav {
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchWav {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn write_scratch_wav(dst_hint: &Path, samples: &[i32], info: &StreamInfo) -> Result<ScratchWav> {
    let name = dst_hint.file_name().unwrap_or_default().to_string_lossy();
    let path = dst_hint.with_file_name(format!(".{name}.lh-fix-{}.wav", std::process::id()));
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let file = File::create(&path).map_err(|e| Error::io(&path, e))?;
    let mut writer = WavWriter::new(BufWriter::new(file), info).map_err(|e| Error::io(&path, e))?;
    writer
        .write_samples(samples)
        .map_err(|e| Error::io(&path, e))?;
    writer.finish().map_err(|e| Error::io(&path, e))?;
    Ok(ScratchWav(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{AudioFormat, StreamInfo};
    use std::path::PathBuf;

    fn file(name: &str, frames: u64) -> AudioFile {
        AudioFile {
            path: PathBuf::from(name),
            format: AudioFormat::Flac,
            file_size: 0,
            stream_info: StreamInfo {
                sample_rate: 44_100,
                channels: 2,
                bits_per_sample: 16,
                total_frames: Some(frames),
                audio_md5: None,
            },
            encoder: None,
        }
    }

    #[test]
    fn data_ranges_cut_the_concatenation_at_the_new_lengths() {
        // 3 bytes handed forward out of the first file, 2 borrowed back from the third.
        assert_eq!(
            data_ranges(&[10, 5, 8], &[7, 10, 6]),
            vec![
                vec![(0, 0..7)],
                vec![(0, 7..10), (1, 0..5), (2, 0..2)],
                vec![(2, 2..8)],
            ]
        );
        // A file handed on in full, and an empty one skipped over.
        assert_eq!(
            data_ranges(&[4, 0, 2, 6], &[0, 0, 8, 4]),
            vec![
                vec![],
                vec![],
                vec![(0, 0..4), (2, 0..2), (3, 0..2)],
                vec![(3, 2..6)]
            ]
        );
    }

    #[test]
    fn set_format_refuses_a_mixed_set() {
        let mut wav = file("t02.wav", 588);
        wav.format = AudioFormat::Wav;
        assert_eq!(
            set_format(std::slice::from_ref(&wav)).unwrap(),
            AudioFormat::Wav
        );
        let err = set_format(&[file("t01.flac", 588), wav]).unwrap_err();
        assert!(err.to_string().contains("convert first"), "{err}");
    }

    #[test]
    fn empty_set_is_refused() {
        assert!(plan_fix(&[], BoundaryDirection::Backward, TailPolicy::Report).is_err());
    }

    #[test]
    fn non_cdda_member_is_refused() {
        let mut f = file("t01.flac", 588);
        f.stream_info.sample_rate = 48_000;
        let err = plan_fix(&[f], BoundaryDirection::Backward, TailPolicy::Report).unwrap_err();
        assert!(err.to_string().contains("not CD audio"));
    }

    #[test]
    fn already_aligned_set_needs_nothing() {
        let files = [file("t01.flac", 588 * 10), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
        assert_eq!(
            plan.boundaries,
            vec![BoundaryFix {
                index: 0,
                shifted_frames: 0
            }]
        );
        assert_eq!(plan.tail_padding_frames, None);
        assert!(plan.fully_fixed);
    }

    #[test]
    fn single_boundary_backward_moves_the_remainder() {
        // t01 is 3 frames past a sector; backward hands those 3 frames to t02.
        let files = [file("t01.flac", 588 * 10 + 3), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
        assert_eq!(
            plan.boundaries,
            vec![BoundaryFix {
                index: 0,
                shifted_frames: 3
            }]
        );
        // t02 now carries 3 extra frames, so it is 3 short of its own next sector.
        assert_eq!(plan.tail_padding_frames, None);
        assert!(!plan.fully_fixed);
    }

    #[test]
    fn single_boundary_forward_borrows_from_the_next_file() {
        let files = [file("t01.flac", 588 * 10 + 3), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Forward, TailPolicy::Report).unwrap();
        // t01 needs 585 more frames to reach the next sector up.
        assert_eq!(
            plan.boundaries,
            vec![BoundaryFix {
                index: 0,
                shifted_frames: -585
            }]
        );
        assert!(!plan.fully_fixed); // t02 lost 585 frames, now short by 585 of its own.
    }

    #[test]
    fn nearest_picks_the_smaller_shift() {
        // Remainder of 3 (out of 588): backward moves 3, forward would move 585 — nearest
        // picks backward.
        let files = [file("t01.flac", 588 * 10 + 3), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Nearest, TailPolicy::Report).unwrap();
        assert_eq!(plan.boundaries[0].shifted_frames, 3);

        // Remainder of 585: backward would move 585, forward moves only 3 — nearest picks
        // forward.
        let files = [file("t01.flac", 588 * 10 + 585), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Nearest, TailPolicy::Report).unwrap();
        assert_eq!(plan.boundaries[0].shifted_frames, -3);
    }

    #[test]
    fn chained_boundaries_carry_the_remainder_forward() {
        // t01 is 3 over; backward hands 3 frames to t02, which was itself exactly aligned
        // and is now 3 over in turn, so boundary 2 has to move those same 3 frames again.
        let files = [
            file("t01.flac", 588 * 10 + 3),
            file("t02.flac", 588 * 20),
            file("t03.flac", 588 * 5),
        ];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
        assert_eq!(
            plan.boundaries,
            vec![
                BoundaryFix {
                    index: 0,
                    shifted_frames: 3
                },
                BoundaryFix {
                    index: 1,
                    shifted_frames: 3
                },
            ]
        );
        assert_eq!(plan.tail_padding_frames, None);
        assert!(!plan.fully_fixed); // t03 now carries the 3 frames, still misaligned.
    }

    #[test]
    fn misaligned_tail_is_reported_not_padded_by_default() {
        let files = [file("t01.flac", 588 * 10 + 141)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
        assert!(plan.boundaries.is_empty());
        assert_eq!(plan.tail_padding_frames, None);
        assert!(!plan.fully_fixed);
    }

    #[test]
    fn pad_tail_closes_the_gap() {
        let files = [file("t01.flac", 588 * 10 + 141)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Pad).unwrap();
        assert_eq!(plan.tail_padding_frames, Some(588 - 141));
        assert!(plan.fully_fixed);
    }

    #[test]
    fn aligned_tail_is_never_padded_even_when_pad_is_requested() {
        let files = [file("t01.flac", 588 * 10)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Pad).unwrap();
        assert_eq!(plan.tail_padding_frames, None);
        assert!(plan.fully_fixed);
    }

    fn dummy_tool() -> Tool {
        Tool {
            id: crate::tools::ToolId::Flac,
            path: PathBuf::from("flac"),
            source: crate::tools::ToolSource::Path,
            version: "test".into(),
            sha256: String::new(),
        }
    }

    /// `execute_fix`'s own shape checks run before any file is touched, so they are
    /// testable without a real `flac` binary or real audio on disk.
    #[test]
    fn execute_fix_refuses_a_destination_count_that_does_not_match() {
        let files = [file("t01.flac", 588 * 10), file("t02.flac", 588 * 20)];
        let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
        let tool = dummy_tool();
        let opts = EncodeOpts::default();
        let encode = RepairEncode {
            flac: Some(&tool),
            opts: &opts,
            overwrite: false,
        };
        let dsts = [PathBuf::from("out/t01.flac")]; // one destination, two files
        let err = execute_fix(&files, &plan, &dsts, &encode, &|_, _| {}).unwrap_err();
        assert!(err.to_string().contains("same length"), "{err}");
    }

    #[test]
    fn execute_fix_refuses_a_plan_computed_for_a_different_file_count() {
        let files = [file("t01.flac", 588 * 10), file("t02.flac", 588 * 20)];
        // A plan for three files, deliberately mismatched to this two-file set.
        let plan = FixPlan {
            boundaries: vec![
                BoundaryFix {
                    index: 0,
                    shifted_frames: 0,
                },
                BoundaryFix {
                    index: 1,
                    shifted_frames: 0,
                },
            ],
            tail_padding_frames: None,
            fully_fixed: true,
        };
        let tool = dummy_tool();
        let opts = EncodeOpts::default();
        let encode = RepairEncode {
            flac: Some(&tool),
            opts: &opts,
            overwrite: false,
        };
        let dsts = [PathBuf::from("out/t01.flac"), PathBuf::from("out/t02.flac")];
        let err = execute_fix(&files, &plan, &dsts, &encode, &|_, _| {}).unwrap_err();
        assert!(
            err.to_string().contains("not computed for this file set"),
            "{err}"
        );
    }

    #[test]
    fn execute_fix_refuses_an_empty_set() {
        let plan = FixPlan {
            boundaries: vec![],
            tail_padding_frames: None,
            fully_fixed: true,
        };
        let tool = dummy_tool();
        let opts = EncodeOpts::default();
        let encode = RepairEncode {
            flac: Some(&tool),
            opts: &opts,
            overwrite: false,
        };
        assert!(execute_fix(&[], &plan, &[], &encode, &|_, _| {}).is_err());
    }
}
