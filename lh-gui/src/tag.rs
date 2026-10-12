//! Prepare → Tag: the six show-level fields and a title per track in, an etree tag set per
//! FLAC out, with each file's diff previewed as you type — the counterpart of
//! `lh-tui/src/screens/tag.rs`. The fields start from what the files already carry, then
//! the folder's etree date (`tag::seed_show`); an empty field or title line leaves that
//! tag alone, as an unset flag does for `lh tag`.
//!
//! Each changed file is one job, `tag::write` then a fresh `tag::read`, so a row ends on
//! what the file now carries; a failed write is that row's alone.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::job::{Event, Queue};
use lh_core::tag::{self, Tags, Tracks};
use std::path::{Path, PathBuf};

use crate::screen::Screen;
use crate::steps::{Audio, StepDone, StepResult};
use crate::ui::{background_then, bridge, card};

/// How a write went, and what the file carries afterwards — read back even when the
/// write failed, so the next edit diffs against what is on disk rather than the plan.
type Outcome = (lh_core::Result<()>, Option<Tags>);

/// The folder's tracks and their tags, or why there is nothing to tag. Off the UI thread.
fn load(audio: Audio) -> Result<Tracks, String> {
    let tracks = tag::read_tracks(audio?).map_err(|e| format!("reading tags: {e:#}"))?;
    if tracks.files.is_empty() {
        return Err("No FLAC files to tag — only FLAC carries Vorbis comments.".into());
    }
    Ok(tracks)
}

/// The titles box's lines as one title per track, `None` (leave it alone) for an empty or
/// missing line, and how many lines run past the last track — refused rather than
/// silently dropped.
pub fn titles(text: &str, tracks: usize) -> (Vec<Option<String>>, usize) {
    let lines = tag::title_lines(text);
    let extra = lines.len().saturating_sub(tracks);
    let mut titles: Vec<Option<String>> = lines
        .into_iter()
        .map(|t| (!t.is_empty()).then_some(t))
        .collect();
    titles.resize(tracks, None);
    (titles, extra)
}

/// One line saying what an edit changes in a file; empty when it changes nothing.
pub fn describe(before: &Tags, edit: &Tags) -> String {
    before
        .changes(edit)
        .iter()
        .map(|(field, old, new)| format!("{}: “{}” → “{new}”", field.key(), old.unwrap_or("")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, PartialEq, Eq)]
enum RowStatus {
    Unchanged,
    Changes,
    Pending,
    Running,
    Written,
    Failed(String),
}

struct Row {
    status: RowStatus,
    number: SharedString,
    file: SharedString,
    changes: SharedString,
}

struct Rows {
    rows: Vec<Row>,
}

impl TableDelegate for Rows {
    fn columns_count(&self, _: &App) -> usize {
        4
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("status", "Status").width(px(110.)),
            1 => Column::new("number", "#").width(px(60.)),
            2 => Column::new("file", "File").width(px(300.)),
            _ => Column::new("changes", "Changes").width(px(700.)),
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
                    RowStatus::Changes => (Tag::info(), "changes"),
                    RowStatus::Pending => (Tag::secondary(), "pending"),
                    RowStatus::Running => (Tag::info(), "writing"),
                    RowStatus::Written => (Tag::success(), "written"),
                    RowStatus::Failed(_) => (Tag::danger(), "FAILED"),
                };
                tag.small().outline().child(label).into_any_element()
            }
            1 => div()
                .text_color(muted)
                .child(row.number.clone())
                .into_any_element(),
            2 => div().child(row.file.clone()).into_any_element(),
            _ => match &row.status {
                RowStatus::Failed(e) => div()
                    .text_color(cx.theme().danger)
                    .child(e.clone())
                    .into_any_element(),
                status => div()
                    .when(*status == RowStatus::Unchanged, |d| d.text_color(muted))
                    .child(row.changes.clone())
                    .into_any_element(),
            },
        }
    }
}

/// The edit the fields describe for every file, each diffed once against its file.
struct Preview {
    edits: Vec<Tags>,
    /// What each edit changes, described; empty for a file it leaves as it is.
    diffs: Vec<String>,
    /// Title lines past the last track.
    extra_titles: usize,
}

impl Preview {
    fn new(before: &[Tags], edits: Vec<Tags>, extra_titles: usize) -> Self {
        Preview {
            diffs: before
                .iter()
                .zip(&edits)
                .map(|(b, e)| describe(b, e))
                .collect(),
            edits,
            extra_titles,
        }
    }

    fn changed(&self) -> usize {
        self.diffs.iter().filter(|d| !d.is_empty()).count()
    }
}

enum Load {
    /// No folder yet, or its tags are still being read.
    Reading,
    Ready(Tracks),
    Refused(String),
}

enum Stage {
    Editing,
    /// Holds the queue while its jobs run, and which row each job is.
    Writing {
        _queue: Queue<Outcome>,
        rows: Vec<usize>,
    },
    /// What happened, as the sidebar and the alert both say it.
    Done(Result<String, String>),
}

pub struct TagEdit {
    dir: Option<PathBuf>,
    load: Load,
    show: [Entity<InputState>; 6],
    titles: Entity<TextareaState>,
    preview: Option<Preview>,
    stage: Stage,
    statuses: Vec<RowStatus>,
    table: Entity<TableState<Rows>>,
}

impl EventEmitter<StepDone> for TagEdit {}

impl Screen for TagEdit {
    fn running(&self) -> bool {
        matches!(self.stage, Stage::Writing { .. })
    }

    /// A new show folder: its tags are read in the background, then seed the fields.
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
        self.dir = Some(dir.to_path_buf());
        self.load = Load::Reading;
        self.stage = Stage::Editing;
        self.preview = None;
        cx.notify();

        let dir = dir.to_path_buf();
        let audio = audio.clone();
        background_then(
            window,
            cx,
            move || load(audio),
            move |this, loaded, window, cx| {
                // A folder chosen while this one was reading wins.
                if this.dir.as_ref() == Some(&dir) {
                    this.loaded(loaded, &dir, window, cx);
                }
            },
        );
    }
}

impl TagEdit {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let show = tag::Field::SHOW.map(|_| {
            let state = cx.new(|cx| InputState::new(window, cx));
            Self::replan_on_change(&state, cx);
            state
        });
        let titles = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(6, 14)
                .placeholder("One title per line, in file order")
        });
        Self::replan_on_change(&titles, cx);
        let table = cx.new(|cx| TableState::new(Rows { rows: Vec::new() }, window, cx));
        TagEdit {
            dir: None,
            load: Load::Reading,
            show,
            titles,
            preview: None,
            stage: Stage::Editing,
            statuses: Vec::new(),
            table,
        }
    }

    fn replan_on_change<S: EventEmitter<InputEvent>>(state: &Entity<S>, cx: &mut Context<Self>) {
        cx.subscribe(state, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.replan(cx);
            }
        })
        .detach();
    }

    /// The folder's tags have been read: seed the fields from them and preview.
    fn loaded(
        &mut self,
        loaded: Result<Tracks, String>,
        dir: &Path,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tracks = match loaded {
            Ok(t) => t,
            Err(why) => {
                self.load = Load::Refused(why);
                cx.notify();
                return;
            }
        };
        let seed = tag::seed_show(&tracks.before, dir);
        for (state, field) in self.show.iter().zip(tag::Field::SHOW) {
            let value = seed.get(field).unwrap_or_default().to_string();
            state.update(cx, |s, cx| s.set_value(value, window, cx));
        }
        let titles = tracks
            .before
            .iter()
            .map(|t| t.title.as_deref().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        self.titles
            .update(cx, |s, cx| s.set_value(titles, window, cx));
        self.load = Load::Ready(tracks);
        self.replan(cx);
    }

    fn editing(&self) -> bool {
        matches!(self.stage, Stage::Editing)
    }

    /// Recomputes every file's edit from the fields; only while editing, since after a
    /// write the table shows what happened.
    fn replan(&mut self, cx: &mut Context<Self>) {
        if !self.editing() {
            return;
        }
        let Load::Ready(tracks) = &self.load else {
            return;
        };
        // An empty field leaves the tag alone, as an unset `lh tag` flag does.
        let mut fields = Tags::default();
        for (state, field) in self.show.iter().zip(tag::Field::SHOW) {
            let value = state.read(cx).value().to_string();
            fields.set(field, (!value.is_empty()).then_some(value));
        }
        let total = tracks.files.len();
        let (titles, extra) = titles(&self.titles.read(cx).value(), total);
        let edits = titles
            .into_iter()
            .enumerate()
            .map(|(i, title)| Tags::for_track(&fields, i, total, title))
            .collect();
        let preview = Preview::new(&tracks.before, edits, extra);
        self.statuses = preview
            .diffs
            .iter()
            .map(|d| {
                if d.is_empty() {
                    RowStatus::Unchanged
                } else {
                    RowStatus::Changes
                }
            })
            .collect();
        self.preview = Some(preview);
        self.show_rows(cx);
    }

    /// Fills the table from the statuses and the preview's diffs.
    fn show_rows(&mut self, cx: &mut Context<Self>) {
        let rows = match (&self.load, &self.preview) {
            (Load::Ready(tracks), Some(preview)) => tracks
                .files
                .iter()
                .zip(&preview.edits)
                .zip(&preview.diffs)
                .zip(&self.statuses)
                .map(|(((file, edit), diff), status)| Row {
                    status: status.clone(),
                    number: format!(
                        "{}/{}",
                        edit.track_number.as_deref().unwrap_or_default(),
                        edit.track_total.as_deref().unwrap_or_default()
                    )
                    .into(),
                    file: file.file_name().into(),
                    changes: match status {
                        RowStatus::Unchanged => "already carries these tags".into(),
                        _ => diff.clone().into(),
                    },
                })
                .collect(),
            _ => Vec::new(),
        };
        self.table.update(cx, |t, cx| {
            t.delegate_mut().rows = rows;
            t.refresh(cx);
        });
        cx.notify();
    }

    /// The preview the Write button would apply, when there is one it may.
    fn writable(&self) -> Option<&Preview> {
        self.preview
            .as_ref()
            .filter(|p| self.editing() && p.extra_titles == 0)
    }

    fn write(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        let (Some(preview), Load::Ready(tracks)) = (self.writable(), &self.load) else {
            return;
        };
        if preview.changed() == 0 {
            self.finish(Ok("Every file already carries these tags.".into()), cx);
            return;
        }

        let queue: Queue<Outcome> = Queue::new();
        let mut rows = Vec::new();
        for (i, (file, edit)) in tracks.files.iter().zip(&preview.edits).enumerate() {
            if self.statuses[i] != RowStatus::Changes {
                continue;
            }
            let path = file.path.clone();
            let edit = edit.clone();
            queue.submit(file.file_name(), move |_| {
                let wrote = tag::write(&path, &edit);
                (wrote, tag::read(&path).ok())
            });
            rows.push(i);
        }
        for &i in &rows {
            self.statuses[i] = RowStatus::Pending;
        }
        bridge(&queue, cx, Self::on_event);
        self.stage = Stage::Writing {
            _queue: queue,
            rows,
        };
        self.show_rows(cx);
    }

    fn on_event(&mut self, event: Event<Outcome>, cx: &mut Context<Self>) {
        let Stage::Writing { rows, .. } = &self.stage else {
            return;
        };
        let (id, status) = match event {
            Event::Started { id, .. } => (id, RowStatus::Running),
            Event::Progress { .. } => return,
            Event::Finished {
                id,
                output: (wrote, after),
                ..
            } => {
                // What the file carries now is the next edit's "before".
                if let (Some(tags), Load::Ready(tracks)) = (after, &mut self.load) {
                    tracks.before[rows[id.index()]] = tags;
                }
                match wrote {
                    Ok(()) => (id, RowStatus::Written),
                    Err(e) => (id, RowStatus::Failed(e.to_string())),
                }
            }
            Event::Cancelled { id, .. } => (id, RowStatus::Failed("cancelled".into())),
        };
        self.statuses[rows[id.index()]] = status;

        let done = |s: &RowStatus| matches!(s, RowStatus::Written | RowStatus::Failed(_));
        if !rows.iter().all(|&i| done(&self.statuses[i])) {
            self.show_rows(cx);
            return;
        }
        let total = rows.len();
        let failed = rows
            .iter()
            .filter(|&&i| matches!(self.statuses[i], RowStatus::Failed(_)))
            .count();
        self.finish(
            match failed {
                0 => Ok(format!("{total} files tagged.")),
                _ => Err(format!(
                    "{failed} of {total} writes failed — see the table."
                )),
            },
            cx,
        );
    }

    /// Ends the run: the sidebar and the alert take `outcome`.
    fn finish(&mut self, outcome: Result<String, String>, cx: &mut Context<Self>) {
        cx.emit(StepDone(match &outcome {
            Ok(msg) => StepResult::Clean(msg.clone()),
            Err(e) => StepResult::Unclean(e.clone()),
        }));
        self.stage = Stage::Done(outcome);
        self.show_rows(cx);
    }

    /// Back to editing, the fields as they were, diffed against what the files now carry.
    fn edit_again(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.stage = Stage::Editing;
        self.replan(cx);
    }

    fn render_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let editing = self.editing();
        let fields = tag::Field::SHOW
            .iter()
            .zip(&self.show)
            .map(|(field, state)| {
                Field::new()
                    .label(field.key())
                    .child(Input::new(state).disabled(!editing))
            });
        h_flex()
            .gap_4()
            .items_start()
            .child(
                card(cx, "Show")
                    .flex_1()
                    .min_w_0()
                    .child(Form::new().label_width(px(100.)).children(fields)),
            )
            .child(
                card(cx, "Titles")
                    .flex_1()
                    .min_w_0()
                    .child(Textarea::new(&self.titles).disabled(!editing))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(
                                "Paste a setlist: one title per line, in file order. \
                                 An empty field or line leaves that tag as it is.",
                            ),
                    ),
            )
    }

    fn render_actions(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Load::Ready(show) = &self.load else {
            unreachable!("render shows no actions until the tags are read");
        };
        let total = show.files.len();
        let ignored = match show.ignored {
            0 => String::new(),
            1 => " 1 non-FLAC file is left out.".into(),
            n => format!(" {n} non-FLAC files are left out."),
        };
        let (summary, alert): (String, Option<Alert>) = match (&self.stage, &self.preview) {
            (Stage::Editing, Some(p)) if p.extra_titles > 0 => (
                String::new(),
                Some(
                    Alert::error(
                        "tag-result",
                        format!(
                            "{} more title lines than the {total} FLAC files. \
                             Remove the extras to write.",
                            p.extra_titles
                        ),
                    )
                    .title("Too many titles"),
                ),
            ),
            (Stage::Editing, Some(p)) => (
                format!("{} of {total} files would change.{ignored}", p.changed()),
                None,
            ),
            (Stage::Editing, None) => (String::new(), None),
            (Stage::Writing { .. }, _) => ("Writing…".into(), None),
            (Stage::Done(Ok(msg)), _) => (
                String::new(),
                Some(Alert::success("tag-result", msg.clone()).title("Tagged")),
            ),
            (Stage::Done(Err(e)), _) => (
                String::new(),
                Some(Alert::error("tag-result", e.clone()).title("Not every file was tagged")),
            ),
        };

        let button = if matches!(self.stage, Stage::Done(_)) {
            Button::new("edit-again")
                .outline()
                .icon(IconName::Tag)
                .label("Edit again")
                .on_click(cx.listener(Self::edit_again))
        } else {
            Button::new("write")
                .primary()
                .icon(IconName::Tag)
                .label("Write tags")
                .loading(self.running())
                .disabled(self.writable().is_none())
                .on_click(cx.listener(Self::write))
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

impl Render for TagEdit {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.load {
            Load::Refused(why) => {
                return Alert::warning("tag-refused", why.clone())
                    .title("Nothing to tag")
                    .into_any_element();
            }
            Load::Reading => {
                return card(cx, "Tags")
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Reading the files' tags…"),
                    )
                    .into_any_element();
            }
            Load::Ready(_) => {}
        }
        v_flex()
            .id("tag-page")
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_4()
            .overflow_y_scroll()
            .child(self.render_fields(cx))
            .child(self.render_actions(cx))
            .child(
                div()
                    .flex_shrink_0()
                    .h(px(320.))
                    .child(DataTable::new(&self.table).stripe(true).bordered(true)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's own `test` attribute in over the standard one.
    use super::{Tags, describe, load, tag, titles};
    use crate::steps::tests::show;
    use lh_core::scan;

    #[test]
    fn titles_leave_empty_and_missing_lines_alone() {
        let (t, extra) = titles("One\r\n\r\nThree", 4);
        assert_eq!(t, [Some("One".into()), None, Some("Three".into()), None]);
        assert_eq!(extra, 0);
        // Extras are counted, not kept.
        let (t, extra) = titles("One\nTwo\nThree", 2);
        assert_eq!(t.len(), 2);
        assert_eq!(extra, 1);
    }

    #[test]
    fn describe_lists_each_change() {
        let mut before = Tags::default();
        before.set(tag::Field::Artist, Some("gd".into()));
        let mut edit = before.clone();
        edit.set(tag::Field::Artist, Some("Grateful Dead".into()));
        edit.set(tag::Field::TrackNumber, Some("1".into()));
        assert_eq!(
            describe(&before, &edit),
            "ARTIST: “gd” → “Grateful Dead”, TRACKNUMBER: “” → “1”"
        );
        assert_eq!(describe(&before, &before), "");
    }

    /// A show of fixtures: only the FLACs are tracks, every one is read, and a write
    /// through `tag::write` lands and reads back.
    #[test]
    fn load_reads_only_the_flacs_and_a_write_reads_back() {
        let dir = show(
            "tag",
            &["cdda-aligned.flac", "hires-24bit.flac", "cdda-aligned.wav"],
        );
        let audio = scan::scan(&dir, false).unwrap().files;
        let tracks = load(Ok(audio)).unwrap();
        assert_eq!((tracks.files.len(), tracks.ignored), (2, 1));

        let first = &tracks.files[0].path;
        let mut fields = Tags::default();
        fields.set(tag::Field::Artist, Some("Grateful Dead".into()));
        let edit = Tags::for_track(&fields, 0, 2, Some("Bertha".into()));
        tag::write(first, &edit).unwrap();
        assert!(tag::read(first).unwrap().changes(&edit).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_refuses_a_folder_without_flacs() {
        assert!(load(Ok(Vec::new())).is_err());
        assert_eq!(load(Err("why".into())).err().as_deref(), Some("why"));
    }
}
