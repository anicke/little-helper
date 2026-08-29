//! T2 tests: finding a torrent's files on disk and comparing sizes.
//!
//! These build real directory trees in a temp dir, because path resolution is exactly the
//! part that cannot be tested against a mock.

use lh_core::Error;
use lh_core::torrent::{FileStatus, Metainfo, Verdict, check_sizes, join_checked, resolve_root};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/torrents")
        .join(name)
}

fn meta(name: &str) -> Metainfo {
    Metainfo::read(&fixture(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// Write a file of exactly `len` bytes.
fn put(root: &Path, rel: &str, len: u64) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, vec![b'x'; len as usize]).unwrap();
}

/// The layout multi-file.torrent describes: show/{d1t01,d1t02,d1t03}.flac
fn good_show(root: &Path) {
    put(root, "show/d1t01.flac", 100);
    put(root, "show/d1t02.flac", 250);
    put(root, "show/d1t03.flac", 150);
}

fn statuses(report: &lh_core::torrent::TorrentReport) -> Vec<&FileStatus> {
    report.files.iter().map(|f| &f.status).collect()
}

#[test]
fn all_sizes_matching_is_a_pass() {
    let tmp = TempDir::new().unwrap();
    good_show(tmp.path());
    let m = meta("multi-file.torrent");
    let report = check_sizes(&m, &fixture("multi-file.torrent"), tmp.path()).unwrap();

    assert_eq!(report.verdict(), Verdict::SizesMatch);
    assert!(report.quick, "sizes alone cannot conclude Complete");
    assert_eq!(statuses(&report), vec![&FileStatus::SizeOk; 3]);
    assert_eq!(report.failures().count(), 0);
}

/// Users point at either the containing folder or the show folder; both must work.
#[test]
fn root_resolves_from_either_side_of_the_show_folder() {
    let tmp = TempDir::new().unwrap();
    good_show(tmp.path());
    let m = meta("multi-file.torrent");

    let from_outside = resolve_root(&m, tmp.path()).unwrap();
    let from_inside = resolve_root(&m, &tmp.path().join("show")).unwrap();
    assert_eq!(from_outside, tmp.path().join("show"));
    assert_eq!(from_inside, tmp.path().join("show"));
}

#[test]
fn a_short_file_is_reported_with_both_sizes() {
    let tmp = TempDir::new().unwrap();
    good_show(tmp.path());
    put(tmp.path(), "show/d1t02.flac", 200); // 50 bytes short

    let m = meta("multi-file.torrent");
    let report = check_sizes(&m, &fixture("multi-file.torrent"), tmp.path()).unwrap();
    assert_eq!(report.verdict(), Verdict::Incomplete);
    assert_eq!(
        report.files[1].status,
        FileStatus::WrongSize {
            expected: 250,
            actual: 200
        }
    );
}

#[test]
fn a_missing_file_is_missing_not_wrong_size() {
    let tmp = TempDir::new().unwrap();
    good_show(tmp.path());
    std::fs::remove_file(tmp.path().join("show/d1t03.flac")).unwrap();

    let m = meta("multi-file.torrent");
    let report = check_sizes(&m, &fixture("multi-file.torrent"), tmp.path()).unwrap();
    assert_eq!(report.files[2].status, FileStatus::Missing);
    assert_eq!(report.failures().count(), 1);
}

/// Traders keep info.txt, artwork and .ffp sidecars beside a show. Acknowledge them,
/// never fail on them.
#[test]
fn extra_local_files_are_listed_but_do_not_fail_the_check() {
    let tmp = TempDir::new().unwrap();
    good_show(tmp.path());
    put(tmp.path(), "show/info.txt", 12);
    put(tmp.path(), "show/show.ffp", 40);

    let m = meta("multi-file.torrent");
    let report = check_sizes(&m, &fixture("multi-file.torrent"), tmp.path()).unwrap();
    assert_eq!(
        report.verdict(),
        Verdict::SizesMatch,
        "extras are not failures"
    );
    let names: Vec<_> = report
        .extra_local
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["info.txt", "show.ffp"]);
}

#[test]
fn nested_directories_are_resolved() {
    let tmp = TempDir::new().unwrap();
    put(tmp.path(), "show/disc1/d1t01.flac", 200);
    put(tmp.path(), "show/disc2/d2t01.flac", 300);

    let m = meta("nested-dirs.torrent");
    let report = check_sizes(&m, &fixture("nested-dirs.torrent"), tmp.path()).unwrap();
    assert_eq!(report.verdict(), Verdict::SizesMatch);
    assert!(report.files[0].path.ends_with("disc1/d1t01.flac"));
}

/// Padding never exists on disk. Reporting it as missing would call a good download broken.
#[test]
fn padding_is_not_expected_on_disk() {
    let tmp = TempDir::new().unwrap();
    put(tmp.path(), "show/d1t01.flac", 100);
    put(tmp.path(), "show/d1t02.flac", 250);

    let m = meta("padded.torrent");
    let report = check_sizes(&m, &fixture("padded.torrent"), tmp.path()).unwrap();
    assert_eq!(report.files[1].status, FileStatus::Padding);
    assert_eq!(report.verdict(), Verdict::SizesMatch);
    assert_eq!(report.failures().count(), 0);
}

/// A single-file torrent's directory belongs to the user; listing its other contents
/// would be noise, not information.
#[test]
fn single_file_torrents_do_not_report_extras() {
    let tmp = TempDir::new().unwrap();
    let m = meta("debian-13.6.0-amd64-netinst.iso.torrent");
    put(tmp.path(), &m.name, m.total_length.min(64));
    put(tmp.path(), "unrelated-holiday-photo.jpg", 10);

    let report = check_sizes(
        &m,
        &fixture("debian-13.6.0-amd64-netinst.iso.torrent"),
        tmp.path(),
    )
    .unwrap();
    assert!(report.extra_local.is_empty());
    // The stub file is the wrong size, which is the only thing it should complain about.
    assert!(matches!(
        report.files[0].status,
        FileStatus::WrongSize { .. }
    ));
}

#[test]
fn pointing_at_nothing_says_so() {
    let m = meta("multi-file.torrent");
    let err = resolve_root(&m, Path::new("/definitely/not/here")).unwrap_err();
    assert!(matches!(err, Error::Torrent { .. }), "{err}");
}

// --- join safety ---

#[test]
fn windows_reserved_device_names_are_refused() {
    let root = Path::new("/tmp/root");
    for name in ["CON", "con", "NUL.txt", "com1", "LPT9.flac"] {
        let err = join_checked(root, &[name.to_string()], Path::new("t.torrent"))
            .unwrap_err_or_else(name);
        assert!(
            err.to_string().contains("reserved device name"),
            "{name}: {err}"
        );
    }
}

#[test]
fn ordinary_names_still_join() {
    let root = Path::new("/tmp/root");
    let joined = join_checked(
        root,
        &["disc1".into(), "d1t01.flac".into()],
        Path::new("t.torrent"),
    )
    .unwrap();
    assert_eq!(joined, root.join("disc1").join("d1t01.flac"));
}

trait UnwrapErrOrElse<T> {
    fn unwrap_err_or_else(self, what: &str) -> Error;
}
impl<T: std::fmt::Debug> UnwrapErrOrElse<T> for Result<T, Error> {
    fn unwrap_err_or_else(self, what: &str) -> Error {
        self.expect_err(&format!("{what} should have been refused"))
    }
}
