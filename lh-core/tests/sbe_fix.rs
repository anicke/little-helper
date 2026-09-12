//! R2 of docs/sbe-repair.md: executing a single boundary fix between two files. The
//! round-trip PCM invariant (§1, §5) is the correctness argument, so it is the spine of
//! every test here — plus tag survival and the atomic-commit guarantee, the two other
//! things §1/§4 call out as non-negotiable.
//!
//! Tests needing `flac` skip when it is absent, the same convention `convert.rs` uses.

use lh_core::analysis::{
    BoundaryDirection, RepairEncode, TailPolicy, execute_single_boundary, plan_fix,
};
use lh_core::convert::EncodeOpts;
use lh_core::format;
use lh_core::model::AudioFile;
use lh_core::tools::{Registry, Tool, ToolId};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

fn reference_flac() -> Option<Tool> {
    match Registry::discover_one(ToolId::Flac).require(ToolId::Flac) {
        Ok(t) => Some(t.clone()),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

fn probe(path: &Path) -> AudioFile {
    format::probe(path).unwrap_or_else(|e| panic!("probing {}: {e}", path.display()))
}

/// A misaligned track (`cdda-sbe.flac`, +137 frames past a sector) followed by an aligned
/// one (`cdda-aligned.flac`), copied into `dir` as `t01.flac`/`t02.flac` — the ordered pair
/// `plan_fix` would see for one directory.
fn ordered_pair(dir: &Path) -> (AudioFile, AudioFile) {
    let a = dir.join("t01.flac");
    let b = dir.join("t02.flac");
    std::fs::copy(fixture("cdda-sbe.flac"), &a).unwrap();
    std::fs::copy(fixture("cdda-aligned.flac"), &b).unwrap();
    (probe(&a), probe(&b))
}

#[test]
fn single_boundary_shifts_exactly_the_planned_remainder() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ordered_pair(dir.path());
    assert_eq!(a.stream_info.total_frames.unwrap() % 588, 137);

    let plan = plan_fix(&[a.clone(), b.clone()], BoundaryDirection::Backward, TailPolicy::Report)
        .unwrap();
    assert_eq!(plan.boundaries.len(), 1);
    let shifted = plan.boundaries[0].shifted_frames;
    assert_eq!(shifted, 137, "backward must move exactly the remainder");

    let out_a = dir.path().join("out/t01.flac");
    let out_b = dir.path().join("out/t02.flac");
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: false,
    };
    let (fixed_a, fixed_b) =
        execute_single_boundary(&a, &b, shifted, &out_a, &out_b, &encode).unwrap();

    assert_eq!(fixed_a.shifted_out, 137);
    assert_eq!(fixed_a.shifted_in, 0);
    assert_eq!(fixed_b.shifted_in, 137);
    assert_eq!(fixed_b.shifted_out, 0);
    assert!(out_a.exists());
    assert!(out_b.exists());

    let probed_a = format::probe(&out_a).unwrap();
    let probed_b = format::probe(&out_b).unwrap();
    assert_eq!(
        probed_a.stream_info.total_frames.unwrap() % 588,
        0,
        "the earlier file must now be sector-aligned"
    );
    // The later file absorbed 137 frames it did not have before.
    let orig_b_frames = probe(&fixture("cdda-aligned.flac")).stream_info.total_frames;
    assert_eq!(
        probed_b.stream_info.total_frames,
        orig_b_frames.map(|f| f + 137)
    );

    // Principle 2: still the reference encoder's own vendor string.
    assert!(
        probed_a
            .encoder
            .as_deref()
            .is_some_and(|v| v.starts_with("reference libFLAC")),
        "{:?}",
        probed_a.encoder
    );
}

/// The invariant from §1: decode the set before and after, concatenated — the two digests
/// must match. This is the test that would catch a shift in the wrong direction or an
/// off-by-one, neither of which `sbe()` reporting "aligned" afterward would notice.
#[test]
fn round_trip_pcm_is_unchanged_by_the_shift() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ordered_pair(dir.path());

    let (_, before_a) = format::flac::decode_to_samples(&a.path).unwrap();
    let (_, before_b) = format::flac::decode_to_samples(&b.path).unwrap();
    let before = format::flac::concatenated_pcm_md5(&[&before_a, &before_b], 16);

    let out_a = dir.path().join("out/t01.flac");
    let out_b = dir.path().join("out/t02.flac");
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: false,
    };
    execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap();

    let (_, after_a) = format::flac::decode_to_samples(&out_a).unwrap();
    let (_, after_b) = format::flac::decode_to_samples(&out_b).unwrap();
    let after = format::flac::concatenated_pcm_md5(&[&after_a, &after_b], 16);

    assert_eq!(before, after, "concatenated audio changed across the repair");
}

/// Repair inherits `convert`'s tag-dropping round trip unless it explicitly restores tags
/// (docs/sbe-repair.md §2) — so a file with real Vorbis comments on both sides of the
/// boundary must come out byte-identical in its tags, not just its audio.
#[test]
fn tags_survive_the_repair() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ordered_pair(dir.path());

    tag(&a.path, &[("TITLE", "First Set"), ("ARTIST", "Traders")]);
    tag(&b.path, &[("TITLE", "Second Set"), ("TRACKNUMBER", "2")]);
    // Re-probe: tagging changed the files' STREAMINFO block layout, not their audio.
    let a = probe(&a.path);
    let b = probe(&b.path);

    let out_a = dir.path().join("out/t01.flac");
    let out_b = dir.path().join("out/t02.flac");
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: false,
    };
    execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap();

    assert_eq!(read_tag(&out_a, "TITLE"), vec!["First Set"]);
    assert_eq!(read_tag(&out_a, "ARTIST"), vec!["Traders"]);
    assert_eq!(read_tag(&out_b, "TITLE"), vec!["Second Set"]);
    assert_eq!(read_tag(&out_b, "TRACKNUMBER"), vec!["2"]);
}

fn tag(path: &Path, pairs: &[(&str, &str)]) {
    let mut t = metaflac::Tag::read_from_path(path).unwrap();
    for (key, value) in pairs {
        t.set_vorbis(key.to_string(), vec![value.to_string()]);
    }
    t.save().unwrap();
}

fn read_tag(path: &Path, key: &str) -> Vec<String> {
    let t = metaflac::Tag::read_from_path(path).unwrap();
    t.vorbis_comments()
        .and_then(|vc| vc.comments.get(key))
        .cloned()
        .unwrap_or_default()
}

/// §4 step 7: the two outputs commit together or not at all. Forced here by pointing the
/// second output at a destination `TempOutput::stage` will refuse (it already exists,
/// without `overwrite`) — the first output must not survive alone either.
#[test]
fn a_failed_second_output_leaves_neither_file_committed() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ordered_pair(dir.path());

    let out_a = dir.path().join("out_a.flac");
    let out_b = dir.path().join("out_b.flac");
    std::fs::create_dir_all(dir.path().join("out_b.flac")).unwrap(); // sabotage: a directory
    // sits where the second output wants to write, so encoding b fails outright — before
    // either output is ever staged for commit, which is the cheap way to prove the first
    // one was never renamed into place on its own.

    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: false,
    };
    let err = execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap_err();
    eprintln!("expected failure: {err}");

    assert!(!out_a.exists(), "the first output must not survive alone");
}

/// A non-CD-audio member is refused outright, the same case `plan_fix` reports.
#[test]
fn non_cdda_pair_is_refused() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, mut b) = ordered_pair(dir.path());
    b.stream_info.sample_rate = 48_000;

    let out_a = dir.path().join("out/t01.flac");
    let out_b = dir.path().join("out/t02.flac");
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: &flac,
        opts: &opts,
        overwrite: false,
    };
    let err = execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap_err();
    assert!(err.to_string().contains("not CD audio"), "{err}");
    assert!(!out_a.exists());
}
