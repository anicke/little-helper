//! `lh-tui` — a terminal UI for Little Helper. It parses the exact same subcommands as
//! `lh` (`lh-cli/src/lib.rs`, shared as a library so both binaries stay in lockstep), so
//! every command `lh` knows is callable here too — `lh-tui convert --to flac .` works the
//! same as `lh convert --to flac .`.
//!
//! Most commands have an actual screen by now (`docs/tui.md` tracks which); a command with
//! none yet runs exactly as `lh` would — printing to the terminal rather than drawing one —
//! via `run_headless`, which is the permanent fallback for commands that never earn a screen
//! of their own, not a placeholder to delete.
//!
//! `lh_core::analysis::verify` decodes the whole file and never calls `Progress::report`
//! mid-file, so every job here goes straight from Running to a terminal status with no
//! visible sub-progress — the queue's own `Started`/`Finished` events are enough to drive
//! the table and the overall gauge.

use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as CtEvent, KeyCode, KeyEventKind,
    KeyModifiers,
};
use lh_cli::{
    ChecksumArgs, Cli, Command, ConvertArgs, Direction as SbeFixDirection, Paths, RenameArgs,
    SbeFixArgs, SbeSub, TagArgs, Target, TorrentCommand, TorrentCreateArgs,
};
use lh_core::analysis::{
    BoundaryDirection, FixPlan, Fixed, RepairEncode, Sbe, TailPolicy, Verification, execute_fix,
    plan_fix, sbe, verify,
};
use lh_core::checksum::{ChecksumFile, ChecksumKind, Entry, compute, ffp};
use lh_core::convert::{
    Conversion, EncodeOpts, destination, to_flac_cancellable, to_wav_with_progress,
};
use lh_core::etree::ShowDate;
use lh_core::etree::ShowName;
use lh_core::job::{CancelToken, Event, Queue};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::rename::{NameSpec, RenamePlan, RenameStatus, execute_rename, plan_rename};
use lh_core::scan;
use lh_core::tag::{self, Tags};
use lh_core::tools::{Registry, Tool, ToolId};
use lh_core::torrent::{
    CreateOpts, Created, FileStatus, Metainfo, Passkeys, Resolved, TorrentReport, Tracker,
    TrackerList, Verdict, check_sizes, check_with_progress, create_with_progress, default_output,
    resolve,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Gauge, List, ListItem, ListState, Paragraph, Row, Table,
};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

const SPINNER: [char; 4] = ['⠋', '⠙', '⠸', '⠴'];

/// The elapsed time a header shows: keeps advancing every frame while `done` is false, then
/// freezes at the instant `done` first turns true. Every screen redraws on an 80ms poll even
/// once its work is finished (waiting for `q`), so a header computing `start.elapsed()` live
/// would otherwise keep climbing while nothing is actually happening. `finished_at` is the
/// caller's own `Option<Instant>`, `None` until that instant, so the freeze survives frames.
fn header_elapsed(start: Instant, finished_at: &mut Option<Instant>, done: bool) -> f32 {
    if done && finished_at.is_none() {
        *finished_at = Some(Instant::now());
    }
    finished_at
        .unwrap_or_else(Instant::now)
        .duration_since(start)
        .as_secs_f32()
}

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

/// Color themes this UI can draw with. `Default` relies on the terminal's own ANSI
/// palette (see `Theme::new`'s doc comment); the rest are fixed RGB palettes already
/// shipped by name in plenty of other Go/Rust TUIs (bottom, yazi, lazygit, bat, ...),
/// so `--theme nord` etc. reads the same set of hues here as it does there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ThemeName {
    Default,
    CatppuccinMocha,
    CatppuccinLatte,
    Nord,
    Dracula,
    Gruvbox,
    TokyoNight,
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
    fn new(name: ThemeName) -> Self {
        match name {
            ThemeName::Default => Theme {
                accent: Style::default().fg(Color::Cyan),
                ok: Style::default().fg(Color::Green).bold(),
                warn: Style::default().fg(Color::Yellow),
                error: Style::default().fg(Color::Red).bold(),
                dim: Style::default().fg(Color::DarkGray),
                header: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            },
            // Mauve, green, yellow, red, overlay0.
            ThemeName::CatppuccinMocha => {
                Theme::palette(0xcba6f7, 0xa6e3a1, 0xf9e2af, 0xf38ba8, 0x6c7086)
            }
            // Mauve, green, yellow, red, overlay0 (Latte's light-background variants).
            ThemeName::CatppuccinLatte => {
                Theme::palette(0x8839ef, 0x40a02b, 0xdf8e1d, 0xd20f39, 0x9ca0b0)
            }
            // Frost cyan (nord8), aurora green (nord14)/yellow (nord13)/red (nord11), nord3.
            ThemeName::Nord => Theme::palette(0x88c0d0, 0xa3be8c, 0xebcb8b, 0xbf616a, 0x4c566a),
            // Purple, green, yellow, red, comment.
            ThemeName::Dracula => Theme::palette(0xbd93f9, 0x50fa7b, 0xf1fa8c, 0xff5555, 0x6272a4),
            // Bright purple, green, yellow, red, gray.
            ThemeName::Gruvbox => Theme::palette(0xd3869b, 0xb8bb26, 0xfabd2f, 0xfb4934, 0x928374),
            // Purple, green, yellow, red (magenta/pink role omitted), comment.
            ThemeName::TokyoNight => {
                Theme::palette(0xbb9af7, 0x9ece6a, 0xe0af68, 0xf7768e, 0x565f89)
            }
        }
    }

    /// Builds a theme from five packed-RGB hex colors, one per role — the same five every
    /// named palette above maps its own hues onto. `header` stays modifier-only regardless
    /// of theme: nothing here ever sets a background, so bold+underline is what stays
    /// legible no matter what the terminal's own background is.
    fn palette(accent: u32, ok: u32, warn: u32, error: u32, dim: u32) -> Self {
        Theme {
            accent: Style::default().fg(rgb(accent)),
            ok: Style::default().fg(rgb(ok)).bold(),
            warn: Style::default().fg(rgb(warn)),
            error: Style::default().fg(rgb(error)).bold(),
            dim: Style::default().fg(rgb(dim)),
            header: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        }
    }
}

fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `lh-tui`'s own top-level args: the same subcommands `lh_cli::Cli` parses, plus a
/// `--theme` flag that only makes sense for a screen-drawing binary, so it lives here
/// rather than on the `Cli` shared with the headless `lh` binary.
#[derive(Parser)]
#[command(
    name = "lh-tui",
    version,
    about = "Little Helper — lossless audio for traders, from a terminal UI"
)]
struct Args {
    /// Color theme for the verify/checksum/torrent screens.
    #[arg(long, value_enum, default_value = "default")]
    theme: ThemeName,
    #[command(subcommand)]
    command: Command,
}

fn main() -> ExitCode {
    let cli = Args::parse();
    let theme = cli.theme;
    match cli.command {
        Command::Verify(paths) => run_verify(paths, theme),
        Command::Sbe(a) => match a.command {
            Some(SbeSub::Fix(fix_args)) => run_sbe_fix(fix_args, theme),
            None => run_sbe(a.paths, theme),
        },
        Command::Ffp(args) => run_checksum(ChecksumKind::Ffp, args, theme),
        Command::Md5(args) => run_checksum(ChecksumKind::Md5, args, theme),
        Command::St5(args) => run_checksum(ChecksumKind::St5, args, theme),
        Command::Check { file } => run_check(file, theme),
        Command::Convert(args) => run_convert(args, theme),
        Command::Tag(args) => run_tag(args, theme),
        Command::Rename(args) => run_rename(args, theme),
        Command::Torrent {
            command: TorrentCommand::Info { file, no_files },
        } => run_torrent_info(file, !no_files, theme),
        Command::Torrent {
            command: TorrentCommand::Create(args),
        } => run_torrent_create(args, theme),
        Command::Torrent {
            command: TorrentCommand::Check { file, path, quick },
        } => run_torrent_check(file, path, quick, theme),
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

fn run_verify(paths: Paths, theme: ThemeName) -> ExitCode {
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
fn run(
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

fn draw_header(
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

fn run_checksum(kind: ChecksumKind, args: ChecksumArgs, theme: ThemeName) -> ExitCode {
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
fn run_checksum_screen(
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
}

fn draw_checksum(
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

fn draw_checksum_header(
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

// --- Check (verify files against an existing .ffp/.md5/.st5) ------------------------
//
// Unlike verify/checksum, the row list here doesn't come from scanning a folder for
// audio files — it comes from the checksum file's own entries (`lh_cli::cmd_check`'s
// shape), and checking one just means recomputing its digest and comparing, so a row can
// also come back `Missing`: the entry names a file that isn't there at all, which is not
// the same kind of trouble as a digest that doesn't match or a read that failed outright.

#[derive(Clone)]
enum CheckOutcome {
    Ok,
    Mismatch {
        expected: [u8; 16],
        actual: [u8; 16],
    },
    Missing,
    Failed(String),
}

#[derive(Clone)]
enum CheckStatus {
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

struct CheckRow {
    name: String,
    status: CheckStatus,
}

fn run_check(file: PathBuf, theme: ThemeName) -> ExitCode {
    let kind = match lh_cli::checksum_kind_for(&file) {
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
fn run_check_screen(
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

    let queue: Queue<CheckOutcome> = Queue::new();
    let cancel = queue.cancel_token();
    for e in &entries {
        let target_dir = dir.clone();
        let file_name = e.file_name.clone();
        let expected = e.digest;
        queue.submit(e.file_name.clone(), move |_progress| {
            let target = target_dir.join(&file_name);
            if !target.exists() {
                return CheckOutcome::Missing;
            }
            match compute(kind, &target) {
                Ok(actual) if actual == expected => CheckOutcome::Ok,
                Ok(actual) => CheckOutcome::Mismatch { expected, actual },
                Err(e) => CheckOutcome::Failed(e.to_string()),
            }
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
                        CheckOutcome::Ok => {
                            ok_count += 1;
                            CheckStatus::Ok
                        }
                        CheckOutcome::Mismatch { expected, actual } => {
                            mismatch_count += 1;
                            CheckStatus::Mismatch { expected, actual }
                        }
                        CheckOutcome::Missing => {
                            missing_count += 1;
                            CheckStatus::Missing
                        }
                        CheckOutcome::Failed(e) => {
                            failed_count += 1;
                            CheckStatus::Failed(e)
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

struct CheckStats {
    done: usize,
    total: usize,
    ok: usize,
    missing: usize,
    mismatch: usize,
    failed: usize,
}

/// The bits of the header that don't change frame to frame, grouped the same way
/// `ChecksumMeta` is (`docs/tui.md` §3).
struct CheckMeta<'a> {
    kind: ChecksumKind,
    label: &'a str,
}

fn draw_checklist(
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

fn draw_checklist_header(
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

fn draw_checklist_table(
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

fn checklist_status_cell(status: &CheckStatus, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        CheckStatus::Pending => ("pending".to_string(), theme.dim),
        CheckStatus::Running => (format!("{spin} running"), theme.accent),
        CheckStatus::Ok => ("OK".to_string(), theme.ok),
        CheckStatus::Missing => ("MISSING".to_string(), theme.warn),
        CheckStatus::Mismatch { .. } => ("MISMATCH".to_string(), theme.error),
        CheckStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

fn checklist_status_detail(status: &CheckStatus) -> String {
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

fn draw_checklist_gauge(frame: &mut Frame, area: Rect, stats: &CheckStats, theme: &Theme) {
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

// --- SBE (sector boundary error) ------------------------------------------------------
//
// `analysis::sbe` is a pure, infallible function over a `StreamInfo` `collect` already
// probed — no decode, no I/O, no `Result` — so this screen is the same per-file batch shape
// as verify/checksum (`docs/tui.md` §2) with `T = Sbe` directly rather than `Result<Sbe>`,
// mirroring `cmd_sbe`'s own `run_batch(&files, |f, _| sbe(&f.stream_info))`.

#[derive(Clone)]
enum SbeStatus {
    Pending,
    Running,
    Aligned,
    Misaligned { remainder_frames: u64 },
    NotApplicable { reason: &'static str },
    Failed(String),
}

struct SbeRow {
    name: String,
    status: SbeStatus,
}

fn run_sbe(paths: Paths, theme: ThemeName) -> ExitCode {
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
fn run_sbe_screen(
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

struct SbeStats {
    done: usize,
    total: usize,
    aligned: usize,
    misaligned: usize,
    not_applicable: usize,
    failed: usize,
}

fn draw_sbe(
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

fn draw_sbe_header(
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

fn draw_sbe_table(frame: &mut Frame, area: Rect, rows: &[SbeRow], tick: usize, theme: &Theme) {
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

fn sbe_status_cell(status: &SbeStatus, spin: char, theme: &Theme) -> (String, Style) {
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

fn sbe_status_detail(status: &SbeStatus) -> String {
    match status {
        SbeStatus::Pending | SbeStatus::Running | SbeStatus::Aligned => String::new(),
        SbeStatus::Misaligned { remainder_frames } => {
            format!("+{remainder_frames} frames past a sector boundary")
        }
        SbeStatus::NotApplicable { reason } => reason.to_string(),
        SbeStatus::Failed(e) => e.clone(),
    }
}

fn draw_sbe_gauge(frame: &mut Frame, area: Rect, stats: &SbeStats, theme: &Theme) {
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

// --- SBE fix (TUI R4) -----------------------------------------------------------------
//
// Unlike every batch screen above, a fix's unit of work is the whole ordered set, not one
// file judged alone (docs/sbe-repair.md §7): `plan_fix` needs every file's frame count
// before it can say anything about boundary 1, and `execute_fix` shifts/re-encodes/commits
// the whole set atomically in one call — there is no independent per-file result to stream
// into a `Queue<T>` table the way verify/checksum/sbe do. So this screen skips that shape
// entirely and follows the "one job on a queue of one" pattern torrent create/check already
// use for a single sequential operation over a whole set — resolving docs/sbe-repair.md
// §9 open question 3 in favor of routing around `Queue<T>` rather than growing it a
// chained-submission mode nothing else needs.
//
// The plan itself is pure arithmetic over headers `scan` already read — no decode — so it's
// computed up front and shown as a table before any job runs (`--dry-run` never opens a
// job at all). `execute_fix` has no per-boundary progress to report and no cancellation
// checkpoint, so once it starts, the row for every file just spins until the one `Finished`
// event lands with every file's result at once, the same "no checkpoint, quit doesn't
// actually stop it" shape `run_torrent_check_screen` already accepts for `check_with_progress`.

/// One row per file in the set (not per boundary): `shifted_in`/`shifted_out` are computed
/// straight from the plan using the same formula `execute_fix` uses to fill in `Fixed`'s own
/// fields, so a row's numbers never change between the pre-execution plan and the
/// post-execution result.
struct FixRow {
    name: String,
    shifted_in: i64,
    shifted_out: i64,
    is_tail: bool,
}

fn fix_rows(files: &[AudioFile], plan: &FixPlan) -> Vec<FixRow> {
    let last = files.len() - 1;
    (0..files.len())
        .map(|i| FixRow {
            name: files[i].file_name(),
            shifted_in: if i == 0 {
                0
            } else {
                plan.boundaries[i - 1].shifted_frames
            },
            shifted_out: plan
                .boundaries
                .get(i)
                .map(|b| b.shifted_frames)
                .unwrap_or(0),
            is_tail: i == last,
        })
        .collect()
}

/// What a boundary moved, in the direction it moved it — the file-centric view of
/// `BoundaryFix`'s sign convention (`lh-core/src/analysis/sbe_fix.rs`).
fn fix_shift_note(shifted_in: i64, shifted_out: i64) -> String {
    let mut parts = Vec::new();
    match shifted_in.cmp(&0) {
        std::cmp::Ordering::Greater => parts.push(format!("+{shifted_in} from prev")),
        std::cmp::Ordering::Less => parts.push(format!("{shifted_in} lent to prev")),
        std::cmp::Ordering::Equal => {}
    }
    match shifted_out.cmp(&0) {
        std::cmp::Ordering::Greater => parts.push(format!("-{shifted_out} to next")),
        std::cmp::Ordering::Less => parts.push(format!("+{} from next", -shifted_out)),
        std::cmp::Ordering::Equal => {}
    }
    if parts.is_empty() {
        "aligned".to_string()
    } else {
        parts.join("  ")
    }
}

fn fix_row_note(row: &FixRow, plan: &FixPlan) -> String {
    let shift = fix_shift_note(row.shifted_in, row.shifted_out);
    if !row.is_tail {
        return shift;
    }
    match plan.tail_padding_frames {
        Some(pad) if shift == "aligned" => format!("+{pad} frames padded (silence)"),
        Some(pad) => format!("{shift}, +{pad} frames padded (silence)"),
        None if !plan.fully_fixed => format!("{shift}, still misaligned (needs --pad-tail)"),
        None => shift,
    }
}

enum FixStage {
    /// `--dry-run`: the plan is all there is, nothing runs.
    Planned,
    /// `execute_fix` is running; no progress or cancellation checkpoint to show.
    Fixing,
    Done(Box<lh_core::Result<Vec<Fixed>>>),
}

fn run_sbe_fix(args: SbeFixArgs, theme: ThemeName) -> ExitCode {
    if !args.dir.is_dir() {
        eprintln!("lh-tui: {} is not a directory", args.dir.display());
        return ExitCode::from(2);
    }
    let set = match scan::scan(&args.dir, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lh-tui: scanning {}: {e:#}", args.dir.display());
            return ExitCode::from(2);
        }
    };
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        eprintln!("no audio files found in {}", args.dir.display());
        return ExitCode::from(1);
    }

    let direction = match args.direction {
        SbeFixDirection::Backward => BoundaryDirection::Backward,
        SbeFixDirection::Forward => BoundaryDirection::Forward,
        SbeFixDirection::Nearest => BoundaryDirection::Nearest,
    };
    let tail = if args.pad_tail {
        TailPolicy::Pad
    } else {
        TailPolicy::Report
    };
    let plan = match plan_fix(&set.files, direction, tail) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lh-tui: planning a fix for {}: {e:#}", args.dir.display());
            return ExitCode::from(2);
        }
    };

    if args.dry_run {
        let terminal = ratatui::init();
        let result =
            run_sbe_fix_plan_screen(terminal, &args.dir, &set.files, &plan, Theme::new(theme));
        ratatui::restore();
        return match result {
            Ok(()) if plan.fully_fixed => ExitCode::SUCCESS,
            Ok(()) => ExitCode::from(1),
            Err(e) => {
                eprintln!("lh-tui: {e}");
                ExitCode::from(2)
            }
        };
    }

    let out_dir = match &args.output {
        Some(o) => o.clone(),
        None => {
            eprintln!(
                "lh-tui: sbe fix needs -o/--output to execute — it never writes over the \
                 originals (Principle 1)"
            );
            return ExitCode::from(2);
        }
    };
    for f in &set.files {
        if f.format != AudioFormat::Flac {
            eprintln!(
                "lh-tui: sbe fix can only execute against FLAC ({} is {}); other formats have \
                 no repair path yet",
                f.file_name(),
                f.format
            );
            return ExitCode::from(2);
        }
    }
    let flac = match Registry::discover_one(ToolId::Flac).require(ToolId::Flac) {
        Ok(t) => t.clone(),
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };
    let mut dsts: Vec<PathBuf> = Vec::with_capacity(set.files.len());
    for f in &set.files {
        match destination(&f.path, "flac", Some(&out_dir)) {
            Some(d) => dsts.push(d),
            None => {
                eprintln!("lh-tui: {} has no file name", f.path.display());
                return ExitCode::from(2);
            }
        }
    }

    let terminal = ratatui::init();
    let result = run_sbe_fix_execute_screen(
        terminal,
        &args.dir,
        set.files.clone(),
        plan.clone(),
        dsts,
        flac,
        args.overwrite,
        Theme::new(theme),
    );
    ratatui::restore();

    match result {
        Ok(Ok(fixed)) => {
            for (f, fx) in set.files.iter().zip(&fixed) {
                println!(
                    "FIXED     {} -> {}   audio md5 {}",
                    f.file_name(),
                    fx.path.display(),
                    hex::encode(fx.audio_md5)
                );
            }
            if !plan.fully_fixed {
                println!(
                    "{}   still misaligned — rerun with --pad-tail to close it with silence",
                    set.files
                        .last()
                        .expect("checked non-empty above")
                        .file_name()
                );
            }
            if plan.fully_fixed {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Ok(Err(lh_core::Error::Cancelled)) => {
            eprintln!("cancelled before writing anything");
            ExitCode::from(1)
        }
        Ok(Err(e)) => {
            eprintln!("lh-tui: repairing {}: {e:#}", args.dir.display());
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_sbe_fix_plan_screen(
    mut terminal: DefaultTerminal,
    dir: &Path,
    files: &[AudioFile],
    plan: &FixPlan,
    theme: Theme,
) -> io::Result<()> {
    let rows = fix_rows(files, plan);
    let start = Instant::now();
    loop {
        terminal.draw(|frame| {
            draw_sbe_fix(
                frame,
                dir,
                &rows,
                plan,
                &FixStage::Planned,
                start,
                0,
                &theme,
            )
        })?;
        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// Runs `execute_fix` as the queue's one job. Quitting cancels the queue and breaks the
/// draw loop immediately rather than waiting for `Done`, the same call
/// `run_torrent_check_screen` makes for `check_with_progress`: `execute_fix` has no
/// cancellation checkpoint of its own to honor, so waiting would just mean waiting for work
/// that can't be told to stop early.
#[allow(clippy::too_many_arguments)]
fn run_sbe_fix_execute_screen(
    mut terminal: DefaultTerminal,
    dir: &Path,
    files: Vec<AudioFile>,
    plan: FixPlan,
    dsts: Vec<PathBuf>,
    flac: Tool,
    overwrite: bool,
    theme: Theme,
) -> io::Result<lh_core::Result<Vec<Fixed>>> {
    let rows = fix_rows(&files, &plan);

    let queue: Queue<lh_core::Result<Vec<Fixed>>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();
    let job_files = files;
    let job_plan = plan.clone();
    let job_dsts = dsts;
    queue.submit("sbe fix", move |_progress| {
        let opts = EncodeOpts::default();
        let encode = RepairEncode {
            flac: &flac,
            opts: &opts,
            overwrite,
        };
        execute_fix(&job_files, &job_plan, &job_dsts, &encode)
    });
    let events = queue.events();

    let mut stage = FixStage::Fixing;
    let start = Instant::now();
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { .. } | Event::Progress { .. } => {}
                Event::Finished { output, .. } => stage = FixStage::Done(Box::new(output)),
                Event::Cancelled { .. } => {
                    stage = FixStage::Done(Box::new(Err(lh_core::Error::Cancelled)));
                }
            }
        }

        terminal
            .draw(|frame| draw_sbe_fix(frame, dir, &rows, &plan, &stage, start, tick, &theme))?;

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
        if matches!(stage, FixStage::Done(_)) {
            break;
        }
        tick = tick.wrapping_add(1);
    }

    Ok(match stage {
        FixStage::Done(result) => *result,
        _ => Err(lh_core::Error::Cancelled),
    })
}

#[allow(clippy::too_many_arguments)]
fn draw_sbe_fix(
    frame: &mut Frame,
    dir: &Path,
    rows: &[FixRow],
    plan: &FixPlan,
    stage: &FixStage,
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
    let mode = match stage {
        FixStage::Planned => "sbe fix --dry-run",
        _ => "sbe fix",
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {mode}  ")),
        Span::styled(dir.display().to_string(), theme.dim),
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

    draw_fix_table(frame, chunks[1], rows, plan, stage, tick, theme);
    draw_fix_gauge(frame, chunks[2], plan, stage, tick, theme);
    draw_footer(frame, chunks[3], theme);
}

fn draw_fix_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[FixRow],
    plan: &FixPlan,
    stage: &FixStage,
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().enumerate().map(|(i, row)| {
        let (label, style, detail) = fix_row_cells(i, row, plan, stage, spin, theme);
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
            Constraint::Percentage(30),
            Constraint::Percentage(60),
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

fn fix_row_cells(
    i: usize,
    row: &FixRow,
    plan: &FixPlan,
    stage: &FixStage,
    spin: char,
    theme: &Theme,
) -> (String, Style, String) {
    match stage {
        FixStage::Planned => ("PLAN".to_string(), theme.dim, fix_row_note(row, plan)),
        FixStage::Fixing => (
            format!("{spin} fixing"),
            theme.accent,
            fix_row_note(row, plan),
        ),
        FixStage::Done(result) => match result.as_ref() {
            Ok(fixed) => {
                let f = &fixed[i];
                let name = f
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| f.path.display().to_string());
                (
                    "FIXED".to_string(),
                    theme.ok,
                    format!(
                        "{}  -> {name}  md5 {}",
                        fix_row_note(row, plan),
                        hex::encode(f.audio_md5)
                    ),
                )
            }
            Err(_) => (
                "FAILED".to_string(),
                theme.error,
                "nothing written — see the error below".to_string(),
            ),
        },
    }
}

fn draw_fix_gauge(
    frame: &mut Frame,
    area: Rect,
    plan: &FixPlan,
    stage: &FixStage,
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let (ratio, style, label) = match stage {
        FixStage::Planned => {
            let style = if plan.fully_fixed {
                theme.ok
            } else {
                theme.warn
            };
            let label = if plan.fully_fixed {
                "fully aligned by this plan".to_string()
            } else {
                "tail stays misaligned — rerun with --pad-tail to close it".to_string()
            };
            (1.0, style, label)
        }
        FixStage::Fixing => (0.0, theme.accent, format!("{spin} fixing…")),
        FixStage::Done(result) => match result.as_ref() {
            Ok(fixed) => {
                let style = if plan.fully_fixed {
                    theme.ok
                } else {
                    theme.warn
                };
                let label = if plan.fully_fixed {
                    format!("{} files fixed", fixed.len())
                } else {
                    format!("{} files fixed, tail still misaligned", fixed.len())
                };
                (1.0, style, label)
            }
            Err(e) => (1.0, theme.error, format!("failed: {e:#}")),
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

// --- Convert -------------------------------------------------------------------------
//
// Unlike verify/checksum, this is the one screen where a file's own progress is worth
// showing: `to_wav_with_progress` reports (frames written, frames total) once per decoded
// block, so a decoding row can show a live percentage rather than just a spinner
// (`docs/tui.md` §5 calls this out as "the reason §2 calls out progress rendering as the
// real per-screen variable"). `to_flac_cancellable` has no such number to relay — `flac`
// only draws its own percentage when stderr is a terminal, which piped through `Command`
// it never is (`lh-core/src/convert/mod.rs`'s own doc comment) — so an encoding row just
// spins.

#[derive(Clone)]
enum ConvertStatus {
    Pending,
    Running { done: u32, total: u32 },
    Skipped,
    Done { unchecked: bool, output: String },
    Failed(String),
}

struct ConvertRow {
    name: String,
    status: ConvertStatus,
}

/// Mirrors `lh-cli`'s own (private) `ConvertOutcome` (`lh-cli/src/lib.rs`) — small enough
/// that duplicating it here beats exporting an internal type just for this screen, the
/// same call every other screen's own `Status` enum already makes.
enum ConvertOutcome {
    Skipped,
    NoFileName,
    Done(Box<Conversion>),
    Failed(lh_core::Error),
}

fn run_convert(args: ConvertArgs, theme: ThemeName) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match lh_cli::collect(&args.paths) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e:#}");
            return ExitCode::from(2);
        }
    };

    // Discovered once, up front, exactly like `cmd_convert`: if the encoder is missing,
    // say so before converting anything rather than after half a show.
    let encoder = match args.to {
        Target::Flac => match Registry::discover_one(ToolId::Flac).require(ToolId::Flac) {
            Ok(t) => Some(t.clone()),
            Err(e) => {
                eprintln!("lh-tui: {e:#}");
                return ExitCode::from(2);
            }
        },
        Target::Wav => None,
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
    let result = run_convert_screen(terminal, &label, files, &args, encoder, Theme::new(theme));
    ratatui::restore();

    let (ok, written) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            return ExitCode::from(2);
        }
    };
    clean &= ok;

    // A table cell is nowhere near wide enough for a full provenance render, so
    // `--provenance` prints it after the screen exits instead — same information
    // `cmd_convert`'s own `report_conversion` shows inline, just relocated.
    if args.provenance {
        for done in &written {
            for line in done.provenance.render().lines() {
                println!("{line}");
            }
        }
    }

    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The bits of the header that don't change frame to frame.
struct ConvertMeta<'a> {
    root: &'a str,
    want: AudioFormat,
}

/// Returns whether every file converted cleanly (a skip counts as clean, same as
/// `cmd_convert`'s own exit code) plus every successful conversion's record, in
/// submission order — used only for the post-loop `--provenance` dump above.
fn run_convert_screen(
    mut terminal: DefaultTerminal,
    root: &str,
    files: Vec<AudioFile>,
    args: &ConvertArgs,
    encoder: Option<Tool>,
    theme: Theme,
) -> io::Result<(bool, Vec<Conversion>)> {
    let total = files.len();
    let mut rows: Vec<ConvertRow> = files
        .iter()
        .map(|f| ConvertRow {
            name: f.file_name(),
            status: ConvertStatus::Pending,
        })
        .collect();

    let (want, extension) = match args.to {
        Target::Wav => (AudioFormat::Wav, "wav"),
        Target::Flac => (AudioFormat::Flac, "flac"),
    };
    let to = args.to;
    let force = args.force;
    let out_dir = args.out_dir.clone();
    let opts = EncodeOpts {
        compression_level: args.level,
        ..EncodeOpts::default()
    };

    let queue: Queue<ConvertOutcome> = Queue::new();
    let cancel = queue.cancel_token();
    for f in &files {
        let path = f.path.clone();
        let format = f.format;
        let out_dir = out_dir.clone();
        let opts = opts.clone();
        let encoder = encoder.clone();
        queue.submit(f.file_name(), move |progress| -> ConvertOutcome {
            if format == want {
                return ConvertOutcome::Skipped;
            }
            let dst = match destination(&path, extension, out_dir.as_deref()) {
                Some(d) => d,
                None => return ConvertOutcome::NoFileName,
            };
            let result = match to {
                Target::Wav => to_wav_with_progress(&path, &dst, force, &mut |done, total| {
                    progress.report(done, total);
                    !progress.is_cancelled()
                }),
                Target::Flac => to_flac_cancellable(
                    &path,
                    &dst,
                    encoder
                        .as_ref()
                        .expect("discovered before the screen opened"),
                    &opts,
                    force,
                    &mut || !progress.is_cancelled(),
                ),
            };
            match result {
                Ok(done) => ConvertOutcome::Done(Box::new(done)),
                Err(e) => ConvertOutcome::Failed(e),
            }
        });
    }
    let events = queue.events();

    let mut conversions: Vec<Option<Conversion>> = (0..total).map(|_| None).collect();
    let mut done = 0usize;
    let mut written_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failed_count = 0usize;
    let meta = ConvertMeta { root, want };
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => {
                    rows[id.index()].status = ConvertStatus::Running { done: 0, total: 0 };
                }
                Event::Progress {
                    id,
                    done: d,
                    total: t,
                } => {
                    rows[id.index()].status = ConvertStatus::Running { done: d, total: t };
                }
                Event::Finished { id, output, .. } => {
                    done += 1;
                    rows[id.index()].status = match output {
                        ConvertOutcome::Skipped => {
                            skipped_count += 1;
                            ConvertStatus::Skipped
                        }
                        ConvertOutcome::NoFileName => {
                            failed_count += 1;
                            ConvertStatus::Failed("has no file name to work from".to_string())
                        }
                        ConvertOutcome::Done(c) => {
                            written_count += 1;
                            let status = ConvertStatus::Done {
                                unchecked: !c.checked_against_source,
                                output: c
                                    .output
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| c.output.display().to_string()),
                            };
                            conversions[id.index()] = Some(*c);
                            status
                        }
                        ConvertOutcome::Failed(e) => {
                            failed_count += 1;
                            ConvertStatus::Failed(e.to_string())
                        }
                    };
                }
                Event::Cancelled { id, .. } => {
                    done += 1;
                    failed_count += 1;
                    rows[id.index()].status = ConvertStatus::Failed("cancelled".to_string());
                }
            }
        }

        let stats = ConvertStats {
            done,
            total,
            written: written_count,
            skipped: skipped_count,
            failed: failed_count,
        };
        let elapsed = header_elapsed(start, &mut finished_at, done == total);
        terminal.draw(|frame| draw_convert(frame, &meta, &rows, &stats, elapsed, tick, &theme))?;

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

    Ok((
        failed_count == 0,
        conversions.into_iter().flatten().collect(),
    ))
}

struct ConvertStats {
    done: usize,
    total: usize,
    written: usize,
    skipped: usize,
    failed: usize,
}

fn draw_convert(
    frame: &mut Frame,
    meta: &ConvertMeta,
    rows: &[ConvertRow],
    stats: &ConvertStats,
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

    draw_convert_header(frame, chunks[0], meta, stats, elapsed, theme);
    draw_convert_table(frame, chunks[1], meta, rows, tick, theme);
    draw_convert_gauge(frame, chunks[2], stats, theme);
    draw_footer(frame, chunks[3], theme);
}

fn draw_convert_header(
    frame: &mut Frame,
    area: Rect,
    meta: &ConvertMeta,
    stats: &ConvertStats,
    elapsed: f32,
    theme: &Theme,
) {
    let line = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" convert --to {}  ", meta.want)),
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

fn draw_convert_table(
    frame: &mut Frame,
    area: Rect,
    meta: &ConvertMeta,
    rows: &[ConvertRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style) = convert_status_cell(&row.status, spin, theme);
        let detail = convert_status_detail(&row.status, meta.want);
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
    .header(Row::new(vec!["status", "file", "detail"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

fn convert_status_cell(status: &ConvertStatus, spin: char, theme: &Theme) -> (String, Style) {
    match status {
        ConvertStatus::Pending => ("pending".to_string(), theme.dim),
        ConvertStatus::Running { done, total } if *total > 0 => {
            let pct = (u64::from(*done) * 100 / u64::from(*total)).min(100);
            (format!("{spin} {pct}%"), theme.accent)
        }
        ConvertStatus::Running { .. } => (format!("{spin} running"), theme.accent),
        ConvertStatus::Skipped => ("SKIPPED".to_string(), theme.dim),
        ConvertStatus::Done { .. } => ("OK".to_string(), theme.ok),
        ConvertStatus::Failed(_) => ("FAILED".to_string(), theme.error),
    }
}

fn convert_status_detail(status: &ConvertStatus, want: AudioFormat) -> String {
    match status {
        ConvertStatus::Pending | ConvertStatus::Running { .. } => String::new(),
        ConvertStatus::Skipped => format!("already {want}"),
        ConvertStatus::Done { unchecked, output } => {
            if *unchecked {
                format!("-> {output}  (unchecked: nothing to compare against)")
            } else {
                format!("-> {output}")
            }
        }
        ConvertStatus::Failed(e) => e.clone(),
    }
}

fn draw_convert_gauge(frame: &mut Frame, area: Rect, stats: &ConvertStats, theme: &Theme) {
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
        "{}/{} written:{} skipped:{} failed:{}",
        stats.done, stats.total, stats.written, stats.skipped, stats.failed
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
    /// Picking which trackers to announce to, before anything about the payload is
    /// touched — pre-selected from `--tracker` if any were given, but always confirmed
    /// on screen rather than assumed (TUI9: a TUI whose only tracker input is a
    /// command-line flag isn't much of a TUI).
    ChoosingTrackers,
    /// Walking the payload — after trackers are confirmed, before the first `Progress`
    /// event.
    Preparing,
    Hashing {
        done: u32,
        total: u32,
    },
    Done(Box<lh_core::Result<Created>>),
}

/// Which widget owns keystrokes on the tracker-picker stage. `None` is "nothing is being
/// edited," the same convention `TagFocus::None` uses on the tag screen, so a stray
/// `a`/`q` on the list itself confirms or quits rather than being typed into a field.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CreateFocus {
    None,
    List,
    Custom,
}

fn run_torrent_create(args: TorrentCreateArgs, theme: ThemeName) -> ExitCode {
    let source = match args.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e}", args.path.display());
            return ExitCode::from(2);
        }
    };

    // Tracker resolution (`lh_core::torrent::resolve`, same as `cmd_torrent_create`) now
    // happens on the picker stage inside the screen itself, seeded from `--tracker` —
    // an unknown id or a broken entry surfaces there as an inline error to fix, rather
    // than a preflight exit, since the picker gives a way to fix it without restarting.
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
        &args,
        &list,
        &keys,
        Theme::new(theme),
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

/// Turns a confirmed tracker pick into the running job: resolves `picked` against the
/// list, builds `CreateOpts`, and starts the queue. On success returns what the rest of
/// the screen needs to show and to keep resolving further edits against.
struct Started {
    queue: Queue<lh_core::Result<Created>>,
    cancel: CancelToken,
}

fn start_create(
    source: &Path,
    dst: &Path,
    args: &TorrentCreateArgs,
    picked: &[String],
    list: &TrackerList,
    keys: &Passkeys,
) -> lh_core::Result<(Started, Resolved, bool, Option<String>)> {
    let chosen = resolve(picked, list, keys)?;
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

    let queue: Queue<lh_core::Result<Created>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();
    let job_source = source.to_path_buf();
    let job_dst = dst.to_path_buf();
    queue.submit("torrent create", move |progress| {
        create_with_progress(&job_source, &job_dst, &opts, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });

    Ok((Started { queue, cancel }, chosen, private, source_tag))
}

#[allow(clippy::too_many_arguments)]
fn run_torrent_create_screen(
    mut terminal: DefaultTerminal,
    source: &Path,
    dst: &Path,
    args: &TorrentCreateArgs,
    list: &TrackerList,
    keys: &Passkeys,
    theme: Theme,
) -> io::Result<lh_core::Result<Created>> {
    let entries: Vec<Tracker> = list.iter().cloned().collect();
    let mut picked: Vec<String> = args.trackers.clone();
    let mut cursor = 0usize;
    let mut custom = Field::default();
    let mut focus = CreateFocus::None;
    let mut pick_error: Option<String> = None;

    let mut stage = CreateStage::ChoosingTrackers;
    let mut started: Option<Started> = None;
    let mut chosen = Resolved::default();
    let mut private = false;
    let mut source_tag: Option<String> = None;

    let mut start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;
    let mut want_quit = false;

    loop {
        if let Some(s) = &started {
            while let Ok(event) = s.queue.events().try_recv() {
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
        }

        // Picking trackers is the user thinking, not the tool working — the clock
        // starts once creation actually kicks off (below), not when this screen opens.
        let elapsed = if matches!(stage, CreateStage::ChoosingTrackers) {
            0.0
        } else {
            header_elapsed(
                start,
                &mut finished_at,
                matches!(stage, CreateStage::Done(_)),
            )
        };
        terminal.draw(|frame| {
            draw_torrent_create(
                frame,
                source,
                dst,
                &entries,
                &picked,
                cursor,
                &custom,
                focus,
                pick_error.as_deref(),
                &chosen,
                private,
                &source_tag,
                &stage,
                elapsed,
                tick,
                &theme,
            )
        })?;

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    match &stage {
                        CreateStage::ChoosingTrackers => {
                            if ctrl_c
                                || (focus == CreateFocus::None
                                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
                            {
                                // Nothing has started yet — quitting here is immediate,
                                // not a cancel-and-wait.
                                break;
                            }
                            match focus {
                                CreateFocus::None => match key.code {
                                    KeyCode::Tab => focus = CreateFocus::List,
                                    KeyCode::BackTab => focus = CreateFocus::Custom,
                                    KeyCode::Char('a') | KeyCode::Enter => {
                                        match start_create(source, dst, args, &picked, list, keys) {
                                            Ok((s, c, p, t)) => {
                                                chosen = c;
                                                private = p;
                                                source_tag = t;
                                                started = Some(s);
                                                pick_error = None;
                                                stage = CreateStage::Preparing;
                                                start = Instant::now();
                                            }
                                            Err(e) => pick_error = Some(format!("{e:#}")),
                                        }
                                    }
                                    _ => {}
                                },
                                CreateFocus::List => match key.code {
                                    KeyCode::Esc => focus = CreateFocus::None,
                                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                                    KeyCode::Down => {
                                        cursor = (cursor + 1).min(entries.len().saturating_sub(1));
                                    }
                                    KeyCode::Tab => focus = CreateFocus::Custom,
                                    KeyCode::BackTab => focus = CreateFocus::None,
                                    KeyCode::Char(' ') => {
                                        if let Some(t) = entries.get(cursor) {
                                            match picked.iter().position(|p| *p == t.id) {
                                                Some(i) => {
                                                    picked.remove(i);
                                                }
                                                None => picked.push(t.id.clone()),
                                            }
                                            pick_error = None;
                                        }
                                    }
                                    _ => {}
                                },
                                CreateFocus::Custom => match key.code {
                                    KeyCode::Esc => focus = CreateFocus::None,
                                    KeyCode::Tab => focus = CreateFocus::None,
                                    KeyCode::BackTab => focus = CreateFocus::List,
                                    KeyCode::Enter => {
                                        for spec in custom.value.split(',') {
                                            let spec = spec.trim();
                                            if !spec.is_empty() {
                                                picked.push(spec.to_string());
                                            }
                                        }
                                        custom = Field::default();
                                        pick_error = None;
                                    }
                                    // An unknown id or a typo'd URL, once added, has no row
                                    // in the list to toggle back off — backspace on an
                                    // already-empty field pops the most recent pick instead,
                                    // the same "backspace removes the last chip" convention
                                    // a comma-separated tag input uses.
                                    KeyCode::Backspace if custom.value.is_empty() => {
                                        picked.pop();
                                        pick_error = None;
                                    }
                                    other => {
                                        custom.on_key(other);
                                    }
                                },
                            }
                        }
                        CreateStage::Preparing
                        | CreateStage::Hashing { .. }
                        | CreateStage::Done(_) => {
                            let quit =
                                ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc);
                            if quit {
                                if let Some(s) = &started {
                                    s.cancel.cancel();
                                }
                                want_quit = true;
                            }
                        }
                    }
                }
                CtEvent::Paste(text)
                    if matches!(stage, CreateStage::ChoosingTrackers)
                        && focus == CreateFocus::Custom =>
                {
                    for c in text.chars().filter(|c| *c != '\n' && *c != '\r') {
                        custom.on_key(KeyCode::Char(c));
                    }
                }
                _ => {}
            }
        }
        if want_quit && matches!(stage, CreateStage::Done(_)) {
            break;
        }
        tick = tick.wrapping_add(1);
    }

    match stage {
        CreateStage::Done(result) => Ok(*result),
        _ => Ok(Err(lh_core::Error::Cancelled)),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_torrent_create(
    frame: &mut Frame,
    source: &Path,
    dst: &Path,
    entries: &[Tracker],
    picked: &[String],
    cursor: usize,
    custom: &Field,
    focus: CreateFocus,
    pick_error: Option<&str>,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
    stage: &CreateStage,
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

    let mode = if matches!(stage, CreateStage::ChoosingTrackers) {
        " torrent create — trackers  "
    } else {
        " torrent create  "
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(mode),
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

    if matches!(stage, CreateStage::ChoosingTrackers) {
        draw_tracker_picker(
            frame, chunks[1], entries, picked, cursor, custom, focus, theme,
        );
        let status = match pick_error {
            Some(e) => Line::styled(format!(" {e}"), theme.error),
            None if picked.is_empty() => {
                Line::styled(" no trackers picked yet (a trackerless torrent)", theme.dim)
            }
            None => Line::styled(format!(" picked: {}", picked.join(", ")), theme.dim),
        };
        frame.render_widget(
            Paragraph::new(status).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.dim),
            ),
            chunks[2],
        );
    } else {
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
    }

    let footer = match (stage, focus) {
        (CreateStage::ChoosingTrackers, CreateFocus::None) => {
            " tab select   space toggle   a/enter start   q/esc quit "
        }
        (CreateStage::ChoosingTrackers, CreateFocus::List) => {
            " ↑/↓ move   space toggle   tab custom   esc back "
        }
        (CreateStage::ChoosingTrackers, CreateFocus::Custom) => {
            " type id or URL, comma-separated   enter add   backspace-on-empty removes last   esc back "
        }
        _ => " q/esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), chunks[3]);
}

#[allow(clippy::too_many_arguments)]
fn draw_tracker_picker(
    frame: &mut Frame,
    area: Rect,
    entries: &[Tracker],
    picked: &[String],
    cursor: usize,
    custom: &Field,
    focus: CreateFocus,
    theme: &Theme,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let is_picked = picked.contains(&t.id);
            let focused = focus == CreateFocus::List && i == cursor;
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            let mark = if is_picked { "[x]" } else { "[ ]" };
            let marker = if focused { ">" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{mark} "), theme.dim),
                Span::styled(format!("{:<14}", t.id), style),
                Span::styled(format!("{:<26}", t.name), style),
                Span::styled(t.health.label(), theme.dim),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if focus == CreateFocus::List {
        state.select(Some(cursor));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" known trackers ")
            .border_style(theme.dim),
    );
    frame.render_stateful_widget(list, rows[0], &mut state);

    let custom_focused = focus == CreateFocus::Custom;
    let custom_line = Line::from(vec![
        Span::raw("add: "),
        Span::styled(
            custom.display(custom_focused),
            if custom_focused {
                theme.accent
            } else {
                Style::default()
            },
        ),
    ]);
    frame.render_widget(
        Paragraph::new(custom_line).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" custom id or URL (comma-separated) ")
                .border_style(theme.dim),
        ),
        rows[1],
    );
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
        CreateStage::ChoosingTrackers => {
            unreachable!("draw_torrent_create only calls create_lines once trackers are chosen")
        }
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
        CreateStage::ChoosingTrackers => {
            unreachable!(
                "draw_torrent_create only calls draw_create_gauge once trackers are chosen"
            )
        }
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

fn run_torrent_check(file: PathBuf, path: PathBuf, quick: bool, theme: ThemeName) -> ExitCode {
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

fn run_torrent_check_screen(
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
            check_with_progress(&meta, &torrent_path, &given, &mut |done, total| {
                progress.report(done, total);
            })
        }
    });
    let events = queue.events();

    let mut stage = CheckStage::Preparing;
    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

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

// --- Torrent info ------------------------------------------------------------------------
//
// The one screen with no `Queue` at all (`docs/tui.md` §9): `Metainfo::read` is a single
// in-process parse, not a job worth submitting anywhere, so there is nothing to stream —
// the screen reads the file once before `ratatui::init()` and just redraws the same static
// content every 80ms tick until `q`/`Esc`/`Ctrl-C`, the same three keys every other screen
// uses to quit. Redrawing on every tick rather than once is what lets a terminal resize
// reflow the layout for free, same as every live screen already does.

fn run_torrent_info(file: PathBuf, list_files: bool, theme: ThemeName) -> ExitCode {
    let t = match Metainfo::read(&file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e:#}", file.display());
            return ExitCode::from(2);
        }
    };

    let terminal = ratatui::init();
    let result = run_torrent_info_screen(terminal, &t, list_files, Theme::new(theme));
    ratatui::restore();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

fn run_torrent_info_screen(
    mut terminal: DefaultTerminal,
    t: &Metainfo,
    list_files: bool,
    theme: Theme,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw_torrent_info(frame, t, list_files, &theme))?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

fn draw_torrent_info(frame: &mut Frame, t: &Metainfo, list_files: bool, theme: &Theme) {
    let area = frame.area();
    let lines = torrent_info_lines(t, theme);
    let detail_height = lines.len() as u16 + 2;

    let chunks = if list_files {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(detail_height),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area)
    };

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" torrent info  "),
        Span::styled(t.name.clone(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" torrent ")
                .border_style(theme.dim),
        ),
        chunks[1],
    );

    if list_files {
        draw_torrent_info_files(frame, chunks[2], t, theme);
        draw_footer(frame, chunks[3], theme);
    } else {
        draw_footer(frame, chunks[2], theme);
    }
}

/// Mirrors `lh-cli`'s `cmd_torrent_info` line-by-line so the two front ends never drift.
fn torrent_info_lines<'a>(t: &Metainfo, theme: &'a Theme) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(format!("infohash     {}", t.info_hash_hex())),
        Line::from(format!(
            "pieces       {} x {}",
            t.pieces.len(),
            format_bytes(t.piece_length)
        )),
        Line::from(format!(
            "total        {} ({} bytes)",
            format_bytes(t.total_length),
            t.total_length
        )),
    ];
    let real = t.real_files().count();
    let pad = t.files.len() - real;
    lines.push(if pad > 0 {
        Line::from(format!("files        {real} ({pad} padding)"))
    } else {
        Line::from(format!("files        {real}"))
    });
    if t.private {
        lines.push(Line::styled(
            "private      yes (BEP 27; part of the infohash)",
            theme.warn,
        ));
    }
    if let Some(v) = &t.source {
        lines.push(Line::from(format!("source       {v}")));
    }
    if let Some(v) = &t.created_by {
        lines.push(Line::from(format!("created by   {v}")));
    }
    if let Some(ts) = t.creation_date {
        lines.push(Line::from(format!(
            "created      {}",
            lh_cli::format_date(ts)
        )));
    }
    if let Some(v) = &t.comment {
        for (i, line) in v.lines().enumerate() {
            lines.push(Line::from(format!(
                "{:<12} {line}",
                if i == 0 { "comment" } else { "" }
            )));
        }
    }
    for (i, tracker) in t.trackers().enumerate() {
        lines.push(Line::from(format!(
            "{:<12} {tracker}",
            if i == 0 { "trackers" } else { "" }
        )));
    }
    lines
}

fn draw_torrent_info_files(frame: &mut Frame, area: Rect, t: &Metainfo, theme: &Theme) {
    let rows = t.real_files().map(|f| {
        Row::new(vec![
            Cell::from(format_bytes(f.length)),
            Cell::from(f.display_path()),
        ])
    });

    let table = Table::new(rows, [Constraint::Length(11), Constraint::Percentage(100)])
        .header(Row::new(vec!["size", "file"]).style(theme.header))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" files ")
                .border_style(theme.dim),
        );

    frame.render_widget(table, area);
}

// --- Tag / Rename ----------------------------------------------------------------------
//
// The first screens in this repo that edit rather than watch (docs/tagging.md §6). Every
// earlier screen submits jobs to a `Queue` and folds events into a table; nothing above
// takes keyboard input beyond quitting. Two things follow, both recorded in
// `docs/tui.md`'s editor-screen section:
//
// * **`q` cannot mean quit while a field has focus** — it is a letter someone is typing.
//   `Esc` leaves the focused field (or, from no field, leaves the screen); `Ctrl-C`
//   always aborts, everywhere; `q` quits only when nothing is focused.
// * **Bracketed paste is enabled for the lifetime of these two screens only** —
//   `ratatui::init()` does not turn it on, so a pasted setlist would otherwise arrive as
//   individual `Char` events racing the 80ms poll loop.

/// A single-line text input: the value plus a cursor kept as a *character* index (not a
/// byte one), so it walks a non-ASCII title correctly. No new dependency — every field on
/// both screens below is a handful of `char`/`Backspace`/arrow-key cases away from a
/// `String`.
#[derive(Clone, Default)]
struct Field {
    value: String,
    cursor: usize,
}

impl Field {
    fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        let cursor = value.chars().count();
        Field { value, cursor }
    }

    /// An empty field means *leave this alone* — the same thing a CLI flag left unset
    /// means for `Tags`/`TagArgs` (docs/tagging.md §4) — not *set it to the empty
    /// string*. There is deliberately no way to force-clear a field from this screen;
    /// `lh tag --artist ''` still can, from the CLI.
    fn edit_value(&self) -> Option<String> {
        (!self.value.is_empty()).then(|| self.value.clone())
    }

    fn byte_index(&self) -> usize {
        self.value
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    /// A thin cursor glyph spliced into the text at render time — not a real terminal
    /// cursor, which would need this widget to know its own screen coordinates inside
    /// whatever layout is drawing it.
    fn display(&self, focused: bool) -> String {
        if !focused {
            return self.value.clone();
        }
        let idx = self.byte_index();
        let mut s = String::with_capacity(self.value.len() + 3);
        s.push_str(&self.value[..idx]);
        s.push('▏');
        s.push_str(&self.value[idx..]);
        s
    }

    /// Handles one key; `false` means this field had nothing to do with it.
    fn on_key(&mut self, code: KeyCode) -> bool {
        match code {
            KeyCode::Char(c) => {
                let idx = self.byte_index();
                self.value.insert(idx, c);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let idx = self.byte_index();
                    self.value.remove(idx);
                }
                true
            }
            KeyCode::Delete => {
                let idx = self.byte_index();
                if idx < self.value.len() {
                    self.value.remove(idx);
                }
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                true
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                true
            }
            KeyCode::Home => {
                self.cursor = 0;
                true
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                true
            }
            _ => false,
        }
    }
}

/// Splits a pasted block on newlines into exactly `len` titles, padding with blanks or
/// truncating extras. Unlike `cmd_tag`'s one-shot `--titles FILE` read, which errors out
/// on a count mismatch (docs/tagging.md §5), this is a live editable field — a paste that
/// is briefly the wrong length is finished by editing afterwards, not by failing the
/// screen.
fn paste_titles(text: &str, len: usize) -> Vec<Field> {
    let mut fields: Vec<Field> = text.lines().map(Field::new).collect();
    fields.resize_with(len, Field::default);
    fields
}

// --- Tag ---------------------------------------------------------------------------------

/// The six show-level fields a person edits directly, in the order shown on screen. The
/// other two of `tag::Field::ALL` are handled separately: `TITLE` is per-track (the
/// titles pane below), `TRACKNUMBER` is always derived from position and has no field at
/// all (docs/tagging.md §5).
const TAG_SHOW_FIELDS: [tag::Field; 6] = [
    tag::Field::Artist,
    tag::Field::Album,
    tag::Field::Date,
    tag::Field::Genre,
    tag::Field::Comment,
    tag::Field::Location,
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagFocus {
    None,
    Show(usize),
    Titles,
}

enum TagStage {
    Editing,
    Writing,
    Done,
}

#[derive(Clone)]
enum WriteStatus {
    Pending,
    Running,
    NotApplicable,
    Unchanged,
    Ok,
    Failed(String),
}

struct WriteRow {
    name: String,
    status: WriteStatus,
}

fn run_tag(args: TagArgs, theme: ThemeName) -> ExitCode {
    if !args.dir.is_dir() {
        eprintln!("lh-tui: {} is not a directory", args.dir.display());
        return ExitCode::from(2);
    }
    let set = match scan::scan(&args.dir, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lh-tui: scanning {}: {e:#}", args.dir.display());
            return ExitCode::from(2);
        }
    };
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        eprintln!("no audio files found in {}", args.dir.display());
        return ExitCode::from(1);
    }

    // Read every taggable file's existing tags up front — the "before" state the diff
    // pane and the apply stage's audio-unchanged postcondition are both built from
    // (docs/tagging.md §1 contract point 2).
    let mut before: Vec<Tags> = Vec::with_capacity(set.files.len());
    for f in &set.files {
        if tag::is_taggable(f.format) {
            match tag::read(&f.path) {
                Ok(t) => before.push(t),
                Err(e) => {
                    eprintln!("lh-tui: reading tags from {}: {e:#}", f.path.display());
                    return ExitCode::from(2);
                }
            }
        } else {
            before.push(Tags::default());
        }
    }

    let show_name = args
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ShowName::parse);

    // Seed the show-level fields from the first taggable file that already carries any
    // tags, then from the folder name's own date where that leaves it blank
    // (docs/tagging.md §6).
    let mut show = before
        .iter()
        .find(|t| !t.is_empty())
        .cloned()
        .unwrap_or_default();
    if show.date.is_none() {
        if let Some(sn) = &show_name {
            show.date = Some(sn.date.render_iso());
        }
    }
    if let Some(v) = &args.artist {
        show.artist = Some(v.clone());
    }
    if let Some(v) = &args.album {
        show.album = Some(v.clone());
    }
    if let Some(v) = &args.date {
        show.date = Some(v.clone());
    }
    if let Some(v) = &args.genre {
        show.genre = Some(v.clone());
    }
    if let Some(v) = &args.comment {
        show.comment = Some(v.clone());
    }
    if let Some(v) = &args.location {
        show.location = Some(v.clone());
    }

    let mut titles: Vec<String> = before
        .iter()
        .map(|t| t.title.clone().unwrap_or_default())
        .collect();
    if let Some(path) = &args.titles {
        let text = if path == Path::new("-") {
            match io::read_to_string(io::stdin()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("lh-tui: reading titles from stdin: {e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("lh-tui: reading {}: {e}", path.display());
                    return ExitCode::from(2);
                }
            }
        };
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() != set.files.len() {
            eprintln!(
                "lh-tui: {} titles given but {} files in {}",
                lines.len(),
                set.files.len(),
                args.dir.display()
            );
            return ExitCode::from(2);
        }
        titles = lines;
    }

    let terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
    let result = run_tag_screen(
        terminal,
        &args.dir,
        set.files,
        before,
        show,
        titles,
        Theme::new(theme),
    );
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();

    match result {
        Ok(ok) => {
            if ok {
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

fn tag_edit(fields: &[Field; 6], titles: &[Field], track: usize) -> Tags {
    let mut edit = Tags::default();
    for (field, value) in TAG_SHOW_FIELDS.iter().zip(fields.iter()) {
        edit.set(*field, value.edit_value());
    }
    edit.track_number = Some((track + 1).to_string());
    edit.title = titles.get(track).and_then(Field::edit_value);
    edit
}

/// Builds the write queue for the current edit, or `None` when there is nothing taggable
/// to change — the interactive equivalent of `cmd_tag`'s "nothing to change" early return.
/// Returns the rows to show immediately (covering every file, including the ones that
/// never get a job) alongside them, since a row's final status for `NotApplicable` and
/// `Unchanged` is already known without running anything.
fn start_tag_write(
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
) -> (
    Option<Queue<lh_core::Result<()>>>,
    Vec<usize>,
    Vec<WriteRow>,
) {
    let mut rows = Vec::with_capacity(files.len());
    let mut edits = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let edit = tag_edit(fields, titles, i);
        let status = if !taggable[i] {
            WriteStatus::NotApplicable
        } else if before[i].changes(&edit).is_empty() {
            WriteStatus::Unchanged
        } else {
            WriteStatus::Pending
        };
        rows.push(WriteRow {
            name: f.file_name(),
            status,
        });
        edits.push(edit);
    }

    if !rows
        .iter()
        .any(|r| matches!(r.status, WriteStatus::Pending))
    {
        return (None, Vec::new(), rows);
    }

    let queue: Queue<lh_core::Result<()>> = Queue::new();
    let mut submitted = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if !matches!(rows[i].status, WriteStatus::Pending) {
            continue;
        }
        let path = f.path.clone();
        let edit = edits[i].clone();
        queue.submit(f.file_name(), move |_progress| -> lh_core::Result<()> {
            let audio_before = ffp(&path)?;
            tag::apply(&path, &edit)?;
            tag::assert_audio_unchanged(&path, audio_before)?;
            Ok(())
        });
        submitted.push(i);
    }
    (Some(queue), submitted, rows)
}

#[allow(clippy::too_many_arguments)]
fn run_tag_screen(
    mut terminal: DefaultTerminal,
    dir: &Path,
    files: Vec<AudioFile>,
    before: Vec<Tags>,
    show_seed: Tags,
    title_seed: Vec<String>,
    theme: Theme,
) -> io::Result<bool> {
    let taggable: Vec<bool> = files.iter().map(|f| tag::is_taggable(f.format)).collect();
    let mut fields: [Field; 6] =
        TAG_SHOW_FIELDS.map(|f| Field::new(show_seed.get(f).unwrap_or_default()));
    let mut titles: Vec<Field> = title_seed.into_iter().map(Field::new).collect();
    let mut focus = TagFocus::None;
    let mut title_idx = 0usize;

    let mut stage = TagStage::Editing;
    let mut rows: Vec<WriteRow> = files
        .iter()
        .map(|f| WriteRow {
            name: f.file_name(),
            status: WriteStatus::Pending,
        })
        .collect();
    let mut queue: Option<Queue<lh_core::Result<()>>> = None;
    let mut submitted_rows: Vec<usize> = Vec::new();
    let mut wrote = 0usize;
    let mut failed = 0usize;

    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        if let Some(q) = &queue {
            while let Ok(event) = q.events().try_recv() {
                match event {
                    Event::Started { id, .. } => {
                        rows[submitted_rows[id.index()]].status = WriteStatus::Running;
                    }
                    Event::Progress { .. } => {}
                    Event::Finished { id, output, .. } => {
                        let row = submitted_rows[id.index()];
                        rows[row].status = match output {
                            Ok(()) => {
                                wrote += 1;
                                WriteStatus::Ok
                            }
                            Err(e) => {
                                failed += 1;
                                WriteStatus::Failed(e.to_string())
                            }
                        };
                    }
                    Event::Cancelled { id, .. } => {
                        failed += 1;
                        rows[submitted_rows[id.index()]].status =
                            WriteStatus::Failed("cancelled".to_string());
                    }
                }
            }
            if matches!(stage, TagStage::Writing)
                && rows
                    .iter()
                    .all(|r| !matches!(r.status, WriteStatus::Pending | WriteStatus::Running))
            {
                stage = TagStage::Done;
            }
        }

        let elapsed = header_elapsed(start, &mut finished_at, matches!(stage, TagStage::Done));
        terminal.draw(|frame| {
            draw_tag(
                frame, dir, &files, &taggable, &before, &fields, &titles, focus, title_idx, &rows,
                &stage, wrote, failed, elapsed, tick, &theme,
            )
        })?;

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if let Some(q) = &queue {
                            q.cancel_token().cancel();
                        }
                        break;
                    }
                    match stage {
                        TagStage::Editing => match key.code {
                            KeyCode::Esc => focus = TagFocus::None,
                            KeyCode::Char('q') if focus == TagFocus::None => break,
                            KeyCode::Char('a') if focus == TagFocus::None => {
                                let (q, subs, initial_rows) =
                                    start_tag_write(&files, &taggable, &before, &fields, &titles);
                                rows = initial_rows;
                                submitted_rows = subs;
                                stage = if q.is_some() {
                                    TagStage::Writing
                                } else {
                                    TagStage::Done
                                };
                                queue = q;
                            }
                            KeyCode::Char('e') if focus == TagFocus::None => {
                                focus = TagFocus::Titles;
                                title_idx = title_idx.min(titles.len().saturating_sub(1));
                            }
                            KeyCode::Tab => {
                                focus = match focus {
                                    TagFocus::None | TagFocus::Titles => TagFocus::Show(0),
                                    TagFocus::Show(i) => {
                                        TagFocus::Show((i + 1) % TAG_SHOW_FIELDS.len())
                                    }
                                };
                            }
                            KeyCode::BackTab => {
                                focus = match focus {
                                    TagFocus::None | TagFocus::Titles => {
                                        TagFocus::Show(TAG_SHOW_FIELDS.len() - 1)
                                    }
                                    TagFocus::Show(0) => TagFocus::Show(TAG_SHOW_FIELDS.len() - 1),
                                    TagFocus::Show(i) => TagFocus::Show(i - 1),
                                };
                            }
                            other => match focus {
                                TagFocus::Show(i) => {
                                    fields[i].on_key(other);
                                }
                                TagFocus::Titles => match other {
                                    KeyCode::Up => title_idx = title_idx.saturating_sub(1),
                                    KeyCode::Down => {
                                        title_idx =
                                            (title_idx + 1).min(titles.len().saturating_sub(1));
                                    }
                                    _ => {
                                        if let Some(t) = titles.get_mut(title_idx) {
                                            t.on_key(other);
                                        }
                                    }
                                },
                                TagFocus::None => {}
                            },
                        },
                        TagStage::Writing | TagStage::Done => {
                            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                                if let Some(q) = &queue {
                                    q.cancel_token().cancel();
                                }
                                break;
                            }
                        }
                    }
                }
                CtEvent::Paste(text)
                    if matches!(stage, TagStage::Editing) && focus == TagFocus::Titles =>
                {
                    titles = paste_titles(&text, files.len());
                    title_idx = title_idx.min(titles.len().saturating_sub(1));
                }
                _ => {}
            }
        }
        tick = tick.wrapping_add(1);
    }
    Ok(failed == 0)
}

#[allow(clippy::too_many_arguments)]
fn draw_tag(
    frame: &mut Frame,
    dir: &Path,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    rows: &[WriteRow],
    stage: &TagStage,
    wrote: usize,
    failed: usize,
    elapsed: f32,
    tick: usize,
    theme: &Theme,
) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = match stage {
        TagStage::Editing => "tag",
        TagStage::Writing => "tag (writing)",
        TagStage::Done => "tag (done)",
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {mode}  ")),
        Span::styled(dir.display().to_string(), theme.dim),
        Span::raw(format!("   {elapsed:.1}s")),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        outer[0],
    );

    match stage {
        TagStage::Editing => draw_tag_editor(
            frame, outer[1], files, taggable, before, fields, titles, focus, title_idx, theme,
        ),
        TagStage::Writing | TagStage::Done => draw_write_table(frame, outer[1], rows, tick, theme),
    }

    draw_write_gauge(frame, outer[2], rows, stage, wrote, failed, theme);

    let footer = match (stage, focus) {
        (TagStage::Editing, TagFocus::None) => " tab fields   e titles   a apply   q/esc quit ",
        (TagStage::Editing, TagFocus::Titles) => {
            " ↑/↓ select line   paste replaces all   esc leave "
        }
        (TagStage::Editing, _) => " esc leave field   type to edit ",
        _ => " q/esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), outer[3]);
}

#[allow(clippy::too_many_arguments)]
fn draw_tag_editor(
    frame: &mut Frame,
    area: Rect,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    theme: &Theme,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(cols[0]);

    draw_show_fields(frame, left[0], fields, focus, theme);
    draw_titles(frame, left[1], titles, focus, title_idx, theme);
    draw_tag_diff(
        frame, cols[1], files, taggable, before, fields, titles, theme,
    );
}

fn draw_show_fields(
    frame: &mut Frame,
    area: Rect,
    fields: &[Field; 6],
    focus: TagFocus,
    theme: &Theme,
) {
    let lines: Vec<Line> = TAG_SHOW_FIELDS
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let focused = focus == TagFocus::Show(i);
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{:<11}", f.key()), theme.header),
                Span::styled(fields[i].display(focused), style),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" show ")
                .border_style(theme.dim),
        ),
        area,
    );
}

fn draw_titles(
    frame: &mut Frame,
    area: Rect,
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    theme: &Theme,
) {
    let items: Vec<ListItem> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let focused = focus == TagFocus::Titles && i == title_idx;
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            let marker = if focused { ">" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{:>3}  ", i + 1), theme.dim),
                Span::styled(t.display(focused), style),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if focus == TagFocus::Titles {
        state.select(Some(title_idx));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" titles (e to edit, paste to replace) ")
            .border_style(theme.dim),
    );
    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::too_many_arguments)]
fn draw_tag_diff(
    frame: &mut Frame,
    area: Rect,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    theme: &Theme,
) {
    let table_rows = files.iter().enumerate().map(|(i, f)| {
        if !taggable[i] {
            return Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("N/A").style(theme.dim),
                Cell::from("no Vorbis comments").style(theme.dim),
            ]);
        }
        let edit = tag_edit(fields, titles, i);
        let changes = before[i].changes(&edit);
        if changes.is_empty() {
            Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("unchanged").style(theme.dim),
                Cell::from(""),
            ])
        } else {
            let detail = changes
                .iter()
                .map(|(field, old, new)| {
                    format!("{}: {:?} -> {new:?}", field.key(), old.unwrap_or(""))
                })
                .collect::<Vec<_>>()
                .join(", ");
            Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("changed").style(theme.accent),
                Cell::from(detail),
            ])
        }
    });

    let table = Table::new(
        table_rows,
        [
            Constraint::Percentage(30),
            Constraint::Length(10),
            Constraint::Percentage(60),
        ],
    )
    .header(Row::new(vec!["file", "status", "changes"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" diff ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

/// Shared by the tag and rename apply stages: both settle into "one row per file, status
/// going Pending → Running → OK/FAILED" (docs/tagging.md §6), just with a different
/// `WriteStatus` producer behind it.
fn draw_write_table(frame: &mut Frame, area: Rect, rows: &[WriteRow], tick: usize, theme: &Theme) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style, detail) = write_row_cells(&row.status, spin, theme);
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

fn write_row_cells(status: &WriteStatus, spin: char, theme: &Theme) -> (String, Style, String) {
    match status {
        WriteStatus::Pending => ("pending".to_string(), theme.dim, String::new()),
        WriteStatus::Running => (format!("{spin} running"), theme.accent, String::new()),
        WriteStatus::NotApplicable => (
            "N/A".to_string(),
            theme.dim,
            "no Vorbis comments".to_string(),
        ),
        WriteStatus::Unchanged => ("unchanged".to_string(), theme.dim, String::new()),
        WriteStatus::Ok => ("OK".to_string(), theme.ok, String::new()),
        WriteStatus::Failed(e) => ("FAILED".to_string(), theme.error, e.clone()),
    }
}

fn draw_write_gauge(
    frame: &mut Frame,
    area: Rect,
    rows: &[WriteRow],
    stage: &TagStage,
    wrote: usize,
    failed: usize,
    theme: &Theme,
) {
    let total = rows.len();
    let (ratio, style, label) = match stage {
        TagStage::Editing => (
            0.0,
            theme.accent,
            format!("{total} files — press a to write"),
        ),
        _ => {
            let done = rows
                .iter()
                .filter(|r| !matches!(r.status, WriteStatus::Pending | WriteStatus::Running))
                .count();
            let ratio = if total == 0 {
                1.0
            } else {
                done as f64 / total as f64
            };
            let style = if failed > 0 {
                theme.error
            } else if done == total {
                theme.ok
            } else {
                theme.accent
            };
            (
                ratio,
                style,
                format!("{done}/{total} wrote:{wrote} failed:{failed}"),
            )
        }
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

// --- Rename ------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum RenameFocus {
    None,
    Band,
    Date,
    Disc,
    ShortYear,
}

enum RenameStage {
    Editing,
    Renaming,
    Done,
}

#[derive(Clone)]
enum RenameRowStatus {
    Pending,
    Ok,
    Failed(String),
}

fn run_rename(args: RenameArgs, theme: ThemeName) -> ExitCode {
    if !args.dir.is_dir() {
        eprintln!("lh-tui: {} is not a directory", args.dir.display());
        return ExitCode::from(2);
    }
    let set = match scan::scan(&args.dir, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lh-tui: scanning {}: {e:#}", args.dir.display());
            return ExitCode::from(2);
        }
    };
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        eprintln!("no audio files found in {}", args.dir.display());
        return ExitCode::from(1);
    }

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

    let files: Vec<PathBuf> = set.files.iter().map(|f| f.path.clone()).collect();

    let terminal = ratatui::init();
    let _ = crossterm::execute!(io::stdout(), EnableBracketedPaste);
    let result = run_rename_screen(
        terminal,
        &args.dir,
        files,
        band,
        date,
        args.short_year,
        disc,
        Theme::new(theme),
    );
    let _ = crossterm::execute!(io::stdout(), DisableBracketedPaste);
    ratatui::restore();

    match result {
        Ok(ok) => {
            if ok {
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

fn current_spec(band: &Field, date: &Field, disc: &Field, short_year: bool) -> Option<NameSpec> {
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
fn run_rename_screen(
    mut terminal: DefaultTerminal,
    dir: &Path,
    files: Vec<PathBuf>,
    band: String,
    date: String,
    short_year: bool,
    disc: String,
    theme: Theme,
) -> io::Result<bool> {
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

    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        let spec = current_spec(&band_field, &date_field, &disc_field, short_year);
        let plan = spec.as_ref().map(|s| plan_rename(&files, s));

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

        let elapsed = header_elapsed(start, &mut finished_at, matches!(stage, RenameStage::Done));
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
                elapsed,
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

    Ok(rows.iter().all(|r| matches!(r, RenameRowStatus::Ok)))
}

#[allow(clippy::too_many_arguments)]
fn draw_rename(
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
    elapsed: f32,
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
        Span::raw(format!("   {elapsed:.1}s")),
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
            draw_rename_write_table(frame, outer[2], names, rows, tick, theme)
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
fn draw_rename_fields(
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

fn draw_rename_table(
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

fn draw_rename_write_table(
    frame: &mut Frame,
    area: Rect,
    names: &[String],
    rows: &[RenameRowStatus],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = names.iter().enumerate().map(|(i, name)| {
        let (label, style) = match rows.get(i) {
            None | Some(RenameRowStatus::Pending) => (format!("{spin} pending"), theme.accent),
            Some(RenameRowStatus::Ok) => ("OK".to_string(), theme.ok),
            Some(RenameRowStatus::Failed(_)) => ("FAILED".to_string(), theme.error),
        };
        let detail = match rows.get(i) {
            Some(RenameRowStatus::Failed(e)) => e.clone(),
            _ => String::new(),
        };
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(name.clone()),
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

fn draw_rename_gauge(
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
                format!("{} files renamed", rows.len())
            };
            (1.0, style, label)
        }
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
