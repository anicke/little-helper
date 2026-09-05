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
use lh_cli::{ChecksumArgs, Cli, Command, Paths, TorrentCommand, TorrentCreateArgs};
use lh_core::analysis::{Verification, verify};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute};
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::torrent::{
    CreateOpts, Created, FileStatus, Metainfo, Passkeys, Resolved, TorrentReport, TrackerList,
    Verdict, check_sizes, check_with_progress, create_with_progress, default_output, resolve,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

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
        Command::Torrent {
            command: TorrentCommand::Create(args),
        } => run_torrent_create(args),
        Command::Torrent {
            command: TorrentCommand::Check { file, path, quick },
        } => run_torrent_check(file, path, quick),
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

fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

fn pieces_phrase(pieces: &[u32]) -> String {
    if pieces.len() == 1 {
        format!("piece {}", pieces[0])
    } else {
        let list: Vec<String> = pieces.iter().map(u32::to_string).collect();
        format!("pieces {}", list.join(", "))
    }
}

// --- Torrent create -----------------------------------------------------------------
//
// Unlike verify/checksum, `create_with_progress` walks the whole payload as one sequential
// piece-hashing pass, not a batch of independent files (`docs/tui.md` §4) — so there is one
// job on a queue of one, and one row of progress to show, not a table. Its progress
// callback returns a `bool` the same way `lh-cli`'s own `cmd_torrent_create` uses it
// (`lh-cli/src/lib.rs`): `false` stops the hash within one piece, so `q`/`Esc`/`Ctrl-C`
// here waits for the job's own `Done` rather than breaking the draw loop immediately the
// way verify/checksum do — the wait is bounded by a single piece's hash time, and waiting
// for it means the screen reports what actually happened (cancelled vs. finished) instead
// of guessing.

enum CreateStage {
    /// Walking the payload / resolving trackers — before the first `Progress` event.
    Preparing,
    Hashing {
        done: u32,
        total: u32,
    },
    Done(Box<lh_core::Result<Created>>),
}

fn run_torrent_create(args: TorrentCreateArgs) -> ExitCode {
    let source = match args.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e}", args.path.display());
            return ExitCode::from(2);
        }
    };

    // Same tracker resolution `cmd_torrent_create` does: ids and bare URLs become tiers,
    // an entry can only ever add `private`/`source`, never take one away the user asked
    // for.
    let list = match TrackerList::load() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lh-tui: reading the tracker list: {e:#}");
            return ExitCode::from(2);
        }
    };
    let keys = match Passkeys::load() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("lh-tui: reading the passkey list: {e:#}");
            return ExitCode::from(2);
        }
    };
    let chosen = match resolve(&args.trackers, &list, &keys) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };
    for warning in &chosen.warnings {
        eprintln!("warning: {warning}");
    }

    let private = args.private || chosen.private;
    let source_tag = args.source.clone().or_else(|| chosen.source.clone());
    let opts = CreateOpts {
        announce: chosen.tiers.clone(),
        piece_length: args.piece_length,
        private,
        source: source_tag.clone(),
        comment: args.comment.clone(),
        include_all: args.include_all,
        overwrite: args.force,
        ..CreateOpts::default()
    };

    let dst = match &args.output {
        Some(o) => o.clone(),
        None => match default_output(&source) {
            Some(p) => p,
            None => {
                eprintln!(
                    "lh-tui: {} has no parent directory to write a torrent beside",
                    source.display()
                );
                return ExitCode::from(2);
            }
        },
    };
    // Writing the .torrent inside the folder it describes adds a file to that folder, so
    // re-creating it later would produce a different infohash.
    if source.is_dir() && dst.parent().is_some_and(|p| p.starts_with(&source)) {
        eprintln!(
            "warning: writing the torrent inside {} means re-creating it later will not \
             produce the same infohash",
            source.display()
        );
    }

    let terminal = ratatui::init();
    let result = run_torrent_create_screen(
        terminal,
        &source,
        &dst,
        &opts,
        &chosen,
        private,
        &source_tag,
    );
    ratatui::restore();

    match result {
        Ok(Ok(made)) => {
            eprintln!("wrote {}", made.path.display());
            ExitCode::SUCCESS
        }
        Ok(Err(lh_core::Error::Cancelled)) => {
            eprintln!("cancelled before writing a torrent");
            ExitCode::from(1)
        }
        Ok(Err(e)) => {
            eprintln!("lh-tui: creating a torrent for {}: {e:#}", source.display());
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_torrent_create_screen(
    mut terminal: DefaultTerminal,
    source: &Path,
    dst: &Path,
    opts: &CreateOpts,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
) -> io::Result<lh_core::Result<Created>> {
    let queue: Queue<lh_core::Result<Created>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();

    let job_source = source.to_path_buf();
    let job_dst = dst.to_path_buf();
    let job_opts = opts.clone();
    queue.submit("torrent create", move |progress| {
        create_with_progress(&job_source, &job_dst, &job_opts, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });
    let events = queue.events();

    let mut stage = CreateStage::Preparing;
    let start = Instant::now();
    let mut tick = 0usize;
    let mut want_quit = false;
    let theme = Theme::new();

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { .. } => {}
                Event::Progress { done, total, .. } => {
                    stage = CreateStage::Hashing { done, total };
                }
                Event::Finished { output, .. } => stage = CreateStage::Done(Box::new(output)),
                Event::Cancelled { .. } => {
                    stage = CreateStage::Done(Box::new(Err(lh_core::Error::Cancelled)));
                }
            }
        }

        terminal.draw(|frame| {
            draw_torrent_create(
                frame, source, dst, chosen, private, source_tag, &stage, start, tick, &theme,
            )
        })?;

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
        if want_quit && matches!(stage, CreateStage::Done(_)) {
            break;
        }
        tick = tick.wrapping_add(1);
    }

    match stage {
        CreateStage::Done(result) => Ok(*result),
        _ => unreachable!("loop only exits once `stage` is `Done`"),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_torrent_create(
    frame: &mut Frame,
    source: &Path,
    dst: &Path,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
    stage: &CreateStage,
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

    let elapsed = start.elapsed().as_secs_f32();
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" torrent create  "),
        Span::styled(source.display().to_string(), theme.dim),
        Span::raw(format!("   {elapsed:.1}s")),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    let lines = create_lines(source, dst, chosen, private, source_tag, stage, theme);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" torrent ")
                .border_style(theme.dim),
        ),
        chunks[1],
    );

    draw_create_gauge(frame, chunks[2], stage, tick, theme);
    draw_footer(frame, chunks[3], theme);
}

#[allow(clippy::too_many_arguments)]
fn create_lines<'a>(
    source: &Path,
    dst: &Path,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
    stage: &CreateStage,
    theme: &'a Theme,
) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(format!("output     {}", dst.display())),
        Line::from(String::new()),
    ];
    if chosen.chosen.is_empty() {
        lines.push(Line::from("tracker    none (a trackerless torrent)"));
    }
    for (i, c) in chosen.chosen.iter().enumerate() {
        let label = if i == 0 { "tracker" } else { "" };
        let detail = match &c.tracker {
            Some(t) => format!("{}  {}  ({})", t.name, c.announce, t.health.label()),
            None => format!("{}  (given as a URL)", c.announce),
        };
        lines.push(Line::from(format!("{label:<10} {detail}")));
    }
    if private {
        lines.push(Line::from(
            "private    yes (BEP 27; part of the infohash)".to_string(),
        ));
    }
    if let Some(tag) = source_tag {
        lines.push(Line::from(format!(
            "source     {tag} (part of the infohash)"
        )));
    }
    lines.push(Line::from(String::new()));

    match stage {
        CreateStage::Preparing => {
            lines.push(Line::styled(
                format!("walking {}…", source.display()),
                theme.dim,
            ));
        }
        CreateStage::Hashing { .. } => {
            lines.push(Line::styled("hashing pieces…", theme.accent));
        }
        CreateStage::Done(result) => match result.as_ref() {
            Ok(made) => {
                lines.push(Line::styled(
                    format!("{} files", made.files.len()),
                    theme.ok,
                ));
                lines.push(Line::from(format!(
                    "size       {}",
                    format_bytes(made.total_length)
                )));
                lines.push(Line::from(format!(
                    "pieces     {} x {}",
                    made.pieces,
                    format_bytes(made.piece_length)
                )));
                lines.push(Line::from(format!("infohash   {}", made.info_hash_hex())));
                for (path, why) in &made.excluded {
                    let shown = path.strip_prefix(source).unwrap_or(path);
                    lines.push(Line::styled(
                        format!("excluded   {} ({})", shown.display(), why.reason()),
                        theme.warn,
                    ));
                }
            }
            Err(lh_core::Error::Cancelled) => {
                lines.push(Line::styled(
                    "cancelled before writing a torrent",
                    theme.warn,
                ));
            }
            Err(e) => {
                lines.push(Line::styled(format!("error: {e:#}"), theme.error));
            }
        },
    }
    lines
}

fn draw_create_gauge(
    frame: &mut Frame,
    area: Rect,
    stage: &CreateStage,
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let (ratio, style, label) = match stage {
        CreateStage::Preparing => (0.0, theme.accent, format!("{spin} preparing")),
        CreateStage::Hashing { done, total } => {
            let ratio = if *total == 0 {
                0.0
            } else {
                f64::from(*done) / f64::from(*total)
            };
            (ratio, theme.accent, format!("{done}/{total} pieces"))
        }
        CreateStage::Done(result) => match result.as_ref() {
            Ok(_) => (1.0, theme.ok, "done".to_string()),
            Err(lh_core::Error::Cancelled) => (1.0, theme.warn, "cancelled".to_string()),
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

// --- Torrent check --------------------------------------------------------------------
//
// One job on a queue of one, like create — but unlike create's `progress` callback,
// `check_with_progress`'s (`lh-core/src/torrent/verify.rs`) has no cancellation
// checkpoint: it never returns a `bool` the walk can act on, only `()`. `lh-gui`'s G4 hit
// the same gap (`lh-gui/src/main.rs`'s `run_torrent_check` notes it) and accepted it rather
// than changing `lh-core`. So `q`/`Esc`/`Ctrl-C` here breaks the draw loop immediately, the
// same as verify/checksum — the underlying hash keeps running until it finishes, which is
// no worse than plain `lh torrent check`, which cannot be interrupted at all short of
// killing the process.

enum CheckStage {
    Preparing,
    Hashing { done: u32, total: u32 },
    Done(Box<lh_core::Result<TorrentReport>>),
}

struct TorrentFileRow {
    /// Displayed relative to the torrent's root.
    path: String,
    label: &'static str,
    detail: String,
}

fn run_torrent_check(file: PathBuf, path: PathBuf, quick: bool) -> ExitCode {
    let meta = match Metainfo::read(&file) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e:#}", file.display());
            return ExitCode::from(2);
        }
    };

    let terminal = ratatui::init();
    let result = run_torrent_check_screen(terminal, meta, file, path.clone(), quick);
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

fn run_torrent_check_screen(
    mut terminal: DefaultTerminal,
    meta: Metainfo,
    torrent_path: PathBuf,
    given: PathBuf,
    quick: bool,
) -> io::Result<lh_core::Result<TorrentReport>> {
    let queue: Queue<lh_core::Result<TorrentReport>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();

    queue.submit("torrent check", move |progress| {
        if quick {
            check_sizes(&meta, &torrent_path, &given)
        } else {
            check_with_progress(&meta, &torrent_path, &given, &mut |done, total| {
                progress.report(done, total);
            })
        }
    });
    let events = queue.events();

    let mut stage = CheckStage::Preparing;
    let start = Instant::now();
    let mut tick = 0usize;
    let theme = Theme::new();

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

        terminal.draw(|frame| draw_torrent_check(frame, &stage, quick, start, tick, &theme))?;

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

    Ok(match stage {
        CheckStage::Done(result) => *result,
        _ => Err(lh_core::Error::Cancelled),
    })
}

fn draw_torrent_check(
    frame: &mut Frame,
    stage: &CheckStage,
    quick: bool,
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

    let elapsed = start.elapsed().as_secs_f32();
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

fn draw_check_table(frame: &mut Frame, area: Rect, report: &TorrentReport, theme: &Theme) {
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

fn check_row_style(label: &str, theme: &Theme) -> Style {
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
fn check_rows(report: &TorrentReport) -> Vec<TorrentFileRow> {
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
            FileStatus::Corrupt { bad_pieces } => pieces_phrase(bad_pieces),
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

fn draw_check_gauge(frame: &mut Frame, area: Rect, stage: &CheckStage, theme: &Theme) {
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
