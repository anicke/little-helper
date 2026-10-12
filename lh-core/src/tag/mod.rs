//! Vorbis comments, read and written to the etree tagging standard (docs/tagging.md §4).
//!
//! Eight fields, named by [wiki.etree.org/FlacMetadata](http://wiki.etree.org/index.php?page=FlacMetadata),
//! plus `TRACKTOTAL`, which the etree page does not name but Xiph's own field
//! recommendations do, and which players (VLC, foobar2000) read to show "track 1 of 16".
//! They are ordinary Vorbis comments — "the only officially supported tagging mechanism in
//! FLAC", UTF-8 throughout — so this module is a thin, careful layer over `metaflac` rather
//! than a format implementation.
//!
//! Careful in two specific ways, both of which are docs/tagging.md §1's contract:
//!
//! * **Writing is additive.** [`apply`] sets the fields an edit names and leaves every other
//!   comment in the block exactly where it was, including the vendor string — that string is
//!   Principle 2's provenance marker and says which encoder produced the audio, which
//!   retagging does not change and must not appear to.
//! * **Audio is never touched, and that is checked.** A tag write rewrites a metadata block;
//!   the audio frames and the STREAMINFO MD5 over them are untouched. The etree wiki says as
//!   much when it tells people to correct metadata freely, "since it won't change the audio,
//!   nor will it change the FLAC Fingerprint (FFP)". [`assert_audio_unchanged`] turns that
//!   from a claim into a postcondition every caller is expected to enforce.
//!
//! The block-level pair (`read_comment_block`, `restore_comment_block`, `pub(crate)` since
//! only `repair` calls them) lives here too. It was written for SBE repair, which re-encodes
//! a file and has to put its tags back afterwards (docs/sbe-repair.md §4 step 5e); it is the
//! same concern as this module's, so it belongs here rather than privately inside `repair`.

use crate::checksum;
use crate::error::{Error, Result};
use crate::etree::ShowName;
use crate::model::{AudioFile, AudioFormat};
use std::path::Path;

/// One of the eight fields the etree standard names, or `TRACKTOTAL` (see the module docs).
///
/// An enum rather than nine loose accessors because every caller past this module wants to
/// walk them: the CLI prints a diff of the ones that change, and the TUI draws the same diff
/// in a pane. Neither should carry its own copy of the list, or of the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Artist,
    Album,
    Date,
    TrackNumber,
    TrackTotal,
    Genre,
    Comment,
    Location,
}

impl Field {
    /// In the order a person reads them, not alphabetically: what the track is, then what
    /// the show was, then the notes about it.
    pub const ALL: [Field; 9] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Date,
        Self::TrackNumber,
        Self::TrackTotal,
        Self::Genre,
        Self::Comment,
        Self::Location,
    ];

    /// The six show-level fields a person types, in the order a screen shows them. The
    /// other three are per-track (`TITLE`) or derived from position and count
    /// (`TRACKNUMBER`, `TRACKTOTAL`), never typed (docs/tagging.md §5).
    pub const SHOW: [Field; 6] = [
        Self::Artist,
        Self::Album,
        Self::Date,
        Self::Genre,
        Self::Comment,
        Self::Location,
    ];

    /// The Vorbis comment key, which is also what `metaflac` prints. Upper case is the
    /// convention the standard and every tool follow.
    pub fn key(self) -> &'static str {
        match self {
            Self::Title => "TITLE",
            Self::Artist => "ARTIST",
            Self::Album => "ALBUM",
            Self::Date => "DATE",
            Self::TrackNumber => "TRACKNUMBER",
            Self::TrackTotal => "TRACKTOTAL",
            Self::Genre => "GENRE",
            Self::Comment => "COMMENT",
            Self::Location => "LOCATION",
        }
    }

    /// Whether the field describes one track or the whole show. The show-level six are
    /// filled in once and applied to every file; only `TITLE` and `TRACKNUMBER` differ per
    /// track, which is what makes a show's tags a small form plus a list of titles.
    /// `TRACKTOTAL` is show-level too — the same count on every file — but, like
    /// `TRACKNUMBER`, it is derived from the files rather than typed.
    pub fn is_per_track(self) -> bool {
        matches!(self, Self::Title | Self::TrackNumber)
    }
}

/// The etree fields of one file, plus `TRACKTOTAL`.
///
/// Every field is a `String`, `TRACKNUMBER` and `TRACKTOTAL` included, because this type has to report what a
/// file actually says. Real files in circulation carry `1/17` and `01`, and typing the field
/// as a number would make [`read`] lossy and any diff built on it a lie about what is there.
/// [`Tags::track_number`] parses it for the callers that want the number.
///
/// `None` means *the file does not have this field*, and in an edit it means *leave it
/// alone*. To remove a field, set it to `Some(String::new())` — see [`apply`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub date: Option<String>,
    pub track_number: Option<String>,
    pub track_total: Option<String>,
    pub genre: Option<String>,
    pub comment: Option<String>,
    pub location: Option<String>,
}

impl Tags {
    pub fn get(&self, field: Field) -> Option<&str> {
        let value = match field {
            Field::Title => &self.title,
            Field::Artist => &self.artist,
            Field::Album => &self.album,
            Field::Date => &self.date,
            Field::TrackNumber => &self.track_number,
            Field::TrackTotal => &self.track_total,
            Field::Genre => &self.genre,
            Field::Comment => &self.comment,
            Field::Location => &self.location,
        };
        value.as_deref()
    }

    pub fn set(&mut self, field: Field, value: Option<String>) {
        let slot = match field {
            Field::Title => &mut self.title,
            Field::Artist => &mut self.artist,
            Field::Album => &mut self.album,
            Field::Date => &mut self.date,
            Field::TrackNumber => &mut self.track_number,
            Field::TrackTotal => &mut self.track_total,
            Field::Genre => &mut self.genre,
            Field::Comment => &mut self.comment,
            Field::Location => &mut self.location,
        };
        *slot = value;
    }

    /// The track number as a number, when it is one. `None` for a file that spells it
    /// `1/17`, or does not carry it at all.
    pub fn track_number(&self) -> Option<u32> {
        self.track_number.as_deref()?.parse().ok()
    }

    pub fn is_empty(&self) -> bool {
        Field::ALL.iter().all(|f| self.get(*f).is_none())
    }

    /// The edit track `index` of `total` gets: `show`'s show-level fields, its position as
    /// `TRACKNUMBER`, the count as `TRACKTOTAL`, and `title` (docs/tagging.md §5). Every
    /// front end builds a track's edit this way, so numbering cannot drift between them.
    pub fn for_track(show: &Tags, index: usize, total: usize, title: Option<String>) -> Tags {
        Tags {
            title,
            track_number: Some((index + 1).to_string()),
            track_total: Some(total.to_string()),
            ..show.clone()
        }
    }

    /// What a file carrying `self` carries once [`apply`] has written `edit`: the edit's
    /// value where it names one, nothing where it clears one (`Some("")`), and `self`'s
    /// value everywhere it is `None`. A screen shows this before a write lands, then swaps
    /// in a fresh [`read`] once it has.
    pub fn applied(&self, edit: &Tags) -> Tags {
        let mut out = self.clone();
        for field in Field::ALL {
            match edit.get(field) {
                None => {}
                Some("") => out.set(field, None),
                Some(value) => out.set(field, Some(value.to_string())),
            }
        }
        out
    }

    /// Every field where `self` and `other` disagree, in [`Field::ALL`] order, as
    /// `(field, before, after)`. This is what a preview renders — the diff the user confirms
    /// before anything is written (docs/tagging.md §1, contract point 1).
    ///
    /// A field the edit leaves `None` is not a change: it is untouched, not cleared.
    pub fn changes<'a>(&'a self, other: &'a Tags) -> Vec<(Field, Option<&'a str>, &'a str)> {
        Field::ALL
            .iter()
            .filter_map(|&field| {
                let after = other.get(field)?;
                let before = self.get(field);
                (before != Some(after)).then_some((field, before, after))
            })
            .collect()
    }
}

/// Whether a format can carry the tags in this module at all.
///
/// Only FLAC. WAV's own metadata is a `LIST`/`INFO` chunk, which is a different standard
/// that no etree convention names, and the rest of `AudioFormat` is unimplemented anyway.
/// Callers use this to *skip with a reason* rather than to fail: a WAV sitting in a show
/// folder is a normal thing to find, not an error (the same judgement `analysis::sbe` makes
/// for a non-CDDA file). [`read`] and [`apply`] still refuse loudly if called on one
/// regardless, because at that point somebody has ignored the answer.
pub fn is_taggable(format: AudioFormat) -> bool {
    matches!(format, AudioFormat::Flac)
}

/// Read the etree fields and `TRACKTOTAL`. Every other comment in the file is ignored, not lost —
/// [`apply`] preserves what it does not set.
///
/// A field the file does not carry comes back `None`. A field it carries more than once
/// (Vorbis permits repeats) comes back as the first value: the standard describes one value
/// per field, and picking the first is at least what every player does.
pub fn read(path: &Path) -> Result<Tags> {
    require_flac(path)?;
    let block = read_comment_block(path)?;
    let mut tags = Tags::default();
    let Some(block) = block else {
        return Ok(tags);
    };
    for field in Field::ALL {
        let value = block
            .get(field.key())
            .and_then(|values| values.first())
            .cloned();
        tags.set(field, value);
    }
    Ok(tags)
}

/// Write the fields `edit` names, and only those.
///
/// * `None` leaves the file's existing value alone — an edit says what to change, not what
///   the file should end up containing.
/// * `Some("")` removes the field, which is how a screen clears one.
/// * Every comment this edit does not name survives untouched, as does the vendor string.
///
/// The audio is not read or rewritten. Callers enforcing docs/tagging.md §1's contract read
/// [`checksum::ffp`] first and call [`assert_audio_unchanged`] after.
pub fn apply(path: &Path, edit: &Tags) -> Result<()> {
    require_flac(path)?;
    if edit.is_empty() {
        return Ok(());
    }
    let mut tag = open(path)?;
    for field in Field::ALL {
        match edit.get(field) {
            None => {}
            Some("") => tag.remove_vorbis(field.key()),
            Some(value) => tag.set_vorbis(field.key(), vec![value.to_string()]),
        }
    }
    save(path, tag)
}

/// [`apply`] under docs/tagging.md §1's contract: the audio MD5 is read before the write
/// and checked after it, so a write that touched the audio is an error, not a success.
pub fn write(path: &Path, edit: &Tags) -> Result<()> {
    let audio_before = checksum::ffp(path)?;
    apply(path, edit)?;
    assert_audio_unchanged(path, audio_before)
}

/// A show folder's tracks: its taggable files, in order, and what each carries now.
#[derive(Debug, Clone)]
pub struct Tracks {
    pub files: Vec<AudioFile>,
    /// Each file's tags as read — the "before" every diff and the audio check are against
    /// (docs/tagging.md §1 contract point 2).
    pub before: Vec<Tags>,
    /// Files left out for not being taggable: a WAV beside its converted FLAC is not a
    /// track, so it must not shift every track number.
    pub ignored: usize,
}

/// Keeps the taggable files of a folder's scan and reads each one's tags. An editor
/// refuses when `files` comes back empty, in its own words.
pub fn read_tracks(files: Vec<AudioFile>) -> Result<Tracks> {
    let total = files.len();
    let files: Vec<AudioFile> = files
        .into_iter()
        .filter(|f| is_taggable(f.format))
        .collect();
    let before = files
        .iter()
        .map(|f| read(&f.path))
        .collect::<Result<Vec<_>>>()?;
    Ok(Tracks {
        ignored: total - files.len(),
        files,
        before,
    })
}

/// The show-level fields an editor opens with, given every file's tags as read: the first
/// file that carries any, then the folder's etree date where that leaves `DATE` blank
/// (docs/tagging.md §6).
pub fn seed_show(before: &[Tags], dir: &Path) -> Tags {
    let mut show = before
        .iter()
        .find(|t| !t.is_empty())
        .cloned()
        .unwrap_or_default();
    if show.date.is_none() {
        show.date = ShowName::from_dir(dir).map(|n| n.date.render_iso());
    }
    show
}

/// One title per line of `text`, however its lines end: many terminals send a pasted
/// newline as a bare `\r`, which `str::lines` does not split on.
pub fn title_lines(text: &str) -> Vec<String> {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .lines()
        .map(str::to_string)
        .collect()
}

/// docs/tagging.md §1, contract point 2: the audio MD5 a file carried before a write must be
/// the one it carries after.
///
/// `before` is what [`checksum::ffp`] returned before the write — the MD5 of the unencoded
/// audio, read straight from STREAMINFO with no decode, so this costs a header read on each
/// side rather than two passes over the file. A mismatch is a bug in us, and it is reported
/// as one so a caller stops the rest of the run rather than repeating it across a show.
pub fn assert_audio_unchanged(path: &Path, before: [u8; 16]) -> Result<()> {
    let after = checksum::ffp(path)?;
    if after != before {
        return Err(Error::AudioChanged {
            path: path.to_path_buf(),
            before: hex::encode(before),
            after: hex::encode(after),
        });
    }
    Ok(())
}

/// The whole `VORBIS_COMMENT` block, for a caller that has to put it back verbatim rather
/// than edit fields in it.
///
/// `None` when the file carries no block at all — nothing to restore, as opposed to a block
/// with zero comments in it, which is still worth writing back so a custom vendor string
/// round-trips too. In practice every FLAC `convert` or `flac` itself produces has one; this
/// only matters for a file with no metadata block whatsoever.
pub(crate) fn read_comment_block(path: &Path) -> Result<Option<metaflac::block::VorbisComment>> {
    Ok(open(path)?.vorbis_comments().cloned())
}

/// Put a block's comment fields back onto a file, keeping whatever vendor string the file
/// currently has.
///
/// That last part is deliberate and is why this is not simply "write the block back": SBE
/// repair calls this on a file the reference `flac` binary has just written, and `flac`'s own
/// vendor string is the provenance marker the new file should keep (Principle 2). What is
/// being restored is what the tags said, not who encoded the audio they were attached to.
pub(crate) fn restore_comment_block(
    path: &Path,
    original: &Option<metaflac::block::VorbisComment>,
) -> Result<()> {
    let Some(original) = original else {
        return Ok(());
    };
    let mut tag = open(path)?;
    tag.vorbis_comments_mut().comments = original.comments.clone();
    save(path, tag)
}

fn require_flac(path: &Path) -> Result<()> {
    match AudioFormat::from_path(path) {
        Some(format) if is_taggable(format) => Ok(()),
        Some(format) => Err(Error::NotTaggable {
            path: path.to_path_buf(),
            format: format.name(),
        }),
        None => Err(Error::UnknownFormat {
            path: path.to_path_buf(),
        }),
    }
}

fn open(path: &Path) -> Result<metaflac::Tag> {
    metaflac::Tag::read_from_path(path).map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })
}

fn save(path: &Path, mut tag: metaflac::Tag) -> Result<()> {
    tag.save().map_err(|source| Error::FlacMeta {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_round_trips_through_get_and_set() {
        let mut tags = Tags::default();
        assert!(tags.is_empty());
        for field in Field::ALL {
            tags.set(field, Some(field.key().to_lowercase()));
        }
        for field in Field::ALL {
            assert_eq!(tags.get(field), Some(field.key().to_lowercase().as_str()));
        }
        assert!(!tags.is_empty());
    }

    #[test]
    fn only_title_and_track_number_are_per_track() {
        let per_track: Vec<_> = Field::ALL
            .into_iter()
            .filter(|f| f.is_per_track())
            .collect();
        assert_eq!(per_track, [Field::Title, Field::TrackNumber]);
    }

    /// A field the edit does not name is untouched, so it is not a change. This is what
    /// keeps a preview honest: it shows what will happen, not every field that exists.
    #[test]
    fn changes_lists_only_what_the_edit_would_alter() {
        let mut before = Tags::default();
        before.set(Field::Artist, Some("Grateful Dead".into()));
        before.set(Field::Title, Some("Bertha".into()));

        let mut edit = Tags::default();
        edit.set(Field::Artist, Some("Grateful Dead".into())); // same value, not a change
        edit.set(Field::Title, Some("Sugaree".into())); // changed
        edit.set(Field::Date, Some("1977-05-08".into())); // added
        // GENRE left None: untouched, not cleared.

        let changes = before.changes(&edit);
        assert_eq!(
            changes,
            [
                (Field::Title, Some("Bertha"), "Sugaree"),
                (Field::Date, None, "1977-05-08"),
            ]
        );
    }

    /// Clearing is spelled `Some("")`, and it is a change when there was something there.
    #[test]
    fn clearing_a_field_is_a_change() {
        let mut before = Tags::default();
        before.set(Field::Genre, Some("Polka".into()));
        let mut edit = Tags::default();
        edit.set(Field::Genre, Some(String::new()));
        assert_eq!(before.changes(&edit), [(Field::Genre, Some("Polka"), "")]);
    }

    #[test]
    fn for_track_numbers_by_position_and_keeps_the_show_fields() {
        let mut show = Tags::default();
        show.set(Field::Artist, Some("Grateful Dead".into()));
        show.set(Field::Title, Some("not a show field".into()));
        let edit = Tags::for_track(&show, 2, 12, Some("Sugaree".into()));
        assert_eq!(edit.get(Field::Artist), Some("Grateful Dead"));
        assert_eq!(edit.get(Field::Title), Some("Sugaree"));
        assert_eq!(edit.get(Field::TrackNumber), Some("3"));
        assert_eq!(edit.get(Field::TrackTotal), Some("12"));
        assert_eq!(edit.get(Field::Album), None);
    }

    #[test]
    fn seed_show_takes_the_first_tagged_file_then_the_folder_date() {
        let mut tagged = Tags::default();
        tagged.set(Field::Artist, Some("Grateful Dead".into()));
        let before = [Tags::default(), tagged];
        let seeded = seed_show(&before, Path::new("/shows/gd1977-05-08.sbd.shnf"));
        assert_eq!(seeded.get(Field::Artist), Some("Grateful Dead"));
        assert_eq!(seeded.get(Field::Date), Some("1977-05-08"));
        // A date the files carry wins over the folder's.
        let mut dated = Tags::default();
        dated.set(Field::Date, Some("1977-05-09".into()));
        let seeded = seed_show(&[dated], Path::new("/shows/gd1977-05-08.sbd.shnf"));
        assert_eq!(seeded.get(Field::Date), Some("1977-05-09"));
        assert_eq!(seed_show(&[], Path::new("/shows/My Show")), Tags::default());
    }

    #[test]
    fn title_lines_splits_every_newline_style() {
        for text in [
            "One\nTwo\nThree",
            "One\r\nTwo\r\nThree",
            "One\rTwo\rThree\r",
        ] {
            assert_eq!(title_lines(text), ["One", "Two", "Three"]);
        }
    }

    #[test]
    fn applied_sets_clears_and_keeps_as_apply_would() {
        let mut before = Tags::default();
        before.set(Field::Artist, Some("Grateful Dead".into()));
        before.set(Field::Genre, Some("Polka".into()));
        before.set(Field::Title, Some("Bertha".into()));

        let mut edit = Tags::default();
        edit.set(Field::Title, Some("Sugaree".into())); // set
        edit.set(Field::Genre, Some(String::new())); // cleared
        // ARTIST left None: kept.

        let after = before.applied(&edit);
        assert_eq!(after.get(Field::Title), Some("Sugaree"));
        assert_eq!(after.get(Field::Genre), None);
        assert_eq!(after.get(Field::Artist), Some("Grateful Dead"));
    }

    #[test]
    fn a_track_number_is_a_number_only_when_it_is_one() {
        let mut tags = Tags::default();
        tags.set(Field::TrackNumber, Some("01".into()));
        assert_eq!(tags.track_number(), Some(1));
        tags.set(Field::TrackNumber, Some("1/17".into()));
        assert_eq!(tags.track_number(), None);
        assert_eq!(tags.get(Field::TrackNumber), Some("1/17"));
    }

    #[test]
    fn only_flac_can_carry_these_tags() {
        assert!(is_taggable(AudioFormat::Flac));
        for other in [
            AudioFormat::Wav,
            AudioFormat::Shn,
            AudioFormat::Ape,
            AudioFormat::Wv,
            AudioFormat::Tta,
        ] {
            assert!(!is_taggable(other));
        }
    }

    #[test]
    fn reading_a_non_flac_says_so_specifically() {
        let err = read(Path::new("show/track.wav")).unwrap_err();
        assert!(matches!(err, Error::NotTaggable { .. }), "{err}");
        assert!(err.to_string().contains("WAV"), "{err}");

        let err = read(Path::new("show/notes.txt")).unwrap_err();
        assert!(matches!(err, Error::UnknownFormat { .. }), "{err}");
    }
}
