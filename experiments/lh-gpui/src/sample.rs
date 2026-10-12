//! Prepare → Sample: pick a track, set where the clip starts, how long it is, the MP3
//! mode and the size limit, and encode it beside the show folder — the counterpart of
//! `lh-tui/src/screens/sample.rs` (docs/sample.md §4). The size estimate follows the
//! fields as they change, so a clip that will not fit says so before it is asked for.
//!
//! The encoder is found, and every track's title read, off the UI thread when the folder
//! arrives; a folder with no encoder to use is refused the way `lh sample` refuses it.
//! The encode is the queue's one job, with progress and Cancel. An existing sample is
//! only replaced by the separate Replace button.

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, IndexPath, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::sample::{
    self, Clip, Mode, Request, Sample, estimate_bytes, format_length, format_limit, format_size,
    format_time, longest_fitting, parse_size, parse_time,
};
use lh_core::tag::{self, Field as TagField};
use lh_core::tools::Tool;
use std::path::{Path, PathBuf};

use crate::screen::Screen;
use crate::steps::{Audio, StepDone, StepResult, file_name};
use crate::ui::{background_then, bridge, card};

type Outcome = lh_core::Result<Sample>;

/// One track in the list: the file, its title from the tags when it has one, its length.
pub struct Track {
    file: AudioFile,
    title: Option<String>,
    secs: Option<f64>,
}

/// What the screen works with once a folder is read.
pub struct Loaded {
    tracks: Vec<Track>,
    encoder: Tool,
}

/// The folder's tracks and the encoder, or why the screen cannot run — no audio, or
/// neither `lame` nor `ffmpeg`. Off the UI thread: it reads every file's tags.
pub fn load(audio: Audio) -> Result<Loaded, String> {
    let files = audio?;
    let encoder = sample::find_encoder().map_err(|e| format!("{e:#}"))?;
    let tracks = files
        .into_iter()
        .map(|file| Track {
            title: tag::read(&file.path)
                .ok()
                .and_then(|t| t.get(TagField::Title).map(str::to_string))
                .filter(|t| !t.is_empty()),
            secs: file.stream_info.duration_secs(),
            file,
        })
        .collect();
    Ok(Loaded { tracks, encoder })
}

/// The values the fields make, or which one does not parse.
pub struct Settings {
    clip: Result<Clip, &'static str>,
    max_bytes: Option<u64>,
    mode: Mode,
}

impl Settings {
    pub fn read(start: &str, length: &str, limit: &str, mode: Mode) -> Self {
        let start = parse_time(start);
        let length = parse_time(length).filter(|l| *l > 0.0);
        let clip = match (start, length) {
            (None, _) => Err("Start is not a time like 1:30."),
            (_, None) => Err("Length is not a time like 30 or 0:30."),
            (Some(start), Some(length)) => Ok(Clip { start, length }),
        };
        Settings {
            clip,
            max_bytes: parse_size(limit),
            mode,
        }
    }

    /// What Encode would ask for, or why the fields do not make a sample.
    pub fn request(&self, track_secs: Option<f64>, overwrite: bool) -> Result<Request, String> {
        let clip = self.clip.map_err(str::to_string)?;
        let max_bytes = self
            .max_bytes
            .ok_or("Limit is not a size like 1M or 900K.")?;
        if track_secs.is_some_and(|t| clip.start >= t) {
            return Err("The clip starts after the track ends.".into());
        }
        Ok(Request {
            clip,
            mode: self.mode,
            max_bytes: Some(max_bytes),
            overwrite,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Good,
    Bad,
}

/// The lines under the fields: what will be cut, how big it comes out against the limit,
/// and how long a clip the limit allows at this mode.
pub fn describe(track_secs: Option<f64>, settings: &Settings) -> Vec<(&'static str, String, Tone)> {
    let mut lines = Vec::new();
    match settings.clip {
        Err(why) => lines.push(("Clip", why.to_string(), Tone::Bad)),
        Ok(clip) if track_secs.is_some_and(|t| clip.start >= t) => {
            lines.push(("Clip", "Starts after the track ends.".into(), Tone::Bad));
        }
        Ok(clip) => {
            // What the encode will actually cut: a clip running past the end stops there.
            let end = track_secs.map_or(clip.end(), |t| clip.end().min(t));
            let of = track_secs
                .map(|t| format!(" of {}", format_time(t)))
                .unwrap_or_default();
            lines.push((
                "Clip",
                format!("{} – {}{of}", format_time(clip.start), format_time(end)),
                Tone::Plain,
            ));

            let mode = settings.mode;
            let estimate = estimate_bytes(mode, (end - clip.start).max(0.0));
            let approx = if mode.is_exact() { "≈" } else { "~" };
            lines.push(match settings.max_bytes {
                Some(max) if estimate > max => (
                    "Size",
                    format!(
                        "{approx} {} — over the {} limit",
                        format_size(estimate),
                        format_size(max)
                    ),
                    Tone::Bad,
                ),
                Some(max) => (
                    "Size",
                    format!(
                        "{approx} {} of {}{}",
                        format_size(estimate),
                        format_size(max),
                        if mode.is_exact() {
                            ""
                        } else {
                            ", checked after encoding"
                        }
                    ),
                    Tone::Good,
                ),
                None => (
                    "Size",
                    format!("{approx} {}", format_size(estimate)),
                    Tone::Plain,
                ),
            });
        }
    }
    match settings.max_bytes {
        Some(max) => lines.push((
            "Fits",
            format!(
                "Up to {} s at this format.",
                longest_fitting(settings.mode, max)
            ),
            Tone::Plain,
        )),
        None => lines.push(("Limit", "Not a size like 1M or 900K.".into(), Tone::Bad)),
    }
    lines
}

struct Rows {
    tracks: Vec<(SharedString, SharedString, SharedString)>,
}

impl TableDelegate for Rows {
    fn columns_count(&self, _: &App) -> usize {
        3
    }

    fn rows_count(&self, _: &App) -> usize {
        self.tracks.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        match col_ix {
            0 => Column::new("title", "Track").width(px(260.)),
            1 => Column::new("file", "File").width(px(260.)),
            _ => Column::new("length", "Length").width(px(80.)),
        }
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let (title, file, length) = &self.tracks[row_ix];
        match col_ix {
            0 => title.clone().into_any_element(),
            1 => div()
                .text_color(cx.theme().muted_foreground)
                .child(file.clone())
                .into_any_element(),
            _ => length.clone().into_any_element(),
        }
    }
}

enum Stage {
    /// No folder yet, or its tracks are still being read.
    Reading,
    Refused(String),
    Editing,
    Encoding {
        done: u32,
        total: u32,
    },
    Wrote(Sample),
    /// The output is already there; Replace overwrites it.
    Exists(PathBuf),
    Failed(String),
}

pub struct SampleEdit {
    dir: Option<PathBuf>,
    tracks: Vec<Track>,
    encoder: Option<Tool>,
    selected: usize,
    /// Where the selected track's sample goes, or why it has nowhere to.
    output: Result<PathBuf, String>,
    start: Entity<InputState>,
    length: Entity<InputState>,
    limit: Entity<InputState>,
    mode: Entity<SelectState<Vec<SharedString>>>,
    stage: Stage,
    queue: Queue<Outcome>,
    table: Entity<TableState<Rows>>,
}

impl EventEmitter<StepDone> for SampleEdit {}

impl Screen for SampleEdit {
    fn running(&self) -> bool {
        matches!(self.stage, Stage::Encoding { .. })
    }

    /// The fields keep their mode and limit across folders; the clip is each track's own.
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
        self.stage = Stage::Reading;
        cx.notify();
        let audio = audio.clone();
        let dir = dir.to_path_buf();
        background_then(
            window,
            cx,
            move || load(audio),
            move |this, loaded, window, cx| {
                // A folder chosen while this one was reading wins.
                if this.dir.as_ref() != Some(&dir) {
                    return;
                }
                match loaded {
                    Ok(loaded) => this.show(loaded, window, cx),
                    Err(why) => this.stage = Stage::Refused(why),
                }
                cx.notify();
            },
        );
    }
}

impl SampleEdit {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let field = |value: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cx| InputState::new(window, cx).default_value(value));
            cx.subscribe(&state, |this, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.edited(cx);
                }
            })
            .detach();
            state
        };
        let start = field(String::new(), window, cx);
        let length = field(String::new(), window, cx);
        let limit = field(format_limit(sample::DEFAULT_MAX_BYTES), window, cx);

        let labels: Vec<SharedString> = Mode::PRESETS.iter().map(|m| m.label().into()).collect();
        let default = Mode::PRESETS
            .iter()
            .position(|m| *m == Mode::DEFAULT)
            .map(IndexPath::new);
        let mode = cx.new(|cx| SelectState::new(labels, default, window, cx));
        cx.subscribe(&mode, |this, _, _: &SelectEvent<Vec<SharedString>>, cx| {
            this.edited(cx)
        })
        .detach();

        let table = cx.new(|cx| TableState::new(Rows { tracks: Vec::new() }, window, cx));
        cx.subscribe_in(&table, window, |this, _, event: &TableEvent, window, cx| {
            if let TableEvent::SelectRow(ix) = event {
                this.select(*ix, window, cx);
            }
        })
        .detach();

        let queue = Queue::with_workers(1);
        // Ends when the view (and so the queue) drops.
        bridge(&queue, cx, Self::on_event);

        SampleEdit {
            dir: None,
            tracks: Vec::new(),
            encoder: None,
            selected: 0,
            output: Err(String::new()),
            start,
            length,
            limit,
            mode,
            stage: Stage::Reading,
            queue,
            table,
        }
    }

    fn show(&mut self, loaded: Loaded, window: &mut Window, cx: &mut Context<Self>) {
        let rows = loaded
            .tracks
            .iter()
            .map(|t| {
                let name = t.file.file_name();
                (
                    t.title.clone().unwrap_or_else(|| name.clone()).into(),
                    name.into(),
                    t.secs.map_or("?".into(), format_time).into(),
                )
            })
            .collect();
        self.tracks = loaded.tracks;
        self.encoder = Some(loaded.encoder);
        self.stage = Stage::Editing;
        // Whatever was selected before is another folder's track.
        self.selected = usize::MAX;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().tracks = rows;
            t.refresh(cx);
        });
        self.select(0, window, cx);
        self.table.update(cx, |t, cx| t.set_selected_row(0, cx));
    }

    fn track(&self) -> Option<&Track> {
        self.tracks.get(self.selected)
    }

    fn mode(&self, cx: &App) -> Mode {
        let mode = self.mode.read(cx);
        mode.selected_value()
            .and_then(|label| Mode::PRESETS.iter().find(|m| m.label() == label.as_ref()))
            .copied()
            .unwrap_or(Mode::DEFAULT)
    }

    fn settings(&self, cx: &App) -> Settings {
        let value = |s: &Entity<InputState>| s.read(cx).value();
        Settings::read(
            &value(&self.start),
            &value(&self.length),
            &value(&self.limit),
            self.mode(cx),
        )
    }

    /// Picks track `index`, with its own default clip and output.
    fn select(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if index == self.selected || index >= self.tracks.len() || self.running() {
            return;
        }
        self.selected = index;
        self.output =
            sample::default_output(&self.tracks[index].file.path).map_err(|e| e.to_string());
        self.reset_clip(window, cx);
    }

    /// The selected track's default clip at the current mode and limit.
    fn reset_clip(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(track) = self.track() else {
            return;
        };
        let max = parse_size(&self.limit.read(cx).value()).unwrap_or(sample::DEFAULT_MAX_BYTES);
        let clip = Clip::default_for(track.secs, self.mode(cx), max);
        self.start
            .update(cx, |s, cx| s.set_value(format_time(clip.start), window, cx));
        self.length.update(cx, |s, cx| {
            s.set_value(format_length(clip.length), window, cx)
        });
        self.edited(cx);
    }

    /// A field or the mode changed: the last result no longer describes what Encode would
    /// do — in particular, Replace would replace something else.
    fn edited(&mut self, cx: &mut Context<Self>) {
        if matches!(
            self.stage,
            Stage::Wrote(_) | Stage::Exists(_) | Stage::Failed(_)
        ) {
            self.stage = Stage::Editing;
        }
        cx.notify();
    }

    fn encode(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let (Some(track), Some(encoder), Ok(dst)) = (self.track(), &self.encoder, &self.output)
        else {
            return;
        };
        let Ok(request) = self.settings(cx).request(track.secs, overwrite) else {
            return;
        };
        let src = track.file.path.clone();
        let (dst, encoder) = (dst.clone(), encoder.clone());

        self.queue.cancel_token().reset();
        self.queue.submit(track.file.file_name(), move |p| {
            sample::encode(&src, &dst, &encoder, &request, &mut |done, total| {
                p.report(done, total);
                !p.is_cancelled()
            })
        });
        self.stage = Stage::Encoding { done: 0, total: 0 };
        cx.notify();
    }

    fn on_event(&mut self, event: Event<Outcome>, cx: &mut Context<Self>) {
        const STOPPED: &str = "Stopped; nothing was written.";
        self.stage = match event {
            Event::Started { .. } => return,
            Event::Progress { done, total, .. } => Stage::Encoding { done, total },
            Event::Finished { output, .. } => match output {
                Ok(sample) => Stage::Wrote(sample),
                Err(lh_core::Error::OutputExists { path }) => Stage::Exists(path),
                Err(lh_core::Error::Cancelled) => Stage::Failed(STOPPED.into()),
                // The output is named on the page already; the reason is what is new.
                Err(lh_core::Error::SampleTooLarge { detail, .. }) => Stage::Failed(detail),
                Err(e) => Stage::Failed(e.to_string()),
            },
            Event::Cancelled { .. } => Stage::Failed(STOPPED.into()),
        };
        match &self.stage {
            Stage::Wrote(s) => cx.emit(StepDone(StepResult::Clean(format!(
                "Wrote {} ({}).",
                file_name(&s.output),
                format_size(s.bytes)
            )))),
            Stage::Failed(e) => cx.emit(StepDone(StepResult::Unclean(e.clone()))),
            _ => {}
        }
        cx.notify();
    }

    fn render_clip(&self, settings: &Settings, cx: &mut Context<Self>) -> impl IntoElement {
        let editable = !self.running();
        let input = |label: &'static str, state: &Entity<InputState>| {
            Field::new()
                .label(label)
                .child(Input::new(state).disabled(!editable))
        };
        let track_secs = self.track().and_then(|t| t.secs);
        let mut lines = describe(track_secs, settings);
        lines.push(match &self.output {
            Ok(p) => ("Output", file_name(p), Tone::Plain),
            Err(e) => ("Output", e.clone(), Tone::Bad),
        });
        let theme = cx.theme();
        let summary = lines.into_iter().map(|(label, text, tone)| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(
                    div()
                        .w(px(60.))
                        .flex_none()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(tone == Tone::Good, |d| d.text_color(theme.success))
                        .when(tone == Tone::Bad, |d| d.text_color(theme.danger))
                        .child(text),
                )
        });

        card(cx, "Clip")
            .child(
                Form::new()
                    .label_width(px(60.))
                    .child(input("Start", &self.start).description("1:30, or seconds."))
                    .child(input("Length", &self.length).description("Seconds."))
                    .child(
                        Field::new()
                            .label("Format")
                            .child(Select::new(&self.mode).disabled(!editable)),
                    )
                    .child(input("Limit", &self.limit).description("1M, 900K, 1MiB.")),
            )
            .child(v_flex().gap_1().children(summary))
            .child(
                h_flex().justify_end().child(
                    Button::new("sample-default")
                        .outline()
                        .icon(IconName::RefreshCw)
                        .label("Default clip")
                        .disabled(!editable)
                        .on_click(cx.listener(|this, _, window, cx| this.reset_clip(window, cx))),
                ),
            )
    }

    fn render_actions(&self, settings: &Settings, cx: &mut Context<Self>) -> impl IntoElement {
        let track_secs = self.track().and_then(|t| t.secs);
        let ready = settings.request(track_secs, false).is_ok() && self.output.is_ok();

        let status: Option<AnyElement> = match &self.stage {
            Stage::Encoding { done, total } => {
                let pct = if *total > 0 {
                    *done as f32 / *total as f32 * 100.
                } else {
                    0.
                };
                let encoder = self.encoder.as_ref().map_or("the encoder", |e| e.id.name());
                Some(
                    v_flex()
                        .gap_1()
                        .child(
                            Progress::new("sample-progress")
                                .value(pct)
                                .loading(*total == 0),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(if *total > 0 {
                                    format!("Cutting the clip… {}%", pct as u32)
                                } else {
                                    format!("Encoding with {encoder}…")
                                }),
                        )
                        .into_any_element(),
                )
            }
            Stage::Wrote(s) => Some(
                Alert::success(
                    "sample-result",
                    format!(
                        "{}, {}, {} – {}.",
                        s.output.display(),
                        format_size(s.bytes),
                        format_time(s.clip.start),
                        format_time(s.clip.end())
                    ),
                )
                .title("Written")
                .into_any_element(),
            ),
            Stage::Exists(path) => Some(
                Alert::warning(
                    "sample-result",
                    format!(
                        "{} is already there. It is only replaced on purpose.",
                        file_name(path)
                    ),
                )
                .title("Already exists")
                .into_any_element(),
            ),
            Stage::Failed(e) => Some(
                Alert::error("sample-result", e.clone())
                    .title("Not written")
                    .into_any_element(),
            ),
            Stage::Reading | Stage::Refused(_) | Stage::Editing => None,
        };

        let running = self.running();
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .justify_end()
                    .when(running, |row| {
                        row.child(
                            Button::new("sample-cancel")
                                .ghost()
                                .icon(IconName::CircleX)
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, _| this.queue.cancel())),
                        )
                    })
                    .when(matches!(self.stage, Stage::Exists(_)), |row| {
                        row.child(
                            Button::new("sample-replace")
                                .danger()
                                .icon(IconName::Scissors)
                                .label("Replace")
                                .disabled(!ready)
                                .on_click(cx.listener(|this, _, _, cx| this.encode(true, cx))),
                        )
                    })
                    .child(
                        Button::new("sample-encode")
                            .primary()
                            .icon(IconName::Scissors)
                            .label("Encode")
                            .loading(running)
                            .disabled(!ready || running)
                            .on_click(cx.listener(|this, _, _, cx| this.encode(false, cx))),
                    ),
            )
            .children(status)
    }
}

impl Render for SampleEdit {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match &self.stage {
            Stage::Refused(why) => {
                return Alert::warning("sample-refused", why.clone())
                    .title("No sample to cut")
                    .into_any_element();
            }
            Stage::Reading => {
                return card(cx, "Sample")
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Reading the tracks…"),
                    )
                    .into_any_element();
            }
            _ => {}
        }
        let settings = self.settings(cx);
        h_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .items_start()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .min_h(px(200.))
                    .child(DataTable::new(&self.table).stripe(true).bordered(true)),
            )
            .child(
                v_flex()
                    .w(px(400.))
                    .flex_none()
                    .gap_4()
                    .child(self.render_clip(&settings, cx))
                    .child(self.render_actions(&settings, cx)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's own `test` attribute in over the standard one.
    use super::{Mode, Settings, Tone, describe, load};
    use crate::steps::tests::show;
    use lh_core::scan;

    #[test]
    fn settings_say_which_field_is_wrong() {
        let mode = Mode::DEFAULT;
        let ok = Settings::read("1:00", "30", "1M", mode);
        let request = ok.request(Some(300.0), false).unwrap();
        assert_eq!((request.clip.start, request.clip.length), (60.0, 30.0));
        assert_eq!(request.max_bytes, Some(1_000_000));

        let err = |s: Settings| s.request(Some(300.0), false).unwrap_err();
        assert!(err(Settings::read("1:75", "30", "1M", mode)).contains("Start"));
        assert!(err(Settings::read("1:00", "0", "1M", mode)).contains("Length"));
        assert!(err(Settings::read("1:00", "30", "1G", mode)).contains("Limit"));
        assert!(err(Settings::read("5:00", "30", "1M", mode)).contains("after the track"));
        // A track of unknown length cannot be started past.
        assert!(
            Settings::read("5:00", "30", "1M", mode)
                .request(None, false)
                .is_ok()
        );
    }

    #[test]
    fn describe_marks_a_clip_over_the_limit() {
        let tone = |s: Settings| {
            describe(Some(300.0), &s)
                .into_iter()
                .find(|(label, ..)| *label == "Size")
                .map(|(_, _, tone)| tone)
        };
        assert_eq!(
            tone(Settings::read("0", "30", "1M", Mode::DEFAULT)),
            Some(Tone::Good)
        );
        assert_eq!(
            tone(Settings::read("0", "60", "1M", Mode::DEFAULT)),
            Some(Tone::Bad)
        );
        assert_eq!(
            tone(Settings::read("0", "30", "", Mode::DEFAULT)),
            Some(Tone::Plain)
        );
        // A clip past the end is cut where the track ends.
        let lines = describe(
            Some(40.0),
            &Settings::read("0:20", "30", "1M", Mode::DEFAULT),
        );
        assert_eq!(lines[0].1, "0:20 – 0:40 of 0:40");
    }

    #[test]
    fn load_reads_each_track_or_passes_the_refusal_on() {
        assert_eq!(load(Err("why".into())).err().as_deref(), Some("why"));
        // Without lame or ffmpeg the screen is refused; nothing more to check here then.
        if lh_core::sample::find_encoder().is_err() {
            return;
        }
        let dir = show("sample", &["cdda-aligned.flac", "cdda-aligned.wav"]);
        let audio = scan::scan(&dir, false).unwrap().files;
        let loaded = load(Ok(audio)).unwrap();
        assert_eq!(loaded.tracks.len(), 2);
        assert!(loaded.tracks.iter().all(|t| t.secs.is_some()));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
