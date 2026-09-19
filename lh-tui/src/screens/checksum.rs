use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::ChecksumArgs;
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

// --- Checksum (ffp / md5 / st5) ------------------------------------------------------
//
// One screen for all three `ChecksumKind`s, the same way `lh-cli::cmd_checksum` is one
// function parameterized by `kind` rather than three near-duplicates (`docs/tui.md` §3).
// Unlike verify, `checksum::compute` only ever succeeds with a digest or fails outright —
// there is no "no md5 to compare" or "mismatch" outcome — so the digest itself is the
// payload worth showing, not just a status word.

#[derive(Clone)]
pub(crate) enum ChecksumStatus {
    Pending,
    Running,
    Ok([u8; 16]),
    Failed(String),
}

pub(crate) struct ChecksumRow {
    name: String,
    status: ChecksumStatus,
}

pub(crate) fn run_checksum(kind: ChecksumKind, args: ChecksumArgs, theme: ThemeName) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match collect(&args.paths) {
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
    let result = run_checksum_screen(terminal, kind, &label, files, Theme::new(theme));
    ratatui::restore();

    let (ok, entries) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            return ExitCode::from(2);
        }
    };
    clean &= ok;

    let mut out = ChecksumFile::new(kind);
    out.entries = entries;
    match &args.output {
        Some(path) => {
            if let Err(e) = out.write(path) {
                eprintln!("lh-tui: writing {}: {e:#}", path.display());
                return ExitCode::from(2);
            }
            eprintln!(
                "wrote {} {} entries to {}",
                out.entries.len(),
                kind.label(),
                path.display()
            );
        }
        None => print!("{}", out.render()),
    }

    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// Returns whether every file computed cleanly, plus every successful entry in submission
/// order — the order `collect` produced the file list in, not completion order, so a
/// written `.ffp` does not reorder itself between runs just because the queue's worker
/// pool finished files in a different sequence this time (`docs/tui.md` §3, matching
/// `lh-gui`'s S3 checksum-create area).
pub(crate) fn run_checksum_screen(
    mut terminal: DefaultTerminal,
    kind: ChecksumKind,
    root: &str,
    files: Vec<AudioFile>,
    theme: Theme,
) -> io::Result<(bool, Vec<Entry>)> {
    let total = files.len();
    let mut rows: Vec<ChecksumRow> = files
        .iter()
        .map(|f| ChecksumRow {
            name: f.file_name(),
            status: ChecksumStatus::Pending,
        })
        .collect();

    let queue: Queue<lh_core::Result<[u8; 16]>> = Queue::new();
    let cancel = queue.cancel_token();
    for f in &files {
        let path = f.path.clone();
        queue.submit(f.file_name(), move |_progress| compute(kind, &path));
    }
    let events = queue.events();

    let mut done = 0usize;
    let mut ok_count = 0usize;
    let mut failed_count = 0usize;
    let meta = ChecksumMeta { kind, root };
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => rows[id.index()].status = ChecksumStatus::Running,
                Event::Progress { .. } => {}
                Event::Finished { id, output, .. } => {
                    done += 1;
                    rows[id.index()].status = match output {
                        Ok(digest) => {
                            ok_count += 1;
                            ChecksumStatus::Ok(digest)
                        }
                        Err(e) => {
                            failed_count += 1;
                            ChecksumStatus::Failed(e.to_string())
                        }
                    };
                }
                Event::Cancelled { id, .. } => {
                    done += 1;
                    rows[id.index()].status = ChecksumStatus::Failed("cancelled".to_string());
                }
            }
        }

        let stats = ChecksumStats {
            done,
            total,
            ok: ok_count,
            failed: failed_count,
        };
        let elapsed = header_elapsed(start, &mut finished_at, done == total);
        terminal.draw(|frame| draw_checksum(frame, &meta, &rows, &stats, elapsed, tick, &theme))?;

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

    let entries = files
        .iter()
        .zip(&rows)
        .filter_map(|(f, row)| match row.status {
            ChecksumStatus::Ok(digest) => Some(Entry {
                file_name: f.file_name(),
                digest,
            }),
            _ => None,
        })
        .collect();
    Ok((failed_count == 0, entries))
}

pub(crate) struct ChecksumStats {
    done: usize,
    total: usize,
    ok: usize,
    failed: usize,
}

/// The bits of the header that don't change frame to frame, grouped so
/// `draw_checksum`/`draw_checksum_header` don't need clippy's `too_many_arguments` blessing.
pub(crate) struct ChecksumMeta<'a> {
    kind: ChecksumKind,
    root: &'a str,
}

pub(crate) fn draw_checksum(
    frame: &mut Frame,
    meta: &ChecksumMeta,
    rows: &[ChecksumRow],
    stats: &ChecksumStats,
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

    draw_checksum_header(frame, chunks[0], meta, stats, elapsed, theme);
    draw_checksum_table(frame, chunks[1], rows, tick, theme);
    draw_checksum_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

pub(crate) fn draw_checksum_header(
    frame: &mut Frame,
    area: Rect,
    meta: &ChecksumMeta,
    stats: &ChecksumStats,
    elapsed: f32,
    theme: &Theme,
) {
    let line = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {}  ", meta.kind.label())),
        Span::styled(meta.root.to_string(), theme.dim),
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

pub(crate) fn draw_checksum_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[ChecksumRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = checksum_status_cell(&row.status, spin, theme);
        let detail = checksum_status_detail(&row.status);
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
            Constraint::Percentage(35),
            Constraint::Percentage(55),
        ],
    )
    .header(Row::new(vec!["status", "file", "digest"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

pub(crate) fn checksum_status_cell(
    status: &ChecksumStatus,
    spin: char,
    theme: &Theme,
) -> (String, Style) {
    match status {
        ChecksumStatus::Pending => ("pending".to_string(), theme.dim),
        ChecksumStatus::Running => (format!("{spin} running"), theme.accent),
        ChecksumStatus::Ok(_) => ("OK".to_string(), theme.ok),
        ChecksumStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

/// The digest for a successful row, unlike verify's detail column: checksum's whole
/// purpose is the digest, not just an explanation attached to a failure.
pub(crate) fn checksum_status_detail(status: &ChecksumStatus) -> String {
    match status {
        ChecksumStatus::Pending | ChecksumStatus::Running => String::new(),
        ChecksumStatus::Ok(digest) => hex::encode(digest),
        ChecksumStatus::Failed(e) => e.clone(),
    }
}

pub(crate) fn draw_checksum_gauge(
    frame: &mut Frame,
    area: Rect,
    stats: &ChecksumStats,
    theme: &Theme,
) {
    let ratio = if stats.total == 0 {
        0.0
    } else {
        stats.done as f64 / stats.total as f64
    };
    let style = if stats.failed > 0 {
        theme.error
    } else if stats.done == stats.total {
        theme.ok
    } else {
        theme.accent
    };
    let label = format!(
        "{}/{} ok:{} failed:{}",
        stats.done, stats.total, stats.ok, stats.failed
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
