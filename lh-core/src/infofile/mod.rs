//! A show's info `.txt`: header, setlist and track times, from the tags (docs/info-file.md).
//!
//! Three steps, the same plan-then-write split `rename` and `sbe_fix` use: [`plan`] reads
//! every file's tags and length and collects what it could not say cleanly, [`render`] turns
//! a plan into text with no I/O at all, and [`write`] puts that text on disk — refusing to
//! replace an existing file unless told to, since an info file is usually hand-written and
//! cannot be regenerated (docs/info-file.md §1).

use crate::display;
use crate::error::{Error, Result};
use crate::etree::{ShowName, TrackName};
use crate::model::AudioFile;
use crate::output::TempOutput;
use crate::tag::{self, Field, Tags};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The longest a CD-R holds. A disc past this is worth saying, because deciding between a
/// 74 and an 80 minute blank is what the wiki says disc timings are for.
const CD_MAX_SECS: f64 = 80.0 * 60.0;

/// The show-level fields the header and source line come from, in header order.
const SHOW_FIELDS: [Field; 5] = [
    Field::Artist,
    Field::Date,
    Field::Album,
    Field::Location,
    Field::Comment,
];

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    pub file_name: String,
    /// `d1t01`, or `t01` for a set without discs.
    pub label: String,
    pub disc: Option<u32>,
    pub title: Option<String>,
    /// `None` when the file's header does not state its length.
    pub secs: Option<f64>,
    pub is_cdda: bool,
}

/// Something the info file cannot say cleanly, reported rather than silently papered over
/// (Principle 5).
#[derive(Debug, Clone, PartialEq)]
pub enum Problem {
    MissingTitle {
        file: String,
    },
    /// A show-level field that is not the same on every file. The first file's value is used.
    Disagrees {
        field: Field,
        values: Vec<String>,
    },
    NoLength {
        file: String,
    },
    /// A CD-audio disc (`None`: the whole set, when it has no discs) too long for a CD-R.
    TooLongForCd {
        disc: Option<u32>,
        secs: f64,
    },
    NotFlac {
        file: String,
    },
    /// Another `.txt` already in the folder — most likely an info file under another name.
    OtherTextFile {
        file: String,
    },
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTitle { file } => write!(f, "{file} has no TITLE"),
            Self::Disagrees { field, values } => write!(
                f,
                "{} differs between files ({}); using the first",
                field.key(),
                values
                    .iter()
                    .map(|v| format!("{v:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::NoLength { file } => write!(f, "{file} does not state its length"),
            Self::TooLongForCd { disc, secs } => {
                let what = disc.map_or("the set".to_string(), |d| format!("disc {d}"));
                write!(
                    f,
                    "{what} runs {}, longer than an 80-minute CD-R",
                    display::duration_short(*secs)
                )
            }
            Self::NotFlac { file } => write!(f, "{file} is not FLAC, left out"),
            Self::OtherTextFile { file } => write!(f, "{file} is already in the folder"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct InfoPlan {
    /// Where the file goes: `<folder>/<bbyyyy-mm-dd>.txt`.
    pub path: PathBuf,
    /// The show-level tags, one value each — the first file's where they disagree.
    pub show: Tags,
    pub tracks: Vec<Track>,
    pub problems: Vec<Problem>,
}

impl InfoPlan {
    /// Whether any track carries a disc number — the only case disc headers are drawn in.
    pub fn has_discs(&self) -> bool {
        self.tracks.iter().any(|t| t.disc.is_some())
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

/// Read the tags and length of every FLAC in `files`, in the order given — `scan`'s order,
/// the one every other whole-set operation here uses.
///
/// `dir` is the show folder: it names the file and is checked for other `.txt` files.
pub fn plan(dir: &Path, files: &[AudioFile]) -> Result<InfoPlan> {
    let mut problems = Vec::new();
    let mut tracks = Vec::new();
    let mut seen: Vec<Vec<String>> = vec![Vec::new(); SHOW_FIELDS.len()];
    let mut show = Tags::default();

    let flacs: Vec<&AudioFile> = files
        .iter()
        .filter(|f| {
            let keep = tag::is_taggable(f.format);
            if !keep {
                problems.push(Problem::NotFlac {
                    file: f.file_name(),
                });
            }
            keep
        })
        .collect();

    for (i, f) in flacs.iter().enumerate() {
        let file_name = f.file_name();
        let tags = tag::read(&f.path)?;
        for (field, values) in SHOW_FIELDS.iter().zip(seen.iter_mut()) {
            if let Some(v) = tags.get(*field).filter(|v| !v.is_empty()) {
                if values.is_empty() {
                    show.set(*field, Some(v.to_string()));
                }
                if !values.iter().any(|seen| seen == v) {
                    values.push(v.to_string());
                }
            }
        }

        let parsed = TrackName::parse(&file_name);
        let (disc, number) = match &parsed {
            Some(n) => (n.disc, n.track),
            None => (None, i as u32 + 1),
        };
        let label = match disc {
            Some(d) => format!("d{d}t{number:02}"),
            None => format!("t{number:02}"),
        };
        let title = tags.title.filter(|t| !t.is_empty());
        if title.is_none() {
            problems.push(Problem::MissingTitle {
                file: file_name.clone(),
            });
        }
        let secs = f.stream_info.duration_secs();
        if secs.is_none() {
            problems.push(Problem::NoLength {
                file: file_name.clone(),
            });
        }
        tracks.push(Track {
            file_name,
            label,
            disc,
            title,
            secs,
            is_cdda: f.stream_info.is_cdda(),
        });
    }

    for (field, values) in SHOW_FIELDS.iter().zip(seen) {
        if values.len() > 1 {
            problems.push(Problem::Disagrees {
                field: *field,
                values,
            });
        }
    }
    for (disc, group) in discs(&tracks) {
        let secs = total_secs(group);
        if group.iter().all(|t| t.is_cdda) && secs > CD_MAX_SECS {
            problems.push(Problem::TooLongForCd { disc, secs });
        }
    }

    let path = dir.join(file_name(dir, &tracks));
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut others: Vec<String> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p != &path && p.is_file())
            .filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("txt")))
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        others.sort();
        problems.extend(
            others
                .into_iter()
                .map(|file| Problem::OtherTextFile { file }),
        );
    }

    Ok(InfoPlan {
        path,
        show,
        tracks,
        problems,
    })
}

/// The info file's text, with `\n` line endings; [`write`] turns them into CRLF.
pub fn render(plan: &InfoPlan) -> String {
    let mut out = String::new();
    for field in [Field::Artist, Field::Date, Field::Album, Field::Location] {
        if let Some(v) = plan.show.get(field) {
            let _ = writeln!(out, "{v}");
        }
    }
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Source: {}",
        plan.show.get(Field::Comment).unwrap_or_default()
    );
    let _ = writeln!(out);

    let title_width = plan
        .tracks
        .iter()
        .map(|t| title_of(t).chars().count())
        .max()
        .unwrap_or(0);
    let has_discs = plan.has_discs();
    for (disc, group) in discs(&plan.tracks) {
        if has_discs {
            let name = disc.map_or("Other".to_string(), |d| format!("Disc {d}"));
            let _ = writeln!(
                out,
                "{name} [{}]",
                display::duration_short(total_secs(group))
            );
        }
        for t in group {
            let time = t.secs.map_or("?:??".to_string(), display::duration_short);
            let _ = writeln!(
                out,
                "{}  {:<title_width$}  {:>5}",
                t.label,
                title_of(t),
                time
            );
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "Total: {}",
        display::duration_short(total_secs(&plan.tracks))
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Generated by little-helper {}",
        env!("CARGO_PKG_VERSION")
    );
    out
}

/// Write [`render`]'s text to `plan.path`, CRLF, UTF-8 without a BOM (docs/info-file.md §1).
/// Refuses an existing file unless `overwrite`; staged beside it and renamed in, so a
/// failed write never leaves half a file under the real name.
pub fn write(plan: &InfoPlan, overwrite: bool) -> Result<PathBuf> {
    let parent = plan.path.parent().unwrap_or(Path::new("."));
    let temp = TempOutput::stage(parent, &plan.path, overwrite)?;
    let text = render(plan).replace('\n', "\r\n");
    std::fs::write(temp.path(), text).map_err(|e| Error::io(temp.path(), e))?;
    temp.commit()
}

fn title_of(t: &Track) -> &str {
    t.title.as_deref().unwrap_or("(no title)")
}

fn total_secs(tracks: &[Track]) -> f64 {
    tracks.iter().filter_map(|t| t.secs).sum()
}

/// Consecutive runs of tracks sharing a disc number. Consecutive, not grouped, so a set
/// whose files are out of disc order shows that rather than hiding it.
fn discs(tracks: &[Track]) -> Vec<(Option<u32>, &[Track])> {
    let mut out = Vec::new();
    let mut start = 0;
    for i in 1..=tracks.len() {
        if i == tracks.len() || tracks[i].disc != tracks[start].disc {
            out.push((tracks[start].disc, &tracks[start..i]));
            start = i;
        }
    }
    out
}

/// `bbyyyy-mm-dd.txt`, per NamingStandards: from the folder's etree name, else the first
/// track's, else the folder's own name when neither is an etree name.
fn file_name(dir: &Path, tracks: &[Track]) -> String {
    let folder = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = ShowName::parse(&folder)
        .map(|s| format!("{}{}", s.band, s.date.render(s.year_form)))
        .or_else(|| {
            tracks
                .first()
                .and_then(|t| TrackName::parse(&t.file_name))
                .map(|n| format!("{}{}", n.band, n.date.render(n.year_form)))
        })
        .unwrap_or(folder);
    format!("{stem}.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(label: &str, disc: Option<u32>, title: Option<&str>, secs: f64) -> Track {
        Track {
            file_name: format!("gd1977-05-08{label}.flac"),
            label: label.to_string(),
            disc,
            title: title.map(str::to_string),
            secs: Some(secs),
            is_cdda: true,
        }
    }

    fn show() -> Tags {
        Tags {
            artist: Some("Grateful Dead".into()),
            date: Some("1977-05-08".into()),
            album: Some("Barton Hall".into()),
            ..Tags::default()
        }
    }

    #[test]
    fn renders_a_set_without_discs() {
        let plan = InfoPlan {
            path: PathBuf::from("gd1977-05-08.txt"),
            show: show(),
            tracks: vec![
                track("t01", None, Some("Minglewood Blues"), 330.4),
                track("t02", None, Some("Loser"), 425.6),
            ],
            problems: vec![],
        };
        let text = render(&plan);
        assert_eq!(
            text,
            format!(
                "Grateful Dead\n1977-05-08\nBarton Hall\n\nSource: \n\n\
                 t01  Minglewood Blues   5:30\n\
                 t02  Loser              7:06\n\n\
                 Total: 12:36\n\nGenerated by little-helper {}\n",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn disc_totals_sum_frames_not_rounded_times() {
        // Each track rounds down to 0:00; summed first, three of them round to 0:01.
        let plan = InfoPlan {
            path: PathBuf::from("x.txt"),
            show: Tags::default(),
            tracks: vec![
                track("d1t01", Some(1), Some("a"), 0.4),
                track("d1t02", Some(1), Some("b"), 0.4),
                track("d1t03", Some(1), Some("c"), 0.4),
                track("d2t01", Some(2), None, 61.0),
            ],
            problems: vec![],
        };
        let text = render(&plan);
        assert!(text.contains("Disc 1 [0:01]\n"), "{text}");
        assert!(text.contains("Disc 2 [1:01]\n"), "{text}");
        assert!(text.contains("d2t01  (no title)   1:01\n"), "{text}");
        assert!(text.contains("Total: 1:02\n"), "{text}");
    }

    #[test]
    fn names_the_file_from_the_folder_then_the_tracks() {
        let t = vec![track("t01", None, None, 1.0)];
        assert_eq!(
            file_name(Path::new("/x/gd1977-05-08.sbd.miller.flac16"), &t),
            "gd1977-05-08.txt"
        );
        assert_eq!(
            file_name(Path::new("/x/Cornell 77"), &t),
            "gd1977-05-08.txt"
        );
        assert_eq!(file_name(Path::new("/x/Cornell 77"), &[]), "Cornell 77.txt");
    }
}
