//! Planning and executing an SBE repair (docs/sbe-repair.md).
//!
//! R1 is [`plan_fix`]: pure arithmetic over each file's declared frame count — the same
//! header-only read [`super::sbe::sbe`] already does, so a plan can be shown to a user with
//! no decode.
//!
//! R2 is [`execute_single_boundary`]: decode, shift, re-encode, restore tags, and commit
//! two files at once for a single boundary. Chaining that across a whole ordered set, and
//! the tail's own `--pad-tail` policy, are later milestones (R3) — this executes one
//! [`BoundaryFix`] at a time, which already covers the common case of a single split a few
//! frames off.

use crate::convert::{self, EncodeOpts};
use crate::error::{Error, Result};
use crate::format::{self, wav::WavWriter};
use crate::model::{AudioFile, FRAMES_PER_SECTOR, StreamInfo};
use crate::output;
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
    /// True when every file in the set would report [`super::Sbe::Aligned`] after this
    /// plan is applied (a padded tail counts as aligned by construction).
    pub fully_fixed: bool,
}

/// Compute a [`FixPlan`] for `files`, taken in the order given — filename order, the same
/// order [`crate::scan::scan`] already returns.
///
/// Refuses the set if any file is not CD audio, or its length is not stated in its header:
/// repair has nothing to align for a file with no sector concept, the same case
/// [`super::sbe`] reports as `NotApplicable`.
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

/// Execute one [`BoundaryFix`] between exactly two files: decode both fully, move
/// `shifted_frames` across the split (its sign convention is [`BoundaryFix`]'s), re-encode
/// both through the reference `flac` binary, restore each file's original Vorbis comments,
/// verify the round-trip PCM invariant across the pair (docs/sbe-repair.md §1, §5), and
/// commit both outputs atomically — both or neither ([`output::commit_all`]).
///
/// `a` and `b` must be adjacent files from a [`plan_fix`]'d set — CD audio, `shifted_frames`
/// no larger than what either file actually has to give. `dst_a`/`dst_b` are never `a.path`
/// /`b.path` themselves: repair produces new files the same way `convert` does (Principle 1).
pub fn execute_single_boundary(
    a: &AudioFile,
    b: &AudioFile,
    shifted_frames: i64,
    dst_a: &Path,
    dst_b: &Path,
    encode: &RepairEncode,
) -> Result<(Fixed, Fixed)> {
    for f in [a, b] {
        if !f.stream_info.is_cdda() {
            return Err(Error::malformed(
                &f.path,
                "not CD audio (44.1 kHz / 16-bit / stereo); sbe fix has nothing to align",
            ));
        }
    }
    let channels = a.stream_info.channels as usize;
    let bits_per_sample = a.stream_info.bits_per_sample;

    // §4 step 5c: tags are read before anything else touches either file.
    let tags_a = read_vorbis_comments(&a.path)?;
    let tags_b = read_vorbis_comments(&b.path)?;

    // §4 step 5a.
    let (_, orig_a) = format::flac::decode_to_samples(&a.path)?;
    let (_, orig_b) = format::flac::decode_to_samples(&b.path)?;

    let a_frames = orig_a.len() / channels;
    let b_frames = orig_b.len() / channels;
    if shifted_frames > 0 && shifted_frames as usize > a_frames {
        return Err(Error::malformed(
            &a.path,
            format!(
                "cannot shift {shifted_frames} frames off a file that only has {a_frames}"
            ),
        ));
    }
    if shifted_frames < 0 && (-shifted_frames) as usize > b_frames {
        return Err(Error::malformed(
            &b.path,
            format!(
                "cannot borrow {} frames from a file that only has {b_frames}",
                -shifted_frames
            ),
        ));
    }

    // The whole-set invariant's "before": the two files' audio, concatenated, as they are
    // now — untouched by the shift below.
    let before_md5 = format::flac::concatenated_pcm_md5(&[&orig_a, &orig_b], bits_per_sample);

    // §4 step 5b.
    let mut shifted_a = orig_a;
    let mut shifted_b = orig_b;
    move_boundary(&mut shifted_a, &mut shifted_b, shifted_frames, channels);

    // §4 step 5d: re-encode through the reference binary. Staged, not yet committed.
    let scratch_a = write_scratch_wav(dst_a, &shifted_a, &a.stream_info)?;
    let scratch_b = write_scratch_wav(dst_b, &shifted_b, &b.stream_info)?;
    let (temp_a, prov_a, _, _) = convert::encode_flac_staged(
        scratch_a.path(),
        dst_a,
        encode.flac,
        encode.opts,
        encode.overwrite,
        &mut || true,
    )?;
    let (temp_b, prov_b, _, _) = convert::encode_flac_staged(
        scratch_b.path(),
        dst_b,
        encode.flac,
        encode.opts,
        encode.overwrite,
        &mut || true,
    )?;

    // §4 step 5e: restore tags on the staged files, still before commit.
    write_vorbis_comments(temp_a.path(), &tags_a)?;
    write_vorbis_comments(temp_b.path(), &tags_b)?;

    // §4 step 6 / §5: decode what was actually staged and compare against the "before" —
    // this is the whole correctness argument, and it catches an encode-side mistake that
    // aligning-per-`sbe()` alone would not: wrong output that happens to land on a sector.
    let (_, check_a) = format::flac::decode_to_samples(temp_a.path())?;
    let (_, check_b) = format::flac::decode_to_samples(temp_b.path())?;
    let after_md5 = format::flac::concatenated_pcm_md5(&[&check_a, &check_b], bits_per_sample);
    if after_md5 != before_md5 {
        return Err(Error::malformed(
            dst_a,
            format!(
                "round-trip audio MD5 mismatch after repair (before {}, after {}); \
                 nothing was committed",
                hex::encode(before_md5),
                hex::encode(after_md5)
            ),
        ));
    }
    let audio_md5_a = format::flac::audio_md5(temp_a.path())?;
    let audio_md5_b = format::flac::audio_md5(temp_b.path())?;

    // §4 step 7: both or neither.
    let committed = output::commit_all(vec![temp_a, temp_b])?;

    Ok((
        Fixed {
            path: committed[0].clone(),
            shifted_in: 0,
            shifted_out: shifted_frames,
            audio_md5: audio_md5_a,
            provenance: prov_a,
        },
        Fixed {
            path: committed[1].clone(),
            shifted_in: shifted_frames,
            shifted_out: 0,
            audio_md5: audio_md5_b,
            provenance: prov_b,
        },
    ))
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
    let mut writer =
        WavWriter::new(BufWriter::new(file), info).map_err(|e| Error::io(&path, e))?;
    writer
        .write_samples(samples)
        .map_err(|e| Error::io(&path, e))?;
    writer.finish().map_err(|e| Error::io(&path, e))?;
    Ok(ScratchWav(path))
}

/// `None` when the source carries no `VORBIS_COMMENT` block at all — nothing to restore,
/// as opposed to a block with zero comments in it, which is still worth writing back so a
/// custom vendor string (if the source somehow had one) round-trips too. In practice every
/// FLAC our own `convert` or `flac` itself produces has the block; this only matters for a
/// source file with no metadata block whatsoever.
fn read_vorbis_comments(path: &Path) -> Result<Option<metaflac::block::VorbisComment>> {
    let tag = metaflac::Tag::read_from_path(path).map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(tag.vorbis_comments().cloned())
}

/// Restore comment fields onto a freshly encoded FLAC, keeping the vendor string `flac`
/// just wrote — that string is Principle 2's provenance marker, not part of what repair is
/// meant to preserve from the source.
fn write_vorbis_comments(path: &Path, original: &Option<metaflac::block::VorbisComment>) -> Result<()> {
    let Some(original) = original else {
        return Ok(());
    };
    let mut tag = metaflac::Tag::read_from_path(path).map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })?;
    tag.vorbis_comments_mut().comments = original.comments.clone();
    tag.save().map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })
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
}
