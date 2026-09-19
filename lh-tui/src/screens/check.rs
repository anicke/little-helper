use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, EntryOutcome, check_entry};
use lh_core::job::{Event, Queue};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

// --- Check (verify files against an existing .ffp/.md5/.st5) ------------------------
//
// Unlike verify/checksum, the row list here doesn't come from scanning a folder for
// audio files — it comes from the checksum file's own entries (`lh_cli::cmd_check`'s
// shape), and checking one just means recomputing its digest and comparing, so a row can
// also come back `Missing`: the entry names a file that isn't there at all, which is not
// the same kind of trouble as a digest that doesn't match or a read that failed outright.

#[derive(Clone)]
pub(crate) enum CheckStatus {
    Pending,
    Running,
    Ok,
    Mismatch {
        expected: [u8; 16],
        actual: [u8; 16],
    },
    Missing,
    Failed(String),
}

pub(crate) struct CheckRow {
    name: String,
    status: CheckStatus,
}

pub(crate) fn run_check(file: PathBuf, theme: ThemeName) -> ExitCode {
    let kind = match checksum_kind_for(&file) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };
    let list = match ChecksumFile::read(kind, &file) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e:#}", file.display());
            return ExitCode::from(2);
        }
    };
    if list.entries.is_empty() {
        eprintln!("no entries in {}", file.display());
        return ExitCode::SUCCESS;
    }
    let dir = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    let terminal = ratatui::init();
    let result = run_check_screen(
        terminal,
        kind,
        &file.display().to_string(),
        dir,
        list.entries,
        Theme::new(theme),
    );
    ratatui::restore();

    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Returns whether every entry checked out clean — the same notion of "ok" `lh check`'s
/// exit code uses: no missing file, no mismatch, no read failure.
pub(crate) fn run_check_screen(
    mut terminal: DefaultTerminal,
    kind: ChecksumKind,
    label: &str,
    dir: PathBuf,
    entries: Vec<Entry>,
    theme: Theme,
) -> io::Result<bool> {
    let total = entries.len();
    let mut rows: Vec<CheckRow> = entries
        .iter()
        .map(|e| CheckRow {
            name: e.file_name.clone(),
            status: CheckStatus::Pending,
        })
        .collect();

    let queue: Queue<EntryOutcome> = Queue::new();
    let cancel = queue.cancel_token();
    for e in &entries {
        let target_dir = dir.clone();
        let entry = e.clone();
        queue.submit(e.file_name.clone(), move |_progress| {
            check_entry(kind, &target_dir, &entry)
        });
    }
    let events = queue.events();

    let mut done = 0usize;
    let mut ok_count = 0usize;
    let mut missing_count = 0usize;
    let mut mismatch_count = 0usize;
    let mut failed_count = 0usize;
    let meta = CheckMeta { kind, label };
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => rows[id.index()].status = CheckStatus::Running,
                Event::Progress { .. } => {}
                Event::Finished { id, output, .. } => {
                    done += 1;
                    rows[id.index()].status = match output {
                        EntryOutcome::Ok => {
                            ok_count += 1;
                            CheckStatus::Ok
                        }
                        EntryOutcome::Mismatch { expected, actual } => {
                            mismatch_count += 1;
                            CheckStatus::Mismatch { expected, actual }
                        }
                        EntryOutcome::Missing => {
                            missing_count += 1;
                            CheckStatus::Missing
                        }
                        EntryOutcome::Failed(e) => {
                            failed_count += 1;
                            CheckStatus::Failed(e.to_string())
                        }
                    };
                }
                Event::Cancelled { id, .. } => {
                    done += 1;
                    rows[id.index()].status = CheckStatus::Failed("cancelled".to_string());
                }
            }
        }

        let stats = CheckStats {
            done,
            total,
            ok: ok_count,
            missing: missing_count,
            mismatch: mismatch_count,
            failed: failed_count,
        };
        let elapsed = header_elapsed(start, &mut finished_at, done == total);
        terminal
            .draw(|frame| draw_checklist(frame, &meta, &rows, &stats, elapsed, tick, &theme))?;

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

    Ok(missing_count == 0 && mismatch_count == 0 && failed_count == 0)
}

pub(crate) struct CheckStats {
    done: usize,
    total: usize,
    ok: usize,
    missing: usize,
    mismatch: usize,
    failed: usize,
}

/// The bits of the header that don't change frame to frame, grouped the same way
/// `ChecksumMeta` is (`docs/tui.md` §3).
pub(crate) struct CheckMeta<'a> {
    kind: ChecksumKind,
    label: &'a str,
}

pub(crate) fn draw_checklist(
    frame: &mut Frame,
    meta: &CheckMeta,
    rows: &[CheckRow],
    stats: &CheckStats,
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

    draw_checklist_header(frame, chunks[0], meta, stats, elapsed, theme);
    draw_checklist_table(frame, chunks[1], rows, tick, theme);
    draw_checklist_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

pub(crate) fn draw_checklist_header(
    frame: &mut Frame,
    area: Rect,
    meta: &CheckMeta,
    stats: &CheckStats,
    elapsed: f32,
    theme: &Theme,
) {
    let line = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" check {}  ", meta.kind.label())),
        Span::styled(meta.label.to_string(), theme.dim),
        Span::raw(format!(
            "   {} / {} entries   {elapsed:.1}s",
            stats.done, stats.total
        )),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim);
    frame.render_widget(Paragraph::new(line).block(block), area);
}

pub(crate) fn draw_checklist_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[CheckRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = checklist_status_cell(&row.status, spin, theme);
        let detail = checklist_status_detail(&row.status);
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
            .title(" entries ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

pub(crate) fn checklist_status_cell(
    status: &CheckStatus,
    spin: char,
    theme: &Theme,
) -> (String, Style) {
    match status {
        CheckStatus::Pending => ("pending".to_string(), theme.dim),
        CheckStatus::Running => (format!("{spin} running"), theme.accent),
        CheckStatus::Ok => ("OK".to_string(), theme.ok),
        CheckStatus::Missing => ("MISSING".to_string(), theme.warn),
        CheckStatus::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
        CheckStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

pub(crate) fn checklist_status_detail(status: &CheckStatus) -> String {
    match status {
        CheckStatus::Pending | CheckStatus::Running | CheckStatus::Ok => String::new(),
        CheckStatus::Missing => "no such file".to_string(),
        CheckStatus::Mismatch { expected, actual } => {
            format!(
                "expected {} actual {}",
                hex::encode(expected),
                hex::encode(actual)
            )
        }
        CheckStatus::Failed(e) => e.clone(),
    }
}

pub(crate) fn draw_checklist_gauge(
    frame: &mut Frame,
    area: Rect,
    stats: &CheckStats,
    theme: &Theme,
) {
    let ratio = if stats.total == 0 {
        0.0
    } else {
        stats.done as f64 / stats.total as f64
    };
    let style = if stats.missing > 0 || stats.mismatch > 0 || stats.failed > 0 {
        theme.error
    } else if stats.done == stats.total {
        theme.ok
    } else {
        theme.accent
    };
    let label = format!(
        "{}/{} ok:{} missing:{} mismatch:{} failed:{}",
        stats.done, stats.total, stats.ok, stats.missing, stats.mismatch, stats.failed
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
