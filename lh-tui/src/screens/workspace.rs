use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{
    ConvertArgs, Direction as SbeFixDirection, Paths, RenameArgs, TagArgs, Target,
    TorrentCreateArgs,
};
use lh_core::checksum::{ChecksumFile, ChecksumKind};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::scan;
use lh_core::torrent::{Metainfo, Verdict, default_output};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

// --- Workspace ---------------------------------------------------------------------------
//
// `lh-tui <folder>`: every screen `lh-tui` has, listed by what it is for, each opening on
// that one folder with the defaults its subcommand would use given just the folder.
// Quitting a screen comes back here instead of exiting. The folder is rescanned every time
// a screen opens, since the one before it (a rename, a conversion) changes what is in it.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Rename,
    ConvertFlac,
    ConvertWav,
    Tag,
    Verify,
    Sbe,
    SbeFix,
    Check,
    Create(ChecksumKind),
    TorrentCreate,
    TorrentInfo,
    TorrentCheck,
}

/// One menu line: the screen, the key that opens it, its name and what it does.
struct Item {
    step: Step,
    key: char,
    label: &'static str,
    about: &'static str,
}

const fn item(step: Step, key: char, label: &'static str, about: &'static str) -> Item {
    Item {
        step,
        key,
        label,
        about,
    }
}

/// The whole menu, grouped, in the order it is drawn. A screen's index everywhere else
/// (`selected`, `results`) is its position in this list flattened.
const MENU: [(&str, &[Item]); 4] = [
    (
        "Prepare",
        &[
            item(
                Step::Rename,
                'r',
                "rename",
                "name files from band, date and track",
            ),
            item(
                Step::ConvertFlac,
                'c',
                "convert → FLAC",
                "encode WAVs, move checked ones to _original/",
            ),
            item(
                Step::ConvertWav,
                'w',
                "convert → WAV",
                "decode FLACs back to WAV",
            ),
            item(Step::Tag, 't', "tag", "edit show fields and track titles"),
        ],
    ),
    (
        "Inspect",
        &[
            item(
                Step::Verify,
                'v',
                "verify",
                "check each FLAC's embedded MD5",
            ),
            item(Step::Sbe, 's', "sbe", "find sector-boundary errors"),
            item(
                Step::SbeFix,
                'x',
                "sbe fix",
                "repair SBEs, move replaced files to _original/sbe-fix/",
            ),
        ],
    ),
    (
        "Checksums",
        &[
            item(
                Step::Check,
                'k',
                "check checksums",
                "check the folder's .ffp/.md5/.st5 files",
            ),
            item(
                Step::Create(ChecksumKind::Ffp),
                'f',
                "create ffp",
                "write <folder>.ffp inside the folder",
            ),
            item(
                Step::Create(ChecksumKind::Md5),
                'm',
                "create md5",
                "write <folder>.md5 inside the folder",
            ),
            item(
                Step::Create(ChecksumKind::St5),
                '5',
                "create st5",
                "write <folder>.st5 inside the folder",
            ),
        ],
    ),
    (
        "Torrent",
        &[
            item(
                Step::TorrentCreate,
                'n',
                "create torrent",
                "hash the folder into <folder>.torrent beside it",
            ),
            item(
                Step::TorrentInfo,
                'i',
                "torrent info",
                "show <folder>.torrent",
            ),
            item(
                Step::TorrentCheck,
                'h',
                "torrent check",
                "check the folder against <folder>.torrent",
            ),
        ],
    ),
];

fn items() -> impl Iterator<Item = &'static Item> {
    MENU.iter().flat_map(|(_, items)| items.iter())
}

/// How a screen last went, shown beside it.
#[derive(Clone)]
enum StepResult {
    NotRun,
    Clean(String),
    Unclean(String),
    /// The screen never opened, for the reason its subcommand would have printed.
    Refused(String),
}

impl StepResult {
    fn from_clean(ok: bool) -> Self {
        if ok {
            StepResult::Clean("done".to_string())
        } else {
            StepResult::Unclean("finished with problems".to_string())
        }
    }
}

/// Why a step returned before an outcome: the terminal failed, or its screen never opened.
/// Lets every refusal in [`open_step`] be a `?`.
enum Stop {
    Io(io::Error),
    Refused(String),
}

impl From<io::Error> for Stop {
    fn from(e: io::Error) -> Self {
        Stop::Io(e)
    }
}

/// A refusal is worded for stderr; inside the menu, the `lh-tui: ` it opens with says nothing.
impl From<Refusal> for Stop {
    fn from(r: Refusal) -> Self {
        let message = match r.message.strip_prefix("lh-tui: ") {
            Some(rest) => rest.to_string(),
            None => r.message,
        };
        Stop::Refused(message)
    }
}

pub(crate) fn run_workspace(dir: PathBuf, theme: ThemeName) -> ExitCode {
    // Resolved once, so `<folder>` in a checksum or torrent name is the folder's real name
    // even when it was given as `.`.
    let dir = match dir.canonicalize() {
        Ok(d) if d.is_dir() => d,
        _ => {
            eprintln!("lh-tui: {} is not a directory", dir.display());
            return ExitCode::from(2);
        }
    };
    let result = {
        let mut terminal = TerminalGuard::with_paste();
        run_workspace_screen(&mut terminal, &dir, Theme::new(theme))
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_workspace_screen(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    theme: Theme,
) -> io::Result<()> {
    let count = items().count();
    let mut selected = 0usize;
    let mut results = vec![StepResult::NotRun; count];
    let mut summary = summarize(dir);

    loop {
        terminal.draw(|frame| draw_workspace(frame, dir, &summary, selected, &results, &theme))?;

        // Nothing here animates, so block on the next event rather than polling.
        let CtEvent::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let chosen = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                selected = (selected + 1).min(count - 1);
                None
            }
            KeyCode::Home => {
                selected = 0;
                None
            }
            KeyCode::End => {
                selected = count - 1;
                None
            }
            KeyCode::Enter => Some(selected),
            KeyCode::Char(c) => items().position(|i| i.key == c),
            _ => None,
        };
        let Some(index) = chosen else {
            continue;
        };

        selected = index;
        let step = items().nth(index).expect("index comes from items()").step;
        let outcome = match open_step(terminal, dir, step, theme) {
            Ok(outcome) => outcome,
            Err(Stop::Refused(why)) => Some(StepResult::Refused(why)),
            Err(Stop::Io(e)) => return Err(e),
        };
        if let Some(result) = outcome {
            results[index] = result;
        }
        terminal.clear()?;
        summary = summarize(dir);
    }
    Ok(())
}

/// Opens `step`'s screen on `dir` with the defaults its subcommand would use given just the
/// folder. `None` when the person left without an outcome — a rename or tag not applied, a
/// torrent not created, info only looked at — so the step's last result still stands.
fn open_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    step: Step,
    theme: Theme,
) -> Result<Option<StepResult>, Stop> {
    let label = dir.display().to_string();

    let result = match step {
        Step::Rename => {
            let folder = scan_folder(dir)?;
            let args = RenameArgs {
                dir: dir.to_path_buf(),
                band: None,
                date: None,
                short_year: false,
                disc: None,
                yes: false,
            };
            rename_screen(terminal, &args, &folder.files, theme)?.map(StepResult::from_clean)
        }
        Step::Tag => {
            let folder = scan_folder(dir)?;
            let args = TagArgs {
                dir: dir.to_path_buf(),
                artist: None,
                album: None,
                date: None,
                genre: None,
                comment: None,
                location: None,
                titles: None,
                yes: false,
            };
            let setup = prepare_tag(&args, folder.files)?;
            tag_screen(terminal, dir, setup, theme)?.map(StepResult::from_clean)
        }
        Step::ConvertFlac | Step::ConvertWav => {
            let folder = scan_folder(dir)?;
            let to = if step == Step::ConvertFlac {
                Target::Flac
            } else {
                Target::Wav
            };
            let encoder = find_encoder(to)?;
            let args = ConvertArgs {
                paths: Paths {
                    paths: vec![dir.to_path_buf()],
                    recursive: false,
                },
                to,
                out_dir: None,
                // `--level`'s own default.
                level: 8,
                force: false,
                provenance: false,
                // A checked WAV goes into `_original/`, so the steps after this one see
                // only the FLACs. Ignored for FLAC → WAV.
                move_sources: true,
            };
            let (ok, _) =
                run_convert_screen(terminal, &label, &folder.files, &args, encoder, theme)?;
            Some(StepResult::from_clean(ok))
        }
        Step::Verify => {
            let folder = scan_folder(dir)?;
            let ok = run_verify_screen(terminal, &label, &folder.files, theme)?;
            Some(StepResult::from_clean(ok))
        }
        Step::Sbe => {
            let folder = scan_folder(dir)?;
            let ok = run_sbe_screen(terminal, &label, &folder.files, theme)?;
            Some(StepResult::from_clean(ok))
        }
        Step::SbeFix => {
            // `lh sbe fix --in-place`: the plan first, with direction and tail padding to
            // change, and nothing written until it is applied.
            let folder = scan_folder(dir)?;
            let screen = run_sbe_fix_in_place_screen(
                terminal,
                dir,
                &folder.files,
                SbeFixDirection::Backward,
                false,
                theme,
            )?;
            screen.map(|outcome| match outcome {
                Ok((done, plan)) if plan.fully_fixed => {
                    StepResult::Clean(in_place_summary(&done, &plan))
                }
                Ok((done, plan)) => StepResult::Unclean(in_place_summary(&done, &plan)),
                Err(e) => StepResult::Unclean(format!("{e:#}")),
            })
        }
        Step::Check => run_check_step(terminal, dir, theme)?,
        Step::Create(kind) => {
            let folder = scan_folder(dir)?;
            run_create_checksum_step(terminal, dir, &label, kind, &folder.files, theme)?
        }
        Step::TorrentCreate => {
            let args = TorrentCreateArgs {
                path: dir.to_path_buf(),
                output: None,
                trackers: Vec::new(),
                piece_length: None,
                private: false,
                source: None,
                comment: None,
                include_all: false,
                force: false,
            };
            let CreateSetup {
                source,
                dst,
                list,
                keys,
            } = prepare_torrent_create(&args)?;
            match run_torrent_create_screen(terminal, &source, &dst, &args, &list, &keys, theme)? {
                Ok(made) => Some(StepResult::Clean(format!("wrote {}", made.path.display()))),
                Err(lh_core::Error::Cancelled) => None,
                Err(e) => Some(StepResult::Unclean(format!("{e:#}"))),
            }
        }
        Step::TorrentInfo => {
            let (_, meta) = find_torrent(dir)?;
            run_torrent_info_screen(terminal, &meta, true, theme)?;
            None
        }
        Step::TorrentCheck => {
            let (file, meta) = find_torrent(dir)?;
            match run_torrent_check_screen(terminal, meta, file, dir.to_path_buf(), false, theme)? {
                Ok(report) => Some(StepResult::from_clean(
                    report.verdict() != Verdict::Incomplete,
                )),
                Err(lh_core::Error::Cancelled) => None,
                Err(e) => Some(StepResult::Unclean(format!("{e:#}"))),
            }
        }
    };
    Ok(result)
}

/// Checks every `.ffp`/`.md5`/`.st5` in the folder, one screen after another.
fn run_check_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    theme: Theme,
) -> Result<Option<StepResult>, Stop> {
    let mut lists: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && ChecksumKind::from_path(p).is_some())
        .collect();
    if lists.is_empty() {
        return Err(Stop::Refused(
            "no .ffp, .md5 or .st5 file in the folder".to_string(),
        ));
    }
    lists.sort();

    let mut clean = true;
    for path in &lists {
        let (kind, list, list_dir) = prepare_check(path)?;
        if list.entries.is_empty() {
            continue;
        }
        let label = path.display().to_string();
        clean &= run_check_screen(terminal, kind, &label, list_dir, &list.entries, theme)?;
        terminal.clear()?;
    }
    let names: Vec<String> = lists.iter().map(|p| file_name(p)).collect();
    Ok(Some(if clean {
        StepResult::Clean(format!("{} ok", names.join(", ")))
    } else {
        StepResult::from_clean(false)
    }))
}

/// Writes `<folder>.<ext>` inside the folder — only once every file has computed, so a
/// quit or a failure never leaves a partial list there looking like a complete one.
fn run_create_checksum_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    label: &str,
    kind: ChecksumKind,
    files: &[AudioFile],
    theme: Theme,
) -> Result<Option<StepResult>, Stop> {
    let out = dir.join(format!("{}.{}", file_name(dir), kind.extension()));
    if out.exists() {
        return Err(Stop::Refused(format!("{} already exists", file_name(&out))));
    }

    let (ok, entries) = run_checksum_screen(terminal, kind, label, files, theme)?;
    if !ok {
        return Ok(Some(StepResult::Unclean(format!(
            "not all files computed, {} not written",
            file_name(&out)
        ))));
    }
    let mut list = ChecksumFile::new(kind);
    list.entries = entries;
    Ok(Some(match list.write(&out) {
        Ok(()) => StepResult::Clean(format!("wrote {}", file_name(&out))),
        Err(e) => StepResult::Unclean(format!("writing {}: {e:#}", file_name(&out))),
    }))
}

/// The torrent `create torrent` would have written — `<folder>.torrent` beside the folder —
/// or else the one `.torrent` inside it.
fn find_torrent(dir: &Path) -> Result<(PathBuf, Metainfo), Stop> {
    let file = match default_output(dir).filter(|p| p.is_file()) {
        Some(p) => p,
        None => {
            let inside: Vec<PathBuf> = std::fs::read_dir(dir)?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "torrent"))
                .collect();
            match <[PathBuf; 1]>::try_from(inside) {
                Ok([one]) => one,
                Err(inside) if inside.is_empty() => {
                    return Err(Stop::Refused(
                        "no <folder>.torrent beside the folder, nor a .torrent in it".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(Stop::Refused(
                        "several .torrent files in the folder; use lh-tui torrent".to_string(),
                    ));
                }
            }
        }
    };
    let meta = Metainfo::read(&file)
        .map_err(|e| Stop::Refused(format!("reading {}: {e:#}", file_name(&file))))?;
    Ok((file, meta))
}

/// What is in the folder right now, by format — `12 files: 12 WAV`, then `24 files: 12 WAV,
/// 12 FLAC` after a conversion — so the menu shows each step's effect without opening one.
fn summarize(dir: &Path) -> String {
    let set = match scan::scan(dir, false) {
        Ok(s) => s,
        Err(e) => return format!("scanning failed: {e:#}"),
    };
    if set.files.is_empty() {
        return "no audio files".to_string();
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
        line.push_str(&format!("   ({} skipped)", set.skipped.len()));
    }
    line
}

fn draw_workspace(
    frame: &mut Frame,
    dir: &Path,
    summary: &str,
    selected: usize,
    results: &[StepResult],
    theme: &Theme,
) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" workspace  "),
        Span::styled(dir.display().to_string(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        outer[0],
    );

    frame.render_widget(
        Paragraph::new(Line::raw(summary.to_string())).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" folder ")
                .border_style(theme.dim),
        ),
        outer[1],
    );

    // One title row per group, then its screens; `selected_row` tracks where the selected
    // screen landed so the table scrolls to it on a short terminal.
    let mut rows = Vec::new();
    let mut selected_row = 0;
    let mut index = 0;
    for (g, (title, group)) in MENU.iter().enumerate() {
        if g > 0 {
            rows.push(Row::new(vec![Cell::from("")]));
        }
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(*title).style(theme.header),
        ]));
        for item in group.iter() {
            let focused = index == selected;
            if focused {
                selected_row = rows.len();
            }
            let marker = if focused { ">" } else { " " };
            let (last, last_style) = match &results[index] {
                StepResult::NotRun => ("", theme.dim),
                StepResult::Clean(what) => (what.as_str(), theme.ok),
                StepResult::Unclean(what) => (what.as_str(), theme.error),
                StepResult::Refused(why) => (why.as_str(), theme.warn),
            };
            let name_style = if focused {
                theme.accent.bold()
            } else {
                theme.accent
            };
            rows.push(Row::new(vec![
                Cell::from(format!("{marker} {}", item.key)).style(theme.dim),
                Cell::from(item.label).style(name_style),
                Cell::from(item.about).style(theme.dim),
                Cell::from(last).style(last_style),
            ]));
            index += 1;
        }
    }
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(17),
            Constraint::Length(48),
            Constraint::Min(10),
        ],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" screens ")
            .border_style(theme.dim),
    );
    let mut state = TableState::default().with_selected(Some(selected_row));
    frame.render_stateful_widget(table, outer[2], &mut state);

    frame.render_widget(
        Paragraph::new(Line::styled(
            " ↑/↓ select   enter or a key open   q/esc quit ",
            theme.dim,
        )),
        outer[3],
    );
}
