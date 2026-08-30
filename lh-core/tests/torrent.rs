//! T1 tests: parsing `.torrent` files and computing the infohash.
//!
//! The synthetic fixtures come from our own bencoder (`scripts/gen-torrent-fixtures.py`),
//! so they cannot be the only oracle. `debian-13.6.0-amd64-netinst.iso.torrent` is a real,
//! publicly distributed torrent, and its expected infohash was agreed by two independent
//! implementations during the T0 spike. That is what keeps this suite honest.

use lh_core::Error;
use lh_core::torrent::Metainfo;
use std::path::{Path, PathBuf};

/// Agreed by bendy and by an independent bencode implementation during T0.
const DEBIAN_INFOHASH: &str = "481b6e3617be4c88f96cb25e47c9d8272130071e";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/torrents")
        .join(name)
}

fn read(name: &str) -> Metainfo {
    Metainfo::read(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn reject(name: &str) -> Error {
    Metainfo::read(&fixture(name)).expect_err(&format!("{name} should have been refused"))
}

#[test]
fn external_vector_infohash_matches() {
    let t = read("debian-13.6.0-amd64-netinst.iso.torrent");
    assert_eq!(t.info_hash_hex(), DEBIAN_INFOHASH);
}

#[test]
fn external_vector_fields_are_read_correctly() {
    let t = read("debian-13.6.0-amd64-netinst.iso.torrent");
    assert_eq!(t.name, "debian-13.6.0-amd64-netinst.iso");
    assert_eq!(t.piece_length, 262_144);
    assert_eq!(t.pieces.len(), 3020);
    assert_eq!(t.total_length, 791_674_880);
    assert!(t.is_single_file);
    assert_eq!(t.files.len(), 1);
    assert_eq!(t.files[0].path, vec!["debian-13.6.0-amd64-netinst.iso"]);
    assert_eq!(t.created_by.as_deref(), Some("mktorrent 1.1"));
    assert_eq!(
        t.announce,
        vec![vec!["http://bttracker.debian.org:6969/announce"]]
    );
}

#[test]
fn multi_file_torrents_list_every_file_in_stream_order() {
    let t = read("multi-file.torrent");
    assert!(!t.is_single_file);
    assert_eq!(t.total_length, 500);
    let names: Vec<_> = t.files.iter().map(|f| f.display_path()).collect();
    assert_eq!(names, ["d1t01.flac", "d1t02.flac", "d1t03.flac"]);
    // 500 bytes over 128-byte pieces: four pieces, and piece 0 straddles the first two files.
    assert_eq!(t.pieces.len(), 4);
}

#[test]
fn nested_directories_are_preserved() {
    let t = read("nested-dirs.torrent");
    assert_eq!(t.files[0].path, vec!["disc1", "d1t01.flac"]);
    assert_eq!(t.files[0].display_path(), "disc1/d1t01.flac");
}

/// Padding is zero bytes in the stream that never exist on disk. It has to be kept for the
/// piece maths and hidden from the per-file report.
#[test]
fn padding_files_are_flagged_and_excluded_from_real_files() {
    let t = read("padded.torrent");
    assert_eq!(t.files.len(), 3);
    assert_eq!(t.real_files().count(), 2);
    assert!(t.files[1].is_pad);
    assert_eq!(
        t.total_length, 378,
        "padding still counts toward the stream"
    );
    let visible: Vec<_> = t.real_files().map(|f| f.display_path()).collect();
    assert_eq!(visible, ["d1t01.flac", "d1t02.flac"]);
}

// --- files that must be refused ---

/// The zip-slip class of bug. Torrent paths are attacker-controlled.
#[test]
fn path_traversal_is_refused() {
    let err = reject("traversal.torrent");
    assert!(matches!(err, Error::UnsafeTorrentPath { .. }), "{err}");
    assert!(err.to_string().contains(".."), "{err}");
}

#[test]
fn a_path_separator_inside_a_component_is_refused() {
    let err = reject("separator-in-path.torrent");
    assert!(matches!(err, Error::UnsafeTorrentPath { .. }), "{err}");
    assert!(err.to_string().contains("separator"), "{err}");
}

#[test]
fn non_canonical_bencode_is_refused() {
    let err = reject("unsorted-keys.torrent");
    assert!(matches!(err, Error::Bencode { .. }), "{err}");
    assert!(err.to_string().contains("sorted"), "{err}");
}

#[test]
fn pieces_must_be_whole_sha1_digests() {
    let err = reject("short-pieces.torrent");
    assert!(err.to_string().contains("multiple of 20"), "{err}");
}

/// If the piece count and the file lengths disagree, the two halves of the torrent
/// contradict each other and nothing can be verified against it.
#[test]
fn piece_count_must_match_total_length() {
    let err = reject("piece-count-mismatch.torrent");
    assert!(err.to_string().contains("expected"), "{err}");
}

#[test]
fn implausible_piece_length_is_refused() {
    let err = reject("zero-piece-length.torrent");
    assert!(err.to_string().contains("piece length"), "{err}");
}

#[test]
fn a_torrent_cannot_be_both_single_and_multi_file() {
    let err = reject("length-and-files.torrent");
    assert!(err.to_string().contains("both length and files"), "{err}");
}

/// Principle 5: name what it is, not what is missing.
#[test]
fn v2_torrents_say_they_are_v2() {
    let err = reject("v2-only.torrent");
    let msg = err.to_string();
    assert!(msg.contains("v2"), "{msg}");
    assert!(!msg.contains("no pieces"), "unhelpful error: {msg}");
}

#[test]
fn a_non_torrent_is_refused_cleanly() {
    let err = Metainfo::read(&fixture("../cdda-aligned.flac")).unwrap_err();
    assert!(matches!(err, Error::Bencode { .. }), "{err}");
}
