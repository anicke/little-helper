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

use crate::convert::{self, EncodeOpts};
use crate::error::{Error, Result};
use crate::format::{self, wav::WavWriter};
use crate::model::{AudioFile, FRAMES_PER_SECTOR, StreamInfo};
use crate::output;
use crate::tag;
use crate::tools::{Provenance, Tool};
use std::fs::File;
use std::io::BufWriter;
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
pub struct RepairEncode<'a> {
    pub flac: &'a Tool,
    pub opts: &'a EncodeOpts,
    pub overwrite: bool,
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
/// only.
pub fn execute_fix(
    files: &[AudioFile],
    plan: &FixPlan,
    dsts: &[PathBuf],
    encode: &RepairEncode,
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
    let channels = files[0].stream_info.channels as usize;
    let bits_per_sample = files[0].stream_info.bits_per_sample;

    // §4 step 5c: tags are read before anything else touches any file.
    let tags = files
        .iter()
        .map(|f| tag::read_comment_block(&f.path))
        .collect::<Result<Vec<_>>>()?;

    // §4 step 5a.
    let mut buffers = files
        .iter()
        .map(|f| Ok(format::flac::decode_to_samples(&f.path)?.1))
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
    let mut temps = Vec::with_capacity(files.len());
    let mut provenance = Vec::with_capacity(files.len());
    for i in 0..files.len() {
        let scratch = write_scratch_wav(&dsts[i], &buffers[i], &files[i].stream_info)?;
        let (temp, prov, _, _) = convert::encode_flac_staged(
            scratch.path(),
            &dsts[i],
            encode.flac,
            encode.opts,
            encode.overwrite,
            &mut || true,
        )?;
        // §4 step 5e: restore tags on the staged file, still before commit.
        tag::restore_comment_block(temp.path(), &tags[i])?;
        temps.push(temp);
        provenance.push(prov);
    }

    // §4 step 6 / §5: decode what was actually staged and compare against the "before" —
    // this is the whole correctness argument, and it catches an encode-side mistake that
    // aligning-per-`sbe()` alone would not: wrong output that happens to land on a sector.
    let mut checked = temps
        .iter()
        .map(|t| Ok(format::flac::decode_to_samples(t.path())?.1))
        .collect::<Result<Vec<Vec<i32>>>>()?;
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
    let audio_md5s = temps
        .iter()
        .map(|t| format::flac::audio_md5(t.path()))
        .collect::<Result<Vec<_>>>()?;

    // §4 step 7: all or nothing.
    let committed = output::commit_all(temps)?;

    Ok((0..files.len())
        .map(|i| Fixed {
            path: committed[i].clone(),
            shifted_in: if i == 0 {
                0
            } else {
                plan.boundaries[i - 1].shifted_frames
            },
            shifted_out: plan
                .boundaries
                .get(i)
                .map(|b| b.shifted_frames)
                .unwrap_or(0),
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
    let mut fixed = execute_fix(&files, &plan, &dsts, encode)?;
    let fixed_b = fixed.pop().expect("execute_fix returns one Fixed per file");
    let fixed_a = fixed.pop().expect("execute_fix returns one Fixed per file");
    Ok((fixed_a, fixed_b))
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
            flac: &tool,
            opts: &opts,
            overwrite: false,
        };
        let dsts = [PathBuf::from("out/t01.flac")]; // one destination, two files
        let err = execute_fix(&files, &plan, &dsts, &encode).unwrap_err();
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
            flac: &tool,
            opts: &opts,
            overwrite: false,
        };
        let dsts = [PathBuf::from("out/t01.flac"), PathBuf::from("out/t02.flac")];
        let err = execute_fix(&files, &plan, &dsts, &encode).unwrap_err();
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
            flac: &tool,
            opts: &opts,
            overwrite: false,
        };
        assert!(execute_fix(&[], &plan, &[], &encode).is_err());
    }
}
