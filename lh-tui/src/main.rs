//! `lh-tui` — a terminal UI for Little Helper. It parses the exact same subcommands as
//! `lh` (`lh-cli/src/lib.rs`, shared as a library so both binaries stay in lockstep), so
//! every command `lh` knows is callable here too — `lh-tui convert --to flac .` works the
//! same as `lh convert --to flac .`.
//!
//! Only `verify` has an actual screen so far: results streamed live into a table through
//! the same `lh_core::job::Queue` the GUI uses, instead of a scrolling `println!` list.
//! Every other command runs exactly as `lh` would — printing to the terminal rather than
//! drawing one — until it gets a screen of its own.
//!
//! `lh_core::analysis::verify` decodes the whole file and never calls `Progress::report`
//! mid-file, so every job here goes straight from Running to a terminal status with no
//! visible sub-progress — the queue's own `Started`/`Finished` events are enough to drive
//! the table and the overall gauge.

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{ChecksumArgs, Cli, Command, Paths};
use lh_core::analysis::{Verification, verify};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
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

/// Every color/style this UI uses, named once. Deliberately avoids inverted
/// backgrounds (`.bg(Color::Cyan)`, `.bg(Color::DarkGray)`) — those assume a dark
/// terminal and clash on a light one, since ratatui's named colors map to the
/// terminal's own ANSI palette rather than fixed RGB. Bold/underline read as
/// "header" or "accent" regardless of the terminal's background.
struct Theme {
    accent: Style,
    ok: Style,
    warn: Style,
    error: Style,
    dim: Style,
    header: Style,
}

impl Theme {
    fn new() -> Self {
        Theme {
            accent: Style::default().fg(Color::Cyan),
            ok: Style::default().fg(Color::Green).bold(),
            warn: Style::default().fg(Color::Yellow),
            error: Style::default().fg(Color::Red).bold(),
            dim: Style::default().fg(Color::DarkGray),
            header: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Verify(paths) => run_verify(paths),
        Command::Ffp(args) => run_checksum(ChecksumKind::Ffp, args),
        Command::Md5(args) => run_checksum(ChecksumKind::Md5, args),
        Command::St5(args) => run_checksum(ChecksumKind::St5, args),
        other => run_headless(Cli { command: other }),
    }
}

/// Every command besides `verify` doesn't have a screen yet, so it runs exactly the way
/// `lh` itself would — same output, same exit-code contract — just from this binary.
fn run_headless(cli: Cli) -> ExitCode {
    match lh_cli::run(cli) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn run_verify(paths: Paths) -> ExitCode {
    let label = describe(&paths);
    let (files, mut clean) = match lh_cli::collect(&paths) {
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
    let result = run(terminal, &label, files);
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

/// What the header shows for where these files came from: the one path given, or a count
/// when there were several — `Paths` allows more than one, unlike the plain folder this
/// screen used to assume.
fn describe(paths: &Paths) -> String {
    match paths.paths.as_slice() {
        [one] => one.display().to_string(),
        many => format!("{} paths", many.len()),
    }
}

/// Returns whether every file verified cleanly (no mismatches, no failures) — the same
/// notion of "ok" `lh verify`'s exit code uses, so quitting the screen early still leaves
/// scripts able to tell success from trouble via `$?`.
fn run(mut terminal: DefaultTerminal, root: &str, files: Vec<AudioFile>) -> io::Result<bool> {
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
    let theme = Theme::new();

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
        terminal.draw(|frame| draw(frame, root, &rows, &stats, start, tick, &theme))?;

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
    root: &str,
    rows: &[FileRow],
    stats: &Stats,
    start: Instant,
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

    draw_header(frame, chunks[0], root, stats, start, theme);
    draw_table(frame, chunks[1], rows, tick, theme);
    draw_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

fn draw_header(
    frame: &mut Frame,
    area: Rect,
    root: &str,
    stats: &Stats,
    start: Instant,
    theme: &Theme,
) {
    let elapsed = start.elapsed().as_secs_f32();
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

fn draw_table(frame: &mut Frame, area: Rect, rows: &[FileRow], tick: usize, theme: &Theme) {
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

fn status_cell(status: &Status, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        Status::Pending => ("pending".to_string(), theme.dim),
        Status::Running => (format!("{spin} running"), theme.accent),
        Status::Ok => ("OK".to_string(), theme.ok),
        Status::NoMd5 => ("NO MD5".to_string(), theme.warn),
        Status::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
        Status::Failed(_) => ("FAILED".to_string(), theme.error),
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

fn draw_gauge(frame: &mut Frame, area: Rect, stats: &Stats, theme: &Theme) {
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

fn draw_footer(frame: &mut Frame, area: Rect, theme: &Theme) {
    let line = Line::from(Span::styled(" q / esc quit ", theme.dim));
    frame.render_widget(Paragraph::new(line), area);
}

// --- Checksum (ffp / md5 / st5) ------------------------------------------------------
//
// One screen for all three `ChecksumKind`s, the same way `lh-cli::cmd_checksum` is one
// function parameterized by `kind` rather than three near-duplicates (`docs/tui.md` §3).
// Unlike verify, `checksum::compute` only ever succeeds with a digest or fails outright —
// there is no "no md5 to compare" or "mismatch" outcome — so the digest itself is the
// payload worth showing, not just a status word.

#[derive(Clone)]
enum ChecksumStatus {
    Pending,
    Running,
    Ok([u8; 16]),
    Failed(String),
}

struct ChecksumRow {
    name: String,
    status: ChecksumStatus,
}

fn run_checksum(kind: ChecksumKind, args: ChecksumArgs) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match lh_cli::collect(&args.paths) {
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
    let result = run_checksum_screen(terminal, kind, &label, files);
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
fn run_checksum_screen(
    mut terminal: DefaultTerminal,
    kind: ChecksumKind,
    root: &str,
    files: Vec<AudioFile>,
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
    let meta = ChecksumMeta {
        kind,
        root,
        start: Instant::now(),
    };
    let mut tick = 0usize;
    let theme = Theme::new();

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
        terminal.draw(|frame| draw_checksum(frame, &meta, &rows, &stats, tick, &theme))?;

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

struct ChecksumStats {
    done: usize,
    total: usize,
    ok: usize,
    failed: usize,
}

/// The bits of the header that don't change frame to frame, grouped so
/// `draw_checksum`/`draw_checksum_header` don't need clippy's `too_many_arguments` blessing.
struct ChecksumMeta<'a> {
    kind: ChecksumKind,
    root: &'a str,
    start: Instant,
}

fn draw_checksum(
    frame: &mut Frame,
    meta: &ChecksumMeta,
    rows: &[ChecksumRow],
    stats: &ChecksumStats,
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

    draw_checksum_header(frame, chunks[0], meta, stats, theme);
    draw_checksum_table(frame, chunks[1], rows, tick, theme);
    draw_checksum_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

fn draw_checksum_header(
    frame: &mut Frame,
    area: Rect,
    meta: &ChecksumMeta,
    stats: &ChecksumStats,
    theme: &Theme,
) {
    let elapsed = meta.start.elapsed().as_secs_f32();
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

fn draw_checksum_table(
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

fn checksum_status_cell(status: &ChecksumStatus, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        ChecksumStatus::Pending => ("pending".to_string(), theme.dim),
        ChecksumStatus::Running => (format!("{spin} running"), theme.accent),
        ChecksumStatus::Ok(_) => ("OK".to_string(), theme.ok),
        ChecksumStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

/// The digest for a successful row, unlike verify's detail column: checksum's whole
/// purpose is the digest, not just an explanation attached to a failure.
fn checksum_status_detail(status: &ChecksumStatus) -> String {
    match status {
        ChecksumStatus::Pending | ChecksumStatus::Running => String::new(),
        ChecksumStatus::Ok(digest) => hex::encode(digest),
        ChecksumStatus::Failed(e) => e.clone(),
    }
}

fn draw_checksum_gauge(frame: &mut Frame, area: Rect, stats: &ChecksumStats, theme: &Theme) {
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
