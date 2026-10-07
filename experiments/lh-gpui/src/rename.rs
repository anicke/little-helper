//! Prepare → Rename: band, date, disc and year form in, an etree name per file out, with
//! the plan previewed as you type — the counterpart of `lh-tui/src/screens/rename.rs`.
//! The band and date start from the show folder's own name when it is an etree one.
//!
//! The rename itself is `execute_rename` as the queue's one job, as in the TUI: it moves
//! the whole plan or nothing, so every row resolves together from that one result.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::etree::ShowName;
use lh_core::job::{Event, Queue};
use lh_core::rename::{NameSpec, RenamePlan, RenameStatus, SpecError, execute_rename, plan_rename};
use std::path::{Path, PathBuf};

use crate::screen::Screen;
use crate::steps::{Audio, StepDone, StepResult, file_name};
use crate::ui::{bridge, card, scan_then};

type Outcome = lh_core::Result<Vec<PathBuf>>;

/// The spec the fields describe, or what to type to get one — `NameSpec::from_fields` as
/// the TUI uses it, saying why rather than just showing no plan.
pub fn spec(band: &str, date: &str, disc: &str, short_year: bool) -> Result<NameSpec, String> {
    NameSpec::from_fields(band, date, disc, short_year).map_err(|e| match e {
        SpecError::NoBand => "Type a band to preview the new names.".into(),
        SpecError::BadDate(_) => {
            "Type a date as YYYY-MM-DD or YY-MM-DD to preview the new names.".into()
        }
        SpecError::BadDisc(d) => format!("Disc “{d}” isn't a number."),
    })
}

/// A plan with what the page asks of it on every render, counted once.
struct Preview {
    plan: RenamePlan,
    changed: usize,
    collides: bool,
}

impl Preview {
    fn new(plan: RenamePlan) -> Self {
        Preview {
            changed: plan.changed(),
            collides: plan.has_collisions(),
            plan,
        }
    }
}

/// The band and ISO date the folder's name gives, when it is an etree show name.
pub fn seed(dir: &Path) -> (String, String) {
    ShowName::from_dir(dir)
        .map(|s| (s.band, s.date.render_iso()))
        .unwrap_or_default()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowStatus {
    Unchanged,
    Changed,
    Collision,
    Pending,
    Renamed,
    Failed,
}

struct Row {
    status: RowStatus,
    from: SharedString,
    to: SharedString,
}

struct Rows {
    rows: Vec<Row>,
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
            0 => Column::new("status", "Status").width(px(120.)),
            1 => Column::new("from", "From").width(px(400.)),
            _ => Column::new("to", "To").width(px(400.)),
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
        let muted = cx.theme().muted_foreground;
        match col_ix {
            0 => {
                let (tag, label) = match row.status {
                    RowStatus::Unchanged => (Tag::secondary(), "unchanged"),
                    RowStatus::Changed => (Tag::info(), "changes"),
                    RowStatus::Collision => (Tag::danger(), "COLLISION"),
                    RowStatus::Pending => (Tag::secondary(), "pending"),
                    RowStatus::Renamed => (Tag::success(), "renamed"),
                    RowStatus::Failed => (Tag::danger(), "FAILED"),
                };
                tag.small().outline().child(label).into_any_element()
            }
            1 => div()
                .when(row.status != RowStatus::Changed, |d| d.text_color(muted))
                .child(row.from.clone())
                .into_any_element(),
            _ => div()
                .when(row.status == RowStatus::Unchanged, |d| d.text_color(muted))
                .when(row.status == RowStatus::Collision, |d| {
                    d.text_color(cx.theme().danger)
                })
                .child(row.to.clone())
                .into_any_element(),
        }
    }
}

enum Stage {
    Editing,
    /// Holds the queue while its one job runs, so its events keep coming.
    Renaming {
        _queue: Queue<Outcome>,
    },
    /// What happened, as the sidebar and the alert both say it.
    Done(Result<String, String>),
}

pub struct Rename {
    dir: Option<PathBuf>,
    /// The folder's audio files, or why it has none to rename.
    files: Result<Vec<PathBuf>, String>,
    band: Entity<InputState>,
    date: Entity<InputState>,
    disc: Entity<InputState>,
    short_year: bool,
    /// The plan the fields describe, or why there is none.
    plan: Result<Preview, String>,
    stage: Stage,
    table: Entity<TableState<Rows>>,
}

impl EventEmitter<StepDone> for Rename {}

impl Screen for Rename {
    fn running(&self) -> bool {
        matches!(self.stage, Stage::Renaming { .. })
    }

    /// A new show folder, or the same one rescanned after a rename. The fields are seeded
    /// from a new folder's name, and kept for the same one.
    fn use_folder(
        &mut self,
        dir: &Path,
        audio: &Audio,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.running() {
            return;
        }
        if self.dir.as_deref() != Some(dir) {
            let (band, date) = seed(dir);
            self.band.update(cx, |s, cx| s.set_value(band, window, cx));
            self.date.update(cx, |s, cx| s.set_value(date, window, cx));
            self.disc.update(cx, |s, cx| s.set_value("", window, cx));
            self.short_year = false;
            self.dir = Some(dir.to_path_buf());
        }
        self.files = audio
            .as_ref()
            .map(|files| files.iter().map(|f| f.path.clone()).collect())
            .map_err(Clone::clone);
        self.stage = Stage::Editing;
        self.replan(cx);
    }
}

impl Rename {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
            cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.replan(cx);
                }
            })
            .detach();
            state
        };
        let band = field("gd", window, cx);
        let date = field("1977-05-08", window, cx);
        let disc = field("none", window, cx);
        let table = cx.new(|cx| TableState::new(Rows { rows: Vec::new() }, window, cx));
        Rename {
            dir: None,
            files: Ok(Vec::new()),
            band,
            date,
            disc,
            short_year: false,
            plan: Err(String::new()),
            stage: Stage::Editing,
            table,
        }
    }

    fn editing(&self) -> bool {
        matches!(self.stage, Stage::Editing)
    }

    /// Recomputes the preview from the fields; only while editing, since after a run the
    /// table shows what was applied.
    fn replan(&mut self, cx: &mut Context<Self>) {
        if !self.editing() {
            return;
        }
        let Ok(files) = &self.files else {
            return;
        };
        let value = |s: &Entity<InputState>| s.read(cx).value();
        self.plan = spec(
            &value(&self.band),
            &value(&self.date),
            &value(&self.disc),
            self.short_year,
        )
        .map(|spec| Preview::new(plan_rename(files, &spec)));
        self.show_rows(cx);
    }

    /// Fills the table from the plan, each row's status from its entry's and the stage.
    fn show_rows(&mut self, cx: &mut Context<Self>) {
        let status = |e: RenameStatus| match (&self.stage, e) {
            (Stage::Editing, RenameStatus::Unchanged) => RowStatus::Unchanged,
            (Stage::Editing, RenameStatus::Changed) => RowStatus::Changed,
            (Stage::Editing, RenameStatus::Collision) => RowStatus::Collision,
            (Stage::Renaming { .. }, RenameStatus::Changed) => RowStatus::Pending,
            (Stage::Done(Ok(_)), RenameStatus::Changed) => RowStatus::Renamed,
            (Stage::Done(Err(_)), RenameStatus::Changed) => RowStatus::Failed,
            _ => RowStatus::Unchanged,
        };
        let rows = match &self.plan {
            Ok(p) => p
                .plan
                .entries
                .iter()
                .map(|e| Row {
                    status: status(e.status),
                    from: file_name(&e.from).into(),
                    to: file_name(&e.to).into(),
                })
                .collect(),
            Err(_) => Vec::new(),
        };
        self.table.update(cx, |t, cx| {
            t.delegate_mut().rows = rows;
            t.refresh(cx);
        });
        cx.notify();
    }

    /// The plan the Rename button would apply, when there is one it may.
    fn applicable(&self) -> Option<&Preview> {
        self.plan
            .as_ref()
            .ok()
            .filter(|p| self.editing() && !p.collides)
    }

    /// Ends the run: every row and the sidebar take `outcome`.
    fn finish(&mut self, outcome: Result<String, String>, cx: &mut Context<Self>) {
        cx.emit(StepDone(match &outcome {
            Ok(msg) => StepResult::Clean(msg.clone()),
            Err(e) => StepResult::Unclean(format!("Nothing renamed: {e}")),
        }));
        self.stage = Stage::Done(outcome);
        self.show_rows(cx);
    }

    fn apply(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(preview) = self.applicable() else {
            return;
        };
        if preview.changed == 0 {
            self.finish(Ok("Every file already has its name.".into()), cx);
            return;
        }

        let queue: Queue<Outcome> = Queue::new();
        let job_plan = preview.plan.clone();
        queue.submit("rename", move |_| execute_rename(&job_plan));
        bridge(&queue, cx, Self::on_event);
        self.stage = Stage::Renaming { _queue: queue };
        self.show_rows(cx);
    }

    fn on_event(&mut self, event: Event<Outcome>, cx: &mut Context<Self>) {
        let outcome = match event {
            Event::Started { .. } | Event::Progress { .. } => return,
            Event::Finished { output, .. } => output.map_err(|e| e.to_string()),
            Event::Cancelled { .. } => Err("cancelled".into()),
        };
        let (renamed, total) = self
            .plan
            .as_ref()
            .map_or((0, 0), |p| (p.changed, p.plan.entries.len()));
        self.finish(
            outcome.map(|_| match total - renamed {
                0 => format!("{renamed} files renamed."),
                same => format!("{renamed} files renamed, {same} already had their name."),
            }),
            cx,
        );
    }

    /// Back to editing, on the folder as it now is.
    fn edit_again(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(dir) = self.dir.clone() {
            scan_then(dir, window, cx, |this, dir, scan, window, cx| {
                this.use_folder(dir, &scan.audio, window, cx)
            });
        }
    }

    fn render_spec(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.editing();
        let input = |label: &'static str, state: &Entity<InputState>| {
            Field::new()
                .label(label)
                .child(Input::new(state).disabled(!editing))
        };
        card(cx, "New names").child(
            Form::new()
                .columns(2)
                .label_width(px(90.))
                .child(input("Band", &self.band))
                .child(input("Date", &self.date))
                .child(
                    input("Disc", &self.disc)
                        .description("Leave empty for a set without disc numbers."),
                )
                .child(
                    Field::new().label("Year").child(
                        Checkbox::new("short-year")
                            .label("Short — 77-05-08 rather than 1977-05-08")
                            .checked(self.short_year)
                            .disabled(!editing)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.short_year = *checked;
                                this.replan(cx);
                            })),
                    ),
                ),
        )
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (summary, alert): (String, Option<Alert>) = match (&self.stage, &self.plan) {
            (Stage::Editing, Err(why)) => (why.clone(), None),
            (Stage::Editing, Ok(p)) if p.collides => (
                String::new(),
                Some(
                    Alert::error(
                        "rename-result",
                        "Two or more files would get the same name — see the table. \
                         Nothing will be renamed until they don't.",
                    )
                    .title("Collision"),
                ),
            ),
            (Stage::Editing, Ok(p)) => (
                format!(
                    "{} of {} files would change.",
                    p.changed,
                    p.plan.entries.len()
                ),
                None,
            ),
            (Stage::Renaming { .. }, _) => ("Renaming…".into(), None),
            (Stage::Done(Ok(msg)), _) => (
                String::new(),
                Some(Alert::success("rename-result", msg.clone()).title("Renamed")),
            ),
            (Stage::Done(Err(e)), _) => (
                String::new(),
                Some(
                    Alert::error(
                        "rename-result",
                        format!("{e}. Every file kept its old name."),
                    )
                    .title("Nothing renamed"),
                ),
            ),
        };

        let button = if matches!(self.stage, Stage::Done(_)) {
            Button::new("edit-again")
                .outline()
                .icon(IconName::PencilLine)
                .label("Edit again")
                .on_click(cx.listener(Self::edit_again))
        } else {
            Button::new("apply")
                .primary()
                .icon(IconName::PencilLine)
                .label("Rename")
                .loading(self.running())
                .disabled(self.applicable().is_none())
                .on_click(cx.listener(Self::apply))
        };

        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(summary),
                    )
                    .child(button),
            )
            .children(alert)
    }
}

impl Render for Rename {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Err(why) = &self.files {
            return Alert::warning("rename-refused", why.clone())
                .title("Nothing to rename")
                .into_any_element();
        }
        let has_rows = !self.table.read(cx).delegate().rows.is_empty();
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_4()
            .child(self.render_spec(cx))
            .child(self.render_actions(cx))
            .when(has_rows, |this| {
                this.child(
                    div()
                        .flex_1()
                        .min_h(px(160.))
                        .child(DataTable::new(&self.table).stripe(true).bordered(true)),
                )
            })
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's own `test` attribute in over the standard one.
    use super::{Path, seed, spec};

    #[test]
    fn spec_says_what_to_type() {
        assert!(
            spec("", "1977-05-08", "", false)
                .unwrap_err()
                .contains("band")
        );
        assert!(
            spec("gd", "1977-5-8", "", false)
                .unwrap_err()
                .contains("date")
        );
        assert!(
            spec("gd", "77-05-08", "two", false)
                .unwrap_err()
                .contains("two")
        );
        // The parsing itself is `NameSpec::from_fields`', tested in lh-core.
        assert!(spec("gd", "77-05-08", "", false).is_ok());
    }

    #[test]
    fn seed_reads_an_etree_folder_name_only() {
        let (band, date) = seed(Path::new("/shows/gd1969-01-25.sbd.kaplan.7923.sbeok.shnf"));
        assert_eq!((band.as_str(), date.as_str()), ("gd", "1969-01-25"));
        assert_eq!(
            seed(Path::new("/shows/My Show")),
            (String::new(), String::new())
        );
    }
}
