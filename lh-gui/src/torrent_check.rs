//! Torrent → Check: pick a `.torrent` (or take the one the show folder has), see what it
//! describes, check a folder against it with progress and Cancel through
//! `lh_core::job::Queue`, and read the per-file table. Same behaviour as
//! `lh-gui/src/areas/torrent.rs` + `App::run_torrent_check`.
//!
//! The torrent chosen here is the workspace's torrent: Torrent info shows the same one.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::description_list::DescriptionList;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::display;
use lh_core::job::{Event, Queue};
use lh_core::torrent::{FileStatus, Metainfo, TorrentReport, Verdict, check, check_sizes};
use std::path::{Path, PathBuf};

use crate::screen::Screen;
use crate::steps::{Audio, StepDone, StepResult, find_torrent};
use crate::ui::{bridge, card};

type Outcome = lh_core::Result<TorrentReport>;

/// One row of the results table — the same shape as `lh-gui`'s `job::FileRow`.
pub(crate) struct FileRow {
    path: SharedString,
    label: &'static str,
    failure: bool,
    detail: SharedString,
}

/// Mirrors `lh-gui/src/job.rs`'s `report_rows`.
fn report_rows(report: &TorrentReport) -> Vec<FileRow> {
    let mut rows = Vec::new();
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
            FileStatus::Corrupt { bad_pieces } => display::pieces_phrase(bad_pieces),
            FileStatus::Suspect { piece, shared_with } => format!(
                "piece {piece} is shared with {} other file(s); either could be at fault",
                shared_with.len()
            ),
            FileStatus::Partial {
                verified,
                unverifiable,
            } => format!(
                "{verified} pieces verified, {unverifiable} unreadable because a \
                 neighbouring file is bad"
            ),
            _ => String::new(),
        };
        rows.push(FileRow {
            path: shown.display().to_string().into(),
            label: outcome.status.label(),
            failure: outcome.status.is_failure(),
            detail: detail.into(),
        });
    }
    for extra in &report.extra_local {
        let shown = extra.strip_prefix(&report.root).unwrap_or(extra);
        rows.push(FileRow {
            path: shown.display().to_string().into(),
            label: "EXTRA",
            failure: false,
            detail: SharedString::default(),
        });
    }
    rows
}

struct Results {
    rows: Vec<FileRow>,
}

impl TableDelegate for Results {
    fn columns_count(&self, _: &App) -> usize {
        3
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("status", "Status").width(px(120.)),
            1 => Column::new("path", "File").width(px(420.)),
            _ => Column::new("detail", "Detail").width(px(360.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let row = &self.rows[row_ix];
        match col_ix {
            0 => {
                let tag = match row.label {
                    "OK" => Tag::success(),
                    "EXTRA" => Tag::info(),
                    _ if row.failure => Tag::danger(),
                    _ => Tag::warning(),
                };
                tag.small().outline().child(row.label).into_any_element()
            }
            1 => row.path.clone().into_any_element(),
            _ => row.detail.clone().into_any_element(),
        }
    }
}

enum RunState {
    Idle,
    Running { done: u32, total: u32 },
    Finished(Result<Verdict, String>),
    Cancelled,
}

pub struct TorrentCheck {
    pub torrent_path: Option<PathBuf>,
    pub meta: Option<Metainfo>,
    against: Entity<InputState>,
    quick: bool,
    error: Option<SharedString>,
    run: RunState,
    queue: Queue<Outcome>,
    table: Entity<TableState<Results>>,
}

/// A different torrent (or none) is now chosen, for Torrent info to show.
pub struct TorrentLoaded;

impl EventEmitter<TorrentLoaded> for TorrentCheck {}
impl EventEmitter<StepDone> for TorrentCheck {}

impl Screen for TorrentCheck {
    fn running(&self) -> bool {
        matches!(self.run, RunState::Running { .. })
    }

    /// Check against the folder, and take its torrent if it has one.
    fn use_folder(&mut self, dir: &Path, _: &Audio, window: &mut Window, cx: &mut Context<Self>) {
        if self.running() {
            return;
        }
        let shown = dir.display().to_string();
        self.against
            .update(cx, |s, cx| s.set_value(shown, window, cx));
        if let Ok((file, _)) = find_torrent(dir) {
            self.pick_torrent(file, window, cx);
        }
    }
}

impl TorrentCheck {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let against = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Folder that holds the torrent's files")
        });
        let table = cx.new(|cx| TableState::new(Results { rows: Vec::new() }, window, cx));
        let queue = Queue::new();

        // Ends when the view (and so the queue) drops.
        bridge(&queue, cx, Self::on_event);

        Self {
            torrent_path: None,
            meta: None,
            against,
            quick: false,
            error: None,
            run: RunState::Idle,
            queue,
            table,
        }
    }

    pub fn pick_torrent(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        match Metainfo::read(&path) {
            Ok(meta) => {
                self.error = None;
                // Default the folder to the one beside the .torrent — the common layout.
                if self.against.read(cx).value().is_empty()
                    && let Some(dir) = path.parent()
                {
                    let guess = dir.to_path_buf();
                    self.against.update(cx, |s, cx| {
                        s.set_value(guess.display().to_string(), window, cx)
                    });
                }
                self.meta = Some(meta);
            }
            Err(e) => {
                self.error = Some(e.to_string().into());
                self.meta = None;
            }
        }
        self.torrent_path = Some(path);
        self.run = RunState::Idle;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().rows.clear();
            t.refresh(cx);
        });
        cx.emit(TorrentLoaded);
        cx.notify();
    }

    pub fn browse_torrent(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Choose .torrent".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |this, window, cx| this.pick_torrent(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn browse_folder(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose folder".into()),
        });
        let against = self.against.clone();
        cx.spawn_in(window, async move |_, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                against
                    .update_in(cx, |s, window, cx| {
                        s.set_value(path.display().to_string(), window, cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn start(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(torrent_path), Some(meta)) = (self.torrent_path.clone(), self.meta.clone())
        else {
            return;
        };
        let against = PathBuf::from(self.against.read(cx).value().trim());
        let quick = self.quick;

        self.error = None;
        self.run = RunState::Running { done: 0, total: 0 };
        self.queue.cancel_token().reset();
        self.queue
            .submit(format!("torrent check: {}", meta.name), move |p| {
                if quick {
                    check_sizes(&meta, &torrent_path, &against)
                } else {
                    check(&meta, &torrent_path, &against, &mut |done, total| {
                        p.report(done, total);
                        !p.is_cancelled()
                    })
                }
            });
        cx.notify();
    }

    fn on_event(&mut self, event: Event<Outcome>, cx: &mut Context<Self>) {
        match event {
            Event::Started { .. } => {}
            Event::Progress { done, total, .. } => self.run = RunState::Running { done, total },
            Event::Finished { output, .. } => {
                let result = match &output {
                    Ok(report) => match report.verdict() {
                        Verdict::Complete => StepResult::Clean("Complete.".into()),
                        Verdict::SizesMatch => StepResult::Clean("Sizes match.".into()),
                        Verdict::Incomplete => StepResult::Unclean("Incomplete.".into()),
                    },
                    Err(e) => StepResult::Unclean(e.to_string()),
                };
                cx.emit(StepDone(result));
                self.run = RunState::Finished(match output {
                    Ok(report) => {
                        let verdict = report.verdict();
                        let rows = report_rows(&report);
                        self.table.update(cx, |t, cx| {
                            t.delegate_mut().rows = rows;
                            t.refresh(cx);
                        });
                        Ok(verdict)
                    }
                    Err(e) => Err(e.to_string()),
                });
            }
            Event::Cancelled { .. } => self.run = RunState::Cancelled,
        }
        cx.notify();
    }

    fn render_torrent_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let path_label: SharedString = match &self.torrent_path {
            Some(p) => p.display().to_string().into(),
            None => "No .torrent chosen — browse, or drop one on the window.".into(),
        };
        let details = self.meta.as_ref().map(|meta| {
            DescriptionList::new()
                .columns(2)
                .bordered(true)
                .item("Name", meta.name.clone(), 2)
                .item("Info hash", meta.info_hash_hex(), 2)
                .item("Files", meta.real_files().count().to_string(), 1)
                .item(
                    "Pieces",
                    format!(
                        "{} × {}",
                        meta.pieces.len(),
                        display::bytes(meta.piece_length)
                    ),
                    1,
                )
        });

        card(cx, "Torrent")
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .when(self.torrent_path.is_none(), |d| {
                                d.text_color(cx.theme().muted_foreground)
                            })
                            .child(path_label),
                    )
                    .child(
                        Button::new("browse-torrent")
                            .outline()
                            .icon(IconName::FileInput)
                            .label("Browse…")
                            .on_click(cx.listener(Self::browse_torrent)),
                    ),
            )
            .children(details)
    }

    fn render_check_card(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let running = matches!(self.run, RunState::Running { .. });
        let status: Option<AnyElement> = match &self.run {
            RunState::Idle => None,
            RunState::Running { done, total } => {
                let pct = if *total > 0 {
                    *done as f32 / *total as f32 * 100.
                } else {
                    0.
                };
                Some(
                    v_flex()
                        .gap_1()
                        .child(Progress::new("progress").value(pct).loading(*total == 0))
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(if *total > 0 {
                                    format!("Hashing piece {done} of {total}")
                                } else {
                                    "Checking sizes…".into()
                                }),
                        )
                        .into_any_element(),
                )
            }
            RunState::Finished(Ok(Verdict::Complete)) => Some(
                Alert::success(
                    "verdict",
                    "Every file is present and every piece hashes correctly.",
                )
                .title("Complete")
                .into_any_element(),
            ),
            RunState::Finished(Ok(Verdict::SizesMatch)) => Some(
                Alert::info(
                    "verdict",
                    "Every file is present with the right size. Pieces were not hashed.",
                )
                .title("Sizes match")
                .into_any_element(),
            ),
            RunState::Finished(Ok(Verdict::Incomplete)) => {
                let n = self
                    .table
                    .read(cx)
                    .delegate()
                    .rows
                    .iter()
                    .filter(|r| r.label != "OK")
                    .count();
                Some(
                    Alert::warning(
                        "verdict",
                        format!("{n} file(s) need attention — see the table below."),
                    )
                    .title("Incomplete")
                    .into_any_element(),
                )
            }
            RunState::Finished(Err(e)) => Some(
                Alert::error("verdict", e.clone())
                    .title("Check failed")
                    .into_any_element(),
            ),
            RunState::Cancelled => Some(
                Alert::new("verdict", "The check was cancelled before it finished.")
                    .into_any_element(),
            ),
        };

        card(cx, "Check against")
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.against).cleanable(true)),
                    )
                    .child(
                        Button::new("browse-folder")
                            .outline()
                            .icon(IconName::FolderOpen)
                            .label("Browse…")
                            .on_click(cx.listener(Self::browse_folder)),
                    ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        Checkbox::new("quick")
                            .label("Quick check — compare sizes only, skip hashing")
                            .checked(self.quick)
                            .disabled(running)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.quick = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .when(running, |row| {
                        row.child(
                            Button::new("cancel")
                                .ghost()
                                .icon(IconName::CircleX)
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, _| this.queue.cancel())),
                        )
                    })
                    .child(
                        Button::new("check")
                            .primary()
                            .icon(IconName::FileSearch)
                            .label("Check")
                            .loading(running)
                            .disabled(self.meta.is_none() || running)
                            .on_click(cx.listener(Self::start)),
                    ),
            )
            .children(status)
    }
}

impl Render for TorrentCheck {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_rows = !self.table.read(cx).delegate().rows.is_empty();

        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_4()
            .when_some(self.error.clone(), |this, e| {
                this.child(Alert::error("error", e).title("Couldn't read the torrent"))
            })
            .child(self.render_torrent_card(cx))
            .child(self.render_check_card(cx))
            .when(has_rows, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_h(px(160.))
                        .child(DataTable::new(&self.table).stripe(true).bordered(true)),
                )
            })
    }
}
