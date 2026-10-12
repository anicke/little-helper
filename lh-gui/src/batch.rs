//! The one view behind verify, sbe, convert, check and create-checksum — the gpui
//! counterpart of `lh-tui/src/batch.rs`: a status table driven by a fresh
//! `lh_core::job::Queue` per run, a tally and a progress bar. Each step decides what its
//! jobs are, how a job's output reads as a row, and what the finished run amounts to
//! (`steps.rs`); everything else is here.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::job::{self, Event, Queue};
use std::time::{Duration, Instant};

use crate::steps::{StepDone, StepResult};
use crate::ui::bridge;

/// How a finished row's status reads.
#[derive(Clone, Copy)]
pub enum Tone {
    Ok,
    Warn,
    Bad,
    Neutral,
}

/// One job's finished row, built on the worker thread from the operation's own result.
pub struct Done {
    pub label: &'static str,
    pub tone: Tone,
    pub detail: String,
    /// Whether this row makes the run unclean — the notion each command's exit code uses.
    pub bad: bool,
    /// The digest a checksum-create row computed, for writing the list afterwards.
    pub digest: Option<[u8; 16]>,
}

impl Done {
    pub fn good(label: &'static str, tone: Tone, detail: impl Into<String>) -> Self {
        Done {
            label,
            tone,
            detail: detail.into(),
            bad: false,
            digest: None,
        }
    }

    pub fn bad(label: &'static str, tone: Tone, detail: impl Into<String>) -> Self {
        Done {
            bad: true,
            ..Done::good(label, tone, detail)
        }
    }

    pub fn failed(detail: impl Into<String>) -> Self {
        Done::bad("FAILED", Tone::Bad, detail)
    }
}

pub type Job = Box<dyn FnOnce(&job::Progress<Done>) -> Done + Send>;
/// Gets whether the run was clean and each row's digest (`None` but for checksum create).
pub type Finish = Box<dyn FnOnce(bool, Vec<Option<[u8; 16]>>) -> StepResult>;

/// What a step hands the batch to run.
pub struct Prepared {
    pub names: Vec<String>,
    pub jobs: Vec<Job>,
    /// The tally's labels, in display order — every `Done::label` a job can produce.
    pub labels: &'static [&'static str],
    pub detail: &'static str,
    /// Turns the finished rows into the step's result, doing any last write (a checksum
    /// list) only now that every row is in.
    pub finish: Finish,
}

pub enum RowState {
    Pending,
    Running { done: u32, total: u32 },
    Done(Done),
    Cancelled,
}

impl RowState {
    fn finished(&self) -> bool {
        matches!(self, RowState::Done(_) | RowState::Cancelled)
    }

    fn digest(&self) -> Option<[u8; 16]> {
        match self {
            RowState::Done(d) => d.digest,
            _ => None,
        }
    }
}

pub struct Row {
    name: SharedString,
    state: RowState,
}

pub struct Rows {
    rows: Vec<Row>,
    detail: &'static str,
}

impl TableDelegate for Rows {
    fn columns_count(&self, _: &App) -> usize {
        3
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("status", "Status").width(px(130.)),
            1 => Column::new("name", "File").width(px(380.)),
            _ => Column::new("detail", self.detail).width(px(460.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => match &row.state {
                RowState::Pending => div()
                    .text_color(cx.theme().muted_foreground)
                    .child("pending")
                    .into_any_element(),
                RowState::Running { done, total } => h_flex()
                    .gap_2()
                    .text_color(cx.theme().primary)
                    .child(Spinner::new().small())
                    .child(if *total > 0 {
                        format!("{}%", (u64::from(*done) * 100 / u64::from(*total)).min(100))
                    } else {
                        "running".into()
                    })
                    .into_any_element(),
                RowState::Done(done) => tag(done.tone)
                    .small()
                    .outline()
                    .child(done.label)
                    .into_any_element(),
                RowState::Cancelled => Tag::secondary()
                    .small()
                    .outline()
                    .child("CANCELLED")
                    .into_any_element(),
            },
            1 => row.name.clone().into_any_element(),
            _ => match &row.state {
                RowState::Done(done) => div()
                    .text_color(cx.theme().muted_foreground)
                    .child(done.detail.clone())
                    .into_any_element(),
                _ => div().into_any_element(),
            },
        }
    }
}

fn tag(tone: Tone) -> Tag {
    match tone {
        Tone::Ok => Tag::success(),
        Tone::Warn => Tag::warning(),
        Tone::Bad => Tag::danger(),
        Tone::Neutral => Tag::secondary(),
    }
}

pub struct Batch {
    table: Entity<TableState<Rows>>,
    labels: &'static [&'static str],
    queue: Option<Queue<Done>>,
    finish: Option<Finish>,
    /// Bumped per run, so a previous run's late events are ignored.
    generation: u64,
    started: Option<Instant>,
    took: Option<Duration>,
}

impl EventEmitter<StepDone> for Batch {}

impl Batch {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(
                Rows {
                    rows: Vec::new(),
                    detail: "Detail",
                },
                window,
                cx,
            )
        });
        Batch {
            table,
            labels: &[],
            queue: None,
            finish: None,
            generation: 0,
            started: None,
            took: None,
        }
    }

    pub fn running(&self) -> bool {
        self.queue.is_some()
    }

    pub fn has_rows(&self, cx: &App) -> bool {
        !self.table.read(cx).delegate().rows.is_empty()
    }

    pub fn start(&mut self, prepared: Prepared, cx: &mut Context<Self>) {
        let Prepared {
            names,
            jobs,
            labels,
            detail,
            finish,
        } = prepared;

        self.generation += 1;
        self.labels = labels;
        self.finish = Some(finish);
        self.started = Some(Instant::now());
        self.took = None;
        self.table.update(cx, |t, cx| {
            let rows = t.delegate_mut();
            rows.detail = detail;
            rows.rows = names
                .iter()
                .map(|name| Row {
                    name: name.clone().into(),
                    state: RowState::Pending,
                })
                .collect();
            t.refresh(cx);
        });

        let queue = Queue::new();
        for (name, job) in names.into_iter().zip(jobs) {
            // The queue says `Started` at submission, not when a worker picks the job up,
            // so the job says so itself: a `(0, 0)` report is "running, no figure yet".
            queue.submit(name, move |p| {
                p.report(0, 0);
                job(p)
            });
        }

        // Ends when the run is over and the queue dropped, so a finished batch holds no
        // background thread.
        let generation = self.generation;
        bridge(&queue, cx, move |this, event, cx| {
            this.on_event(generation, event, cx)
        });

        self.queue = Some(queue);
        cx.notify();
    }

    pub fn cancel(&mut self) {
        if let Some(queue) = &self.queue {
            queue.cancel();
        }
    }

    fn on_event(&mut self, generation: u64, event: Event<Done>, cx: &mut Context<Self>) {
        if generation != self.generation {
            return;
        }
        let (index, state) = match event {
            Event::Started { .. } => return,
            Event::Progress { id, done, total } => (id.index(), RowState::Running { done, total }),
            Event::Finished { id, output, .. } => (id.index(), RowState::Done(output)),
            Event::Cancelled { id, .. } => (id.index(), RowState::Cancelled),
        };
        let all_done = self.table.update(cx, |t, cx| {
            let rows = &mut t.delegate_mut().rows;
            // A late progress report must not pull a finished row back to running.
            if !rows[index].state.finished() {
                rows[index].state = state;
            }
            cx.notify();
            rows.iter().all(|r| r.state.finished())
        });

        if all_done {
            self.queue = None;
            self.took = self.started.map(|s| s.elapsed());
            let (clean, _) = self.tally(cx);
            if let Some(finish) = self.finish.take() {
                let digests = self
                    .table
                    .read(cx)
                    .delegate()
                    .rows
                    .iter()
                    .map(|r| r.state.digest())
                    .collect();
                cx.emit(StepDone(finish(clean, digests)));
            }
        }
        cx.notify();
    }

    /// Whether no finished row was bad (a cancelled one is), and per-label counts.
    fn tally(&self, cx: &App) -> (bool, Vec<(&'static str, Tone, usize)>) {
        let rows = &self.table.read(cx).delegate().rows;
        let mut clean = true;
        let mut counts: Vec<(&'static str, Tone, usize)> =
            self.labels.iter().map(|l| (*l, Tone::Neutral, 0)).collect();
        for row in rows {
            match &row.state {
                RowState::Done(done) => {
                    clean &= !done.bad;
                    if let Some(c) = counts.iter_mut().find(|c| c.0 == done.label) {
                        c.1 = done.tone;
                        c.2 += 1;
                    }
                }
                RowState::Cancelled => clean = false,
                _ => {}
            }
        }
        (clean, counts)
    }

    /// The progress bar, tally and Cancel, under the table.
    pub fn render_footer(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let rows = &self.table.read(cx).delegate().rows;
        let total = rows.len();
        let done = rows.iter().filter(|r| r.state.finished()).count();
        let (clean, counts) = self.tally(cx);
        let pct = if total > 0 {
            done as f32 / total as f32 * 100.
        } else {
            0.
        };
        let finished = !self.running();
        let color = if !clean {
            cx.theme().danger
        } else if finished {
            cx.theme().success
        } else {
            cx.theme().primary
        };
        let summary = match self.took {
            Some(took) => format!("{done} of {total} · {:.1}s", took.as_secs_f32()),
            None => format!("{done} of {total}"),
        };

        v_flex()
            .gap_2()
            .child(Progress::new("batch-progress").value(pct).color(color))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
                    .children(
                        counts
                            .into_iter()
                            .filter(|c| c.2 > 0)
                            .map(|(label, tone, n)| {
                                tag(tone).small().child(format!("{label} {n}"))
                            }),
                    )
                    .child(div().flex_1())
                    .when(!finished, |row| {
                        row.child(
                            Button::new("cancel")
                                .ghost()
                                .small()
                                .icon(IconName::CircleX)
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, _| this.cancel())),
                        )
                    }),
            )
    }
}

impl Render for Batch {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_h(px(160.))
                    .child(DataTable::new(&self.table).stripe(true).bordered(true)),
            )
            .child(self.render_footer(cx))
    }
}
