//! T3 tests: piece hashing and per-file attribution.
//!
//! `verify-multi.torrent` describes three files over four 128-byte pieces:
//!
//! ```text
//! files    [-- d1t01 100 --][-------- d1t02 250 --------][---- d1t03 150 ----]
//! pieces   [--- p0 128 ---][--- p1 128 ---][--- p2 128 ---][--- p3 116 ---]
//! ```
//!
//! p0 spans d1t01/d1t02 and p2 spans d1t02/d1t03; p1 is d1t02 alone and p3 is d1t03 alone.
//! That geometry is what makes the boundary rule testable, so the offsets below are chosen
//! deliberately rather than arbitrarily.

use lh_core::torrent::{FileStatus, Metainfo, Verdict, check};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/torrents")
}

fn meta(name: &str) -> Metainfo {
    Metainfo::read(&fixtures().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Copy the committed payload into a temp dir so tests can damage it freely.
fn payload(kind: &str) -> TempDir {
    let tmp = TempDir::new().unwrap();
    let src = fixtures().join("payload").join(kind);
    let dst = tmp.path().join(kind);
    std::fs::create_dir_all(&dst).unwrap();
    for entry in std::fs::read_dir(&src).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), dst.join(entry.file_name())).unwrap();
    }
    tmp
}

/// Flip one byte at `offset` within a file, leaving its length untouched — so the size
/// pre-check still passes and only hashing can catch it.
fn flip(path: &Path, offset: u64) {
    let mut data = std::fs::read(path).unwrap();
    data[offset as usize] ^= 0xFF;
    std::fs::write(path, data).unwrap();
}

fn run(kind: &str, torrent: &str, tmp: &TempDir) -> lh_core::torrent::TorrentReport {
    let m = meta(torrent);
    check(&m, &fixtures().join(torrent), &tmp.path().join(kind)).unwrap()
}

#[test]
fn an_intact_fileset_verifies() {
    let tmp = payload("verified");
    let report = run("verified", "verify-multi.torrent", &tmp);

    assert_eq!(report.verdict(), Verdict::Complete);
    assert!(!report.quick);
    let p = report.pieces.expect("full check reports piece counts");
    assert_eq!((p.total, p.ok, p.failed, p.unverifiable), (4, 4, 0, 0));
    for f in &report.files {
        assert_eq!(f.status, FileStatus::Complete);
    }
}

/// p1 lies wholly inside d1t02, so a failure there convicts d1t02 and nothing else.
#[test]
fn damage_inside_one_file_convicts_only_that_file() {
    let tmp = payload("verified");
    // Stream offset 200 = 100 bytes into d1t02, which is inside p1 (128..256).
    flip(&tmp.path().join("verified/d1t02.flac"), 100);
    let report = run("verified", "verify-multi.torrent", &tmp);

    assert_eq!(report.files[0].status, FileStatus::Complete);
    assert_eq!(
        report.files[1].status,
        FileStatus::Corrupt {
            bad_pieces: vec![1]
        }
    );
    assert_eq!(report.files[2].status, FileStatus::Complete);
    assert_eq!(report.verdict(), Verdict::Incomplete);
}

/// The heart of it: p0 covers the end of d1t01 and the start of d1t02. When it fails, the
/// data does not say which file is wrong, so neither may be convicted.
#[test]
fn damage_on_a_boundary_convicts_neither_neighbour() {
    let tmp = payload("verified");
    // Stream offset 50, inside d1t01 and inside p0 (0..128), which d1t02 also occupies.
    flip(&tmp.path().join("verified/d1t01.flac"), 50);
    let report = run("verified", "verify-multi.torrent", &tmp);

    match &report.files[0].status {
        FileStatus::Suspect { piece, shared_with } => {
            assert_eq!(*piece, 0);
            assert_eq!(shared_with, &[1]);
        }
        other => panic!("d1t01 should be suspect, got {other:?}"),
    }
    match &report.files[1].status {
        FileStatus::Suspect { piece, shared_with } => {
            assert_eq!(*piece, 0);
            assert_eq!(shared_with, &[0]);
        }
        other => panic!("d1t02 should be suspect too, got {other:?}"),
    }
    // d1t03 shares nothing with p0 and stays clean.
    assert_eq!(report.files[2].status, FileStatus::Complete);
}

/// A missing file makes its neighbour's shared piece unreadable. That is not corruption,
/// and the innocent neighbour must not be blamed for it.
#[test]
fn a_missing_file_leaves_its_neighbour_partial_not_corrupt() {
    let tmp = payload("verified");
    std::fs::remove_file(tmp.path().join("verified/d1t03.flac")).unwrap();
    let report = run("verified", "verify-multi.torrent", &tmp);

    assert_eq!(report.files[2].status, FileStatus::Missing);
    // d1t02 covers p0, p1 and p2; p2 is shared with the missing file.
    assert_eq!(
        report.files[1].status,
        FileStatus::Partial {
            verified: 2,
            unverifiable: 1
        }
    );
    // d1t01 only touches p0, which is entirely readable.
    assert_eq!(report.files[0].status, FileStatus::Complete);

    let p = report.pieces.unwrap();
    assert_eq!(p.unverifiable, 2, "p2 and p3 both touch the missing file");
    assert_eq!(
        p.failed, 0,
        "nothing failed; two pieces could not be checked"
    );
}

/// A truncated file is caught by the size pre-check, so we never read it and never
/// misattribute its neighbours.
#[test]
fn a_wrong_sized_file_is_not_hashed() {
    let tmp = payload("verified");
    let path = tmp.path().join("verified/d1t02.flac");
    let data = std::fs::read(&path).unwrap();
    std::fs::write(&path, &data[..200]).unwrap();
    let report = run("verified", "verify-multi.torrent", &tmp);

    assert_eq!(
        report.files[1].status,
        FileStatus::WrongSize {
            expected: 250,
            actual: 200
        }
    );
    assert_eq!(report.pieces.unwrap().failed, 0);
    assert!(matches!(report.files[0].status, FileStatus::Partial { .. }));
}

/// Padding is zero bytes substituted into the stream. If we got this wrong, a perfectly
/// good download would report as broken.
#[test]
fn padding_is_hashed_as_zeros_and_verifies() {
    let tmp = payload("padded");
    let report = run("padded", "verify-padded.torrent", &tmp);

    assert_eq!(report.verdict(), Verdict::Complete);
    assert_eq!(report.files[1].status, FileStatus::Padding);
    assert_eq!(report.pieces.unwrap().ok, 3);
}
