//! Every screen `lh-tui`'s workspace lists (`lh-tui/src/screens/workspace.rs`), grouped the
//! same way, and what each one does given only the show folder — the same defaults the
//! workspace uses. The batch screens build their jobs here; their rows read the way the
//! TUI's `RowStatus` impls do.

use gpui_kit::assets::IconName;
use lh_core::analysis::{Sbe, Verification, sbe, verify};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, EntryOutcome, check_entry, compute};
use lh_core::convert::{EncodeOpts, ORIGINALS_DIR, destination, to_flac, to_wav};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::scan;
use lh_core::tools::{Registry, ToolId};
use lh_core::torrent::{Metainfo, default_output};
use std::path::{Path, PathBuf};

use crate::batch::{Done, Job, Prepared, Tone};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Rename,
    ConvertFlac,
    ConvertWav,
    Tag,
    Setlist,
    Sample,
    Verify,
    Sbe,
    SbeFix,
    Check,
    Create(ChecksumKind),
    TorrentCreate,
    TorrentInfo,
    TorrentCheck,
}

pub struct Item {
    pub step: Step,
    pub label: &'static str,
    pub about: &'static str,
    pub icon: IconName,
    /// The `lh-tui` subcommand to use until this screen is ported; `None` once it is.
    pub tui_only: Option<&'static str>,
}

const fn item(step: Step, label: &'static str, about: &'static str, icon: IconName) -> Item {
    Item {
        step,
        label,
        about,
        icon,
        tui_only: None,
    }
}

const fn tui(
    step: Step,
    label: &'static str,
    about: &'static str,
    icon: IconName,
    command: &'static str,
) -> Item {
    Item {
        step,
        label,
        about,
        icon,
        tui_only: Some(command),
    }
}

pub const MENU: [(&str, &[Item]); 4] = [
    (
        "Prepare",
        &[
            tui(
                Step::Rename,
                "Rename",
                "Name files from band, date and track.",
                IconName::PencilLine,
                "rename",
            ),
            item(
                Step::ConvertFlac,
                "Convert → FLAC",
                "Encode WAVs with the reference flac; move checked ones to _original/.",
                IconName::FileMusic,
            ),
            item(
                Step::ConvertWav,
                "Convert → WAV",
                "Decode FLACs back to WAV.",
                IconName::AudioWaveform,
            ),
            tui(
                Step::Tag,
                "Tag",
                "Edit show fields and track titles.",
                IconName::Tag,
                "tag",
            ),
            tui(
                Step::Setlist,
                "Setlist",
                "Write the info .txt: setlist and times.",
                IconName::ListMusic,
                "setlist",
            ),
            tui(
                Step::Sample,
                "Sample",
                "Cut a short MP3 clip, beside the folder.",
                IconName::Scissors,
                "sample",
            ),
        ],
    ),
    (
        "Inspect",
        &[
            item(
                Step::Verify,
                "Verify",
                "Decode each FLAC and check it against its embedded MD5.",
                IconName::ShieldCheck,
            ),
            item(
                Step::Sbe,
                "SBE",
                "Find sector-boundary errors.",
                IconName::Ruler,
            ),
            tui(
                Step::SbeFix,
                "SBE fix",
                "Repair SBEs, moving replaced files to _original/sbe-fix/.",
                IconName::Wrench,
                "sbe fix --in-place",
            ),
        ],
    ),
    (
        "Checksums",
        &[
            item(
                Step::Check,
                "Check checksums",
                "Check every .ffp, .md5 and .st5 in the folder.",
                IconName::ListChecks,
            ),
            item(
                Step::Create(ChecksumKind::Ffp),
                "Create FFP",
                "Write <folder>.ffp inside the folder.",
                IconName::Hash,
            ),
            item(
                Step::Create(ChecksumKind::Md5),
                "Create MD5",
                "Write <folder>.md5 inside the folder.",
                IconName::Hash,
            ),
            item(
                Step::Create(ChecksumKind::St5),
                "Create ST5",
                "Write <folder>.st5 inside the folder.",
                IconName::Hash,
            ),
        ],
    ),
    (
        "Torrent",
        &[
            tui(
                Step::TorrentCreate,
                "Create torrent",
                "Hash the folder into <folder>.torrent beside it.",
                IconName::PackagePlus,
                "torrent create",
            ),
            item(
                Step::TorrentInfo,
                "Torrent info",
                "What a .torrent describes: hash, pieces, trackers, files.",
                IconName::Info,
            ),
            item(
                Step::TorrentCheck,
                "Torrent check",
                "Verify a folder against a .torrent's sizes and piece hashes.",
                IconName::PackageCheck,
            ),
        ],
    ),
];

pub fn items() -> impl Iterator<Item = &'static Item> {
    MENU.iter().flat_map(|(_, items)| items.iter())
}

/// How a screen last went, shown on its page and beside it in the sidebar.
#[derive(Clone)]
pub enum StepResult {
    NotRun,
    Clean(String),
    Unclean(String),
    /// The screen never ran, for the reason its subcommand would have printed.
    Refused(String),
}

impl StepResult {
    pub fn from_clean(ok: bool) -> Self {
        if ok {
            StepResult::Clean("Done.".into())
        } else {
            StepResult::Unclean("Finished with problems — see the table.".into())
        }
    }
}

/// A path's last component for display, or the whole path when it has none.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// One show folder's audio files, not recursive; refuses a folder with none.
fn scan_folder(dir: &Path) -> Result<Vec<AudioFile>, String> {
    let set = scan::scan(dir, false).map_err(|e| format!("scanning {}: {e:#}", dir.display()))?;
    if set.files.is_empty() {
        return Err(format!("No audio files in {}.", file_name(dir)));
    }
    Ok(set.files)
}

/// What is in the folder right now, by format — `12 files: 12 WAV`.
pub fn summarize(dir: &Path) -> String {
    let set = match scan::scan(dir, false) {
        Ok(s) => s,
        Err(e) => return format!("scanning failed: {e:#}"),
    };
    if set.files.is_empty() {
        return "no audio files".into();
    }
    let mut by_format: Vec<(AudioFormat, usize)> = Vec::new();
    for f in &set.files {
        match by_format.iter_mut().find(|(format, _)| *format == f.format) {
            Some((_, n)) => *n += 1,
            None => by_format.push((f.format, 1)),
        }
    }
    let parts: Vec<String> = by_format
        .iter()
        .map(|(format, n)| format!("{n} {format}"))
        .collect();
    let mut line = format!("{} files: {}", set.files.len(), parts.join(", "));
    if !set.skipped.is_empty() {
        line.push_str(&format!(" ({} skipped)", set.skipped.len()));
    }
    line
}

/// `<folder>.torrent` beside the folder, or else the one `.torrent` inside it.
pub fn find_torrent(dir: &Path) -> Result<(PathBuf, Metainfo), String> {
    let file = match default_output(dir).filter(|p| p.is_file()) {
        Some(p) => p,
        None => {
            let inside: Vec<PathBuf> = std::fs::read_dir(dir)
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "torrent"))
                .collect();
            match <[PathBuf; 1]>::try_from(inside) {
                Ok([one]) => one,
                Err(inside) if inside.is_empty() => {
                    return Err(
                        "No <folder>.torrent beside the folder, nor a .torrent in it.".into(),
                    );
                }
                Err(_) => return Err("Several .torrent files in the folder; choose one.".into()),
            }
        }
    };
    let meta = Metainfo::read(&file).map_err(|e| format!("reading {}: {e:#}", file_name(&file)))?;
    Ok((file, meta))
}

/// The jobs for a batch step on `dir`, or why it can't run.
pub fn prepare(step: Step, dir: &Path) -> Result<Prepared, String> {
    match step {
        Step::Verify => prepare_verify(dir),
        Step::Sbe => prepare_sbe(dir),
        Step::ConvertFlac => prepare_convert(dir, AudioFormat::Flac),
        Step::ConvertWav => prepare_convert(dir, AudioFormat::Wav),
        Step::Check => prepare_check(dir),
        Step::Create(kind) => prepare_create(dir, kind),
        _ => unreachable!("not a batch step"),
    }
}

fn names(files: &[AudioFile]) -> Vec<String> {
    files.iter().map(AudioFile::file_name).collect()
}

fn from_clean() -> crate::batch::Finish {
    Box::new(|clean, _| StepResult::from_clean(clean))
}

fn prepare_verify(dir: &Path) -> Result<Prepared, String> {
    let files = scan_folder(dir)?;
    let jobs = files
        .iter()
        .map(|f| {
            let path = f.path.clone();
            Box::new(move |_: &_| match verify(&path) {
                Ok(Verification::Ok) => Done::good("OK", Tone::Ok, ""),
                Ok(Verification::NoStoredMd5 { .. }) => {
                    Done::good("NO MD5", Tone::Warn, "decoded cleanly, nothing to compare")
                }
                Ok(Verification::Md5Mismatch { stored, computed }) => Done::bad(
                    "MISMATCH",
                    Tone::Bad,
                    format!(
                        "stored {} computed {}",
                        hex::encode(stored),
                        hex::encode(computed)
                    ),
                ),
                Err(e) => Done::failed(e.to_string()),
            }) as Job
        })
        .collect();
    Ok(Prepared {
        names: names(&files),
        jobs,
        labels: &["OK", "NO MD5", "MISMATCH", "FAILED"],
        detail: "Detail",
        finish: from_clean(),
    })
}

fn prepare_sbe(dir: &Path) -> Result<Prepared, String> {
    let files = scan_folder(dir)?;
    let jobs = files
        .iter()
        .map(|f| {
            let info = f.stream_info.clone();
            Box::new(move |_: &_| match sbe(&info) {
                Sbe::Aligned => Done::good("ALIGNED", Tone::Ok, ""),
                Sbe::Misaligned { remainder_frames } => Done::bad(
                    "MISALIGNED",
                    Tone::Bad,
                    format!("+{remainder_frames} frames past a sector boundary"),
                ),
                // Neutral, not a warning: most non-CDDA files hit this.
                Sbe::NotApplicable { reason } => Done::good("N/A", Tone::Neutral, reason),
            }) as Job
        })
        .collect();
    Ok(Prepared {
        names: names(&files),
        jobs,
        labels: &["ALIGNED", "N/A", "MISALIGNED", "FAILED"],
        detail: "Detail",
        finish: from_clean(),
    })
}

/// The workspace's convert: level 8, no `--force`, and to FLAC with `--move-sources`, so
/// a checked WAV moves into `_original/` and the steps after see only FLACs.
fn prepare_convert(dir: &Path, want: AudioFormat) -> Result<Prepared, String> {
    let files = scan_folder(dir)?;
    let encoder = match want {
        AudioFormat::Flac => Some(
            Registry::discover_one(ToolId::Flac)
                .require(ToolId::Flac)
                .cloned()
                .map_err(|e| format!("{e:#}"))?,
        ),
        _ => None,
    };
    let extension = if want == AudioFormat::Flac {
        "flac"
    } else {
        "wav"
    };
    let move_sources = want == AudioFormat::Flac;

    let jobs = files
        .iter()
        .map(|f| {
            let path = f.path.clone();
            let format = f.format;
            let encoder = encoder.clone();
            Box::new(move |progress: &lh_core::job::Progress<Done>| {
                if format == want {
                    return Done::good("SKIPPED", Tone::Neutral, format!("already {want}"));
                }
                let Ok(dst) = destination(&path, extension, None) else {
                    return Done::failed("has no file name to work from");
                };
                let on_progress = &mut |done, total| {
                    progress.report(done, total);
                    !progress.is_cancelled()
                };
                let result = match &encoder {
                    None => to_wav(&path, &dst, false, on_progress),
                    Some(flac) => to_flac(
                        &path,
                        &dst,
                        flac,
                        &EncodeOpts::default(),
                        false,
                        on_progress,
                    ),
                };
                let done = match result {
                    Ok(done) => done,
                    Err(e) => return Done::failed(e.to_string()),
                };
                let mut detail = format!("→ {}", file_name(&done.output));
                if !done.checked_against_source {
                    detail.push_str("  (unchecked: nothing to compare against)");
                }
                if !move_sources {
                    return Done::good("OK", Tone::Ok, detail);
                }
                match done.move_source_to_originals() {
                    Ok(_) => Done::good(
                        "OK",
                        Tone::Ok,
                        format!("{detail}, source → {ORIGINALS_DIR}/"),
                    ),
                    // The FLAC stands, but the folder still holds a source it was meant not to.
                    Err(e) => Done::bad("KEPT", Tone::Warn, format!("{detail}, source kept: {e}")),
                }
            }) as Job
        })
        .collect();
    Ok(Prepared {
        names: names(&files),
        jobs,
        labels: &["OK", "KEPT", "SKIPPED", "FAILED"],
        detail: "Detail",
        finish: from_clean(),
    })
}

/// Every entry of every `.ffp`/`.md5`/`.st5` in the folder, as one table — the TUI runs one
/// screen per list instead.
fn prepare_check(dir: &Path) -> Result<Prepared, String> {
    let mut lists: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && ChecksumKind::from_path(p).is_some())
        .collect();
    if lists.is_empty() {
        return Err("No .ffp, .md5 or .st5 file in the folder.".into());
    }
    lists.sort();

    let several = lists.len() > 1;
    let mut row_names = Vec::new();
    let mut jobs: Vec<Job> = Vec::new();
    for list_path in &lists {
        let kind = ChecksumKind::from_path(list_path).expect("filtered above");
        let list = ChecksumFile::read(kind, list_path)
            .map_err(|e| format!("reading {}: {e:#}", file_name(list_path)))?;
        let list_dir = list_path.parent().unwrap_or(dir).to_path_buf();
        for entry in list.entries {
            row_names.push(if several {
                format!("{} · {}", file_name(list_path), entry.file_name)
            } else {
                entry.file_name.clone()
            });
            let list_dir = list_dir.clone();
            jobs.push(Box::new(move |_| {
                match check_entry(kind, &list_dir, &entry) {
                    EntryOutcome::Ok => Done::good("OK", Tone::Ok, ""),
                    EntryOutcome::Missing => Done::bad("MISSING", Tone::Warn, "no such file"),
                    EntryOutcome::Mismatch { expected, actual } => Done::bad(
                        "MISMATCH",
                        Tone::Bad,
                        format!(
                            "expected {} actual {}",
                            hex::encode(expected),
                            hex::encode(actual)
                        ),
                    ),
                    EntryOutcome::Failed(e) => Done::failed(e.to_string()),
                }
            }));
        }
    }
    if jobs.is_empty() {
        return Err("The checksum files have no entries.".into());
    }

    let names: Vec<String> = lists.iter().map(|p| file_name(p)).collect();
    Ok(Prepared {
        names: row_names,
        jobs,
        labels: &["OK", "MISSING", "MISMATCH", "FAILED"],
        detail: "Detail",
        finish: Box::new(move |clean, _| {
            if clean {
                StepResult::Clean(format!("{} checked out.", names.join(", ")))
            } else {
                StepResult::from_clean(false)
            }
        }),
    })
}

/// Writes `<folder>.<ext>` inside the folder — only once every file has computed, so a
/// cancel or a failure never leaves a partial list there looking complete.
fn prepare_create(dir: &Path, kind: ChecksumKind) -> Result<Prepared, String> {
    let out = dir.join(format!("{}.{}", file_name(dir), kind.extension()));
    if out.exists() {
        return Err(format!("{} already exists.", file_name(&out)));
    }
    let files = scan_folder(dir)?;
    let jobs = files
        .iter()
        .map(|f| {
            let path = f.path.clone();
            Box::new(move |_: &_| match compute(kind, &path) {
                Ok(digest) => Done {
                    digest: Some(digest),
                    ..Done::good("OK", Tone::Ok, hex::encode(digest))
                },
                Err(e) => Done::failed(e.to_string()),
            }) as Job
        })
        .collect();
    let file_names = names(&files);
    Ok(Prepared {
        names: file_names.clone(),
        jobs,
        labels: &["OK", "FAILED"],
        detail: "Digest",
        finish: Box::new(move |clean, digests| {
            let shown = file_name(&out);
            if !clean {
                return StepResult::Unclean(format!(
                    "Not every file computed; {shown} not written."
                ));
            }
            let mut list = ChecksumFile::new(kind);
            list.entries = file_names
                .into_iter()
                .zip(digests)
                .filter_map(|(file_name, digest)| {
                    Some(Entry {
                        file_name,
                        digest: digest?,
                    })
                })
                .collect();
            match list.write(&out) {
                Ok(()) => StepResult::Clean(format!("Wrote {shown}.")),
                Err(e) => StepResult::Unclean(format!("writing {shown}: {e:#}")),
            }
        }),
    })
}

/// What each batch step does on a real folder, without a window: its jobs run in turn on
/// this thread, the way the queue would run them, and the step's finish sees the rows.
#[cfg(test)]
mod tests {
    use super::*;
    use lh_core::job::{CancelToken, Progress};

    /// A show folder of fixtures, fresh per test.
    fn show(name: &str, fixtures: &[&str]) -> PathBuf {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../lh-core/tests/fixtures");
        let dir = std::env::temp_dir().join(format!("lh-gpui-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in fixtures {
            std::fs::copy(src.join(f), dir.join(f)).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    /// Each row's (name, label), and the step's result.
    fn run(step: Step, dir: &Path) -> (Vec<(String, &'static str)>, StepResult) {
        let prepared = prepare(step, dir).unwrap_or_else(|e| panic!("refused: {e}"));
        let progress = Progress::detached(CancelToken::new());
        let done: Vec<Done> = prepared
            .jobs
            .into_iter()
            .map(|job| job(&progress))
            .collect();
        for d in &done {
            assert!(
                prepared.labels.contains(&d.label),
                "{} not in the tally",
                d.label
            );
        }
        let clean = done.iter().all(|d| !d.bad);
        let rows = prepared
            .names
            .into_iter()
            .zip(done.iter().map(|d| d.label))
            .collect();
        let result = (prepared.finish)(clean, done.iter().map(|d| d.digest).collect());
        (rows, result)
    }

    fn label_of<'a>(rows: &'a [(String, &'static str)], name: &str) -> &'a str {
        rows.iter().find(|r| r.0 == name).unwrap().1
    }

    #[test]
    fn verify_reads_each_outcome() {
        let dir = show(
            "verify",
            &["cdda-aligned.flac", "wrong-md5.flac", "truncated.flac"],
        );
        let (rows, result) = run(Step::Verify, &dir);
        assert_eq!(label_of(&rows, "cdda-aligned.flac"), "OK");
        assert_eq!(label_of(&rows, "wrong-md5.flac"), "MISMATCH");
        assert_eq!(label_of(&rows, "truncated.flac"), "FAILED");
        assert!(matches!(result, StepResult::Unclean(_)));
    }

    #[test]
    fn sbe_flags_the_misaligned_file() {
        let dir = show("sbe", &["cdda-aligned.flac", "cdda-sbe.flac"]);
        let (rows, result) = run(Step::Sbe, &dir);
        assert_eq!(label_of(&rows, "cdda-aligned.flac"), "ALIGNED");
        assert_eq!(label_of(&rows, "cdda-sbe.flac"), "MISALIGNED");
        assert!(matches!(result, StepResult::Unclean(_)));
    }

    #[test]
    fn create_writes_a_list_that_check_accepts_and_refuses_a_second_time() {
        let dir = show("ffp", &["cdda-aligned.flac", "hires-24bit.flac"]);
        let (rows, result) = run(Step::Create(ChecksumKind::Ffp), &dir);
        assert!(rows.iter().all(|r| r.1 == "OK"));
        assert!(matches!(result, StepResult::Clean(_)));
        let list = dir.join(format!("{}.ffp", file_name(&dir)));
        assert!(list.is_file());

        assert!(prepare(Step::Create(ChecksumKind::Ffp), &dir).is_err());

        let (rows, result) = run(Step::Check, &dir);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.1 == "OK"));
        assert!(matches!(result, StepResult::Clean(_)));
    }

    #[test]
    fn check_reports_a_missing_file() {
        let dir = show("missing", &["cdda-aligned.flac", "hires-24bit.flac"]);
        run(Step::Create(ChecksumKind::Md5), &dir);
        std::fs::remove_file(dir.join("hires-24bit.flac")).unwrap();
        let (rows, result) = run(Step::Check, &dir);
        assert_eq!(label_of(&rows, "hires-24bit.flac"), "MISSING");
        assert!(matches!(result, StepResult::Unclean(_)));
    }

    #[test]
    fn convert_to_flac_moves_checked_sources_aside_and_back_to_wav_skips_wavs() {
        let dir = show("convert", &["cdda-aligned.wav", "cdda-sbe.flac"]);
        let (rows, result) = run(Step::ConvertFlac, &dir);
        assert_eq!(label_of(&rows, "cdda-aligned.wav"), "OK");
        assert_eq!(label_of(&rows, "cdda-sbe.flac"), "SKIPPED");
        assert!(matches!(result, StepResult::Clean(_)));
        assert!(dir.join("cdda-aligned.flac").is_file());
        assert!(dir.join(ORIGINALS_DIR).join("cdda-aligned.wav").is_file());

        let (rows, _) = run(Step::ConvertWav, &dir);
        assert!(rows.iter().all(|r| r.1 == "OK"));
        assert!(dir.join("cdda-sbe.wav").is_file());
    }

    #[test]
    fn a_folder_without_audio_or_lists_is_refused() {
        let dir = show("empty", &[]);
        assert!(prepare(Step::Verify, &dir).is_err());
        assert!(prepare(Step::Check, &dir).is_err());
        assert!(find_torrent(&dir).is_err());
    }
}
