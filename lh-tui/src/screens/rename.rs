use std::io;
use std::process::ExitCode;
use std::time::Duration;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::RenameArgs;
use lh_core::etree::ShowDate;
use lh_core::etree::ShowName;
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::rename::{NameSpec, RenamePlan, RenameStatus, execute_rename, plan_rename};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

// --- Rename ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenameFocus {
    None,
    Band,
    Date,
    Disc,
    ShortYear,
}

pub(crate) enum RenameStage {
    Editing,
    Renaming,
    Done,
}

#[derive(Clone)]
pub(crate) enum RenameRowStatus {
    Pending,
    Ok,
    Failed(String),
}

pub(crate) fn run_rename(args: RenameArgs, theme: ThemeName) -> ExitCode {
    let folder = match scan_folder(&args.dir) {
        Ok(f) => f,
        Err(refusal) => return refusal.exit(),
    };
    for line in &folder.skipped {
        eprintln!("{line}");
    }

    let result = {
        let mut terminal = TerminalGuard::with_paste();
        rename_screen(&mut terminal, &args, &folder.files, Theme::new(theme))
    };

    match result {
        // Leaving without applying anything is not a failure, same as today's `$?`.
        Ok(ok) => {
            if ok.unwrap_or(true) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Seeds the spec from `args`, then from the folder's own name where that is an etree show
/// name, and runs the screen on an already-open terminal — the part the workspace shares.
/// `None` when the person left without applying; otherwise whether every rename landed.
pub(crate) fn rename_screen(
    terminal: &mut DefaultTerminal,
    args: &RenameArgs,
    files: &[AudioFile],
    theme: Theme,
) -> io::Result<Option<bool>> {
    let show_name = args
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ShowName::parse);
    let band = args
        .band
        .clone()
        .or_else(|| show_name.as_ref().map(|s| s.band.clone()))
        .unwrap_or_default();
    let date = args
        .date
        .clone()
        .or_else(|| show_name.as_ref().map(|s| s.date.render_iso()))
        .unwrap_or_default();
    let disc = args.disc.map(|d| d.to_string()).unwrap_or_default();

    let files: Vec<PathBuf> = files.iter().map(|f| f.path.clone()).collect();

    run_rename_screen(
        terminal,
        &args.dir,
        files,
        band,
        date,
        args.short_year,
        disc,
        theme,
    )
}

pub(crate) fn current_spec(
    band: &Field,
    date: &Field,
    disc: &Field,
    short_year: bool,
) -> Option<NameSpec> {
    let band_val = band.value.trim();
    if band_val.is_empty() {
        return None;
    }
    let (date_val, _) = ShowDate::parse(date.value.trim())?;
    let disc_val = if disc.value.trim().is_empty() {
        None
    } else {
        Some(disc.value.trim().parse::<u32>().ok()?)
    };
    Some(NameSpec {
        band: band_val.to_string(),
        date: date_val,
        short_year,
        disc: disc_val,
        keep_suffix: true,
    })
}

/// Runs `execute_rename` as the queue's one job, not one job per file — unlike tagging,
/// a rename is atomic over the whole plan (`execute_rename`'s own two-phase move with
/// rollback, docs/tagging.md §1 contract point 3), so there is no per-file write to
/// submit independently. The applying table still shows one row per file, same as tag's;
/// it is just that every row resolves together, from the one job's single result, the
/// same way `run_sbe_fix_execute_screen` renders per-file `FixRow`s from one `execute_fix`
/// call.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_rename_screen(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    files: Vec<PathBuf>,
    band: String,
    date: String,
    short_year: bool,
    disc: String,
    theme: Theme,
) -> io::Result<Option<bool>> {
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();

    let mut band_field = Field::new(band);
    let mut date_field = Field::new(date);
    let mut disc_field = Field::new(disc);
    let mut short_year = short_year;
    let mut focus = RenameFocus::None;

    let mut stage = RenameStage::Editing;
    let mut rows: Vec<RenameRowStatus> = Vec::new();
    let mut queue: Option<Queue<lh_core::Result<Vec<PathBuf>>>> = None;
    // The plan `a` applied, kept so the result table can show from → to after the files
    // have moved; re-planning from `files` then would describe names that no longer exist.
    let mut applied: Option<RenamePlan> = None;

    let mut tick = 0usize;

    loop {
        let plan = match &applied {
            Some(a) => Some(a.clone()),
            None => current_spec(&band_field, &date_field, &disc_field, short_year)
                .map(|s| plan_rename(&files, &s)),
        };

        if let Some(q) = &queue {
            while let Ok(event) = q.events().try_recv() {
                match event {
                    Event::Started { .. } | Event::Progress { .. } => {}
                    Event::Finished { output, .. } => {
                        rows = match &output {
                            Ok(paths) => paths.iter().map(|_| RenameRowStatus::Ok).collect(),
                            Err(e) => rows
                                .iter()
                                .map(|_| RenameRowStatus::Failed(e.to_string()))
                                .collect(),
                        };
                        stage = RenameStage::Done;
                    }
                    Event::Cancelled { .. } => {
                        rows = rows
                            .iter()
                            .map(|_| RenameRowStatus::Failed("cancelled".to_string()))
                            .collect();
                        stage = RenameStage::Done;
                    }
                }
            }
        }

        terminal.draw(|frame| {
            draw_rename(
                frame,
                dir,
                &names,
                &band_field,
                &date_field,
                &disc_field,
                short_year,
                focus,
                plan.as_ref(),
                &rows,
                &stage,
                tick,
                &theme,
            )
        })?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if let Some(q) = &queue {
                            q.cancel_token().cancel();
                        }
                        break;
                    }
                    match stage {
                        RenameStage::Editing => match key.code {
                            KeyCode::Esc => focus = RenameFocus::None,
                            KeyCode::Char('q') if focus == RenameFocus::None => break,
                            KeyCode::Char('a') if focus == RenameFocus::None => {
                                if let Some(plan) = &plan {
                                    let changed = plan
                                        .entries
                                        .iter()
                                        .any(|e| e.status == RenameStatus::Changed);
                                    if plan.has_collisions() {
                                        // Refused silently — the table already shows the
                                        // collision in `theme.error` (docs/tagging.md §6).
                                    } else if !changed {
                                        rows = plan
                                            .entries
                                            .iter()
                                            .map(|_| RenameRowStatus::Ok)
                                            .collect();
                                        applied = Some(plan.clone());
                                        stage = RenameStage::Done;
                                    } else {
                                        let q: Queue<lh_core::Result<Vec<PathBuf>>> = Queue::new();
                                        let job_plan = plan.clone();
                                        q.submit("rename", move |_progress| {
                                            execute_rename(&job_plan)
                                        });
                                        rows = plan
                                            .entries
                                            .iter()
                                            .map(|_| RenameRowStatus::Pending)
                                            .collect();
                                        queue = Some(q);
                                        applied = Some(plan.clone());
                                        stage = RenameStage::Renaming;
                                    }
                                }
                            }
                            KeyCode::Tab => {
                                focus = match focus {
                                    RenameFocus::None => RenameFocus::Band,
                                    RenameFocus::Band => RenameFocus::Date,
                                    RenameFocus::Date => RenameFocus::Disc,
                                    RenameFocus::Disc => RenameFocus::ShortYear,
                                    RenameFocus::ShortYear => RenameFocus::Band,
                                };
                            }
                            KeyCode::BackTab => {
                                focus = match focus {
                                    RenameFocus::None => RenameFocus::ShortYear,
                                    RenameFocus::Band => RenameFocus::ShortYear,
                                    RenameFocus::Date => RenameFocus::Band,
                                    RenameFocus::Disc => RenameFocus::Date,
                                    RenameFocus::ShortYear => RenameFocus::Disc,
                                };
                            }
                            KeyCode::Char(' ') | KeyCode::Enter
                                if focus == RenameFocus::ShortYear =>
                            {
                                short_year = !short_year;
                            }
                            other => match focus {
                                RenameFocus::Band => {
                                    band_field.on_key(other);
                                }
                                RenameFocus::Date => {
                                    date_field.on_key(other);
                                }
                                RenameFocus::Disc => {
                                    disc_field.on_key(other);
                                }
                                RenameFocus::ShortYear | RenameFocus::None => {}
                            },
                        },
                        RenameStage::Renaming | RenameStage::Done => {
                            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                                if let Some(q) = &queue {
                                    q.cancel_token().cancel();
                                }
                                break;
                            }
                        }
                    }
                }
            }
        }
        tick = tick.wrapping_add(1);
    }

    if applied.is_none() {
        return Ok(None);
    }
    Ok(Some(rows.iter().all(|r| matches!(r, RenameRowStatus::Ok))))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_rename(
    frame: &mut Frame,
    dir: &Path,
    names: &[String],
    band: &Field,
    date: &Field,
    disc: &Field,
    short_year: bool,
    focus: RenameFocus,
    plan: Option<&RenamePlan>,
    rows: &[RenameRowStatus],
    stage: &RenameStage,
    tick: usize,
    theme: &Theme,
) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(6),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = match stage {
        RenameStage::Editing => "rename",
        RenameStage::Renaming => "rename (renaming)",
        RenameStage::Done => "rename (done)",
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {mode}  ")),
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

    draw_rename_fields(frame, outer[1], band, date, disc, short_year, focus, theme);

    match stage {
        RenameStage::Editing => match plan {
            Some(p) => draw_rename_table(frame, outer[2], names, p, theme),
            None => frame.render_widget(
                Paragraph::new(Line::styled(
                    "type a band and a date (YYYY-MM-DD or YY-MM-DD) to preview a plan",
                    theme.warn,
                ))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" plan ")
                        .border_style(theme.dim),
                ),
                outer[2],
            ),
        },
        RenameStage::Renaming | RenameStage::Done => {
            draw_rename_write_table(frame, outer[2], names, plan, rows, tick, theme)
        }
    }

    draw_rename_gauge(frame, outer[3], plan, rows, stage, theme);

    let footer = match (stage, focus) {
        (RenameStage::Editing, RenameFocus::None) => " tab fields   a apply   q/esc quit ",
        (RenameStage::Editing, RenameFocus::ShortYear) => " space toggle   esc leave field ",
        (RenameStage::Editing, _) => " esc leave field   type to edit ",
        _ => " q/esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), outer[4]);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_rename_fields(
    frame: &mut Frame,
    area: Rect,
    band: &Field,
    date: &Field,
    disc: &Field,
    short_year: bool,
    focus: RenameFocus,
    theme: &Theme,
) {
    let style_for = |f: RenameFocus| {
        if focus == f {
            theme.accent
        } else {
            Style::default()
        }
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(format!("{:<11}", "BAND"), theme.header),
            Span::styled(
                band.display(focus == RenameFocus::Band),
                style_for(RenameFocus::Band),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<11}", "DATE"), theme.header),
            Span::styled(
                date.display(focus == RenameFocus::Date),
                style_for(RenameFocus::Date),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<11}", "DISC"), theme.header),
            Span::styled(
                disc.display(focus == RenameFocus::Disc),
                style_for(RenameFocus::Disc),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<11}", "SHORT YEAR"), theme.header),
            Span::styled(
                if short_year { "yes" } else { "no" },
                style_for(RenameFocus::ShortYear),
            ),
        ]),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" spec ")
                .border_style(theme.dim),
        ),
        area,
    );
}

pub(crate) fn draw_rename_table(
    frame: &mut Frame,
    area: Rect,
    names: &[String],
    plan: &RenamePlan,
    theme: &Theme,
) {
    let table_rows = names.iter().zip(&plan.entries).map(|(name, e)| {
        let to =
            e.to.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
        let (label, style) = match e.status {
            RenameStatus::Unchanged => ("unchanged", theme.dim),
            RenameStatus::Changed => ("changed", theme.accent),
            RenameStatus::Collision => ("COLLISION", theme.error),
        };
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(name.clone()),
            Cell::from(to).style(style),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(45),
            Constraint::Percentage(45),
        ],
    )
    .header(Row::new(vec!["status", "from", "to"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" plan ")
            .border_style(theme.dim),
    );
    frame.render_widget(table, area);
}

pub(crate) fn draw_rename_write_table(
    frame: &mut Frame,
    area: Rect,
    names: &[String],
    plan: Option<&RenamePlan>,
    rows: &[RenameRowStatus],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let entries = plan.map(|p| p.entries.as_slice()).unwrap_or_default();
    let table_rows = names.iter().enumerate().map(|(i, name)| {
        let entry = entries.get(i);
        let unchanged = entry.is_some_and(|e| e.status == RenameStatus::Unchanged);
        let (label, style) = match rows.get(i) {
            None | Some(RenameRowStatus::Pending) => (format!("{spin} pending"), theme.accent),
            Some(RenameRowStatus::Ok) if unchanged => ("unchanged".to_string(), theme.dim),
            Some(RenameRowStatus::Ok) => ("renamed".to_string(), theme.ok),
            // The whole plan rolls back together, so every row failed for the one reason
            // the gauge below already prints.
            Some(RenameRowStatus::Failed(_)) => ("FAILED".to_string(), theme.error),
        };
        let to = entry
            .map(|e| {
                e.to.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
            .unwrap_or_default();
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(name.clone()).style(theme.dim),
            Cell::from(to).style(style),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(45),
            Constraint::Percentage(45),
        ],
    )
    .header(Row::new(vec!["status", "from", "to"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );
    frame.render_widget(table, area);
}

pub(crate) fn draw_rename_gauge(
    frame: &mut Frame,
    area: Rect,
    plan: Option<&RenamePlan>,
    rows: &[RenameRowStatus],
    stage: &RenameStage,
    theme: &Theme,
) {
    let (ratio, style, label) = match stage {
        RenameStage::Editing => match plan {
            Some(p) if p.has_collisions() => (1.0, theme.error, "refusing: collision".to_string()),
            Some(p) => {
                let changed = p
                    .entries
                    .iter()
                    .filter(|e| e.status == RenameStatus::Changed)
                    .count();
                (
                    1.0,
                    theme.accent,
                    format!(
                        "{changed} of {} would change — press a to rename",
                        p.entries.len()
                    ),
                )
            }
            None => (0.0, theme.warn, "no plan yet".to_string()),
        },
        RenameStage::Renaming => (0.0, theme.accent, "renaming…".to_string()),
        RenameStage::Done => {
            let failed = rows
                .iter()
                .filter(|r| matches!(r, RenameRowStatus::Failed(_)))
                .count();
            let style = if failed > 0 { theme.error } else { theme.ok };
            let label = if failed > 0 {
                let msg = rows
                    .iter()
                    .find_map(|r| match r {
                        RenameRowStatus::Failed(e) => Some(e.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                format!("failed: {msg}")
            } else {
                let renamed = plan.map_or(0, |p| {
                    p.entries
                        .iter()
                        .filter(|e| e.status == RenameStatus::Changed)
                        .count()
                });
                match rows.len() - renamed {
                    0 => format!("{renamed} files renamed"),
                    same => format!("{renamed} files renamed, {same} already had their name"),
                }
            };
            (1.0, style, label)
        }
    };
    draw_gauge(frame, area, ratio, style, label, theme);
}
