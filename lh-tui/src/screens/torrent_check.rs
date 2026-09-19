use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_core::display;
use lh_core::job::{Event, Queue};
use lh_core::torrent::{FileStatus, Metainfo, TorrentReport, Verdict, check, check_sizes};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;

// --- Torrent check --------------------------------------------------------------------
//
// One job on a queue of one, exactly like create: `check`'s progress callback
// (`lh-core/src/torrent/verify.rs`) returns a `bool` the walk polls per piece, so
// `q`/`Esc`/`Ctrl-C` cancels the hash itself rather than just abandoning the draw loop
// (`docs/architecture-cleanup.md` A3) — the same `want_quit`-then-wait-for-`Done` shape
// create's screen uses, so the reported outcome (cancelled vs. finished) is always what
// actually happened.

pub(crate) enum CheckStage {
    Preparing,
    Hashing { done: u32, total: u32 },
    Done(Box<lh_core::Result<TorrentReport>>),
}

pub(crate) struct TorrentFileRow {
    /// Displayed relative to the torrent's root.
    path: String,
    label: &'static str,
    detail: String,
}

pub(crate) fn run_torrent_check(
    file: PathBuf,
    path: PathBuf,
    quick: bool,
    theme: ThemeName,
) -> ExitCode {
    let meta = match Metainfo::read(&file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e:#}", file.display());
            return ExitCode::from(2);
        }
    };

    let terminal = ratatui::init();
    let result =
        run_torrent_check_screen(terminal, meta, file, path.clone(), quick, Theme::new(theme));
    ratatui::restore();

    match result {
        Ok(Ok(report)) => {
            if report.verdict() == Verdict::Incomplete {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            }
        }
        Ok(Err(e)) => {
            eprintln!("lh-tui: checking against {}: {e:#}", path.display());
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn run_torrent_check_screen(
    mut terminal: DefaultTerminal,
    meta: Metainfo,
    torrent_path: PathBuf,
    given: PathBuf,
    quick: bool,
    theme: Theme,
) -> io::Result<lh_core::Result<TorrentReport>> {
    let queue: Queue<lh_core::Result<TorrentReport>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();

    queue.submit("torrent check", move |progress| {
        if quick {
            check_sizes(&meta, &torrent_path, &given)
        } else {
            check(&meta, &torrent_path, &given, &mut |done, total| {
                progress.report(done, total);
                !progress.is_cancelled()
            })
        }
    });
    let events = queue.events();

    let mut stage = CheckStage::Preparing;
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;
    let mut want_quit = false;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { .. } => {}
                Event::Progress { done, total, .. } => {
                    stage = CheckStage::Hashing { done, total };
                }
                Event::Finished { output, .. } => stage = CheckStage::Done(Box::new(output)),
                // Never produced: this queue holds one job, already running by the time
                // any key could cancel it.
                Event::Cancelled { .. } => {}
            }
        }

        let elapsed = header_elapsed(
            start,
            &mut finished_at,
            matches!(stage, CheckStage::Done(_)),
        );
        terminal.draw(|frame| draw_torrent_check(frame, &stage, quick, elapsed, tick, &theme))?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        cancel.cancel();
                        want_quit = true;
                    }
                }
            }
        }
        // `--quick` has no cancellation checkpoint (`check_sizes` is a plain stat pass, not
        // a hash), so waiting for its own `Done` costs nothing; a real hash honors `cancel`
        // via the progress callback above and reports `Error::Cancelled` through `Done` the
        // same way a completed check reports its own result.
        if want_quit && matches!(stage, CheckStage::Done(_)) {
            break;
        }
        tick = tick.wrapping_add(1);
    }

    Ok(match stage {
        CheckStage::Done(result) => *result,
        _ => Err(lh_core::Error::Cancelled),
    })
}

pub(crate) fn draw_torrent_check(
    frame: &mut Frame,
    stage: &CheckStage,
    quick: bool,
    elapsed: f32,
    tick: usize,
    theme: &Theme,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = if quick { "check --quick" } else { "check" };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" torrent {mode}   {elapsed:.1}s")),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    match stage {
        CheckStage::Done(result) => match result.as_ref() {
            Ok(report) => draw_check_table(frame, chunks[1], report, theme),
            Err(e) => frame.render_widget(
                Paragraph::new(Line::styled(format!("error: {e:#}"), theme.error)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(theme.dim),
                ),
                chunks[1],
            ),
        },
        _ => {
            let spin = SPINNER[tick / 2 % SPINNER.len()];
            let text = match stage {
                CheckStage::Preparing => {
                    format!("{spin} reading the torrent and the local files…")
                }
                CheckStage::Hashing { .. } => format!("{spin} hashing pieces…"),
                CheckStage::Done(_) => unreachable!(),
            };
            frame.render_widget(
                Paragraph::new(Line::styled(text, theme.accent)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" files ")
                        .border_style(theme.dim),
                ),
                chunks[1],
            );
        }
    }

    draw_check_gauge(frame, chunks[2], stage, theme);
    draw_footer(frame, chunks[3], theme);
}

pub(crate) fn draw_check_table(
    frame: &mut Frame,
    area: Rect,
    report: &TorrentReport,
    theme: &Theme,
) {
    let rows = check_rows(report);
    let table_rows = rows.iter().map(|row| {
        let style = check_row_style(row.label, theme);
        Row::new(vec![
            Cell::from(row.label).style(style),
            Cell::from(row.path.clone()),
            Cell::from(row.detail.clone()).style(theme.dim),
        ])
    });

    let table = Table::new(
        table_rows,
        [
            Constraint::Length(11),
            Constraint::Percentage(40),
            Constraint::Percentage(49),
        ],
    )
    .header(Row::new(vec!["status", "file", "detail"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

pub(crate) fn check_row_style(label: &str, theme: &Theme) -> Style {
    match label {
        "OK" | "SIZE OK" => theme.ok,
        "PADDING" | "EXTRA" => theme.dim,
        "PARTIAL" => theme.warn,
        _ => theme.error,
    }
}

/// Mirrors `lh-cli`'s `cmd_torrent_check` line-by-line, and `lh-gui`'s `report_rows`
/// (`lh-gui/src/job.rs`): skip padding, show every real file's status with the same detail
/// the CLI prints in parentheses, then the extras the torrent does not list.
pub(crate) fn check_rows(report: &TorrentReport) -> Vec<TorrentFileRow> {
    let mut rows = Vec::with_capacity(report.files.len() + report.extra_local.len());
    for outcome in &report.files {
        if outcome.status == FileStatus::Padding {
            continue;
        }
        let shown = outcome
            .path
            .strip_prefix(&report.root)
            .unwrap_or(&outcome.path);
        let detail = match &outcome.status {
            FileStatus::WrongSize { expected, actual } => {
                format!("expected {expected} bytes, found {actual}")
            }
            FileStatus::Unreadable { reason } => reason.clone(),
            FileStatus::Corrupt { bad_pieces } => display::pieces_phrase(bad_pieces),
            FileStatus::Suspect { piece, shared_with } => format!(
                "piece {piece} is shared with {} other file(s); either could be at fault",
                shared_with.len()
            ),
            FileStatus::Partial {
                verified,
                unverifiable,
            } => format!("{verified} verified, {unverifiable} unreadable"),
            _ => String::new(),
        };
        rows.push(TorrentFileRow {
            path: shown.display().to_string(),
            label: outcome.status.label(),
            detail,
        });
    }
    for extra in &report.extra_local {
        let shown = extra.strip_prefix(&report.root).unwrap_or(extra);
        rows.push(TorrentFileRow {
            path: shown.display().to_string(),
            label: "EXTRA",
            detail: String::new(),
        });
    }
    rows
}

pub(crate) fn draw_check_gauge(frame: &mut Frame, area: Rect, stage: &CheckStage, theme: &Theme) {
    let (ratio, style, label) = match stage {
        CheckStage::Preparing => (0.0, theme.accent, "preparing".to_string()),
        CheckStage::Hashing { done, total } => {
            let ratio = if *total == 0 {
                0.0
            } else {
                f64::from(*done) / f64::from(*total)
            };
            (ratio, theme.accent, format!("{done}/{total} pieces"))
        }
        CheckStage::Done(result) => match result.as_ref() {
            Ok(report) => {
                let n = report.needs_attention().count();
                let style = if n > 0 { theme.error } else { theme.ok };
                let label = match report.pieces {
                    Some(p) if p.failed > 0 || p.unverifiable > 0 => format!(
                        "{} of {} pieces verified, {} failed, {} unverifiable",
                        p.ok, p.total, p.failed, p.unverifiable
                    ),
                    Some(p) => format!("{} of {} pieces verified", p.ok, p.total),
                    None => "sizes match (contents not read)".to_string(),
                };
                (1.0, style, label)
            }
            Err(_) => (1.0, theme.error, "failed".to_string()),
        },
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        )
        .gauge_style(style)
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}
