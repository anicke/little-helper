//! The etree naming standards, as types (docs/tagging.md §3).
//!
//! Two names carry the same facts in two shapes. A track is
//! `bbYYYY-MM-DDdNtNN.flac` — band, date, disc, track, optional title suffix — and the show
//! folder that holds it is `bbYYYY-MM-DD.mics.taper.sourceid.sbestatus.flac16`. This module
//! parses and renders the first, and only *parses* the second: renaming a show folder is
//! deliberately not a thing this tool can do (docs/tagging.md §2), and the cheapest way to
//! guarantee that is for the function that would do it not to exist.
//!
//! Pure: no I/O, no dependency, nothing that can fail slowly. A name is either an etree name
//! or it is not, so every parser returns `Option` and never a half-parse that silently drops
//! a segment — a caller that gets `Some` can trust every field in it.

use std::fmt;

/// Two-digit years below this are 20xx; the rest are 19xx.
///
/// Both year forms are in circulation (`gd1977-05-08` and `gd77-05-08` name the same show),
/// and a two-digit year cannot say which century it means. Live taping culture has nothing
/// from the 1930s to disambiguate against, so anything from `31` up is 19xx and the pivot
/// only has to be far enough ahead of today to keep parsing recent shows correctly.
const SHORT_YEAR_PIVOT: u16 = 30;

/// Which year form a name was written in. Kept because rendering has to give a name back in
/// the form it came in — rewriting `gd77-05-08d1t01` as `gd1977-05-08d1t01` would be a
/// rename nobody asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YearForm {
    /// `1977-05-08`
    Long,
    /// `77-05-08`
    Short,
}

/// The date of a show. Not a general calendar type — three fields and a validity check is
/// all any part of this standard asks for, which is why there is no date crate in the
/// workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShowDate {
    pub year: u16,
    pub month: u8,
    pub day: u8,
}

impl ShowDate {
    /// `None` for a date that does not exist. February 30th is a typo, not a show, and
    /// accepting it here would put it in a filename and in a `DATE` tag.
    pub fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
            return None;
        }
        Some(Self { year, month, day })
    }

    /// Parse `YYYY-MM-DD` or `YY-MM-DD`, reporting which form it was.
    ///
    /// Month and day must carry their leading zeros: `1977-5-8` is not a name this standard
    /// produces, and guessing at it would mean guessing at the filename it belongs to.
    pub fn parse(s: &str) -> Option<(Self, YearForm)> {
        let mut parts = s.split('-');
        let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || m.len() != 2 || d.len() != 2 {
            return None;
        }
        let form = match y.len() {
            4 => YearForm::Long,
            2 => YearForm::Short,
            _ => return None,
        };
        let year = parse_number(y)? as u16;
        let year = match form {
            YearForm::Long => year,
            YearForm::Short if year <= SHORT_YEAR_PIVOT => 2000 + year,
            YearForm::Short => 1900 + year,
        };
        let date = Self::new(year, parse_number(m)? as u8, parse_number(d)? as u8)?;
        Some((date, form))
    }

    pub fn render(&self, form: YearForm) -> String {
        match form {
            YearForm::Long => format!("{:04}-{:02}-{:02}", self.year, self.month, self.day),
            YearForm::Short => format!("{:02}-{:02}-{:02}", self.year % 100, self.month, self.day),
        }
    }

    /// ISO `YYYY-MM-DD`, which is what the `DATE` Vorbis comment wants regardless of which
    /// form the filename uses (docs/tagging.md §0).
    pub fn render_iso(&self) -> String {
        self.render(YearForm::Long)
    }
}

impl fmt::Display for ShowDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render_iso())
    }
}

/// One track's file name: `gd1977-05-08d1t01.flac`, or with a title suffix,
/// `gd1973-02-09d1t01bertha.shn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackName {
    pub band: String,
    pub date: ShowDate,
    pub year_form: YearForm,
    /// `None` for a set laid out without disc numbers at all.
    pub disc: Option<u32>,
    pub track: u32,
    /// Whatever followed the track number, if anything — usually the song title, run
    /// together and lowercase.
    pub suffix: Option<String>,
    /// Without the dot.
    pub ext: String,
}

impl TrackName {
    /// `None` unless the whole name is an etree track name. A name that merely starts like
    /// one (`gd1977-05-08.txt`, with no track number) is not one.
    ///
    /// The track number is the run of digits after `t`, so a suffix beginning with a digit
    /// is read as part of it. That is inherent in a format with no separator, and the
    /// alternative — stopping after two digits — would misread `t100`.
    pub fn parse(name: &str) -> Option<Self> {
        let (stem, ext) = name.rsplit_once('.')?;
        if ext.is_empty() || !ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        let (band, date, year_form, rest) = split_band_date(stem)?;

        let (disc, rest) = match rest.strip_prefix('d') {
            Some(after) => {
                let (disc, rest) = take_number(after)?;
                (Some(disc), rest)
            }
            None => (None, rest),
        };
        let (track, suffix) = take_number(rest.strip_prefix('t')?)?;

        Some(Self {
            band: band.to_string(),
            date,
            year_form,
            disc,
            track,
            suffix: (!suffix.is_empty()).then(|| suffix.to_string()),
            ext: ext.to_string(),
        })
    }

    /// The track number is always at least two digits.
    ///
    /// This is the one rule the standard states a consequence for: "without the 0, 10 would
    /// come before 2 during file sorting", and that sort order is what ends up burned to a
    /// disc. It lives here and nowhere else.
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(self.band.len() + 24);
        out.push_str(&self.band);
        out.push_str(&self.date.render(self.year_form));
        if let Some(disc) = self.disc {
            out.push('d');
            out.push_str(&disc.to_string());
        }
        out.push('t');
        if self.track < 10 {
            out.push('0');
        }
        out.push_str(&self.track.to_string());
        if let Some(suffix) = &self.suffix {
            out.push_str(suffix);
        }
        out.push('.');
        out.push_str(&self.ext);
        out
    }
}

impl fmt::Display for TrackName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.render())
    }
}

/// The format tag a show folder ends with. The standard is explicit that a FLAC folder says
/// how many bits it holds: "Don't use .flac or .flacf when naming a directory."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShowFormat {
    Shnf,
    Flac16,
    Flac24,
}

impl ShowFormat {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "shnf" => Self::Shnf,
            "flac16" => Self::Flac16,
            "flac24" => Self::Flac24,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shnf => "shnf",
            Self::Flac16 => "flac16",
            Self::Flac24 => "flac24",
        }
    }
}

impl fmt::Display for ShowFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A show folder's name: `gd1969-01-25.sbd.kaplan.7923.sbeok.shnf`.
///
/// **Parse only, on purpose.** This exists so a rename or a tag edit can offer the band and
/// date without anyone retyping them; renaming the folder itself is out of scope
/// (docs/tagging.md §2, §8 Q2), and there is deliberately no `render` here that could grow
/// into it by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShowName {
    pub band: String,
    pub date: ShowDate,
    pub year_form: YearForm,
    /// Everything between the date and the format tag, in order: mic description, taper,
    /// the db.etree.org source id, `sbeok`/`sbefail`. All optional in practice and all
    /// free-form, so they are kept as written rather than given fields that would be empty
    /// most of the time.
    pub segments: Vec<String>,
    /// `None` for a folder that names no format — common for older sets.
    pub format: Option<ShowFormat>,
}

impl ShowName {
    pub fn parse(name: &str) -> Option<Self> {
        let mut parts = name.split('.');
        let (band, date, year_form, rest) = split_band_date(parts.next()?)?;
        if !rest.is_empty() {
            return None;
        }
        let mut segments: Vec<String> = parts.map(str::to_string).collect();
        let format = segments.last().and_then(|s| ShowFormat::parse(s));
        if format.is_some() {
            segments.pop();
        }
        Some(Self {
            band: band.to_string(),
            date,
            year_form,
            segments,
            format,
        })
    }
}

/// Drop everything the standard does not allow in a name: "Use only letters, numbers,
/// hyphens (-), and when necessary, periods (.) or underscores (_). Do not include spaces,
/// ampersands (&), slashes or any other special characters."
///
/// Removes rather than substitutes, which is what the standard's own examples look like —
/// a title suffix reads `bertha`, not `Bertha_`. Case is left alone: lowercase is the
/// convention for a band abbreviation but not a rule, and this is a charset filter, not a
/// style one.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect()
}

/// Split a stem into its band, its date, and whatever follows.
///
/// The band has no fixed length and can itself end in a digit (`u2`, `sci`), so the date is
/// found rather than counted to: take the first `-` that has a whole valid date wrapped
/// around it, trying the four-digit year before the two-digit one. That order matters —
/// `u21983-02-23` is `u2` in 1983, and looking for a short year first would read it as `u219`
/// in 1983… and then fail on the rest, silently, in a way nobody would be able to explain.
fn split_band_date(stem: &str) -> Option<(&str, ShowDate, YearForm, &str)> {
    for (dash, _) in stem.match_indices('-') {
        for year_len in [4usize, 2] {
            let Some(start) = dash.checked_sub(year_len) else {
                continue;
            };
            // `get` rather than indexing: a band name can be non-ASCII, and a byte offset
            // landing mid-character must be skipped, not panicked on.
            let Some(text) = stem.get(start..dash + 6) else {
                continue;
            };
            let Some((date, form)) = ShowDate::parse(text) else {
                continue;
            };
            let band = stem.get(..start)?;
            if !is_band(band) {
                continue;
            }
            return Some((band, date, form, stem.get(dash + 6..)?));
        }
    }
    None
}

/// A band abbreviation has at least one letter in it.
///
/// Digits alone are not an abbreviation of anything, and requiring a letter is what stops
/// `1977-05-08d1t01.flac` — a name with no band at all — from being read as band `19` plus
/// the short-year date `77-05-08`. Both readings are arithmetically fine; only one of them
/// is a name anybody wrote.
fn is_band(s: &str) -> bool {
    s.chars().any(char::is_alphabetic)
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-'))
}

/// The leading run of digits, and what is left after it. `None` when there is no digit to
/// take, which is what makes a missing track number a parse failure rather than a zero.
fn take_number(s: &str) -> Option<(u32, &str)> {
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_digit())
        .unwrap_or(s.len());
    Some((parse_number(s.get(..end)?)?, s.get(end..)?))
}

/// ASCII digits only — `str::parse` would accept `+1` and a pile of non-ASCII digits, none
/// of which belong in a name this standard describes.
fn parse_number(s: &str) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every track-name example on the two wiki pages, plus the short-year form in
    /// circulation alongside them. Parsing and rendering have to agree about all of them,
    /// or a rename would quietly rewrite names that were already correct.
    #[test]
    fn wiki_track_names_round_trip() {
        for name in [
            "ph2000-04-20d1t01.shn",
            "gd1973-02-09d1t01bertha.shn",
            "gd77-05-08d1t01.flac",
            "jgb1987-01-25d2t14.flac",
        ] {
            let parsed = TrackName::parse(name).unwrap_or_else(|| panic!("{name} did not parse"));
            assert_eq!(parsed.render(), name, "{name} did not round-trip");
        }
    }

    #[test]
    fn track_name_fields() {
        let t = TrackName::parse("gd1973-02-09d1t01bertha.shn").unwrap();
        assert_eq!(t.band, "gd");
        assert_eq!(t.date, ShowDate::new(1973, 2, 9).unwrap());
        assert_eq!(t.year_form, YearForm::Long);
        assert_eq!(t.disc, Some(1));
        assert_eq!(t.track, 1);
        assert_eq!(t.suffix.as_deref(), Some("bertha"));
        assert_eq!(t.ext, "shn");
    }

    #[test]
    fn a_short_year_stays_short_and_still_means_1977() {
        let t = TrackName::parse("gd77-05-08d1t01.flac").unwrap();
        assert_eq!(t.date.year, 1977);
        assert_eq!(t.year_form, YearForm::Short);
        assert_eq!(t.render(), "gd77-05-08d1t01.flac");
        assert_eq!(t.date.render_iso(), "1977-05-08");
    }

    #[test]
    fn a_two_digit_year_picks_its_century_at_the_pivot() {
        assert_eq!(ShowDate::parse("30-01-01").unwrap().0.year, 2030);
        assert_eq!(ShowDate::parse("31-01-01").unwrap().0.year, 1931);
        assert_eq!(ShowDate::parse("99-12-31").unwrap().0.year, 1999);
        assert_eq!(ShowDate::parse("00-01-01").unwrap().0.year, 2000);
    }

    /// The standard's one stated consequence: without the leading zero, 10 sorts before 2,
    /// and that order is what gets burned to a disc.
    #[test]
    fn single_digit_tracks_are_padded_and_longer_ones_are_not() {
        let mut t = TrackName::parse("ph2000-04-20d1t01.shn").unwrap();
        for (track, expected) in [
            (1, "ph2000-04-20d1t01.shn"),
            (9, "ph2000-04-20d1t09.shn"),
            (10, "ph2000-04-20d1t10.shn"),
            (100, "ph2000-04-20d1t100.shn"),
        ] {
            t.track = track;
            assert_eq!(t.render(), expected);
        }
    }

    #[test]
    fn a_band_ending_in_a_digit_is_not_eaten_by_the_year() {
        let t = TrackName::parse("u21983-02-23d1t05.flac").unwrap();
        assert_eq!(t.band, "u2");
        assert_eq!(t.date.year, 1983);
        assert_eq!(t.track, 5);
    }

    #[test]
    fn a_set_without_disc_numbers_parses_and_renders_without_them() {
        let t = TrackName::parse("ph2000-04-20t07.flac").unwrap();
        assert_eq!(t.disc, None);
        assert_eq!(t.track, 7);
        assert_eq!(t.render(), "ph2000-04-20t07.flac");
    }

    /// Anything that is not an etree name must be `None` rather than a partial reading —
    /// a half-parse here becomes a wrong rename later.
    #[test]
    fn names_that_are_not_etree_names_do_not_parse() {
        for name in [
            "track01.flac",            // no band, no date
            "01 - Bertha.flac",        // the thing people rename *from*
            "gd1977-05-08.flac",       // a date, but no track
            "gd1977-05-08d1.flac",     // a disc, but still no track
            "gd1977-05-08d1t.flac",    // `t` with no number
            "1977-05-08d1t01.flac",    // no band
            "gd1977-13-08d1t01.flac",  // month 13
            "gd1977-02-30d1t01.flac",  // February 30th
            "gd1977-5-8d1t01.flac",    // no leading zeros
            "gd1977-05-08d1t01",       // no extension
            "gd1977-05-08d1t01.",      // empty extension
            "gd.1977-05-08d1t01.flac", // a dot where the standard has none
            "gd1977-05-08d1t01.fl ac", // space in the extension
        ] {
            assert!(
                TrackName::parse(name).is_none(),
                "{name} parsed but should not have"
            );
        }
    }

    #[test]
    fn leap_days_are_real_and_only_on_leap_years() {
        assert!(ShowDate::new(2000, 2, 29).is_some()); // divisible by 400
        assert!(ShowDate::new(1980, 2, 29).is_some());
        assert!(ShowDate::new(1900, 2, 29).is_none()); // divisible by 100, not 400
        assert!(ShowDate::new(1977, 2, 29).is_none());
    }

    /// Every folder example on the two wiki pages.
    #[test]
    fn wiki_show_folders_parse() {
        let s = ShowName::parse("gd1969-01-25.sbd.kaplan.7923.sbeok.shnf").unwrap();
        assert_eq!(s.band, "gd");
        assert_eq!(s.date, ShowDate::new(1969, 1, 25).unwrap());
        assert_eq!(s.segments, ["sbd", "kaplan", "7923", "sbeok"]);
        assert_eq!(s.format, Some(ShowFormat::Shnf));

        let s = ShowName::parse("jgb1987-01-25.nak.corley.19810.sbeok.shnf").unwrap();
        assert_eq!(s.band, "jgb");
        assert_eq!(s.segments, ["nak", "corley", "19810", "sbeok"]);

        // A hyphen inside a segment must not be mistaken for the date's.
        let s = ShowName::parse("gd1979-10-31.sbd-aud.shephard.9372.sbefail.shnf").unwrap();
        assert_eq!(s.date, ShowDate::new(1979, 10, 31).unwrap());
        assert_eq!(s.segments, ["sbd-aud", "shephard", "9372", "sbefail"]);

        let s = ShowName::parse("ph2003-04-20.flac16").unwrap();
        assert!(s.segments.is_empty());
        assert_eq!(s.format, Some(ShowFormat::Flac16));

        let s = ShowName::parse("ph2000-04-20.shnf").unwrap();
        assert_eq!(s.format, Some(ShowFormat::Shnf));
    }

    #[test]
    fn a_folder_naming_no_format_keeps_its_last_segment() {
        let s = ShowName::parse("gd1977-05-08.sbd.miller").unwrap();
        assert_eq!(s.format, None);
        assert_eq!(s.segments, ["sbd", "miller"]);
    }

    #[test]
    fn folders_that_are_not_etree_names_do_not_parse() {
        for name in [
            "Grateful Dead 1977-05-08",
            "gd1977-05-08d1", // a track-name fragment, not a folder
            "sbd.kaplan.7923",
            "downloads",
        ] {
            assert!(
                ShowName::parse(name).is_none(),
                "{name} parsed but should not have"
            );
        }
    }

    #[test]
    fn sanitize_drops_what_the_standard_forbids() {
        assert_eq!(sanitize("Scarlet Begonias"), "ScarletBegonias");
        assert_eq!(sanitize("drums/space"), "drumsspace");
        assert_eq!(sanitize("sugaree & co."), "sugareeco.");
        assert_eq!(sanitize("st._stephen-1"), "st._stephen-1");
        assert_eq!(sanitize("über"), "ber");
    }
}
