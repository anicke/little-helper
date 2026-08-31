//! The exit codes are a contract: scripts branch on them.
//! 0 = everything passed, 1 = a file failed, 2 = the command itself failed.

use assert_cmd::Command;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lh-core/tests/fixtures")
        .canonicalize()
        .expect("fixture corpus should exist")
}

fn lh() -> Command {
    Command::cargo_bin("lh").expect("lh binary")
}

#[test]
fn check_against_the_reference_ffp_passes() {
    lh().arg("check")
        .arg(fixtures().join("reference.ffp"))
        .assert()
        .success();
}

#[test]
fn verify_reports_failure_with_exit_code_1() {
    lh().arg("verify")
        .arg(fixtures().join("wrong-md5.flac"))
        .assert()
        .code(1)
        .stdout(predicates::str::contains("MISMATCH"));
}

#[test]
fn a_broken_checksum_file_is_a_command_failure_not_a_file_failure() {
    lh().arg("check")
        .arg(fixtures().join("cdda-aligned.flac"))
        .assert()
        .code(2)
        .stderr(predicates::str::contains("expected .ffp, .md5 or .st5"));
}

#[test]
fn ffp_output_matches_the_reference() {
    let expected = std::fs::read_to_string(fixtures().join("reference.ffp")).unwrap();
    let line = expected
        .lines()
        .find(|l| l.starts_with("cdda-aligned.flac:"))
        .expect("fixture entry");
    lh().arg("ffp")
        .arg(fixtures().join("cdda-aligned.flac"))
        .assert()
        .success()
        .stdout(predicates::str::contains(line));
}

#[test]
fn torrent_info_reports_the_infohash() {
    lh().arg("torrent")
        .arg("info")
        .arg(fixtures().join("torrents/debian-13.6.0-amd64-netinst.iso.torrent"))
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "481b6e3617be4c88f96cb25e47c9d8272130071e",
        ))
        .stdout(predicates::str::contains("debian-13.6.0-amd64-netinst.iso"));
}

#[test]
fn torrent_info_on_a_non_torrent_is_a_command_failure() {
    lh().arg("torrent")
        .arg("info")
        .arg(fixtures().join("cdda-aligned.flac"))
        .assert()
        .code(2)
        .stderr(predicates::str::contains("not valid bencode"));
}

/// A tool the user pointed at by hand and that is not there must fail loudly, and take
/// the exit code with it: a scripted install check is exactly what this command is for.
#[test]
fn tools_reports_a_configured_flac_that_is_absent() {
    lh().arg("tools")
        .env("LH_FLAC", fixtures().join("no-such-flac"))
        .assert()
        .code(1)
        .stdout(predicates::str::contains("not found"))
        .stdout(predicates::str::contains("LH_FLAC"))
        .stdout(predicates::str::contains("flac is required"));
}

/// Conversion writes new files and never touches the source, so the test asserts the
/// input is still there and still byte-for-byte itself.
#[test]
fn convert_writes_a_wav_and_leaves_the_flac_alone() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cdda-aligned.flac");
    std::fs::copy(fixtures().join("cdda-aligned.flac"), &src).unwrap();
    let before = std::fs::read(&src).unwrap();

    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "wav"])
        .assert()
        .success()
        .stdout(predicates::str::contains("WROTE     cdda-aligned.wav"));

    assert!(dir.path().join("cdda-aligned.wav").exists());
    assert_eq!(std::fs::read(&src).unwrap(), before);
}

/// An output that already exists is a file failure, not a command failure: the rest of
/// the batch still runs, and the exit code says something needs attention.
#[test]
fn convert_refuses_to_overwrite_an_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("cdda-aligned.flac"),
    )
    .unwrap();
    std::fs::write(dir.path().join("cdda-aligned.wav"), b"mine").unwrap();

    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "wav"])
        .assert()
        .code(1)
        .stdout(predicates::str::contains("already exists"));

    assert_eq!(
        std::fs::read(dir.path().join("cdda-aligned.wav")).unwrap(),
        b"mine"
    );
}

/// Encoding without the reference encoder is a command failure, and it is reported before
/// any file is touched rather than halfway through a show.
#[test]
fn convert_to_flac_without_the_encoder_is_a_command_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.wav"),
        dir.path().join("cdda-aligned.wav"),
    )
    .unwrap();

    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "flac"])
        .env("LH_FLAC", dir.path().join("no-such-flac"))
        .assert()
        .code(2)
        .stderr(predicates::str::contains(
            "encoding WAV to FLAC requires flac",
        ));

    assert!(!dir.path().join("cdda-aligned.flac").exists());
}

/// Creating a torrent writes one new file beside the show and touches nothing inside it.
#[test]
fn torrent_create_writes_beside_the_show_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("gd1977-05-08");
    std::fs::create_dir_all(show.join("d1")).unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        show.join("d1/t01.flac"),
    )
    .unwrap();
    std::fs::write(show.join("Thumbs.db"), b"noise").unwrap();
    let before = std::fs::read(show.join("d1/t01.flac")).unwrap();

    lh().arg("torrent")
        .arg("create")
        .arg(&show)
        .args(["--tracker", "http://tracker.etree.org:6969/announce"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Thumbs.db (not part of the recording)",
        ))
        .stdout(predicates::str::contains("infohash"));

    let torrent = dir.path().join("gd1977-05-08.torrent");
    assert!(
        torrent.exists(),
        "the torrent goes beside the show, not inside it"
    );
    assert!(!show.join("gd1977-05-08.torrent").exists());
    assert_eq!(std::fs::read(show.join("d1/t01.flac")).unwrap(), before);

    // And it describes the show it was made from.
    lh().arg("torrent")
        .arg("check")
        .arg(&torrent)
        .args(["--path"])
        .arg(dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("all 1 files verified"));
}

/// A piece length clients would reject is a command failure, and nothing is written.
#[test]
fn torrent_create_refuses_a_piece_length_clients_would_reject() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    std::fs::create_dir_all(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("t01.flac")).unwrap();

    lh().arg("torrent")
        .arg("create")
        .arg(&show)
        .args(["--piece-length", "100000"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("power of two"));

    assert!(!dir.path().join("show.torrent").exists());
}

/// The listing's whole job is to let a user check us rather than trust us, so every entry
/// shown has to carry a date — and only the entries that answered when checked are shown at
/// all, so the ones we found dead or unreachable don't clutter every run.
#[test]
fn torrent_trackers_lists_only_the_ones_that_answered() {
    let empty = tempfile::tempdir().unwrap();
    let out = lh()
        .arg("torrent")
        .arg("trackers")
        .env("LH_CONFIG_DIR", empty.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();

    for id in ["dime", "etree", "genesis", "tradersden"] {
        assert!(text.contains(id), "{id} is missing from the listing");
    }
    for id in [
        "crosstown",
        "jamtothis",
        "losslesslegs",
        "mindwarp",
        "yeeshkul",
        "zappateers",
        "zomb",
    ] {
        assert!(
            !text.contains(id),
            "{id} never answered when checked and should not be in the listing:\n{text}"
        );
    }
    assert_eq!(
        text.matches("checked 2026-08-30").count(),
        4,
        "every listed entry carries the date it was checked:\n{text}"
    );
    assert!(text.contains("personal URL needed"), "{text}");
    assert!(
        text.contains("2 of 4 entries can be used as they stand"),
        "{text}"
    );
    // And it says where a list of the user's own would go.
    assert!(text.contains("trackers.lst"), "{text}");
}

/// An id we have checked and found unusable never becomes an announce URL, and nothing is
/// written. The escape hatch is in the error, because we might be the ones who are wrong.
#[test]
fn torrent_create_refuses_a_tracker_we_know_cannot_work() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("show");
    std::fs::create_dir_all(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("t01.flac")).unwrap();

    lh().arg("torrent")
        .arg("create")
        .arg(&show)
        .args(["--tracker", "zomb"])
        .env("LH_CONFIG_DIR", dir.path().join("no-config"))
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no DNS A record"))
        .stderr(predicates::str::contains("--tracker <URL>"));

    assert!(!dir.path().join("show.torrent").exists());
}

/// The user's own list, in TLH's format, with a passkey filled from the config directory —
/// and `create` saying out loud that a table we read decided part of the infohash.
#[test]
fn a_user_tracker_list_supplies_the_announce_url_the_passkey_and_the_private_flag() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("trackers.lst"),
        "The Pit|https://pit.example/announce/{passkey}|private|source=PIT|id=pit\r\n",
    )
    .unwrap();
    std::fs::write(config.join("passkeys.lst"), "pit|s3cr3t\n").unwrap();

    let show = dir.path().join("show");
    std::fs::create_dir_all(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("t01.flac")).unwrap();

    lh().arg("torrent")
        .arg("create")
        .arg(&show)
        .args(["--tracker", "pit"])
        .env("LH_CONFIG_DIR", &config)
        .assert()
        .success()
        // The flag is invisible in the file and changes the infohash, so it is named.
        .stdout(predicates::str::contains("private    yes"))
        .stdout(predicates::str::contains("set by The Pit"))
        .stdout(predicates::str::contains("source     PIT"));

    let torrent = dir.path().join("show.torrent");
    lh().arg("torrent")
        .arg("info")
        .arg(&torrent)
        .arg("--no-files")
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "https://pit.example/announce/s3cr3t",
        ))
        .stdout(predicates::str::contains("private      yes"));
}

/// A passkey we do not have would make a torrent nobody ever connects to, which the user
/// would only discover much later. Stop instead.
#[test]
fn torrent_create_refuses_an_unfilled_passkey() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(
        config.join("trackers.lst"),
        "The Pit|https://pit.example/announce/{passkey}|id=pit\n",
    )
    .unwrap();

    let show = dir.path().join("show");
    std::fs::create_dir_all(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("t01.flac")).unwrap();

    lh().arg("torrent")
        .arg("create")
        .arg(&show)
        .args(["--tracker", "pit"])
        .env("LH_CONFIG_DIR", &config)
        .assert()
        .code(2)
        .stderr(predicates::str::contains("passkeys.lst"));

    assert!(!dir.path().join("show.torrent").exists());
}
