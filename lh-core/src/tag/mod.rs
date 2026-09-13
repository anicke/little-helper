//! Vorbis comments, read and written to the etree tagging standard (docs/tagging.md §4).
//!
//! Eight fields, named by [wiki.etree.org/FlacMetadata](http://wiki.etree.org/index.php?page=FlacMetadata).
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
use crate::model::AudioFormat;
use std::path::Path;

/// One of the eight fields the etree standard names.
///
/// An enum rather than eight loose accessors because every caller past this module wants to
/// walk them: the CLI prints a diff of the ones that change, and the TUI draws the same diff
/// in a pane. Neither should carry its own copy of the list, or of the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Title,
    Artist,
    Album,
    Date,
    TrackNumber,
    Genre,
    Comment,
    Location,
}

impl Field {
    /// In the order a person reads them, not alphabetically: what the track is, then what
    /// the show was, then the notes about it.
    pub const ALL: [Field; 8] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::Date,
        Self::TrackNumber,
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
            Self::Genre => "GENRE",
            Self::Comment => "COMMENT",
            Self::Location => "LOCATION",
        }
    }

    /// Whether the field describes one track or the whole show. The show-level six are
    /// filled in once and applied to every file; only `TITLE` and `TRACKNUMBER` differ per
    /// track, which is what makes a show's tags a small form plus a list of titles.
    pub fn is_per_track(self) -> bool {
        matches!(self, Self::Title | Self::TrackNumber)
    }
}

/// The eight etree fields of one file.
///
/// Every field is a `String`, `TRACKNUMBER` included, because this type has to report what a
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

/// Read the eight etree fields. Every other comment in the file is ignored, not lost —
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
