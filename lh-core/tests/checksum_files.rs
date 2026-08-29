//! Parsing and rendering the sidecar files. These have to cope with whatever is actually
//! circulating, which includes CRLF, BOMs, comments and both md5sum styles.

use lh_core::checksum::{ChecksumFile, ChecksumKind};
use std::path::Path;

fn parse(kind: ChecksumKind, text: &str) -> ChecksumFile {
    ChecksumFile::parse(kind, text, Path::new("<test>")).expect("should parse")
}

#[test]
fn ffp_round_trips() {
    let text = "d1t01.flac:0123456789abcdef0123456789abcdef\n\
                d1t02.flac:fedcba9876543210fedcba9876543210\n";
    let parsed = parse(ChecksumKind::Ffp, text);
    assert_eq!(parsed.entries.len(), 2);
    assert_eq!(parsed.entries[0].file_name, "d1t01.flac");
    assert_eq!(parsed.render(), text);
}

#[test]
fn md5_accepts_both_binary_and_text_styles() {
    let binary = parse(
        ChecksumKind::Md5,
        "0123456789abcdef0123456789abcdef *d1t01.flac\n",
    );
    let text = parse(
        ChecksumKind::Md5,
        "0123456789abcdef0123456789abcdef  d1t01.flac\n",
    );
    assert_eq!(binary.entries, text.entries);
    assert_eq!(binary.entries[0].file_name, "d1t01.flac");
}

#[test]
fn tolerates_crlf_bom_blank_lines_and_comments() {
    let text = "\u{feff}; made by some other tool\r\n\
                \r\n\
                # another comment\r\n\
                d1t01.flac:0123456789abcdef0123456789abcdef\r\n";
    let parsed = parse(ChecksumKind::Ffp, text);
    assert_eq!(parsed.entries.len(), 1);
    assert_eq!(parsed.entries[0].file_name, "d1t01.flac");
}

/// Filenames contain colons more often than you would like; split at the last one.
#[test]
fn ffp_filename_may_contain_a_colon() {
    let parsed = parse(
        ChecksumKind::Ffp,
        "1977-05-08 d1t01: scarlet.flac:0123456789abcdef0123456789abcdef\n",
    );
    assert_eq!(
        parsed.entries[0].file_name,
        "1977-05-08 d1t01: scarlet.flac"
    );
}

#[test]
fn syntax_errors_name_the_line() {
    let err = ChecksumFile::parse(
        ChecksumKind::Ffp,
        "d1t01.flac:nothex\n",
        Path::new("show.ffp"),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("show.ffp:1"), "{msg}");
    assert!(msg.contains("hexadecimal"), "{msg}");
}

#[test]
fn short_digests_are_rejected() {
    assert!(ChecksumFile::parse(ChecksumKind::Md5, "abcd  x.flac\n", Path::new("t")).is_err());
}
