use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::Paths;
use lh_core::analysis::{Verification, verify};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

#[derive(Clone)]
pub(crate) enum Status {
    Pending,
    Running,
    Ok,
    NoMd5,
    Mismatch {
        stored: [u8; 16],
        computed: [u8; 16],
    },
    Failed(String),
}

pub(crate) struct FileRow {
    name: String,
    status: Status,
}

pub(crate) fn run_verify(paths: Paths, theme: ThemeName) -> ExitCode {
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
    let result = run(terminal, &label, files, Theme::new(theme));
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

/// Returns whether every file verified cleanly (no mismatches, no failures) — the same
/// notion of "ok" `lh verify`'s exit code uses, so quitting the screen early still leaves
/// scripts able to tell success from trouble via `$?`.
pub(crate) fn run(
    mut terminal: DefaultTerminal,
    root: &str,
    files: Vec<AudioFile>,
    theme: Theme,
) -> io::Result<bool> {
    let total = files.len();
    let mut rows: Vec<FileRow> = files
        .iter()
        .map(|f| FileRow {
            name: f.file_name(),
            status: Status::Pending,
        })
        .collect();

    let queue: Queue<lh_core::Result<Verification>> = Queue::new();
    let cancel = queue.cancel_token();
    for f in &files {
        let path = f.path.clone();
        queue.submit(f.file_name(), move |_progress| verify(&path));
    }
    let events = queue.events();

    let mut done = 0usize;
    let mut ok_count = 0usize;
    let mut mismatch_count = 0usize;
    let mut no_md5_count = 0usize;
    let mut failed_count = 0usize;
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => rows[id.index()].status = Status::Running,
                Event::Progress { .. } => {}
                Event::Finished { id, output, .. } => {
                    done += 1;
                    rows[id.index()].status = match output {
                        Ok(Verification::Ok) => {
                            ok_count += 1;
                            Status::Ok
                        }
                        Ok(Verification::NoStoredMd5 { .. }) => {
                            no_md5_count += 1;
                            Status::NoMd5
                        }
                        Ok(Verification::Md5Mismatch { stored, computed }) => {
                            mismatch_count += 1;
                            Status::Mismatch { stored, computed }
                        }
                        Err(e) => {
                            failed_count += 1;
                            Status::Failed(e.to_string())
                        }
                    };
                }
                Event::Cancelled { id, .. } => {
                    done += 1;
                    rows[id.index()].status = Status::Failed("cancelled".to_string());
                }
            }
        }

        let stats = Stats {
            done,
            total,
            ok: ok_count,
            no_md5: no_md5_count,
            mismatch: mismatch_count,
            failed: failed_count,
        };
        let elapsed = header_elapsed(start, &mut finished_at, done == total);
        terminal.draw(|frame| draw(frame, root, &rows, &stats, elapsed, tick, &theme))?;

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
    Ok(mismatch_count == 0 && failed_count == 0)
}

pub(crate) struct Stats {
    done: usize,
    total: usize,
    ok: usize,
    no_md5: usize,
    mismatch: usize,
    failed: usize,
}

pub(crate) fn draw(
    frame: &mut Frame,
    root: &str,
    rows: &[FileRow],
    stats: &Stats,
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

    draw_header(frame, chunks[0], root, stats, elapsed, theme);
    draw_table(frame, chunks[1], rows, tick, theme);
    draw_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

pub(crate) fn draw_header(
    frame: &mut Frame,
    area: Rect,
    root: &str,
    stats: &Stats,
    elapsed: f32,
    theme: &Theme,
) {
    let line = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" verify  "),
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

pub(crate) fn draw_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[FileRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = status_cell(&row.status, spin, theme);
        let detail = status_detail(&row.status);
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

pub(crate) fn status_cell(status: &Status, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        Status::Pending => ("pending".to_string(), theme.dim),
        Status::Running => (format!("{spin} running"), theme.accent),
        Status::Ok => ("OK".to_string(), theme.ok),
        Status::NoMd5 => ("NO MD5".to_string(), theme.warn),
        Status::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
        Status::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

pub(crate) fn status_detail(status: &Status) -> String {
    match status {
        Status::Pending | Status::Running | Status::Ok => String::new(),
        Status::NoMd5 => "decoded cleanly, nothing to compare".to_string(),
        Status::Mismatch { stored, computed } => {
            format!(
                "stored {} computed {}",
                hex::encode(stored),
                hex::encode(computed)
            )
        }
        Status::Failed(e) => e.clone(),
    }
}

pub(crate) fn draw_gauge(frame: &mut Frame, area: Rect, stats: &Stats, theme: &Theme) {
    let ratio = if stats.total == 0 {
        0.0
    } else {
        stats.done as f64 / stats.total as f64
    };
    let style = if stats.mismatch > 0 || stats.failed > 0 {
        theme.error
    } else if stats.done == stats.total {
        theme.ok
    } else {
        theme.accent
    };
    let label = format!(
        "{}/{} ok:{} no-md5:{} mismatch:{} failed:{}",
        stats.done, stats.total, stats.ok, stats.no_md5, stats.mismatch, stats.failed
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

pub(crate) fn draw_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = Line::from(Span::styled(" q / esc quit ", theme.dim));
    frame.render_widget(Paragraph::new(line), area);
}
