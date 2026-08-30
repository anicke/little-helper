//! Creating torrents.
//!
//! The test that matters is the first one: our infohash has to equal `mktorrent`'s for the
//! same folder. One assertion pins the bencoding, the key order, the *file* order, the piece
//! length and every piece hash simultaneously — and a torrent whose infohash differs from
//! what everyone else's tool produces will never deduplicate or cross-seed, however
//! internally consistent it is.
//!
//! `mktorrent` is not packaged for Windows, so those tests skip there and say so; a green
//! run without them is not the same as a complete one.

use lh_core::torrent::{CreateOpts, Metainfo, Skipped, Verdict, check, create};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Deterministic, non-repeating content, so a file swapped into the wrong position in the
/// stream changes the hashes instead of hiding in identical bytes.
fn payload(dir: &Path, files: &[(&str, usize)]) {
    for (rel, len) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let seed = rel.bytes().map(usize::from).sum::<usize>();
        let bytes: Vec<u8> = (0..*len)
            .map(|i| (i.wrapping_mul(31) + seed) as u8)
            .collect();
        std::fs::write(&path, bytes).unwrap();
    }
}

fn mktorrent() -> Option<PathBuf> {
    match which::which("mktorrent") {
        Ok(p) => Some(p),
        Err(_) => {
            eprintln!("skipping: mktorrent is not installed, so there is nothing to agree with");
            None
        }
    }
}

/// What `mktorrent` makes of the same folder at the same piece length.
fn mktorrent_info_hash(tool: &Path, source: &Path, out: &Path, piece_length: u64) -> String {
    let exponent = piece_length.trailing_zeros().to_string();
    let status = Command::new(tool)
        .args(["-l", &exponent, "-o"])
        .arg(out)
        .arg(source)
        .status()
        .expect("running mktorrent");
    assert!(status.success(), "mktorrent failed on {}", source.display());
    Metainfo::read(out)
        .expect("mktorrent's own output")
        .info_hash_hex()
}

/// The oracle. Four shapes, including the two the original Trader's Little Helper shipped
/// bugs for: a file ending exactly on a piece boundary, and a zero-byte file.
#[test]
fn our_infohash_matches_mktorrent() {
    let Some(tool) = mktorrent() else { return };
    let piece_length = 32 * 1024;

    let cases: Vec<(&str, Vec<(&str, usize)>)> = vec![
        (
            "a file ending exactly on a piece boundary",
            vec![("d1/t01.flac", 32 * 1024), ("d1/t02.flac", 5000)],
        ),
        (
            "a zero-byte file, in the middle and at the end",
            vec![
                ("aaa.flac", 40_000),
                ("empty-middle.txt", 0),
                ("zzz.flac", 9_000),
                ("empty-last.txt", 0),
            ],
        ),
        (
            "nested directories and names that sort awkwardly",
            vec![
                ("d1/t01.flac", 20_000),
                ("d10/t01.flac", 20_000),
                ("d2/sub/deep.flac", 3_000),
                // `d1.txt` against the `d1/` directory is the pair that tells a full-path
                // sort apart from a component-wise one: '.' sorts before '/', so this file
                // comes *before* the directory's contents. Without it the test passes
                // either way.
                ("d1.txt", 100),
                ("a b", 100),
                ("a.txt", 100),
                ("Zed", 100),
                ("info.txt", 700),
            ],
        ),
        (
            "one piece exactly, no remainder",
            vec![("only.flac", 64 * 1024)],
        ),
    ];

    for (what, files) in cases {
        let dir = tempfile::tempdir().unwrap();
        let show = dir.path().join("gd1977-05-08");
        payload(&show, &files);

        let ours = create(
            &show,
            &dir.path().join("ours.torrent"),
            &CreateOpts {
                piece_length: Some(piece_length),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("{what}: {e}"));

        let theirs = mktorrent_info_hash(
            &tool,
            &show,
            &dir.path().join("theirs.torrent"),
            piece_length,
        );
        assert_eq!(
            ours.info_hash_hex(),
            theirs,
            "{what}: our torrent is not the torrent mktorrent would have made"
        );
    }
}

/// A single-file torrent takes the `length` branch instead of `files`, and mktorrent has to
/// agree about that one too.
#[test]
fn a_single_file_torrent_matches_mktorrent() {
    let Some(tool) = mktorrent() else { return };
    let dir = tempfile::tempdir().unwrap();
    payload(dir.path(), &[("show.flac", 100_000)]);
    let source = dir.path().join("show.flac");

    let ours = create(
        &source,
        &dir.path().join("ours.torrent"),
        &CreateOpts {
            piece_length: Some(32 * 1024),
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(ours.files.len(), 1);
    assert_eq!(
        ours.info_hash_hex(),
        mktorrent_info_hash(
            &tool,
            &source,
            &dir.path().join("theirs.torrent"),
            32 * 1024
        )
    );
}

/// The end-to-end proof: a torrent we made describes the folder we made it from.
#[test]
fn a_created_torrent_verifies_against_its_own_payload() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("gd1977-05-08");
    payload(
        &show,
        &[
            ("d1/t01.flac", 70_000),
            ("d1/t02.flac", 32_768),
            ("info.txt", 400),
        ],
    );

    let torrent = dir.path().join("show.torrent");
    let made = create(&show, &torrent, &CreateOpts::default()).unwrap();

    let meta = Metainfo::read(&torrent).unwrap();
    assert_eq!(meta.info_hash, made.info_hash);
    assert_eq!(meta.created_by.as_deref(), Some("Little Helper 0.1.0"));

    let report = check(&meta, &torrent, &show).unwrap();
    assert_eq!(report.verdict(), Verdict::Complete, "{:?}", report.files);
}

/// Two runs over the same folder are the same torrent. `creation date` sits outside `info`,
/// so it moves while the infohash does not — which is what makes a re-created torrent
/// recognisable as the same one.
#[test]
fn the_same_folder_twice_gives_the_same_infohash() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(&show, &[("a.flac", 50_000), ("b.flac", 50_000)]);

    let first = create(&show, &dir.path().join("1.torrent"), &CreateOpts::default()).unwrap();
    let second = create(&show, &dir.path().join("2.torrent"), &CreateOpts::default()).unwrap();
    assert_eq!(first.info_hash, second.info_hash);

    // Different trackers, same content: still the same torrent, because announce is outside
    // `info`. Private is not, and that one must differ.
    let tracked = create(
        &show,
        &dir.path().join("3.torrent"),
        &CreateOpts {
            announce: vec![vec!["http://tracker.etree.org:6969/announce".into()]],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(first.info_hash, tracked.info_hash);

    let private = create(
        &show,
        &dir.path().join("4.torrent"),
        &CreateOpts {
            private: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(first.info_hash, private.info_hash);
}

/// The examples in the plan, kept honest.
#[test]
fn the_automatic_piece_length_lands_where_the_plan_says() {
    use lh_core::torrent::create::auto_piece_length;
    assert_eq!(auto_piece_length(400 * 1024 * 1024), 256 * 1024);
    assert_eq!(auto_piece_length(1_181_116_006), 1024 * 1024);
    // Small payloads stay at the floor; enormous ones stop at the ceiling.
    assert_eq!(auto_piece_length(1), 16 * 1024);
    assert_eq!(auto_piece_length(1 << 40), 16 * 1024 * 1024);
}

#[test]
fn a_piece_length_that_clients_would_reject_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(&show, &[("a.flac", 10_000)]);

    for bad in [1000, 100_000, 8 * 1024, 32 * 1024 * 1024] {
        let err = create(
            &show,
            &dir.path().join("out.torrent"),
            &CreateOpts {
                piece_length: Some(bad),
                ..Default::default()
            },
        )
        .expect_err("should refuse");
        assert!(err.to_string().contains("power of two"), "{bad}: {err}");
    }
}

/// Excluded files are named, not silently dropped — the user has to be able to see that
/// their artwork went in and their `Thumbs.db` did not.
#[test]
fn noise_is_excluded_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(
        &show,
        &[
            ("t01.flac", 20_000),
            ("Thumbs.db", 10),
            (".DS_Store", 10),
            ("._t01.flac", 10),
            ("old.torrent", 10),
            ("cover.jpg", 500),
        ],
    );

    let made = create(
        &show,
        &dir.path().join("out.torrent"),
        &CreateOpts::default(),
    )
    .unwrap();
    let kept: Vec<String> = made.files.iter().map(|f| f.display_path()).collect();
    assert_eq!(kept, vec!["cover.jpg", "t01.flac"]);
    assert_eq!(made.excluded.len(), 4);
    assert!(made.excluded.iter().all(|(_, s)| *s == Skipped::Noise));

    // And with --include-all, everything goes in.
    let all = create(
        &show,
        &dir.path().join("all.torrent"),
        &CreateOpts {
            include_all: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(all.files.len(), 6);
    assert!(all.excluded.is_empty());
}

/// A v1 torrent cannot express an empty directory. Report it rather than let the user find
/// out when it does not come back.
#[test]
fn an_empty_directory_is_reported_not_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(&show, &[("t01.flac", 20_000)]);
    std::fs::create_dir(show.join("artwork")).unwrap();

    let made = create(
        &show,
        &dir.path().join("out.torrent"),
        &CreateOpts::default(),
    )
    .unwrap();
    assert_eq!(made.files.len(), 1);
    assert_eq!(
        made.excluded
            .iter()
            .filter(|(_, s)| *s == Skipped::EmptyDirectory)
            .count(),
        1
    );
}

/// Following a symlink would put data from outside the folder into a torrent the user
/// believes describes the folder.
#[cfg(unix)]
#[test]
fn a_symlink_is_refused_rather_than_followed() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(&show, &[("t01.flac", 20_000)]);
    payload(dir.path(), &[("outside.flac", 5_000)]);
    std::os::unix::fs::symlink(dir.path().join("outside.flac"), show.join("linked.flac")).unwrap();

    let err = create(
        &show,
        &dir.path().join("out.torrent"),
        &CreateOpts::default(),
    )
    .expect_err("a symlink must be refused");
    let message = err.to_string();
    assert!(message.contains("symbolic link"), "{message}");
    assert!(message.contains("linked.flac"), "{message}");
    assert!(!dir.path().join("out.torrent").exists());
}

#[test]
fn an_existing_torrent_is_not_overwritten_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    payload(&show, &[("t01.flac", 20_000)]);
    let out = dir.path().join("out.torrent");
    std::fs::write(&out, b"someone else's torrent").unwrap();

    let err = create(&show, &out, &CreateOpts::default()).expect_err("should refuse");
    assert!(err.to_string().contains("already exists"), "{err}");
    assert_eq!(std::fs::read(&out).unwrap(), b"someone else's torrent");

    create(
        &show,
        &out,
        &CreateOpts {
            overwrite: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert_ne!(std::fs::read(&out).unwrap(), b"someone else's torrent");
}

/// Nothing to seed is an error, not an empty torrent.
#[test]
fn a_folder_with_no_data_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    std::fs::create_dir_all(&show).unwrap();

    let err = create(
        &show,
        &dir.path().join("out.torrent"),
        &CreateOpts::default(),
    )
    .expect_err("an empty folder must be refused");
    assert!(err.to_string().contains("no files"), "{err}");
}
