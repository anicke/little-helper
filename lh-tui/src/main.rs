//! `lh-tui` — a spike testing whether a terminal UI (ratatui) is easier to make look decent
//! than `lh-gui`'s Iced shell. One command for now: verify, run over a folder exactly like
//! `lh verify` (`lh-cli/src/main.rs`'s `cmd_verify`), its results streamed live into a table
//! through the same `lh_core::job::Queue` the GUI uses, instead of a scrolling `println!`
//! list or a widget tree.
//!
//! `lh_core::analysis::verify` decodes the whole file and never calls `Progress::report`
//! mid-file, so every job here goes straight from Running to a terminal status with no
//! visible sub-progress — the queue's own `Started`/`Finished` events are enough to drive
//! the table and the overall gauge.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_core::analysis::{Verification, verify};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::scan;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

const SPINNER: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];

#[derive(Clone)]
enum Status {
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

struct FileRow {
    name: String,
    status: Status,
}

fn main() -> io::Result<()> {
    let mut path = PathBuf::from(".");
    let mut recursive = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-r" | "--recursive" => recursive = true,
            other => path = PathBuf::from(other),
        }
    }

    let set = scan::scan(&path, recursive).map_err(io::Error::other)?;
    if set.files.is_empty() {
        eprintln!("no audio files found under {}", path.display());
        return Ok(());
    }

    let terminal = ratatui::init();
    let result = run(terminal, &path, set.files);
    ratatui::restore();
    result
}

fn run(mut terminal: DefaultTerminal, root: &Path, files: Vec<AudioFile>) -> io::Result<()> {
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
        terminal.draw(|frame| draw(frame, root, &rows, &stats, start, tick))?;

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
    Ok(())
}

struct Stats {
    done: usize,
    total: usize,
    ok: usize,
    no_md5: usize,
    mismatch: usize,
    failed: usize,
}

fn draw(
    frame: &mut Frame,
    root: &Path,
    rows: &[FileRow],
    stats: &Stats,
    start: Instant,
    tick: usize,
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

    draw_header(frame, chunks[0], root, stats, start);
    draw_table(frame, chunks[1], rows, tick);
    draw_gauge(frame, chunks[2], stats);
    draw_footer(frame, chunks[3]);
}

fn draw_header(frame: &mut Frame, area: Rect, root: &Path, stats: &Stats, start: Instant) {
    let elapsed = start.elapsed().as_secs_f32();
    let line = Line::from(vec![
        Span::styled(
            " lh-tui ",
            Style::default().bg(Color::Cyan).fg(Color::Black).bold(),
        ),
        Span::raw(" verify  "),
        Span::styled(root.display().to_string(), Style::default().fg(Color::Gray)),
        Span::raw(format!(
            "   {} / {} files   {elapsed:.1}s",
            stats.done, stats.total
        )),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_table(frame: &mut Frame, area: Rect, rows: &[FileRow], tick: usize) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = status_cell(&row.status, spin);
        let detail = status_detail(&row.status);
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(row.name.clone()),
            Cell::from(detail).style(Style::default().fg(Color::DarkGray)),
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
    .header(
        Row::new(vec!["status", "file", "detail"]).style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        ),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );

    frame.render_widget(table, area);
}

fn status_cell(status: &Status, spin: char) -> (String, Style) {
    match status {
        Status::Pending => ("pending".to_string(), Style::default().fg(Color::DarkGray)),
        Status::Running => (format!("{spin} running"), Style::default().fg(Color::Cyan)),
        Status::Ok => ("OK".to_string(), Style::default().fg(Color::Green).bold()),
        Status::NoMd5 => ("NO MD5".to_string(), Style::default().fg(Color::Yellow)),
        Status::Mismatch { .. } => (
            "MISMATCH".to_string(),
            Style::default().fg(Color::Red).bold(),
        ),
        Status::Failed(_) => ("FAILED".to_string(), Style::default().fg(Color::Red).bold()),
    }
}

fn status_detail(status: &Status) -> String {
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

fn draw_gauge(frame: &mut Frame, area: Rect, stats: &Stats) {
    let ratio = if stats.total == 0 {
        0.0
    } else {
        stats.done as f64 / stats.total as f64
    };
    let color = if stats.mismatch > 0 || stats.failed > 0 {
        Color::Red
    } else if stats.done == stats.total {
        Color::Green
    } else {
        Color::Cyan
    };
    let label = format!(
        "{}/{} ok:{} no-md5:{} mismatch:{} failed:{}",
        stats.done, stats.total, stats.ok, stats.no_md5, stats.mismatch, stats.failed
    );
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        )
        .gauge_style(Style::default().fg(color))
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}

fn draw_footer(frame: &mut Frame, area: Rect) {
    let line = Line::from(Span::styled(
        " q / esc quit ",
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(Paragraph::new(line), area);
}
