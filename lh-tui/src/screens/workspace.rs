use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{ConvertArgs, Paths, RenameArgs, TagArgs, Target, TorrentCreateArgs};
use lh_core::checksum::{ChecksumFile, ChecksumKind};
use lh_core::model::AudioFormat;
use lh_core::repair::{BoundaryDirection, TailPolicy, plan_fix};
use lh_core::scan;
use lh_core::torrent::{Metainfo, Passkeys, TrackerList, Verdict, default_output};
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
enum Group {
    Prepare,
    Inspect,
    Checksums,
    Torrent,
}

impl Group {
    const ALL: [Group; 4] = [
        Group::Prepare,
        Group::Inspect,
        Group::Checksums,
        Group::Torrent,
    ];

    fn title(self) -> &'static str {
        match self {
            Group::Prepare => "Prepare",
            Group::Inspect => "Inspect",
            Group::Checksums => "Checksums",
            Group::Torrent => "Torrent",
        }
    }
}

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

impl Step {
    /// Grouped, and in menu order within each group.
    const ALL: [Step; 14] = [
        Step::Rename,
        Step::ConvertFlac,
        Step::ConvertWav,
        Step::Tag,
        Step::Verify,
        Step::Sbe,
        Step::SbeFix,
        Step::Check,
        Step::Create(ChecksumKind::Ffp),
        Step::Create(ChecksumKind::Md5),
        Step::Create(ChecksumKind::St5),
        Step::TorrentCreate,
        Step::TorrentInfo,
        Step::TorrentCheck,
    ];

    fn group(self) -> Group {
        match self {
            Step::Rename | Step::ConvertFlac | Step::ConvertWav | Step::Tag => Group::Prepare,
            Step::Verify | Step::Sbe | Step::SbeFix => Group::Inspect,
            Step::Check | Step::Create(_) => Group::Checksums,
            Step::TorrentCreate | Step::TorrentInfo | Step::TorrentCheck => Group::Torrent,
        }
    }

    fn key(self) -> char {
        match self {
            Step::Rename => 'r',
            Step::ConvertFlac => 'c',
            Step::ConvertWav => 'w',
            Step::Tag => 't',
            Step::Verify => 'v',
            Step::Sbe => 's',
            Step::SbeFix => 'x',
            Step::Check => 'k',
            Step::Create(ChecksumKind::Ffp) => 'f',
            Step::Create(ChecksumKind::Md5) => 'm',
            Step::Create(ChecksumKind::St5) => '5',
            Step::TorrentCreate => 'n',
            Step::TorrentInfo => 'i',
            Step::TorrentCheck => 'h',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Step::Rename => "rename",
            Step::ConvertFlac => "convert → FLAC",
            Step::ConvertWav => "convert → WAV",
            Step::Tag => "tag",
            Step::Verify => "verify",
            Step::Sbe => "sbe",
            Step::SbeFix => "sbe fix preview",
            Step::Check => "check checksums",
            Step::Create(ChecksumKind::Ffp) => "create ffp",
            Step::Create(ChecksumKind::Md5) => "create md5",
            Step::Create(ChecksumKind::St5) => "create st5",
            Step::TorrentCreate => "create torrent",
            Step::TorrentInfo => "torrent info",
            Step::TorrentCheck => "torrent check",
        }
    }

    fn about(self) -> &'static str {
        match self {
            Step::Rename => "name files from band, date and track",
            Step::ConvertFlac => "encode WAVs, move checked ones to _original/",
            Step::ConvertWav => "decode FLACs back to WAV",
            Step::Tag => "edit show fields and track titles",
            Step::Verify => "check each FLAC's embedded MD5",
            Step::Sbe => "find sector-boundary errors",
            Step::SbeFix => "plan a sector-boundary repair (writes nothing)",
            Step::Check => "check the folder's .ffp/.md5/.st5 files",
            Step::Create(ChecksumKind::Ffp) => "write <folder>.ffp inside the folder",
            Step::Create(ChecksumKind::Md5) => "write <folder>.md5 inside the folder",
            Step::Create(ChecksumKind::St5) => "write <folder>.st5 inside the folder",
            Step::TorrentCreate => "hash the folder into <folder>.torrent beside it",
            Step::TorrentInfo => "show <folder>.torrent",
            Step::TorrentCheck => "check the folder against <folder>.torrent",
        }
    }
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

pub(crate) fn run_workspace(dir: PathBuf, theme: ThemeName) -> ExitCode {
    if !dir.is_dir() {
        eprintln!("lh-tui: {} is not a directory", dir.display());
        return ExitCode::from(2);
    }
    let result = {
        let mut terminal = TerminalGuard::with_paste();
        run_workspace_screen(&mut terminal, &dir, theme)
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
    theme_name: ThemeName,
) -> io::Result<()> {
    let theme = Theme::new(theme_name);
    let mut selected = 0usize;
    let mut results = vec![StepResult::NotRun; Step::ALL.len()];
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
        let step = match key.code {
            KeyCode::Char('q') | KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Up => {
                selected = selected.saturating_sub(1);
                None
            }
            KeyCode::Down => {
                selected = (selected + 1).min(Step::ALL.len() - 1);
                None
            }
            KeyCode::Home => {
                selected = 0;
                None
            }
            KeyCode::End => {
                selected = Step::ALL.len() - 1;
                None
            }
            KeyCode::Enter => Some(selected),
            KeyCode::Char(c) => Step::ALL.iter().position(|s| s.key() == c),
            _ => None,
        };
        let Some(index) = step else {
            continue;
        };

        selected = index;
        if let Some(result) = run_step(terminal, dir, Step::ALL[index], theme_name)? {
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
fn run_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    step: Step,
    theme_name: ThemeName,
) -> io::Result<Option<StepResult>> {
    let theme = Theme::new(theme_name);
    let label = dir.display().to_string();

    // The steps that work on the folder's audio files; the checksum-file and torrent steps
    // below do not need any to be there.
    let folder = || scan_folder(dir).map_err(|r| StepResult::Refused(r.message));

    let result = match step {
        Step::Rename => {
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
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
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
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
            let setup = match prepare_tag(&args, folder.files) {
                Ok(s) => s,
                Err(refusal) => return Ok(Some(StepResult::Refused(refusal.message))),
            };
            tag_screen(terminal, dir, setup, theme)?.map(StepResult::from_clean)
        }
        Step::ConvertFlac | Step::ConvertWav => {
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
            let to = if step == Step::ConvertFlac {
                Target::Flac
            } else {
                Target::Wav
            };
            let encoder = match find_encoder(to) {
                Ok(e) => e,
                Err(refusal) => return Ok(Some(StepResult::Refused(refusal.message))),
            };
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
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
            let ok = run_verify_screen(terminal, &label, &folder.files, theme)?;
            Some(StepResult::from_clean(ok))
        }
        Step::Sbe => {
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
            let ok = run_sbe_screen(terminal, &label, &folder.files, theme)?;
            Some(StepResult::from_clean(ok))
        }
        Step::SbeFix => {
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
            // `lh sbe fix --dry-run`'s defaults. Executing needs an output folder, which is
            // the subcommand's to ask for, not this menu's.
            let plan = match plan_fix(
                &folder.files,
                BoundaryDirection::Backward,
                TailPolicy::Report,
            ) {
                Ok(p) => p,
                Err(e) => return Ok(Some(StepResult::Refused(format!("planning a fix: {e:#}")))),
            };
            run_sbe_fix_plan_screen(terminal, dir, &folder.files, &plan, theme)?;
            Some(if plan.fully_fixed {
                StepResult::Clean("fix would align every file".to_string())
            } else {
                StepResult::Unclean("last file stays misaligned without --pad-tail".to_string())
            })
        }
        Step::Check => run_check_step(terminal, dir, theme_name)?,
        Step::Create(kind) => {
            let folder = match folder() {
                Ok(f) => f,
                Err(refused) => return Ok(Some(refused)),
            };
            run_create_checksum_step(terminal, dir, kind, &folder.files, theme)?
        }
        Step::TorrentCreate => run_torrent_create_step(terminal, dir, theme)?,
        Step::TorrentInfo => {
            let (_, meta) = match find_torrent(dir) {
                Ok(v) => v,
                Err(refused) => return Ok(Some(refused)),
            };
            run_torrent_info_screen(terminal, &meta, true, theme)?;
            None
        }
        Step::TorrentCheck => {
            let (file, meta) = match find_torrent(dir) {
                Ok(v) => v,
                Err(refused) => return Ok(Some(refused)),
            };
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
    theme_name: ThemeName,
) -> io::Result<Option<StepResult>> {
    let mut lists: Vec<(PathBuf, ChecksumKind)> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file())
        .filter_map(|p| ChecksumKind::from_path(&p).map(|kind| (p, kind)))
        .collect();
    if lists.is_empty() {
        return Ok(Some(StepResult::Refused(
            "no .ffp, .md5 or .st5 file in the folder".to_string(),
        )));
    }
    lists.sort_by(|a, b| a.0.cmp(&b.0));

    let mut clean = true;
    for (path, kind) in &lists {
        let list = match ChecksumFile::read(*kind, path) {
            Ok(l) => l,
            Err(e) => {
                return Ok(Some(StepResult::Refused(format!(
                    "reading {}: {e:#}",
                    file_name(path)
                ))));
            }
        };
        if list.entries.is_empty() {
            continue;
        }
        let label = path.display().to_string();
        clean &= run_check_screen(
            terminal,
            *kind,
            &label,
            dir.to_path_buf(),
            &list.entries,
            Theme::new(theme_name),
        )?;
        terminal.clear()?;
    }
    let names: Vec<String> = lists.iter().map(|(p, _)| file_name(p)).collect();
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
    kind: ChecksumKind,
    files: &[lh_core::model::AudioFile],
    theme: Theme,
) -> io::Result<Option<StepResult>> {
    let Some(name) = folder_name(dir) else {
        return Ok(Some(StepResult::Refused(
            "the folder has no name to call the checksum file by".to_string(),
        )));
    };
    let out = dir.join(format!("{name}.{}", kind.extension()));
    if out.exists() {
        return Ok(Some(StepResult::Refused(format!(
            "{} already exists",
            file_name(&out)
        ))));
    }

    let label = dir.display().to_string();
    let (ok, entries) = run_checksum_screen(terminal, kind, &label, files, theme)?;
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

fn run_torrent_create_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    theme: Theme,
) -> io::Result<Option<StepResult>> {
    let source = dir.canonicalize()?;
    let Some(dst) = default_output(&source) else {
        return Ok(Some(StepResult::Refused(
            "the folder has no parent directory to write a torrent beside".to_string(),
        )));
    };
    let list = match TrackerList::load() {
        Ok(l) => l,
        Err(e) => {
            return Ok(Some(StepResult::Refused(format!(
                "reading the tracker list: {e:#}"
            ))));
        }
    };
    let keys = match Passkeys::load() {
        Ok(k) => k,
        Err(e) => {
            return Ok(Some(StepResult::Refused(format!(
                "reading the passkey list: {e:#}"
            ))));
        }
    };
    let args = TorrentCreateArgs {
        path: source.clone(),
        output: None,
        trackers: Vec::new(),
        piece_length: None,
        private: false,
        source: None,
        comment: None,
        include_all: false,
        force: false,
    };
    Ok(
        match run_torrent_create_screen(terminal, &source, &dst, &args, &list, &keys, theme)? {
            Ok(made) => Some(StepResult::Clean(format!("wrote {}", made.path.display()))),
            Err(lh_core::Error::Cancelled) => None,
            Err(e) => Some(StepResult::Unclean(format!("{e:#}"))),
        },
    )
}

/// The torrent `create torrent` would have written — `<folder>.torrent` beside the folder —
/// or else the one `.torrent` inside it.
fn find_torrent(dir: &Path) -> Result<(PathBuf, Metainfo), StepResult> {
    let beside = dir.canonicalize().ok().and_then(|d| default_output(&d));
    let file = match beside.filter(|p| p.is_file()) {
        Some(p) => p,
        None => {
            let inside: Vec<PathBuf> = std::fs::read_dir(dir)
                .map_err(|e| StepResult::Refused(format!("reading the folder: {e}")))?
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.is_file() && p.extension().is_some_and(|x| x == "torrent"))
                .collect();
            match <[PathBuf; 1]>::try_from(inside) {
                Ok([one]) => one,
                Err(inside) if inside.is_empty() => {
                    return Err(StepResult::Refused(
                        "no <folder>.torrent beside the folder, nor a .torrent in it".to_string(),
                    ));
                }
                Err(_) => {
                    return Err(StepResult::Refused(
                        "several .torrent files in the folder; use lh-tui torrent".to_string(),
                    ));
                }
            }
        }
    };
    match Metainfo::read(&file) {
        Ok(meta) => Ok((file, meta)),
        Err(e) => Err(StepResult::Refused(format!(
            "reading {}: {e:#}",
            file_name(&file)
        ))),
    }
}

fn folder_name(dir: &Path) -> Option<String> {
    let dir = dir.canonicalize().ok()?;
    Some(dir.file_name()?.to_string_lossy().into_owned())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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
    for (g, group) in Group::ALL.iter().enumerate() {
        if g > 0 {
            rows.push(Row::new(vec![Cell::from("")]));
        }
        rows.push(Row::new(vec![
            Cell::from(""),
            Cell::from(group.title()).style(theme.header),
        ]));
        for (i, (step, result)) in Step::ALL.iter().zip(results).enumerate() {
            if step.group() != *group {
                continue;
            }
            let focused = i == selected;
            if focused {
                selected_row = rows.len();
            }
            let marker = if focused { ">" } else { " " };
            let (last, last_style) = match result {
                StepResult::NotRun => (String::new(), theme.dim),
                StepResult::Clean(what) => (what.clone(), theme.ok),
                StepResult::Unclean(what) => (what.clone(), theme.error),
                StepResult::Refused(why) => (why.clone(), theme.warn),
            };
            let name_style = if focused {
                theme.accent.bold()
            } else {
                theme.accent
            };
            rows.push(Row::new(vec![
                Cell::from(format!("{marker} {}", step.key())).style(theme.dim),
                Cell::from(step.label()).style(name_style),
                Cell::from(step.about()).style(theme.dim),
                Cell::from(last).style(last_style),
            ]));
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
