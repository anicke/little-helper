use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{ConvertArgs, Paths, RenameArgs, TagArgs, Target};
use lh_core::model::AudioFormat;
use lh_core::scan;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

// --- Workspace ---------------------------------------------------------------------------
//
// `lh-tui <folder>`: one show folder, the screens a show usually goes through, in order.
// Each step opens the very same screen its subcommand does; quitting that screen comes back
// here instead of exiting. The folder is rescanned every time a step opens, since the step
// before it (a rename, a conversion) changes what is in it.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Rename,
    ConvertFlac,
    Tag,
    Verify,
    ConvertWav,
}

impl Step {
    /// The usual order — rename, convert, tag — then the steps a show only sometimes needs.
    const ALL: [Step; 5] = [
        Step::Rename,
        Step::ConvertFlac,
        Step::Tag,
        Step::Verify,
        Step::ConvertWav,
    ];

    fn key(self) -> char {
        match self {
            Step::Rename => 'r',
            Step::ConvertFlac => 'c',
            Step::Tag => 't',
            Step::Verify => 'v',
            Step::ConvertWav => 'w',
        }
    }

    fn label(self) -> &'static str {
        match self {
            Step::Rename => "rename",
            Step::ConvertFlac => "convert → FLAC",
            Step::Tag => "tag",
            Step::Verify => "verify",
            Step::ConvertWav => "convert → WAV",
        }
    }
}

/// How a step last went, shown beside it.
#[derive(Clone)]
enum StepResult {
    NotRun,
    Clean,
    Unclean,
    /// The step's screen never opened, for the reason its subcommand would have printed.
    Refused(String),
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
            KeyCode::Enter => Some(selected),
            KeyCode::Char(c) => Step::ALL.iter().position(|s| s.key() == c),
            _ => None,
        };
        let Some(index) = step else {
            continue;
        };

        selected = index;
        if let Some(result) = run_step(terminal, dir, Step::ALL[index], theme_name)? {
            // A clean step moves the cursor on, so Enter walks the usual order.
            if matches!(result, StepResult::Clean) {
                selected = (index + 1).min(Step::ALL.len() - 1);
            }
            results[index] = result;
        }
        terminal.clear()?;
        summary = summarize(dir);
    }
    Ok(())
}

/// Opens `step`'s screen on `dir` with the defaults its subcommand would use given just the
/// folder. `None` when the person left a rename or tag without applying it — the step's
/// last result still stands.
fn run_step(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    step: Step,
    theme_name: ThemeName,
) -> io::Result<Option<StepResult>> {
    let folder = match scan_folder(dir) {
        Ok(f) => f,
        Err(refusal) => return Ok(Some(StepResult::Refused(refusal.message))),
    };
    let theme = Theme::new(theme_name);
    let label = dir.display().to_string();

    let clean = match step {
        Step::Rename => {
            let args = RenameArgs {
                dir: dir.to_path_buf(),
                band: None,
                date: None,
                short_year: false,
                disc: None,
                yes: false,
            };
            rename_screen(terminal, &args, &folder.files, theme)?
        }
        Step::Tag => {
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
            tag_screen(terminal, dir, setup, theme)?
        }
        Step::ConvertFlac | Step::ConvertWav => {
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
            Some(ok)
        }
        Step::Verify => Some(run_verify_screen(terminal, &label, &folder.files, theme)?),
    };

    Ok(clean.map(|ok| {
        if ok {
            StepResult::Clean
        } else {
            StepResult::Unclean
        }
    }))
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

    let rows = Step::ALL
        .iter()
        .zip(results)
        .enumerate()
        .map(|(i, (step, result))| {
            let focused = i == selected;
            let marker = if focused { ">" } else { " " };
            let (label, style) = match result {
                StepResult::NotRun => (String::new(), theme.dim),
                StepResult::Clean => ("done".to_string(), theme.ok),
                StepResult::Unclean => ("finished with problems".to_string(), theme.error),
                StepResult::Refused(why) => (why.clone(), theme.warn),
            };
            let name_style = if focused {
                theme.accent.bold()
            } else {
                theme.accent
            };
            Row::new(vec![
                Cell::from(format!("{marker} {}", step.key())).style(theme.dim),
                Cell::from(step.label()).style(name_style),
                Cell::from(label).style(style),
            ])
        });
    let table = Table::new(
        rows,
        [
            Constraint::Length(4),
            Constraint::Length(16),
            Constraint::Min(10),
        ],
    )
    .header(Row::new(vec!["key", "step", "last run"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" steps ")
            .border_style(theme.dim),
    );
    frame.render_widget(table, outer[2]);

    frame.render_widget(
        Paragraph::new(Line::styled(
            " ↑/↓ select   enter or r/c/t/v/w open   q/esc quit ",
            theme.dim,
        )),
        outer[3],
    );
}
