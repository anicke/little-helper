//! The one screen behind verify, checksum, check, sbe and convert (`docs/tui.md` §0): a
//! header, a status table, a tally gauge and a footer, driven by a job queue that only this
//! module reads events from. Each of those screens keeps its own status enum, its
//! [`RowStatus`] impl and how it submits jobs; everything else is here.

use std::io;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyEventKind};
use lh_core::job::{Event, Queue};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Gauge, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

/// One row's state, as far as the shared screen needs to know it.
pub(crate) trait RowStatus {
    fn pending() -> Self;
    /// A job that has started; `(0, 0)` until it reports progress, and forever for a job
    /// with no per-file progress to report.
    fn running(done: u32, total: u32) -> Self;
    /// What a job the user quit on shows as.
    fn cancelled() -> Self;
    fn cell(&self, spin: char, theme: &Theme) -> (String, Style);
    fn detail(&self) -> String;
    /// Adds a finished row to `counts`; a pending or running one adds nothing.
    fn tally(&self, counts: &mut Counts);
}

/// Per-outcome row counts for the gauge, in the order the screen names them.
pub(crate) struct Counts {
    labels: &'static [&'static str],
    counts: Vec<usize>,
    done: usize,
    bad: bool,
}

impl Counts {
    fn new(labels: &'static [&'static str]) -> Self {
        Counts {
            labels,
            counts: vec![0; labels.len()],
            done: 0,
            bad: false,
        }
    }

    fn add(&mut self, label: &str, bad: bool) {
        let i = self
            .labels
            .iter()
            .position(|l| *l == label)
            .unwrap_or_else(|| panic!("no gauge label {label:?}"));
        self.counts[i] += 1;
        self.done += 1;
        self.bad |= bad;
    }

    /// A row that finished as `label` and counts as fine.
    pub(crate) fn good(&mut self, label: &str) {
        self.add(label, false);
    }

    /// A row that finished as `label` and makes the run unclean (a red gauge, exit code 1).
    pub(crate) fn bad(&mut self, label: &str) {
        self.add(label, true);
    }

    fn of<S: RowStatus>(labels: &'static [&'static str], rows: &[S]) -> Self {
        let mut counts = Counts::new(labels);
        for row in rows {
            row.tally(&mut counts);
        }
        counts
    }
}

/// What a batch screen shows that isn't per-row.
pub(crate) struct View<'a> {
    /// The command, after `lh-tui` in the header.
    pub(crate) command: String,
    /// Where the rows came from: a path, or a count of paths.
    pub(crate) target: &'a str,
    /// What a row is, for the table title and the header count: `files` or `entries`.
    pub(crate) unit: &'static str,
    /// The last column's heading.
    pub(crate) detail: &'static str,
    /// Percentage widths of the name and detail columns.
    pub(crate) widths: (u16, u16),
    /// The gauge's outcome labels, in display order.
    pub(crate) labels: &'static [&'static str],
}

pub(crate) struct Outcome<S> {
    /// Every row's final status, in submission order.
    pub(crate) rows: Vec<S>,
    /// Whether no finished row was `bad` — the same notion each command's exit code uses.
    pub(crate) clean: bool,
}

/// Runs `queue` to completion (or until the user quits, which cancels it), drawing each
/// row as it goes. `names` is one label per submitted job, in submission order; `finish`
/// turns a job's output into its row's final status.
pub(crate) fn run_batch<T: Send + 'static, S: RowStatus>(
    terminal: &mut DefaultTerminal,
    view: &View,
    names: &[String],
    queue: Queue<T>,
    mut finish: impl FnMut(usize, T) -> S,
    theme: &Theme,
) -> io::Result<Outcome<S>> {
    let total = names.len();
    let mut rows: Vec<S> = names.iter().map(|_| S::pending()).collect();
    let cancel = queue.cancel_token();
    let events = queue.events();

    let start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;

    loop {
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Started { id, .. } => rows[id.index()] = S::running(0, 0),
                Event::Progress { id, done, total } => rows[id.index()] = S::running(done, total),
                Event::Finished { id, output, .. } => {
                    rows[id.index()] = finish(id.index(), output);
                }
                Event::Cancelled { id, .. } => rows[id.index()] = S::cancelled(),
            }
        }

        let counts = Counts::of(view.labels, &rows);
        let elapsed = header_elapsed(start, &mut finished_at, counts.done == total);
        terminal.draw(|frame| {
            draw_batch(frame, view, names, &rows, &counts, elapsed, tick, theme);
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
        tick = tick.wrapping_add(1);
    }

    let clean = !Counts::of(view.labels, &rows).bad;
    Ok(Outcome { rows, clean })
}

#[allow(clippy::too_many_arguments)]
fn draw_batch<S: RowStatus>(
    frame: &mut Frame,
    view: &View,
    names: &[String],
    rows: &[S],
    counts: &Counts,
    elapsed: f32,
    tick: usize,
    theme: &Theme,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {}  ", view.command)),
        Span::styled(view.target.to_string(), theme.dim),
        Span::raw(format!(
            "   {} / {} {}   {elapsed:.1}s",
            counts.done,
            names.len(),
            view.unit
        )),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.dim);
    frame.render_widget(Paragraph::new(header).block(block), chunks[0]);

    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = names.iter().zip(rows).map(|(name, status)| {
        let (label, style) = status.cell(spin, theme);
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(name.clone()),
            Cell::from(status.detail()).style(theme.dim),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(view.widths.0),
            Constraint::Percentage(view.widths.1),
        ],
    )
    .header(Row::new(vec!["status", "file", view.detail]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", view.unit))
            .border_style(theme.dim),
    );
    frame.render_widget(table, chunks[1]);

    let total = names.len();
    let ratio = if total == 0 {
        0.0
    } else {
        counts.done as f64 / total as f64
    };
    let style = if counts.bad {
        theme.error
    } else if counts.done == total {
        theme.ok
    } else {
        theme.accent
    };
    let mut label = format!("{}/{total}", counts.done);
    for (name, n) in counts.labels.iter().zip(&counts.counts) {
        label.push_str(&format!(" {name}:{n}"));
    }
    draw_gauge(frame, chunks[2], ratio, style, label, theme);

    draw_footer(frame, chunks[3], theme);
}

/// The bordered progress bar every screen ends in, whatever fills it.
pub(crate) fn draw_gauge(
    frame: &mut Frame,
    area: Rect,
    ratio: f64,
    style: Style,
    label: String,
    theme: &Theme,
) {
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
