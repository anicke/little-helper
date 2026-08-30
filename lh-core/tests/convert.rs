//! Conversion is the first thing Little Helper does that *produces* a file someone will
//! trade, so the reference tools are the oracle here in the strongest sense available:
//! our FLAC → WAV output is compared byte-for-byte against `flac -d`, and our WAV → FLAC
//! output is the reference encoder's own.
//!
//! Tests needing `flac` skip when it is absent rather than failing — Windows CI has no
//! package for it — and say so, so a green run is never mistaken for a complete one.

use lh_core::convert::{EncodeOpts, to_flac, to_wav};
use lh_core::tools::{Agent, Registry, Tool, ToolId};
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// The reference `flac`, or `None` with a note on stderr.
fn reference_flac() -> Option<Tool> {
    match Registry::discover_one(ToolId::Flac).require(ToolId::Flac) {
        Ok(t) => Some(t.clone()),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

/// What `flac -d` writes for the same input, for byte-for-byte comparison.
fn reference_decode(flac: &Tool, src: &Path, dst: &Path) -> Vec<u8> {
    let status = Command::new(&flac.path)
        .args(["-d", "--silent", "--force", "-o"])
        .arg(dst)
        .arg(src)
        .status()
        .expect("running the reference decoder");
    assert!(status.success(), "flac -d failed on {}", src.display());
    std::fs::read(dst).expect("reference output")
}

#[test]
fn flac_to_wav_is_byte_identical_to_the_reference_decoder() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    // Both container shapes: legacy 16-byte `fmt ` at 16 bits, WAVE_FORMAT_EXTENSIBLE
    // at 24. Getting the second one wrong is the known limitation this closes.
    for name in ["cdda-aligned.flac", "cdda-sbe.flac", "hires-24bit.flac"] {
        let src = fixture(name);
        let ours = dir.path().join(format!("ours-{name}.wav"));
        let theirs = dir.path().join(format!("theirs-{name}.wav"));

        let done = to_wav(&src, &ours, false).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(
            done.checked_against_source,
            "{name} carried no MD5 to check"
        );

        let expected = reference_decode(&flac, &src, &theirs);
        let actual = std::fs::read(&ours).unwrap();
        assert_eq!(
            actual.len(),
            expected.len(),
            "{name}: our WAV is a different length from flac -d's"
        );
        assert!(
            actual == expected,
            "{name}: our WAV differs from flac -d's at byte {}",
            actual
                .iter()
                .zip(&expected)
                .position(|(a, b)| a != b)
                .unwrap_or(0)
        );
    }
}

#[test]
fn wav_to_flac_goes_through_the_reference_encoder_and_round_trips() {
    let Some(flac) = reference_flac() else { return };
    let dir = tempfile::tempdir().unwrap();

    let src = fixture("cdda-aligned.wav");
    let encoded = dir.path().join("out.flac");
    let done = to_flac(&src, &encoded, &flac, &EncodeOpts::default(), false).unwrap();
    assert!(done.checked_against_source);

    // Principle 2: the vendor string is the point of shelling out at all.
    let probed = lh_core::format::probe(&encoded).unwrap();
    let encoder = probed.encoder.expect("a vendor string");
    assert!(
        encoder.starts_with("reference libFLAC"),
        "vendor string is {encoder:?}"
    );

    match &done.provenance.agent {
        Agent::Tool { argv, version, .. } => {
            assert!(argv.iter().any(|a| a == "--verify"), "{argv:?}");
            assert!(
                argv.iter().any(|a| a == "--compression-level-8"),
                "{argv:?}"
            );
            assert!(version.contains("flac"), "{version}");
        }
        other => panic!("encoding must record the tool that ran, got {other:?}"),
    }

    // The round-trip property from PLAN.md §6, for the canonical 16-bit case.
    let back = dir.path().join("back.wav");
    to_wav(&encoded, &back, false).unwrap();
    assert_eq!(
        std::fs::read(&back).unwrap(),
        std::fs::read(&src).unwrap(),
        "wav → flac → wav is not byte-identical"
    );
}

/// A file that fails its own checksum must not become a WAV. Handing someone audio we
/// already know is wrong is worse than handing them nothing.
#[test]
fn a_flac_that_fails_its_own_checksum_produces_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("bad.wav");

    let err = to_wav(&fixture("wrong-md5.flac"), &out, false).expect_err("should refuse");
    let message = err.to_string();
    assert!(message.contains("does not match the MD5"), "{message}");

    assert!(!out.exists(), "a WAV was written from known-bad audio");
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert!(
        leftovers.is_empty(),
        "partial files left behind: {leftovers:?}"
    );
}

#[test]
fn an_existing_output_is_never_overwritten_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("taken.wav");
    std::fs::write(&out, b"someone else's file").unwrap();

    let err = to_wav(&fixture("cdda-aligned.flac"), &out, false).expect_err("should refuse");
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&out).unwrap(), b"someone else's file");

    // With permission, it goes through.
    to_wav(&fixture("cdda-aligned.flac"), &out, true).unwrap();
    assert_ne!(std::fs::read(&out).unwrap(), b"someone else's file");
}

#[test]
fn the_output_may_not_be_the_input() {
    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("cdda-aligned.flac");
    std::fs::copy(fixture("cdda-aligned.flac"), &copy).unwrap();

    let err = to_wav(&copy, &copy, true).expect_err("should refuse");
    assert!(err.to_string().contains("destroy the original"), "{err}");
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        std::fs::read(fixture("cdda-aligned.flac")).unwrap()
    );
}
