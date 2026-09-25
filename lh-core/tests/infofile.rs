//! I1 of docs/info-file.md: an info file planned from real, tagged FLACs and written to disk.

use lh_core::error::Error;
use lh_core::infofile::{self, Problem};
use lh_core::scan;
use lh_core::tag::{self, Tags};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A show folder holding copies of fixtures under etree names, the FLACs tagged.
fn show(root: &tempfile::TempDir) -> PathBuf {
    let dir = root.path().join("gd1977-05-08.sbd.test.flac16");
    std::fs::create_dir(&dir).unwrap();
    for (src, dst, title) in [
        (
            "cdda-aligned.flac",
            "gd1977-05-08t01.flac",
            Some("Minglewood Blues"),
        ),
        ("cdda-sbe.flac", "gd1977-05-08t02.flac", None),
    ] {
        let path = dir.join(dst);
        std::fs::copy(fixture(src), &path).unwrap();
        let edit = Tags {
            artist: Some("Grateful Dead".into()),
            date: Some("1977-05-08".into()),
            title: title.map(str::to_string),
            ..Tags::default()
        };
        tag::apply(&path, &edit).unwrap();
    }
    std::fs::copy(fixture("cdda-aligned.wav"), dir.join("leftover.wav")).unwrap();
    dir
}

#[test]
fn plans_and_writes_a_crlf_info_file_once() {
    let root = tempfile::tempdir().unwrap();
    let dir = show(&root);
    let set = scan::scan(&dir, false).unwrap();
    let plan = infofile::plan(&dir, &set.files).unwrap();

    assert_eq!(plan.path, dir.join("gd1977-05-08.txt"));
    assert_eq!(plan.tracks.len(), 2);
    assert_eq!(plan.tracks[0].label, "t01");
    assert!(plan.problems.contains(&Problem::NotFlac {
        file: "leftover.wav".into()
    }));
    assert!(plan.problems.contains(&Problem::MissingTitle {
        file: "gd1977-05-08t02.flac".into()
    }));

    let written = infofile::write(&plan, false).unwrap();
    let bytes = std::fs::read(&written).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(
        text.starts_with("Grateful Dead\r\n1977-05-08\r\n"),
        "{text}"
    );
    assert!(text.contains("t01  Minglewood Blues"), "{text}");
    assert!(
        !text.replace("\r\n", "").contains('\n'),
        "bare LF in {text:?}"
    );

    // A second plan now sees the file, and writing again without `force` is refused.
    let again = infofile::plan(&dir, &set.files).unwrap();
    assert!(again.exists());
    assert!(matches!(
        infofile::write(&again, false),
        Err(Error::OutputExists { .. })
    ));
    infofile::write(&again, true).unwrap();
}

#[test]
fn reports_another_txt_already_in_the_folder() {
    let root = tempfile::tempdir().unwrap();
    let dir = show(&root);
    std::fs::write(dir.join("info.txt"), "hand-written").unwrap();
    let set = scan::scan(&dir, false).unwrap();
    let plan = infofile::plan(&dir, &set.files).unwrap();
    assert!(plan.problems.contains(&Problem::OtherTextFile {
        file: "info.txt".into()
    }));
}
