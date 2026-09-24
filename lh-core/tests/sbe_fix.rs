//! R2 (single boundary between two files) and R3 (a whole chained set, tail padding) of
//! docs/sbe-repair.md. The round-trip PCM invariant (§1, §5) is the correctness argument,
//! so it is the spine of every test here — plus tag survival and the atomic-commit
//! guarantee, the two other things §1/§4 call out as non-negotiable.
//!
//! Tests needing `flac` skip when it is absent, the same convention `convert.rs` uses.

use lh_core::convert::{EncodeOpts, to_flac};
use lh_core::format;
use lh_core::model::AudioFile;
use lh_core::repair::{
    BoundaryDirection, FixStep, InPlace, RepairEncode, TailPolicy, execute_fix,
    execute_single_boundary, fix_in_place, plan_fix,
};
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

/// A canonical 44.1/16/2 WAV of exactly `frames` frames, filled with deterministic noise
/// (xorshift32, seeded per file so adjacent tracks are not accidentally identical) rather
/// than silence — a bug that shifted the wrong samples would still pass a silence-only
/// invariant check.
fn synth_wav(path: &Path, frames: u64, seed: u32) {
    let channels: u32 = 2;
    let sample_rate: u32 = 44_100;
    let data_len = frames as u32 * channels * 2;

    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&(channels as u16).to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&(sample_rate * channels * 2).to_le_bytes());
    w.extend_from_slice(&((channels * 2) as u16).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());

    let mut state: u32 = seed | 1;
    for _ in 0..frames * channels as u64 {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        w.extend_from_slice(&(state as i16).to_le_bytes());
    }
    std::fs::write(path, w).unwrap();
}

/// A synthetic FLAC of exactly `frames` frames, encoded through the reference `flac`
/// binary — real audio for an R3 chain test, without depending on a fixture of exactly
/// the right length existing on disk.
fn synth_flac(flac: &Tool, dir: &Path, name: &str, frames: u64, seed: u32) -> AudioFile {
    let wav = dir.join(format!("{name}.wav"));
    synth_wav(&wav, frames, seed);
    let dst = dir.join(format!("{name}.flac"));
    to_flac(
        &wav,
        &dst,
        flac,
        &EncodeOpts::default(),
        false,
        &mut |_, _| true,
    )
    .unwrap();
    probe(&dst)
}

fn concatenated_md5_of(paths: &[PathBuf]) -> [u8; 16] {
    let bufs: Vec<Vec<i32>> = paths
        .iter()
        .map(|p| format::flac::decode_to_samples(p).unwrap().1)
        .collect();
    let refs: Vec<&[i32]> = bufs.iter().map(Vec::as_slice).collect();
    format::flac::concatenated_pcm_md5(&refs, 16)
}

#[test]
fn single_boundary_shifts_exactly_the_planned_remainder() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = ordered_pair(dir.path());
    assert_eq!(a.stream_info.total_frames.unwrap() % 588, 137);

    let plan = plan_fix(
        &[a.clone(), b.clone()],
        BoundaryDirection::Backward,
        TailPolicy::Report,
    )
    .unwrap();
    assert_eq!(plan.boundaries.len(), 1);
    let shifted = plan.boundaries[0].shifted_frames;
    assert_eq!(shifted, 137, "backward must move exactly the remainder");

    let out_a = dir.path().join("out/t01.flac");
    let out_b = dir.path().join("out/t02.flac");
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
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
    let orig_b_frames = probe(&fixture("cdda-aligned.flac"))
        .stream_info
        .total_frames;
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
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap();

    let (_, after_a) = format::flac::decode_to_samples(&out_a).unwrap();
    let (_, after_b) = format::flac::decode_to_samples(&out_b).unwrap();
    let after = format::flac::concatenated_pcm_md5(&[&after_a, &after_b], 16);

    assert_eq!(
        before, after,
        "concatenated audio changed across the repair"
    );
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
        flac: Some(&flac),
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
        flac: Some(&flac),
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
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    let err = execute_single_boundary(&a, &b, 137, &out_a, &out_b, &encode).unwrap_err();
    assert!(err.to_string().contains("not CD audio"), "{err}");
    assert!(!out_a.exists());
}

/// R3: three files where fixing boundary 0 changes what boundary 1 needs — the case that
/// makes this a left-to-right pass rather than independent per-boundary fixes (the same
/// scenario `chained_boundaries_carry_the_remainder_forward` covers for planning alone,
/// executed here for real).
#[test]
fn chained_boundaries_execute_left_to_right() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let a = synth_flac(&flac, dir.path(), "src-t01", 588 * 10 + 3, 1);
    let b = synth_flac(&flac, dir.path(), "src-t02", 588 * 20, 2);
    let c = synth_flac(&flac, dir.path(), "src-t03", 588 * 5, 3);
    let files = [a.clone(), b.clone(), c.clone()];
    let before_md5 = concatenated_md5_of(&[a.path.clone(), b.path.clone(), c.path.clone()]);

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    assert_eq!(plan.boundaries.len(), 2);
    assert_eq!(
        plan.boundaries[0].shifted_frames, 3,
        "t01 hands its +3 to t02"
    );
    assert_eq!(
        plan.boundaries[1].shifted_frames, 3,
        "t02, now +3 in turn, hands the same 3 frames on to t03"
    );
    assert!(!plan.fully_fixed, "t03 ends up +3, still misaligned");

    let out = dir.path().join("out");
    let dsts = vec![
        out.join("t01.flac"),
        out.join("t02.flac"),
        out.join("t03.flac"),
    ];
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };

    let fixed = execute_fix(&files, &plan, &dsts, &encode, &|_, _| {}).unwrap();
    assert_eq!(fixed.len(), 3);
    assert_eq!(fixed[0].shifted_in, 0);
    assert_eq!(fixed[0].shifted_out, 3);
    assert_eq!(fixed[1].shifted_in, 3);
    assert_eq!(fixed[1].shifted_out, 3);
    assert_eq!(fixed[2].shifted_in, 3);
    assert_eq!(fixed[2].shifted_out, 0);

    let probed: Vec<AudioFile> = dsts.iter().map(|d| probe(d)).collect();
    assert_eq!(probed[0].stream_info.total_frames.unwrap() % 588, 0);
    assert_eq!(probed[1].stream_info.total_frames.unwrap() % 588, 0);
    assert_eq!(
        probed[2].stream_info.total_frames.unwrap() % 588,
        3,
        "t03 carries the same remainder the chain handed it"
    );

    let after_md5 = concatenated_md5_of(&dsts);
    assert_eq!(
        before_md5, after_md5,
        "concatenated audio changed across the chained repair"
    );
}

/// R3: executing `TailPolicy::Pad` actually adds silence to close the last file's gap,
/// and the invariant check correctly excludes that added silence rather than failing on
/// it (docs/sbe-repair.md §1, §6 step 6).
#[test]
fn tail_padding_executes_and_is_excluded_from_the_invariant() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let short_by = 141;
    let a = synth_flac(&flac, dir.path(), "src-t01", 588 * 4 + short_by, 7);
    let files = [a.clone()];
    let before_md5 = concatenated_md5_of(std::slice::from_ref(&a.path));

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Pad).unwrap();
    assert!(plan.boundaries.is_empty());
    let pad = 588 - short_by;
    assert_eq!(plan.tail_padding_frames, Some(pad));
    assert!(plan.fully_fixed);

    let dsts = vec![dir.path().join("out/t01.flac")];
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    let fixed = execute_fix(&files, &plan, &dsts, &encode, &|_, _| {}).unwrap();
    assert_eq!(fixed.len(), 1);

    let probed = probe(&dsts[0]);
    assert_eq!(probed.stream_info.total_frames.unwrap() % 588, 0);
    assert_eq!(
        probed.stream_info.total_frames.unwrap(),
        a.stream_info.total_frames.unwrap() + pad,
        "the tail grew by exactly the padding, the one operation allowed to add samples"
    );

    let (_, after_samples) = format::flac::decode_to_samples(&dsts[0]).unwrap();
    let pad_samples = pad as usize * 2;
    let (original_part, silence) = after_samples.split_at(after_samples.len() - pad_samples);
    assert!(
        silence.iter().all(|&s| s == 0),
        "the padding itself must be silence"
    );
    let after_md5_excluding_pad =
        format::flac::concatenated_pcm_md5(&[original_part], probed.stream_info.bits_per_sample);
    assert_eq!(
        before_md5, after_md5_excluding_pad,
        "the original audio, excluding the padding, must be untouched"
    );
}

/// R3's own atomic-commit guarantee: with three files staged, sabotaging the last one's
/// destination must leave none of the first two committed either — extending
/// `a_failed_second_output_leaves_neither_file_committed` from two files to a real chain.
#[test]
fn a_failed_third_output_in_a_chain_leaves_nothing_committed() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let a = synth_flac(&flac, dir.path(), "src-t01", 588 * 10 + 3, 11);
    let b = synth_flac(&flac, dir.path(), "src-t02", 588 * 20, 12);
    let c = synth_flac(&flac, dir.path(), "src-t03", 588 * 5, 13);
    let files = [a, b, c];

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    let out_a = dir.path().join("out_a.flac");
    let out_b = dir.path().join("out_b.flac");
    let out_c = dir.path().join("out_c.flac");
    // Sabotage: a directory sits where the third output wants to write, so encoding it
    // fails outright, after the first two have already been staged.
    std::fs::create_dir_all(&out_c).unwrap();
    let dsts = vec![out_a.clone(), out_b.clone(), out_c];

    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    let err = execute_fix(&files, &plan, &dsts, &encode, &|_, _| {}).unwrap_err();
    eprintln!("expected failure: {err}");

    assert!(!out_a.exists(), "the first output must not survive alone");
    assert!(!out_b.exists(), "the second output must not survive alone");
}

/// In place: every file the plan changes is replaced under its own name with its original
/// moved into `_original/sbe-fix/`; a file the plan leaves alone stays byte-for-byte itself and is
/// not moved; the folder's audio, concatenated, is unchanged; no staging folder is left.
#[test]
fn in_place_replaces_changed_files_and_sets_their_originals_aside() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let a = synth_flac(&flac, dir.path(), "t01", 588 * 4, 1);
    let b = synth_flac(&flac, dir.path(), "t02", 588 * 10 + 3, 2);
    let c = synth_flac(&flac, dir.path(), "t03", 588 * 20 + 585, 3);
    let files = [a.clone(), b.clone(), c.clone()];
    let paths = [a.path.clone(), b.path.clone(), c.path.clone()];
    let before_md5 = concatenated_md5_of(&paths);
    let before = paths.each_ref().map(|p| std::fs::read(p).unwrap());

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    assert!(
        plan.fully_fixed,
        "t02 hands its +3 to t03, which lands on a sector"
    );

    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    let steps = std::sync::Mutex::new(Vec::new());
    let done = fix_in_place(&files, &plan, &encode, &|i, step| {
        steps.lock().unwrap().push((i, step))
    })
    .unwrap();
    let steps = steps.into_inner().unwrap();

    // Indices are the whole set's, and a file the plan leaves alone never reaches a step.
    for i in 1..3 {
        let mine: Vec<FixStep> = steps.iter().filter(|s| s.0 == i).map(|s| s.1).collect();
        assert_eq!(
            mine,
            [
                FixStep::Decoding,
                FixStep::Encoding,
                FixStep::Checking,
                FixStep::Checked
            ]
        );
    }
    assert!(steps.iter().all(|s| s.0 != 0), "t01 is untouched");

    assert!(matches!(&done[0], InPlace::Unchanged { path } if *path == paths[0]));
    assert_eq!(
        std::fs::read(&paths[0]).unwrap(),
        before[0],
        "t01 untouched"
    );
    let originals = dir.path().join("_original/sbe-fix");
    assert!(!originals.join("t01.flac").exists(), "t01 not moved");
    for i in 1..3 {
        let InPlace::Replaced { fixed, original } = &done[i] else {
            panic!("file {i} should be replaced");
        };
        assert_eq!(fixed.path, paths[i]);
        assert_eq!(*original, originals.join(paths[i].file_name().unwrap()));
        assert_eq!(
            std::fs::read(original).unwrap(),
            before[i],
            "original kept as is"
        );
        assert_eq!(probe(&paths[i]).stream_info.total_frames.unwrap() % 588, 0);
    }
    let InPlace::Replaced { fixed, .. } = &done[1] else {
        unreachable!()
    };
    assert_eq!((fixed.shifted_in, fixed.shifted_out), (0, 3));

    assert_eq!(concatenated_md5_of(&paths), before_md5);
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".lh-sbe-fix"))
        .collect();
    assert!(leftovers.is_empty(), "staging folder left behind");
}

/// A changed file whose name is already taken in `_original/sbe-fix/` refuses the whole fix before
/// anything is encoded or moved.
#[test]
fn in_place_refuses_when_original_is_already_taken() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let a = synth_flac(&flac, dir.path(), "t01", 588 * 10 + 3, 1);
    let b = synth_flac(&flac, dir.path(), "t02", 588 * 20 + 585, 2);
    let files = [a.clone(), b.clone()];
    let before_a = std::fs::read(&a.path).unwrap();
    let originals = dir.path().join("_original/sbe-fix");
    std::fs::create_dir_all(&originals).unwrap();
    std::fs::write(originals.join("t02.flac"), b"already here").unwrap();

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    let opts = EncodeOpts::default();
    let encode = RepairEncode {
        flac: Some(&flac),
        opts: &opts,
        overwrite: false,
    };
    let err = fix_in_place(&files, &plan, &encode, &|_, _| {}).unwrap_err();
    assert!(matches!(err, lh_core::Error::OutputExists { .. }), "{err}");
    assert_eq!(std::fs::read(&a.path).unwrap(), before_a);
    assert!(!originals.join("t01.flac").exists());
    assert_eq!(
        std::fs::read(originals.join("t02.flac")).unwrap(),
        b"already here"
    );
}

// --- R5: WAV -------------------------------------------------------------------------
//
// The same §5 list, on WAVs: no `flac` binary needed, so none of these skip.

fn no_encode() -> RepairEncode<'static> {
    static OPTS: std::sync::OnceLock<EncodeOpts> = std::sync::OnceLock::new();
    RepairEncode {
        flac: None,
        opts: OPTS.get_or_init(EncodeOpts::default),
        overwrite: false,
    }
}

fn wav_set(dir: &Path, frames: &[u64]) -> Vec<AudioFile> {
    frames
        .iter()
        .enumerate()
        .map(|(i, &n)| {
            let path = dir.join(format!("t{:02}.wav", i + 1));
            synth_wav(&path, n, 100 + i as u32);
            probe(&path)
        })
        .collect()
}

/// Every `data` chunk laid end to end — WAV's whole-set invariant, read with nothing but
/// the reader `probe` itself uses.
fn wav_data(paths: &[PathBuf]) -> Vec<u8> {
    paths
        .iter()
        .flat_map(|p| {
            let bytes = std::fs::read(p).unwrap();
            let at = bytes.windows(4).position(|w| w == b"data").unwrap();
            let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
            bytes[at + 8..at + 8 + len].to_vec()
        })
        .collect()
}

fn frames_of(path: &Path) -> u64 {
    probe(path).stream_info.total_frames.unwrap()
}

/// A chain where boundary 1 hands frames on to boundary 2, both directions: every file but
/// the tail ends up aligned, the concatenated audio is byte-for-byte what it was, and each
/// reported audio MD5 is the MD5 of that file's `data`.
#[test]
fn wav_chain_round_trips_and_aligns() {
    for direction in [BoundaryDirection::Backward, BoundaryDirection::Forward] {
        let dir = tempfile::tempdir().unwrap();
        let files = wav_set(dir.path(), &[588 * 10 + 3, 588 * 20, 588 * 5 + 100]);
        let srcs: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
        let before = wav_data(&srcs);

        let plan = plan_fix(&files, direction, TailPolicy::Report).unwrap();
        let dsts: Vec<PathBuf> = (1..=3)
            .map(|i| dir.path().join(format!("out/t{i:02}.wav")))
            .collect();
        let steps = std::sync::Mutex::new(Vec::new());
        let fixed = execute_fix(&files, &plan, &dsts, &no_encode(), &|i, s| {
            steps.lock().unwrap().push((i, s))
        })
        .unwrap();

        assert_eq!(wav_data(&dsts), before, "{direction:?}");
        for d in &dsts[..2] {
            assert_eq!(frames_of(d) % 588, 0, "{direction:?}: {}", d.display());
        }
        for (f, d) in fixed.iter().zip(&dsts) {
            assert_eq!(f.path, *d);
            assert_eq!(f.audio_md5, format::audio_md5(d).unwrap());
        }
        for i in 0..3 {
            let mine: Vec<FixStep> = steps
                .lock()
                .unwrap()
                .iter()
                .filter(|s| s.0 == i)
                .map(|s| s.1)
                .collect();
            assert_eq!(
                mine,
                [
                    FixStep::Decoding,
                    FixStep::Encoding,
                    FixStep::Checking,
                    FixStep::Checked
                ]
            );
        }
    }
}

/// A file too short to keep what it is handed passes it on whole: t01's 500 left-over
/// frames and all 10 of t02's end up at the head of t03, which then reads from all three.
#[test]
fn wav_fix_reaches_past_a_short_file() {
    let dir = tempfile::tempdir().unwrap();
    let files = wav_set(dir.path(), &[588 * 3 + 500, 10, 588 * 2]);
    let srcs: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    let before = wav_data(&srcs);
    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Pad).unwrap();
    let dsts: Vec<PathBuf> = (1..=3)
        .map(|i| dir.path().join(format!("out/t{i:02}.wav")))
        .collect();
    execute_fix(&files, &plan, &dsts, &no_encode(), &|_, _| {}).unwrap();

    assert_eq!(frames_of(&dsts[0]), 588 * 3);
    assert_eq!(frames_of(&dsts[1]), 0);
    assert_eq!(frames_of(&dsts[2]), 588 * 3);
    let after = wav_data(&dsts);
    assert_eq!(after[..before.len()], before[..]);
}

/// Tail padding adds zeroed frames to the last file only, and the invariant leaves them out.
#[test]
fn wav_tail_padding_is_silence_and_excluded_from_the_invariant() {
    let dir = tempfile::tempdir().unwrap();
    let files = wav_set(dir.path(), &[588 * 10 + 3, 588 * 5 + 100]);
    let srcs: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    let before = wav_data(&srcs);
    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Pad).unwrap();
    let pad = plan.tail_padding_frames.unwrap();
    assert_eq!(pad, 588 - 103);

    let dsts = vec![dir.path().join("o1.wav"), dir.path().join("o2.wav")];
    execute_fix(&files, &plan, &dsts, &no_encode(), &|_, _| {}).unwrap();

    for d in &dsts {
        assert_eq!(frames_of(d) % 588, 0);
    }
    let after = wav_data(&dsts);
    assert_eq!(after.len(), before.len() + pad as usize * 4);
    assert_eq!(after[..before.len()], before[..]);
    assert!(after[before.len()..].iter().all(|&b| b == 0));
}

/// A `LIST` chunk on either side of `data` survives byte-for-byte, and the file around it is
/// still one `probe` reads.
#[test]
fn wav_list_chunk_is_kept_as_is() {
    let dir = tempfile::tempdir().unwrap();
    let files = wav_set(dir.path(), &[588 * 10 + 3, 588 * 5]);
    let list = b"LIST\x0e\0\0\0INFOINAM\x02\0\0\0t\0";
    let mut bytes = std::fs::read(&files[0].path).unwrap();
    bytes.splice(12..12, list.iter().copied()); // before fmt
    bytes.extend_from_slice(list); // after data
    let riff = (bytes.len() - 8) as u32;
    bytes[4..8].copy_from_slice(&riff.to_le_bytes());
    std::fs::write(&files[0].path, &bytes).unwrap();
    let files = vec![probe(&files[0].path), files[1].clone()];

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    let dsts = vec![dir.path().join("o1.wav"), dir.path().join("o2.wav")];
    execute_fix(&files, &plan, &dsts, &no_encode(), &|_, _| {}).unwrap();

    let out = std::fs::read(&dsts[0]).unwrap();
    assert_eq!(&out[12..12 + list.len()], list);
    assert_eq!(&out[out.len() - list.len()..], list);
    assert_eq!(frames_of(&dsts[0]), 588 * 10);
    assert_eq!(out.len(), bytes.len() - 3 * 4);
}

#[test]
fn wav_failed_third_output_leaves_nothing_committed() {
    let dir = tempfile::tempdir().unwrap();
    let files = wav_set(dir.path(), &[588 * 10 + 3, 588 * 20, 588 * 5]);
    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    let dsts: Vec<PathBuf> = ["o1", "o2", "o3"]
        .iter()
        .map(|n| dir.path().join(format!("{n}.wav")))
        .collect();
    std::fs::create_dir_all(&dsts[2]).unwrap(); // sabotage, as for FLAC
    let err = execute_fix(&files, &plan, &dsts, &no_encode(), &|_, _| {}).unwrap_err();
    eprintln!("expected failure: {err}");
    assert!(!dsts[0].exists());
    assert!(!dsts[1].exists());
}

#[test]
fn a_mixed_wav_flac_set_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut files = wav_set(dir.path(), &[588 * 10 + 3]);
    let b = dir.path().join("t02.flac");
    std::fs::copy(fixture("cdda-aligned.flac"), &b).unwrap();
    files.push(probe(&b));
    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    let dsts = vec![dir.path().join("o1.wav"), dir.path().join("o2.flac")];
    let err = execute_fix(&files, &plan, &dsts, &no_encode(), &|_, _| {}).unwrap_err();
    assert!(err.to_string().contains("convert first"), "{err}");
    let err = fix_in_place(&files, &plan, &no_encode(), &|_, _| {}).unwrap_err();
    assert!(err.to_string().contains("convert first"), "{err}");
    assert!(!dsts[0].exists());
}

#[test]
fn wav_in_place_replaces_changed_files_and_sets_their_originals_aside() {
    let dir = tempfile::tempdir().unwrap();
    let files = wav_set(dir.path(), &[588 * 4, 588 * 10 + 3, 588 * 20 + 585]);
    let paths: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    let before = wav_data(&paths);
    let t01 = std::fs::read(&paths[0]).unwrap();

    let plan = plan_fix(&files, BoundaryDirection::Backward, TailPolicy::Report).unwrap();
    assert!(plan.fully_fixed);
    let done = fix_in_place(&files, &plan, &no_encode(), &|_, _| {}).unwrap();

    assert!(matches!(&done[0], InPlace::Unchanged { .. }));
    assert_eq!(std::fs::read(&paths[0]).unwrap(), t01);
    for (i, d) in done.iter().enumerate().skip(1) {
        let InPlace::Replaced { original, .. } = d else {
            panic!("file {i} should be replaced");
        };
        assert!(original.starts_with(dir.path().join("_original/sbe-fix")));
        assert_eq!(frames_of(&paths[i]) % 588, 0);
    }
    assert_eq!(wav_data(&paths), before);
}
