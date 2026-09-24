use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use clap::ValueEnum;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind};
use lh_cli::{
    Direction as SbeFixDirection, SbeFixArgs, print_fixed, print_in_place, print_tail_note,
    tail_policy,
};
use lh_core::convert::{EncodeOpts, destination};
use lh_core::job::{Event, Queue};
use lh_core::model::{AudioFile, AudioFormat};
use lh_core::repair::{FixPlan, Fixed, InPlace, RepairEncode, execute_fix, fix_in_place, plan_fix};
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
            shifted_in: plan.shifted_in(i),
            shifted_out: plan.shifted_out(i),
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
        None if !plan.fully_fixed => format!("{shift}, still misaligned (needs tail padding)"),
        None => shift,
    }
}

pub(crate) enum FixStage {
    /// `--dry-run`: the plan is all there is, nothing runs.
    Planned,
    /// `execute_fix` is running; no progress or cancellation checkpoint to show.
    Fixing,
    Done(Box<lh_core::Result<Vec<Fixed>>>),
    /// `fix_in_place` finished: one entry per file, replaced or left alone.
    Replaced(Box<lh_core::Result<Vec<InPlace>>>),
}

/// What the in-place screen lets the person change before applying, shown in its header.
pub(crate) struct FixControls {
    direction: SbeFixDirection,
    pad_tail: bool,
    /// Why the last key did nothing — a direction the set can't take, or why applying is
    /// refused — shown in the gauge until the next key.
    note: Option<String>,
}

/// How an `--in-place` fix went, in one line: for the gauge, and beside the workspace item.
pub(crate) fn in_place_summary(done: &[InPlace], plan: &FixPlan) -> String {
    let replaced = done
        .iter()
        .filter(|d| matches!(d, InPlace::Replaced { .. }))
        .count();
    let what = match replaced {
        0 => "nothing needed fixing".to_string(),
        n => format!("{n} files fixed, originals in _original/"),
    };
    if plan.fully_fixed {
        what
    } else {
        format!("{what}, tail still misaligned")
    }
}

/// Every file is FLAC and the reference `flac` binary is there — what executing a fix needs
/// beyond a plan — or why not.
fn require_flac_set(files: &[AudioFile]) -> Result<Tool, String> {
    if let Some(f) = files.iter().find(|f| f.format != AudioFormat::Flac) {
        return Err(format!(
            "sbe fix can only execute against FLAC ({} is {}); other formats have no repair \
             path yet",
            f.file_name(),
            f.format
        ));
    }
    Registry::discover_one(ToolId::Flac)
        .require(ToolId::Flac)
        .cloned()
        .map_err(|e| format!("{e:#}"))
}

/// The folder and the repair plan `args` asks for — all a dry run shows, and what an
/// executing run then carries out.
pub(crate) fn prepare_sbe_fix(args: &SbeFixArgs) -> Result<(Folder, FixPlan), Refusal> {
    let folder = scan_folder(&args.dir)?;
    let plan = plan_fix(
        &folder.files,
        args.direction.into(),
        tail_policy(args.pad_tail),
    )
    .map_err(|e| {
        Refusal::new(
            2,
            format!("lh-tui: planning a fix for {}: {e:#}", args.dir.display()),
        )
    })?;
    Ok((folder, plan))
}

pub(crate) fn run_sbe_fix(args: SbeFixArgs, theme: ThemeName) -> ExitCode {
    let (set, plan) = match prepare_sbe_fix(&args) {
        Ok(v) => v,
        Err(refusal) => return refusal.exit(),
    };
    for line in &set.skipped {
        eprintln!("{line}");
    }

    if args.in_place {
        let result = {
            let mut terminal = TerminalGuard::new();
            run_sbe_fix_in_place_screen(
                &mut terminal,
                &args.dir,
                &set.files,
                args.direction,
                args.pad_tail,
                Theme::new(theme),
            )
        };
        return match result {
            Ok(None) => ExitCode::from(1),
            Ok(Some(Ok((done, plan)))) => {
                print_in_place(&set.files, &done);
                print_tail_note(&set.files, &plan);
                if plan.fully_fixed {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::from(1)
                }
            }
            Ok(Some(Err(e))) => {
                eprintln!("lh-tui: repairing {}: {e:#}", args.dir.display());
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("lh-tui: {e}");
                ExitCode::from(2)
            }
        };
    }

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
                "lh-tui: sbe fix needs -o/--output or --in-place to execute — it never writes \
                 over the originals (Principle 1)"
            );
            return ExitCode::from(2);
        }
    };
    let flac = match require_flac_set(&set.files) {
        Ok(t) => t,
        Err(why) => {
            eprintln!("lh-tui: {why}");
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
            print_fixed(&set.files, &fixed);
            print_tail_note(&set.files, &plan);
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
                None,
                start,
                0,
                &theme,
            )
        })?;
        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = is_quit(&key);
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

        terminal.draw(|frame| {
            draw_sbe_fix(frame, dir, &rows, &plan, &stage, None, start, tick, &theme)
        })?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = is_quit(&key);
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
    controls: Option<&FixControls>,
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

    let folder = Span::styled(dir.display().to_string(), theme.dim);
    let header = Line::from(match controls {
        // The settings go before the folder, which is the part a narrow terminal can lose.
        Some(c) => vec![
            Span::styled(" lh-tui ", theme.accent.bold()),
            Span::raw(format!(
                " sbe fix --in-place   direction {}   pad tail {}   ",
                c.direction
                    .to_possible_value()
                    .expect("no Direction is hidden")
                    .get_name(),
                if c.pad_tail { "on" } else { "off" }
            )),
            folder,
        ],
        None => {
            let mode = match stage {
                FixStage::Planned => "sbe fix --dry-run",
                _ => "sbe fix",
            };
            vec![
                Span::styled(" lh-tui ", theme.accent.bold()),
                Span::raw(format!(" {mode}  ")),
                folder,
                Span::raw(format!("   {:.1}s", start.elapsed().as_secs_f32())),
            ]
        }
    });
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    draw_fix_table(frame, chunks[1], rows, plan, stage, tick, theme);
    let planned = matches!(stage, FixStage::Planned);
    let warning = controls.and_then(|c| {
        c.note.as_deref().or((planned && !plan.fully_fixed)
            .then_some("tail stays misaligned — p pads it with silence"))
    });
    match warning {
        Some(w) => draw_gauge(frame, chunks[2], 1.0, theme.warn, w.to_string(), theme),
        None => draw_fix_gauge(frame, chunks[2], plan, stage, tick, theme),
    }

    let footer = match (controls, stage) {
        (Some(_), FixStage::Planned) => {
            " d direction   p pad tail   a apply in place (originals → _original/)   q/esc back "
        }
        (Some(_), FixStage::Fixing) => " fixing — can't be stopped part way ",
        _ => " q / esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), chunks[3]);
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
                let name = file_name(&f.path);
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
        FixStage::Replaced(result) => match result.as_ref() {
            Ok(done) => match &done[i] {
                InPlace::Replaced { fixed, .. } => (
                    "FIXED".to_string(),
                    theme.ok,
                    format!(
                        "{}  original in _original/  md5 {}",
                        fix_row_note(row, plan),
                        hex::encode(fixed.audio_md5)
                    ),
                ),
                InPlace::Unchanged { .. } => {
                    ("UNCHANGED".to_string(), theme.dim, fix_row_note(row, plan))
                }
            },
            Err(_) => (
                "FAILED".to_string(),
                theme.error,
                "nothing changed — see the error below".to_string(),
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
        FixStage::Replaced(result) => match result.as_ref() {
            Ok(done) => {
                let style = if plan.fully_fixed {
                    theme.ok
                } else {
                    theme.warn
                };
                (1.0, style, in_place_summary(done, plan))
            }
            Err(e) => (1.0, theme.error, format!("failed, nothing changed: {e:#}")),
        },
    };
    draw_gauge(frame, area, ratio, style, label, theme);
}

/// What [`run_sbe_fix_in_place_screen`] ends with: `None` when left without applying,
/// else every file's outcome and the plan that was applied.
pub(crate) type InPlaceOutcome = Option<lh_core::Result<(Vec<InPlace>, FixPlan)>>;

/// The fix, applied to the folder itself: the plan as a table, re-planned as `d` cycles the
/// direction and `p` toggles tail padding, then `a` runs `fix_in_place` — each changed file
/// replaced under its own name, the file it replaced moved into `_original/`, the way the
/// workspace's convert → FLAC leaves a folder holding its FLACs.
///
/// `None` when left without applying. Once applying starts it can't be left until it is
/// done: `fix_in_place` has no cancellation checkpoint, and leaving would only hide a job
/// still about to move files around in the folder.
pub(crate) fn run_sbe_fix_in_place_screen(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    files: &[AudioFile],
    direction: SbeFixDirection,
    pad_tail: bool,
    theme: Theme,
) -> io::Result<InPlaceOutcome> {
    let mut controls = FixControls {
        direction,
        pad_tail,
        note: None,
    };
    let mut plan = match plan_fix(files, direction.into(), tail_policy(pad_tail)) {
        Ok(p) => p,
        Err(e) => return Ok(Some(Err(e))),
    };
    let mut rows = fix_rows(files, &plan);
    let mut stage = FixStage::Planned;
    let mut queue: Option<Queue<lh_core::Result<Vec<InPlace>>>> = None;
    let start = Instant::now();
    let mut tick = 0usize;

    loop {
        if let Some(q) = &queue {
            while let Ok(event) = q.events().try_recv() {
                match event {
                    Event::Started { .. } | Event::Progress { .. } => {}
                    Event::Finished { output, .. } => {
                        stage = FixStage::Replaced(Box::new(output));
                    }
                    Event::Cancelled { .. } => {
                        stage = FixStage::Replaced(Box::new(Err(lh_core::Error::Cancelled)));
                    }
                }
            }
        }

        terminal.draw(|frame| {
            draw_sbe_fix(
                frame,
                dir,
                &rows,
                &plan,
                &stage,
                Some(&controls),
                start,
                tick,
                &theme,
            )
        })?;
        tick = tick.wrapping_add(1);

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let CtEvent::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let quit = is_quit(&key);
        match stage {
            FixStage::Replaced(result) if quit => {
                return Ok(Some(result.map(|done| (done, plan))));
            }
            FixStage::Planned if quit => return Ok(None),
            FixStage::Planned if key.code == KeyCode::Char('a') => {
                controls.note = None;
                match start_in_place(files, &plan) {
                    Ok(q) => {
                        queue = Some(q);
                        stage = FixStage::Fixing;
                    }
                    Err(why) => controls.note = Some(why),
                }
            }
            FixStage::Planned => {
                controls.note = None;
                let (mut direction, mut pad_tail) = (controls.direction, controls.pad_tail);
                match key.code {
                    KeyCode::Char('d') => {
                        let all = SbeFixDirection::value_variants();
                        let at = all.iter().position(|d| *d == direction).unwrap_or(0);
                        direction = all[(at + 1) % all.len()];
                    }
                    KeyCode::Char('p') => pad_tail = !pad_tail,
                    _ => continue,
                }
                match plan_fix(files, direction.into(), tail_policy(pad_tail)) {
                    Ok(p) => {
                        plan = p;
                        rows = fix_rows(files, &plan);
                        controls.direction = direction;
                        controls.pad_tail = pad_tail;
                    }
                    Err(e) => controls.note = Some(format!("{e:#}")),
                }
            }
            // Applying can't be stopped part way (see above).
            _ => {}
        }
    }
}

/// Submits `fix_in_place` as the one job on a queue of one, or says why it can't run.
fn start_in_place(
    files: &[AudioFile],
    plan: &FixPlan,
) -> Result<Queue<lh_core::Result<Vec<InPlace>>>, String> {
    let flac = require_flac_set(files)?;
    let queue = Queue::with_workers(1);
    let job_files = files.to_vec();
    let job_plan = plan.clone();
    queue.submit("sbe fix", move |_progress| {
        let opts = EncodeOpts::default();
        let encode = RepairEncode {
            flac: &flac,
            opts: &opts,
            overwrite: false,
        };
        fix_in_place(&job_files, &job_plan, &encode)
    });
    Ok(queue)
}
