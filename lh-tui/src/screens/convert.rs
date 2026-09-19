use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{ConvertArgs, Target};
use lh_core::convert::{Conversion, EncodeOpts, destination, to_flac, to_wav};
use lh_core::job::{Event, Queue};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::tools::{Registry, Tool, ToolId};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

// --- Convert -------------------------------------------------------------------------
//
// Unlike verify/checksum, this is the one screen where a file's own progress is worth
// showing: `to_wav` reports (frames written, frames total) once per decoded block, so a
// decoding row can show a live percentage rather than just a spinner (`docs/tui.md` §5
// calls this out as "the reason §2 calls out progress rendering as the real per-screen
// variable"). `to_flac` has no such number to relay — `flac` only draws its own percentage
// when stderr is a terminal, which piped through `Command` it never is
// (`lh-core/src/convert/mod.rs`'s own doc comment), calling its progress with `(0, 0)`
// instead — so an encoding row just spins.

#[derive(Clone)]
pub(crate) enum ConvertStatus {
    Pending,
    Running { done: u32, total: u32 },
    Skipped,
    Done { unchecked: bool, output: String },
    Failed(String),
}

pub(crate) struct ConvertRow {
    name: String,
    status: ConvertStatus,
}

/// Mirrors `lh-cli`'s own (private) `ConvertOutcome` (`lh-cli/src/lib.rs`) — small enough
/// that duplicating it here beats exporting an internal type just for this screen, the
/// same call every other screen's own `Status` enum already makes.
pub(crate) enum ConvertOutcome {
    Skipped,
    NoFileName,
    Done(Box<Conversion>),
    Failed(lh_core::Error),
}

pub(crate) fn run_convert(args: ConvertArgs, theme: ThemeName) -> ExitCode {
    let label = describe(&args.paths);
    let (files, mut clean) = match collect(&args.paths) {
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
pub(crate) struct ConvertMeta<'a> {
    root: &'a str,
    want: AudioFormat,
}

/// Returns whether every file converted cleanly (a skip counts as clean, same as
/// `cmd_convert`'s own exit code) plus every successful conversion's record, in
/// submission order — used only for the post-loop `--provenance` dump above.
pub(crate) fn run_convert_screen(
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
                Ok(d) => d,
                Err(_) => return ConvertOutcome::NoFileName,
            };
            let on_progress = &mut |done, total| {
                progress.report(done, total);
                !progress.is_cancelled()
            };
            let result = match to {
                Target::Wav => to_wav(&path, &dst, force, on_progress),
                Target::Flac => to_flac(
                    &path,
                    &dst,
                    encoder
                        .as_ref()
                        .expect("discovered before the screen opened"),
                    &opts,
                    force,
                    on_progress,
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

pub(crate) struct ConvertStats {
    done: usize,
    total: usize,
    written: usize,
    skipped: usize,
    failed: usize,
}

pub(crate) fn draw_convert(
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

pub(crate) fn draw_convert_header(
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

pub(crate) fn draw_convert_table(
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

pub(crate) fn convert_status_cell(
    status: &ConvertStatus,
    spin: char,
    theme: &Theme,
) -> (String, Style) {
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

pub(crate) fn convert_status_detail(status: &ConvertStatus, want: AudioFormat) -> String {
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

pub(crate) fn draw_convert_gauge(
    frame: &mut Frame,
    area: Rect,
    stats: &ConvertStats,
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
