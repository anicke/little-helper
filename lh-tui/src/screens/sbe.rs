use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::Paths;
use lh_core::analysis::{Sbe, sbe};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

// --- SBE (sector boundary error) ------------------------------------------------------
//
// `analysis::sbe` is a pure, infallible function over a `StreamInfo` `collect` already
// probed — no decode, no I/O, no `Result` — so this screen is the same per-file batch shape
// as verify/checksum (`docs/tui.md` §2) with `T = Sbe` directly rather than `Result<Sbe>`,
// mirroring `cmd_sbe`'s own `run_batch(&files, |f, _| sbe(&f.stream_info))`.

#[derive(Clone)]
pub(crate) enum SbeStatus {
    Pending,
    Running,
    Aligned,
    Misaligned { remainder_frames: u64 },
    NotApplicable { reason: &'static str },
    Failed(String),
}

pub(crate) struct SbeRow {
    name: String,
    status: SbeStatus,
}

pub(crate) fn run_sbe(paths: Paths, theme: ThemeName) -> ExitCode {
    let label = describe(&paths);
    let (files, mut clean) = match collect(&paths) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };
    if files.is_empty() {
        eprintln!("no audio files found under {label}");
        return if clean {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }

    let terminal = ratatui::init();
    let result = run_sbe_screen(terminal, &label, files, Theme::new(theme));
    ratatui::restore();

    match result {
        Ok(ok) => {
            clean &= ok;
            if clean {
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

/// Returns whether every file came back clean — no misalignment, no failure — the same
/// notion `lh sbe`'s own exit code uses. `NotApplicable` doesn't count against it, the same
/// way `cmd_sbe` never sets `ok = false` for a file that simply isn't CD audio.
pub(crate) fn run_sbe_screen(
    mut terminal: DefaultTerminal,
    root: &str,
    files: Vec<AudioFile>,
    theme: Theme,
) -> io::Result<bool> {
    let total = files.len();
    let mut rows: Vec<SbeRow> = files
        .iter()
        .map(|f| SbeRow {
            name: f.file_name(),
            status: SbeStatus::Pending,
        })
        .collect();

    let queue: Queue<Sbe> = Queue::new();
    let cancel = queue.cancel_token();
    for f in &files {
        let info = f.stream_info.clone();
        queue.submit(f.file_name(), move |_progress| sbe(&info));
    }
    let events = queue.events();

    let mut done = 0usize;
    let mut aligned_count = 0usize;
    let mut misaligned_count = 0usize;
    let mut not_applicable_count = 0usize;
    let mut failed_count = 0usize;
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => rows[id.index()].status = SbeStatus::Running,
                Event::Progress { .. } => {}
                Event::Finished { id, output, .. } => {
                    done += 1;
                    rows[id.index()].status = match output {
                        Sbe::Aligned => {
                            aligned_count += 1;
                            SbeStatus::Aligned
                        }
                        Sbe::Misaligned { remainder_frames } => {
                            misaligned_count += 1;
                            SbeStatus::Misaligned { remainder_frames }
                        }
                        Sbe::NotApplicable { reason } => {
                            not_applicable_count += 1;
                            SbeStatus::NotApplicable { reason }
                        }
                    };
                }
                Event::Cancelled { id, .. } => {
                    done += 1;
                    failed_count += 1;
                    rows[id.index()].status = SbeStatus::Failed("cancelled".to_string());
                }
            }
        }

        let stats = SbeStats {
            done,
            total,
            aligned: aligned_count,
            misaligned: misaligned_count,
            not_applicable: not_applicable_count,
            failed: failed_count,
        };
        let elapsed = header_elapsed(start, &mut finished_at, done == total);
        terminal.draw(|frame| draw_sbe(frame, root, &rows, &stats, elapsed, tick, &theme))?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        cancel.cancel();
                        break;
                    }
                }
            }
        }
        tick = tick.wrapping_add(1);
    }
    Ok(misaligned_count == 0 && failed_count == 0)
}

pub(crate) struct SbeStats {
    done: usize,
    total: usize,
    aligned: usize,
    misaligned: usize,
    not_applicable: usize,
    failed: usize,
}

pub(crate) fn draw_sbe(
    frame: &mut Frame,
    root: &str,
    rows: &[SbeRow],
    stats: &SbeStats,
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

    draw_sbe_header(frame, chunks[0], root, stats, elapsed, theme);
    draw_sbe_table(frame, chunks[1], rows, tick, theme);
    draw_sbe_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

pub(crate) fn draw_sbe_header(
    frame: &mut Frame,
    area: Rect,
    root: &str,
    stats: &SbeStats,
    elapsed: f32,
    theme: &Theme,
) {
    let line = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" sbe  "),
        Span::styled(root.to_string(), theme.dim),
        Span::raw(format!(
            "   {} / {} files   {elapsed:.1}s",
            stats.done, stats.total
        )),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

pub(crate) fn draw_sbe_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[SbeRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = sbe_status_cell(&row.status, spin, theme);
        let detail = sbe_status_detail(&row.status);
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(row.name.clone()),
            Cell::from(detail).style(theme.dim),
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
    .header(Row::new(vec!["status", "file", "detail"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

pub(crate) fn sbe_status_cell(status: &SbeStatus, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        SbeStatus::Pending => ("pending".to_string(), theme.dim),
        SbeStatus::Running => (format!("{spin} running"), theme.accent),
        SbeStatus::Aligned => ("ALIGNED".to_string(), theme.ok),
        SbeStatus::Misaligned { .. } => ("MISALIGNED".to_string(), theme.error),
        // Neutral, not a warning: most non-CDDA files hit this and it doesn't count
        // against "clean" (same treatment convert gives `Skipped`, not verify's `NoMd5`).
        SbeStatus::NotApplicable { .. } => ("N/A".to_string(), theme.dim),
        SbeStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

pub(crate) fn sbe_status_detail(status: &SbeStatus) -> String {
    match status {
        SbeStatus::Pending | SbeStatus::Running | SbeStatus::Aligned => String::new(),
        SbeStatus::Misaligned { remainder_frames } => {
            format!("+{remainder_frames} frames past a sector boundary")
        }
        SbeStatus::NotApplicable { reason } => reason.to_string(),
        SbeStatus::Failed(e) => e.clone(),
    }
}

pub(crate) fn draw_sbe_gauge(frame: &mut Frame, area: Rect, stats: &SbeStats, theme: &Theme) {
    let ratio = if stats.total == 0 {
        0.0
    } else {
        stats.done as f64 / stats.total as f64
    };
    let style = if stats.misaligned > 0 || stats.failed > 0 {
        theme.error
    } else if stats.done == stats.total {
        theme.ok
    } else {
        theme.accent
    };
    let label = format!(
        "{}/{} aligned:{} n/a:{} misaligned:{} failed:{}",
        stats.done,
        stats.total,
        stats.aligned,
        stats.not_applicable,
        stats.misaligned,
        stats.failed
    );
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
