use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::{Direction as SbeFixDirection, SbeFixArgs};
use lh_core::convert::{EncodeOpts, destination};
use lh_core::job::{Event, Queue};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::repair::{
    BoundaryDirection, FixPlan, Fixed, RepairEncode, TailPolicy, execute_fix, plan_fix,
};
use lh_core::scan;
use lh_core::tools::{Registry, Tool, ToolId};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::{Path, PathBuf};

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
// checkpoint (out of scope for docs/architecture-cleanup.md A3; see docs/sbe-repair.md), so
// once it starts, the row for every file just spins until the one `Finished` event lands
// with every file's result at once, and `q`/`Esc`/`Ctrl-C` cannot stop it early the way
// `run_torrent_check_screen` now can for `check`.

/// One row per file in the set (not per boundary): `shifted_in`/`shifted_out` are computed
/// straight from the plan using the same formula `execute_fix` uses to fill in `Fixed`'s own
/// fields, so a row's numbers never change between the pre-execution plan and the
/// post-execution result.
pub(crate) struct FixRow {
    name: String,
    shifted_in: i64,
    shifted_out: i64,
    is_tail: bool,
}

pub(crate) fn fix_rows(files: &[AudioFile], plan: &FixPlan) -> Vec<FixRow> {
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
pub(crate) fn fix_shift_note(shifted_in: i64, shifted_out: i64) -> String {
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

pub(crate) fn fix_row_note(row: &FixRow, plan: &FixPlan) -> String {
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

pub(crate) enum FixStage {
    /// `--dry-run`: the plan is all there is, nothing runs.
    Planned,
    /// `execute_fix` is running; no progress or cancellation checkpoint to show.
    Fixing,
    Done(Box<lh_core::Result<Vec<Fixed>>>),
}

pub(crate) fn run_sbe_fix(args: SbeFixArgs, theme: ThemeName) -> ExitCode {
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
        let result = {
            let mut terminal = TerminalGuard::new();
            run_sbe_fix_plan_screen(
                &mut terminal,
                &args.dir,
                &set.files,
                &plan,
                Theme::new(theme),
            )
        };
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
            Ok(d) => dsts.push(d),
            Err(_) => {
                eprintln!("lh-tui: {} has no file name", f.path.display());
                return ExitCode::from(2);
            }
        }
    }

    let result = {
        let mut terminal = TerminalGuard::new();
        run_sbe_fix_execute_screen(
            &mut terminal,
            &args.dir,
            set.files.clone(),
            plan.clone(),
            dsts,
            flac,
            args.overwrite,
            Theme::new(theme),
        )
    };

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

pub(crate) fn run_sbe_fix_plan_screen(
    terminal: &mut DefaultTerminal,
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
/// draw loop immediately rather than waiting for `Done`: `execute_fix` has no cancellation
/// checkpoint of its own to honor (unlike `check`'s, which `run_torrent_check_screen` now
/// waits on), so waiting would just mean waiting for work that can't be told to stop early.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_sbe_fix_execute_screen(
    terminal: &mut DefaultTerminal,
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
pub(crate) fn draw_sbe_fix(
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

pub(crate) fn draw_fix_table(
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

pub(crate) fn fix_row_cells(
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

pub(crate) fn draw_fix_gauge(
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
    draw_gauge(frame, area, ratio, style, label, theme);
}
