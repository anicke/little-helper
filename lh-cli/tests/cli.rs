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

/// `--move-sources` sets a checked WAV aside in `_original/`, byte-for-byte itself, so
/// the show folder holds only the FLAC afterwards.
#[test]
fn convert_to_flac_with_move_sources_sets_the_wav_aside() {
    use lh_core::tools::{Registry, ToolId};
    if Registry::discover_one(ToolId::Flac)
        .require(ToolId::Flac)
        .is_err()
    {
        eprintln!("skipping: reference flac not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("cdda-aligned.wav");
    std::fs::copy(fixtures().join("cdda-aligned.wav"), &src).unwrap();
    let before = std::fs::read(&src).unwrap();

    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "flac", "--move-sources"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "MOVED     cdda-aligned.wav -> _original/",
        ));

    assert!(dir.path().join("cdda-aligned.flac").exists());
    assert!(!src.exists());
    let moved = dir.path().join("_original").join("cdda-aligned.wav");
    assert_eq!(std::fs::read(moved).unwrap(), before);
}

/// A FLAC source is the archival copy; setting it aside after decoding is refused outright.
#[test]
fn convert_to_wav_refuses_move_sources() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("cdda-aligned.flac"),
    )
    .unwrap();

    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "wav", "--move-sources"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("only applies to --to flac"));

    assert!(!dir.path().join("cdda-aligned.wav").exists());
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
    assert!(text.contains("personal copy via upload"), "{text}");
    assert!(
        text.contains("4 of 4 entries can be used as they stand"),
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

/// `sbe fix --dry-run` writes nothing and just previews the plan (docs/sbe-repair.md R1).
#[test]
fn sbe_fix_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("t01.flac");
    let b = dir.path().join("t02.flac");
    std::fs::copy(fixtures().join("cdda-sbe.flac"), &a).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), &b).unwrap();

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .arg("--dry-run")
        .assert()
        .code(1) // the tail is left misaligned without --pad-tail
        .stdout(predicates::str::contains("shift 137 frames backward"))
        .stdout(predicates::str::contains("plan only, nothing written"));

    assert_eq!(
        std::fs::read(&a).unwrap(),
        std::fs::read(fixtures().join("cdda-sbe.flac")).unwrap()
    );
}

/// Real execution (R2): the boundary between exactly two files is fixed, written under
/// `-o`, and the earlier file comes out sector-aligned. The source directory is untouched.
#[test]
fn sbe_fix_executes_a_single_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("t01.flac");
    let b = dir.path().join("t02.flac");
    std::fs::copy(fixtures().join("cdda-sbe.flac"), &a).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), &b).unwrap();
    let before_a = std::fs::read(&a).unwrap();
    let before_b = std::fs::read(&b).unwrap();
    let out = dir.path().join("out");

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .args(["-o"])
        .arg(&out)
        .assert()
        .code(1) // tail still misaligned; --pad-tail isn't implemented yet (R3)
        .stdout(predicates::str::contains("FIXED     t01.flac"))
        .stdout(predicates::str::contains("FIXED     t02.flac"));

    assert_eq!(std::fs::read(&a).unwrap(), before_a, "source untouched");
    assert_eq!(std::fs::read(&b).unwrap(), before_b, "source untouched");

    lh().arg("sbe")
        .arg(out.join("t01.flac"))
        .assert()
        .success()
        .stdout(predicates::str::contains("ALIGNED"));
}

/// Repair never overwrites the originals in place (Principle 1) — executing without `-o`
/// is a command failure, not a silent default.
#[test]
fn sbe_fix_without_output_dir_is_a_command_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-sbe.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t02.flac"),
    )
    .unwrap();

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .assert()
        .code(2)
        .stderr(predicates::str::contains("-o/--output"));
}

/// R3: `--pad-tail` at execution time actually closes the last file's gap with silence,
/// leaving the whole set aligned (exit `0`), not just reported.
#[test]
fn sbe_fix_pad_tail_executes_and_fully_aligns_the_set() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-sbe.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t02.flac"),
    )
    .unwrap();
    let out = dir.path().join("out");

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .args(["-o"])
        .arg(&out)
        .arg("--pad-tail")
        .assert()
        .success()
        .stdout(predicates::str::contains("FIXED     t01.flac"))
        .stdout(predicates::str::contains("FIXED     t02.flac"));

    lh().arg("sbe")
        .arg(out.join("t01.flac"))
        .arg(out.join("t02.flac"))
        .assert()
        .success()
        .stdout(predicates::str::contains("ALIGNED   t01.flac"))
        .stdout(predicates::str::contains("ALIGNED   t02.flac"));
}

/// R3: a directory with more than two files chains every boundary left to right instead
/// of being refused, the way R2 alone had to.
#[test]
fn sbe_fix_executes_a_chained_three_file_set() {
    let dir = tempfile::tempdir().unwrap();
    // Same fixtures reused for a third track: the point here is chaining through the CLI,
    // not this specific misalignment, and cdda-aligned.flac stays a stable "no-op" link.
    std::fs::copy(
        fixtures().join("cdda-sbe.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t02.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t03.flac"),
    )
    .unwrap();
    let out = dir.path().join("out");

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .args(["-o"])
        .arg(&out)
        .assert()
        .stdout(predicates::str::contains("FIXED     t01.flac"))
        .stdout(predicates::str::contains("FIXED     t02.flac"))
        .stdout(predicates::str::contains("FIXED     t03.flac"));

    assert!(out.join("t01.flac").exists());
    assert!(out.join("t02.flac").exists());
    assert!(out.join("t03.flac").exists());
}

/// The preview is the command: without `--yes`, the diff is printed and nothing is
/// written (docs/tagging.md §5).
#[test]
fn tag_without_yes_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    let before = std::fs::read(dir.path().join("t01.flac")).unwrap();

    lh().arg("tag")
        .arg(dir.path())
        .args(["--artist", "Grateful Dead"])
        .assert()
        .success()
        .stdout(predicates::str::contains("ARTIST"))
        .stdout(predicates::str::contains(
            "plan only, nothing written — pass --yes to write",
        ));

    assert_eq!(std::fs::read(dir.path().join("t01.flac")).unwrap(), before);
}

/// `--yes` writes the show-level fields to every taggable file plus the position-derived
/// `TRACKNUMBER` and count-derived `TRACKTOTAL`, and the audio survives untouched (docs/tagging.md §1 contract point 2).
#[test]
fn tag_yes_writes_artist_and_track_number() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();

    lh().arg("tag")
        .arg(dir.path())
        .args(["--artist", "Grateful Dead"])
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("wrote 1 files"));

    let tags = lh_core::tag::read(&dir.path().join("t01.flac")).unwrap();
    assert_eq!(tags.get(lh_core::tag::Field::Artist), Some("Grateful Dead"));
    assert_eq!(tags.get(lh_core::tag::Field::TrackNumber), Some("1"));
    assert_eq!(tags.get(lh_core::tag::Field::TrackTotal), Some("1"));
    assert_eq!(
        lh_core::analysis::verify(&dir.path().join("t01.flac")).unwrap(),
        lh_core::analysis::Verification::Ok
    );
}

/// A WAV in the same show folder has nowhere to put a Vorbis comment; it is reported as
/// not applicable rather than failing the whole run (docs/tagging.md §4).
#[test]
fn tag_reports_a_non_flac_member_as_not_applicable_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.wav"),
        dir.path().join("t02.wav"),
    )
    .unwrap();

    lh().arg("tag")
        .arg(dir.path())
        .args(["--artist", "Grateful Dead"])
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("N/A       t02.wav"))
        .stdout(predicates::str::contains("wrote 1 files"));
}

/// The WAVs a conversion leaves beside their FLACs are not tracks: numbering and the
/// `--titles` count run over the FLAC files alone, so `t01.wav` sorting between
/// `t01.flac` and `t02.flac` doesn't turn the second FLAC into track 3 of 4.
#[test]
fn tag_numbers_tracks_among_flac_files_only() {
    let dir = tempfile::tempdir().unwrap();
    for (fixture, name) in [
        ("cdda-aligned.flac", "t01.flac"),
        ("cdda-aligned.wav", "t01.wav"),
        ("cdda-sbe.flac", "t02.flac"),
        ("cdda-sbe.wav", "t02.wav"),
    ] {
        std::fs::copy(fixtures().join(fixture), dir.path().join(name)).unwrap();
    }
    let titles = dir.path().join("titles.txt");
    std::fs::write(&titles, "One\nTwo\n").unwrap();

    lh().arg("tag")
        .arg(dir.path())
        .args(["--titles"])
        .arg(&titles)
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("wrote 2 files"));

    let second = lh_core::tag::read(&dir.path().join("t02.flac")).unwrap();
    assert_eq!(second.get(lh_core::tag::Field::TrackNumber), Some("2"));
    assert_eq!(second.get(lh_core::tag::Field::TrackTotal), Some("2"));
    assert_eq!(second.get(lh_core::tag::Field::Title), Some("Two"));
}

/// A titles file with the wrong number of lines is a command failure naming both counts —
/// never a best-effort partial apply (docs/tagging.md §5).
#[test]
fn tag_titles_count_mismatch_is_a_command_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-sbe.flac"),
        dir.path().join("t02.flac"),
    )
    .unwrap();
    let titles = dir.path().join("titles.txt");
    std::fs::write(&titles, "Only One Title\n").unwrap();

    lh().arg("tag")
        .arg(dir.path())
        .args(["--titles"])
        .arg(&titles)
        .assert()
        .code(2)
        .stderr(predicates::str::contains("1 titles given but 2 FLAC files"));
}

/// The preview is the command here too: without `--yes` nothing is renamed.
#[test]
fn rename_without_yes_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("a.flac"),
    )
    .unwrap();

    lh().arg("rename")
        .arg(dir.path())
        .args(["--band", "gd", "--date", "1977-05-08"])
        .assert()
        .success()
        .stdout(predicates::str::contains("a.flac -> gd1977-05-08t01.flac"))
        .stdout(predicates::str::contains(
            "plan only, nothing written — pass --yes to rename",
        ));

    assert!(dir.path().join("a.flac").exists());
    assert!(!dir.path().join("gd1977-05-08t01.flac").exists());
}

/// `--yes` renames for real, numbering tracks by position, and band/date default from an
/// etree show folder name when not given explicitly (docs/tagging.md §5).
#[test]
fn rename_yes_renames_using_the_folder_name_for_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("gd1977-05-08.sbd.unknown");
    std::fs::create_dir(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("a.flac")).unwrap();
    std::fs::copy(fixtures().join("cdda-sbe.flac"), show.join("b.flac")).unwrap();

    lh().arg("rename")
        .arg(&show)
        .arg("--yes")
        .assert()
        .success()
        .stdout(predicates::str::contains("renamed 2 of 2 files"));

    assert!(show.join("gd1977-05-08t01.flac").exists());
    assert!(show.join("gd1977-05-08t02.flac").exists());
}

/// Neither the folder name nor a flag supplies the band: a specific command failure,
/// naming what is missing, not a silent guess (Principle 5).
#[test]
fn rename_without_band_or_an_etree_folder_name_is_a_command_failure() {
    let dir = tempfile::tempdir().unwrap();
    let show = dir.path().join("not_an_etree_name");
    std::fs::create_dir(&show).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), show.join("a.flac")).unwrap();

    lh().arg("rename")
        .arg(&show)
        .args(["--date", "1977-05-08"])
        .assert()
        .code(2)
        .stderr(predicates::str::contains("no --band given"));
}

/// A target that already exists and is not part of this rename must be refused, not
/// silently overwritten (Principle 1). The realistic way this happens: a file that failed
/// to probe is left out of the plan entirely, but is still sitting on disk under a name a
/// healthy file now computes as its own target.
#[test]
fn rename_refuses_to_overwrite_a_file_outside_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    // Not a real FLAC file, so it fails to probe and is left out of the rename plan
    // entirely — but it already occupies the name "z.flac" would compute to.
    std::fs::write(
        dir.path().join("gd1977-05-08t01.flac"),
        b"not really a flac file",
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.flac"),
        dir.path().join("z.flac"),
    )
    .unwrap();

    lh().arg("rename")
        .arg(dir.path())
        .args(["--band", "gd", "--date", "1977-05-08"])
        .arg("--yes")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("already exists"));

    assert!(
        dir.path().join("z.flac").exists(),
        "the source is untouched"
    );
    assert_eq!(
        std::fs::read(dir.path().join("gd1977-05-08t01.flac")).unwrap(),
        b"not really a flac file"
    );
}

/// `--in-place` replaces each changed file under its own name and moves the file it
/// replaced into `_original/sbe-fix/`, byte-for-byte itself.
#[test]
fn sbe_fix_in_place_sets_the_originals_aside() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("t01.flac");
    let b = dir.path().join("t02.flac");
    std::fs::copy(fixtures().join("cdda-sbe.flac"), &a).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.flac"), &b).unwrap();
    let before_a = std::fs::read(&a).unwrap();

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .arg("--in-place")
        .assert()
        .code(1) // t02 inherits t01's remainder and there is no --pad-tail
        .stdout(predicates::str::contains(
            "FIXED     t01.flac   original moved to",
        ))
        .stdout(predicates::str::contains("FIXED     t02.flac"));

    assert_eq!(
        std::fs::read(dir.path().join("_original/sbe-fix/t01.flac")).unwrap(),
        before_a
    );
    lh().arg("sbe")
        .arg(&a)
        .assert()
        .success()
        .stdout(predicates::str::contains("ALIGNED"));
}

/// R5: a folder of WAVs is fixed as WAVs — written under `-o` with their own names and
/// extension, every file aligned with `--pad-tail`, the sources untouched.
#[test]
fn sbe_fix_executes_on_wav() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("t01.wav");
    let b = dir.path().join("t02.wav");
    std::fs::copy(fixtures().join("cdda-sbe.wav"), &a).unwrap();
    std::fs::copy(fixtures().join("cdda-aligned.wav"), &b).unwrap();
    let before_a = std::fs::read(&a).unwrap();
    let out = dir.path().join("out");

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .arg("-o")
        .arg(&out)
        .arg("--pad-tail")
        .assert()
        .success()
        .stdout(predicates::str::contains("FIXED     t01.wav"))
        .stdout(predicates::str::contains("FIXED     t02.wav"));

    assert_eq!(std::fs::read(&a).unwrap(), before_a, "source untouched");
    lh().arg("sbe")
        .arg(out.join("t01.wav"))
        .arg(out.join("t02.wav"))
        .assert()
        .success();
}

/// A folder mixing WAV and FLAC is refused before anything is written: convert first.
#[test]
fn sbe_fix_refuses_a_mixed_wav_flac_folder() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(
        fixtures().join("cdda-sbe.flac"),
        dir.path().join("t01.flac"),
    )
    .unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.wav"),
        dir.path().join("t02.wav"),
    )
    .unwrap();

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .arg("--in-place")
        .assert()
        .code(2)
        .stderr(predicates::str::contains("convert first"));
    assert!(!dir.path().join("_original").exists());
}

/// The workspace's order, rename → sbe fix → convert → FLAC: WAVs fixed in place, then
/// converted with their sources set aside. The two steps set files aside in different
/// folders, so neither refuses, and the FLACs come out aligned.
#[test]
fn sbe_fix_on_wav_then_convert_to_flac() {
    use lh_core::tools::{Registry, ToolId};
    if Registry::discover_one(ToolId::Flac)
        .require(ToolId::Flac)
        .is_err()
    {
        eprintln!("skipping: reference flac not found");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("t01.wav");
    std::fs::copy(fixtures().join("cdda-sbe.wav"), &a).unwrap();
    std::fs::copy(
        fixtures().join("cdda-aligned.wav"),
        dir.path().join("t02.wav"),
    )
    .unwrap();
    let before_a = std::fs::read(&a).unwrap();

    lh().arg("sbe")
        .arg("fix")
        .arg(dir.path())
        .args(["--in-place", "--pad-tail"])
        .assert()
        .success();
    lh().arg("convert")
        .arg(dir.path())
        .args(["--to", "flac", "--move-sources"])
        .assert()
        .success();

    let originals = dir.path().join("_original");
    assert_eq!(
        std::fs::read(originals.join("sbe-fix/t01.wav")).unwrap(),
        before_a
    );
    assert!(
        originals.join("t01.wav").exists(),
        "the fixed WAV, set aside by convert"
    );
    assert!(!a.exists());
    lh().arg("sbe")
        .arg(dir.path().join("t01.flac"))
        .arg(dir.path().join("t02.flac"))
        .assert()
        .success()
        .stdout(predicates::prelude::PredicateBooleanExt::not(
            predicates::str::contains("SBE"),
        ));
}

/// docs/info-file.md: the preview writes nothing, `--yes` writes `bbyyyy-mm-dd.txt`, and a
/// second `--yes` refuses to replace it (exit 1, as `convert` does) until `--force`.
#[test]
fn setlist_previews_writes_and_refuses_to_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("gd1977-05-08.sbd.test.flac16");
    std::fs::create_dir(&dir).unwrap();
    let track = dir.join("gd1977-05-08t01.flac");
    std::fs::copy(fixtures().join("cdda-aligned.flac"), &track).unwrap();
    let edit = lh_core::tag::Tags {
        title: Some("Minglewood Blues".into()),
        ..Default::default()
    };
    lh_core::tag::apply(&track, &edit).unwrap();
    let txt = dir.join("gd1977-05-08.txt");

    lh().arg("setlist")
        .arg(&dir)
        .assert()
        .success()
        .stdout(predicates::str::contains("t01  Minglewood Blues"))
        .stdout(predicates::str::contains("plan only, nothing written"));
    assert!(!txt.exists());

    lh().arg("setlist")
        .arg(&dir)
        .arg("--yes")
        .assert()
        .success();
    assert!(txt.exists());

    std::fs::write(&txt, "hand-written").unwrap();
    lh().arg("setlist")
        .arg(&dir)
        .arg("--yes")
        .assert()
        .code(1)
        .stdout(predicates::str::contains("already exists"));
    assert_eq!(std::fs::read_to_string(&txt).unwrap(), "hand-written");

    lh().args(["setlist", "--yes", "--force"])
        .arg(&dir)
        .assert()
        .success();
    assert!(
        std::fs::read_to_string(&txt)
            .unwrap()
            .contains("Minglewood Blues")
    );
}
