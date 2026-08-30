//! The encoder, tested the only way that means anything: against torrents made by other
//! people's tools.
//!
//! Re-encoding the `info` dictionary of a real torrent and getting its infohash back proves
//! our bencode is canonical in the same way theirs was — key order, integer form, the lot —
//! and it needs no payload and no external tool. `debian-13.6.0-amd64-netinst.iso.torrent`
//! carries the absolute value two independent implementations agreed on during T0, so the
//! chain is anchored outside this repo.

use lh_core::torrent::{Content, Draft, Metainfo, encode, info_bytes};
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/torrents")
        .join(name)
}

/// Everything a parsed torrent needs to say to be written back out.
fn draft_from(meta: &Metainfo) -> Draft {
    Draft {
        name: meta.name.clone(),
        piece_length: meta.piece_length,
        pieces: meta.pieces.clone(),
        content: if meta.is_single_file {
            Content::Single {
                length: meta.total_length,
            }
        } else {
            Content::Multi {
                files: meta.files.clone(),
            }
        },
        private: meta.private,
        source: meta.source.clone(),
        announce: meta.announce.clone(),
        comment: meta.comment.clone(),
        created_by: meta.created_by.clone(),
        creation_date: meta.creation_date,
    }
}

/// The external vector. If our encoder disagrees with whatever made this file, it disagrees
/// with the rest of the world, and every torrent we write would have the wrong name.
#[test]
fn re_encoding_the_debian_info_dict_reproduces_its_published_infohash() {
    let meta = Metainfo::read(&fixture("debian-13.6.0-amd64-netinst.iso.torrent")).unwrap();
    assert_eq!(
        meta.info_hash_hex(),
        "481b6e3617be4c88f96cb25e47c9d8272130071e",
        "the fixture itself changed"
    );

    let ours: [u8; 20] = Sha1::digest(info_bytes(&draft_from(&meta)).unwrap()).into();
    assert_eq!(
        hex::encode(ours),
        "481b6e3617be4c88f96cb25e47c9d8272130071e",
        "our encoding of the same info dictionary hashes differently"
    );
}

/// The same property across every shape we can write: multi-file, nested directories, and
/// BEP 47 padding, which adds an `attr` key that has to land in the right sorted position.
#[test]
fn re_encoding_reproduces_the_infohash_of_every_well_formed_fixture() {
    for name in [
        "debian-13.6.0-amd64-netinst.iso.torrent",
        "multi-file.torrent",
        "nested-dirs.torrent",
        "verify-multi.torrent",
        "verify-padded.torrent",
        "padded.torrent",
    ] {
        let meta = Metainfo::read(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let ours: [u8; 20] = Sha1::digest(info_bytes(&draft_from(&meta)).unwrap()).into();
        assert_eq!(
            hex::encode(ours),
            meta.info_hash_hex(),
            "{name}: re-encoded info dictionary has a different infohash"
        );
    }
}

/// The infohash must come from the bytes we write, not from a second encoding of the same
/// data. Hashing the `info` substring out of the finished file is how that gets checked.
#[test]
fn the_infohash_is_the_hash_of_the_info_bytes_actually_written() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let draft = draft_from(&meta);
    let out = encode(&draft).unwrap();

    let info = info_bytes(&draft).unwrap();
    let at = find(&out.bytes, b"4:info").expect("an info key") + b"4:info".len();
    assert_eq!(
        &out.bytes[at..at + info.len()],
        &info[..],
        "the file does not contain the info dictionary we hashed"
    );
    assert_eq!(hex::encode(out.info_hash), meta.info_hash_hex());
}

/// A written torrent has to be one we can read back — otherwise we have made a file that
/// looks fine until someone tries to seed it.
#[test]
fn what_we_write_we_can_read() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let out = encode(&draft_from(&meta)).unwrap();

    let round_tripped = Metainfo::from_bytes(&out.bytes, Path::new("written.torrent")).unwrap();
    assert_eq!(round_tripped.info_hash, out.info_hash);
    assert_eq!(round_tripped.name, meta.name);
    assert_eq!(round_tripped.files, meta.files);
    assert_eq!(round_tripped.pieces, meta.pieces);
    assert_eq!(round_tripped.piece_length, meta.piece_length);
    assert_eq!(round_tripped.announce, meta.announce);
}

/// `private` lives inside `info`, so setting it makes a *different torrent* rather than
/// annotating the same one. That is the whole reason it cannot be flipped afterwards.
#[test]
fn private_changes_the_infohash() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let public = draft_from(&meta);
    let private = Draft {
        private: true,
        ..public.clone()
    };

    let a = encode(&public).unwrap();
    let b = encode(&private).unwrap();
    assert_ne!(a.info_hash, b.info_hash);

    let read_back = Metainfo::from_bytes(&b.bytes, Path::new("private.torrent")).unwrap();
    assert!(read_back.private);
    assert_eq!(read_back.info_hash, b.info_hash);
}

/// `source` is the other infohash-bearing key, and it has to sort after `private`.
#[test]
fn source_is_written_read_back_and_changes_the_infohash() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let plain = draft_from(&meta);
    let tagged = Draft {
        private: true,
        source: Some("DIME".into()),
        ..plain.clone()
    };

    let out = encode(&tagged).unwrap();
    let read_back = Metainfo::from_bytes(&out.bytes, Path::new("tagged.torrent")).unwrap();
    assert_eq!(read_back.source.as_deref(), Some("DIME"));
    assert_ne!(out.info_hash, encode(&plain).unwrap().info_hash);
}

/// Tiers are meaningful — clients pick at random within one and fall through between them —
/// so they have to survive the trip rather than being flattened into one list.
#[test]
fn tracker_tiers_survive_a_round_trip() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let tiers = vec![
        vec!["http://tracker.etree.org:6969/announce".to_string()],
        vec![
            "http://bt.dimeadozen.org/announce.php".to_string(),
            "http://backup.example/announce".to_string(),
        ],
    ];
    let out = encode(&Draft {
        announce: tiers.clone(),
        ..draft_from(&meta)
    })
    .unwrap();

    let read_back = Metainfo::from_bytes(&out.bytes, Path::new("tiered.torrent")).unwrap();
    assert_eq!(read_back.announce, tiers);
    // BEP 3's single `announce` is the first URL of the first tier, and it must not come
    // back a second time as a tier of its own.
    assert_eq!(read_back.trackers().count(), 3);
}

/// A trackerless torrent is legal, and is what a DHT-only or private-archive torrent wants.
#[test]
fn a_trackerless_torrent_has_no_announce_keys() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();
    let out = encode(&Draft {
        announce: Vec::new(),
        ..draft_from(&meta)
    })
    .unwrap();

    assert!(find(&out.bytes, b"8:announce").is_none());
    let read_back = Metainfo::from_bytes(&out.bytes, Path::new("bare.torrent")).unwrap();
    assert!(read_back.announce.is_empty());
    assert_eq!(read_back.info_hash, out.info_hash);
}

/// Refuse to write a torrent our own parser would reject, rather than producing a file that
/// looks fine until someone tries to seed it.
#[test]
fn a_draft_that_does_not_add_up_is_refused() {
    let meta = Metainfo::read(&fixture("verify-multi.torrent")).unwrap();

    let short = Draft {
        pieces: meta.pieces[..meta.pieces.len() - 1].to_vec(),
        ..draft_from(&meta)
    };
    let err = encode(&short).expect_err("a missing piece hash must be refused");
    assert!(err.to_string().contains("expected"), "{err}");

    let empty = Draft {
        content: Content::Multi { files: Vec::new() },
        pieces: Vec::new(),
        ..draft_from(&meta)
    };
    let err = encode(&empty).expect_err("nothing to seed must be refused");
    assert!(err.to_string().contains("no data"), "{err}");
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
