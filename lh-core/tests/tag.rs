//! N2 of docs/tagging.md: the eight etree fields and `TRACKTOTAL`, read and written.
//!
//! Two things are being proved here, and only one of them is "the code does what it says".
//!
//! * **We are not the only thing that can read what we wrote.** The reference `metaflac` is
//!   the oracle, exactly as it is for FFP in `corpus.rs` — a tag file only we can read is not
//!   a tagged file, it is a private format.
//! * **A tag write does not touch audio** (docs/tagging.md §1, contract point 2). That is the
//!   promise the whole in-place decision rests on, so it is measured on every write here:
//!   the FFP before and after, and a full decode-and-verify afterwards.
//!
//! Tests needing `metaflac` skip when it is absent, the same convention `convert.rs` and
//! `sbe_fix.rs` use — Windows CI has no reference binaries.

use lh_core::analysis::{Verification, verify};
use lh_core::checksum::ffp;
use lh_core::tag::{self, Field, Tags};
use lh_core::tools::{Registry, Tool, ToolId};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// A writable copy of a fixture. Principle 1 in the test suite too: the corpus is committed
/// and nothing here may edit it in place.
fn copy_of(name: &str, dir: &tempfile::TempDir) -> PathBuf {
    let dst = dir.path().join(name);
    std::fs::copy(fixture(name), &dst).expect("fixture copies");
    dst
}

fn reference_metaflac() -> Option<Tool> {
    match Registry::discover_one(ToolId::Metaflac).require(ToolId::Metaflac) {
        Ok(t) => Some(t.clone()),
        Err(e) => {
            eprintln!("skipping: {e}");
            None
        }
    }
}

/// What `metaflac` itself says the file's tags are.
///
/// Runs the binary directly rather than through `tools::run`, which returns only a
/// `Provenance`-shaped `Agent` and deliberately discards stdout — here the stdout *is* the
/// answer.
fn metaflac_tags(tool: &Tool, path: &Path) -> BTreeMap<String, String> {
    let out = Command::new(&tool.path)
        .arg("--export-tags-to=-")
        .arg(path)
        .output()
        .expect("metaflac runs");
    assert!(
        out.status.success(),
        "metaflac failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("metaflac writes UTF-8")
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn full_edit() -> Tags {
    Tags {
        title: Some("Bertha".into()),
        artist: Some("Grateful Dead".into()),
        album: Some("Barton Hall, Ithaca, NY".into()),
        date: Some("1977-05-08".into()),
        track_number: Some("01".into()),
        track_total: Some("17".into()),
        genre: Some("Rock".into()),
        comment: Some("SBD > MR > DAT > CD > EAC > FLAC".into()),
        location: Some("Barton Hall, Cornell University, Ithaca, NY, USA".into()),
    }
}

/// The oracle test. Ours must not be the only reader of what we wrote.
#[test]
fn metaflac_reads_back_exactly_what_we_wrote() {
    let Some(metaflac) = reference_metaflac() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    let edit = full_edit();
    tag::apply(&path, &edit).unwrap();

    let theirs = metaflac_tags(&metaflac, &path);
    for field in Field::ALL {
        assert_eq!(
            theirs.get(field.key()).map(String::as_str),
            edit.get(field),
            "{}: metaflac disagrees with what we wrote",
            field.key()
        );
    }

    // And our own reader agrees with both.
    assert_eq!(tag::read(&path).unwrap(), edit);
}

/// Tag values are UTF-8, and the corpus already cares about non-ASCII names; it should care
/// about non-ASCII *values* too, since that is most of what a real setlist contains.
#[test]
fn non_ascii_values_survive_the_round_trip() {
    let Some(metaflac) = reference_metaflac() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    let mut edit = Tags::default();
    edit.set(Field::Artist, Some("Björk".into()));
    edit.set(Field::Title, Some("Jóga — live".into()));
    tag::apply(&path, &edit).unwrap();

    let ours = tag::read(&path).unwrap();
    assert_eq!(ours.get(Field::Artist), Some("Björk"));
    assert_eq!(ours.get(Field::Title), Some("Jóga — live"));

    let theirs = metaflac_tags(&metaflac, &path);
    assert_eq!(theirs.get("ARTIST").map(String::as_str), Some("Björk"));
}

/// docs/tagging.md §1, contract point 2, measured rather than asserted: the FFP is the same
/// before and after, `assert_audio_unchanged` agrees, and the file still decodes to the MD5
/// it claims. Three independent checks, because this is the promise the in-place decision
/// rests on.
#[test]
fn writing_tags_does_not_touch_the_audio() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    let before = ffp(&path).unwrap();
    tag::apply(&path, &full_edit()).unwrap();
    let after = ffp(&path).unwrap();

    assert_eq!(hex::encode(after), hex::encode(before), "the FFP moved");
    tag::assert_audio_unchanged(&path, before).unwrap();
    assert_eq!(verify(&path).unwrap(), Verification::Ok);
}

/// The file bytes *do* change — that is the whole point of MD5 being the checksum that
/// breaks on a retag (`checksum`'s own module table). Worth pinning so nobody later
/// "fixes" the FFP test by asserting the wrong invariant.
#[test]
fn the_file_md5_does_change_even_though_the_audio_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    let before = lh_core::checksum::md5(&path).unwrap();
    tag::apply(&path, &full_edit()).unwrap();
    assert_ne!(lh_core::checksum::md5(&path).unwrap(), before);
}

/// An edit says what to change, not what the file should end up containing. Anything it does
/// not name — another tool's comments, and the vendor string that records which encoder made
/// the audio (Principle 2) — has to survive untouched.
#[test]
fn an_edit_leaves_everything_it_does_not_name_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    // A comment from somewhere else entirely, plus one of ours to be overwritten.
    let mut existing = metaflac::Tag::read_from_path(&path).unwrap();
    existing.set_vorbis(
        "REPLAYGAIN_TRACK_GAIN".to_string(),
        vec!["-3.21 dB".to_string()],
    );
    existing.set_vorbis("ARTIST".to_string(), vec!["Wrong".to_string()]);
    existing.save().unwrap();
    let vendor_before = vendor_string(&path);

    let mut edit = Tags::default();
    edit.set(Field::Artist, Some("Grateful Dead".into()));
    tag::apply(&path, &edit).unwrap();

    let after = metaflac::Tag::read_from_path(&path).unwrap();
    let comments = &after.vorbis_comments().unwrap().comments;
    assert_eq!(
        comments.get("REPLAYGAIN_TRACK_GAIN").unwrap(),
        &vec!["-3.21 dB".to_string()],
        "an unrelated comment was lost"
    );
    assert_eq!(
        comments.get("ARTIST").unwrap(),
        &vec!["Grateful Dead".to_string()]
    );
    assert_eq!(
        vendor_string(&path),
        vendor_before,
        "the vendor string moved"
    );
    assert!(
        vendor_before.contains("reference libFLAC"),
        "the fixture should carry the reference encoder's own vendor string: {vendor_before}"
    );
}

/// `None` is "leave it alone"; `Some("")` is "remove it". A screen needs both, and they must
/// not be the same thing.
#[test]
fn an_empty_value_removes_a_field_and_none_leaves_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);

    tag::apply(&path, &full_edit()).unwrap();

    let mut edit = Tags::default();
    edit.set(Field::Genre, Some(String::new())); // remove
    // Everything else left None: untouched.
    tag::apply(&path, &edit).unwrap();

    let after = tag::read(&path).unwrap();
    assert_eq!(after.get(Field::Genre), None, "GENRE should be gone");
    assert_eq!(
        after.get(Field::Title),
        Some("Bertha"),
        "TITLE was untouched"
    );
    assert_eq!(after.get(Field::Artist), Some("Grateful Dead"));
}

/// The preview a user confirms has to describe the write that actually happens — every
/// change it lists happens, and nothing it does not list does.
#[test]
fn changes_predicts_exactly_what_apply_does() {
    let dir = tempfile::tempdir().unwrap();
    let path = copy_of("cdda-aligned.flac", &dir);
    tag::apply(&path, &full_edit()).unwrap();

    let before = tag::read(&path).unwrap();
    let mut edit = Tags::default();
    edit.set(Field::Title, Some("Sugaree".into())); // changed
    edit.set(Field::Artist, Some("Grateful Dead".into())); // same value, no change
    edit.set(Field::Genre, Some(String::new())); // removed

    let predicted = before.changes(&edit);
    assert_eq!(
        predicted.iter().map(|(f, _, _)| *f).collect::<Vec<_>>(),
        [Field::Title, Field::Genre],
        "ARTIST was set to the value it already had; that is not a change"
    );

    tag::apply(&path, &edit).unwrap();
    let after = tag::read(&path).unwrap();

    for field in Field::ALL {
        match predicted.iter().find(|(f, _, _)| *f == field) {
            Some((_, _, wanted)) => assert_eq!(
                after.get(field).unwrap_or(""),
                *wanted,
                "{}: predicted change did not happen",
                field.key()
            ),
            None => assert_eq!(
                after.get(field),
                before.get(field),
                "{}: changed without being predicted",
                field.key()
            ),
        }
    }
}

/// A fixture straight from the reference encoder carries a vendor string and no fields at
/// all — every field comes back `None`, not empty-string.
#[test]
fn a_file_with_no_tags_reads_as_entirely_absent() {
    let tags = tag::read(&fixture("cdda-aligned.flac")).unwrap();
    assert!(tags.is_empty(), "{tags:?}");
    for field in Field::ALL {
        assert_eq!(tags.get(field), None, "{}", field.key());
    }
}

/// A WAV in a show folder is a normal thing to find. Saying which format it is and what that
/// format cannot hold is Principle 5; a generic failure would not be.
#[test]
fn a_wav_is_refused_by_name() {
    let err = tag::read(&fixture("cdda-aligned.wav")).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("WAV"), "{message}");
    assert!(message.contains("Vorbis"), "{message}");

    let err = tag::apply(&fixture("cdda-aligned.wav"), &full_edit()).unwrap_err();
    assert!(err.to_string().contains("WAV"), "{err}");
}

fn vendor_string(path: &Path) -> String {
    metaflac::Tag::read_from_path(path)
        .unwrap()
        .vorbis_comments()
        .map(|vc| vc.vendor_string.clone())
        .unwrap_or_default()
}
