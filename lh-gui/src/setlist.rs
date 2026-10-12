//! Prepare → Setlist: the info `.txt` (docs/info-file.md) — the notes `infofile::plan`
//! collected, then the text exactly as it will be written — the counterpart of
//! `lh-tui/src/screens/setlist.rs`. Like torrent info there is no `Queue`: one small file,
//! written in-process. An existing file is only replaced by the separate Replace button,
//! since it is usually hand-written and cannot be regenerated (§1).
//!
//! The plan reads every file's tags, so it is made off the UI thread, and made again after
//! a run elsewhere that can change what it says (`Step::changes_setlist`).

use gpui_kit::assets::IconName;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use lh_core::infofile::{self, InfoPlan};
use std::path::{Path, PathBuf};

use crate::screen::Screen;
use crate::steps::{Audio, StepDone, StepResult, file_name, scan_show};
use crate::ui::{background_then, card};

/// A plan and the text it renders to.
pub struct Planned {
    plan: InfoPlan,
    text: SharedString,
    exists: bool,
}

/// The folder's info-file plan, or why there is none — refusing a folder with no FLAC
/// in it, the same as `lh setlist`. Off the UI thread.
pub fn load(dir: &Path, audio: Audio) -> Result<Planned, String> {
    let plan = infofile::plan(dir, &audio?).map_err(|e| format!("reading tags: {e:#}"))?;
    if plan.tracks.is_empty() {
        return Err("No FLAC files — the setlist is made from their tags.".into());
    }
    Ok(Planned {
        text: infofile::render(&plan).into(),
        exists: plan.exists(),
        plan,
    })
}

enum Load {
    /// No folder yet, or its tags are still being read.
    Reading,
    Ready(Box<Planned>),
    Refused(String),
}

pub struct Setlist {
    dir: Option<PathBuf>,
    load: Load,
    /// The last write's outcome, until the plan is made again.
    outcome: Option<Result<String, String>>,
}

impl EventEmitter<StepDone> for Setlist {}

impl Screen for Setlist {
    /// The write is in-process and over before the next frame.
    fn running(&self) -> bool {
        false
    }

    fn use_folder(
        &mut self,
        dir: &Path,
        audio: &Audio,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dir = Some(dir.to_path_buf());
        self.plan(audio.clone(), window, cx);
    }
}

impl Setlist {
    pub fn new() -> Self {
        Setlist {
            dir: None,
            load: Load::Reading,
            outcome: None,
        }
    }

    /// Plans again from the folder as it now is: after a run that retagged, renamed or
    /// converted its files, or when asked to.
    pub fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        self.load = Load::Reading;
        cx.notify();
        let scanned = dir.clone();
        background_then(
            window,
            cx,
            move || scan_show(&scanned).audio,
            move |this, audio, window, cx| {
                if this.dir.as_ref() == Some(&dir) {
                    this.plan(audio, window, cx);
                }
            },
        );
    }

    fn plan(&mut self, audio: Audio, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        self.load = Load::Reading;
        self.outcome = None;
        cx.notify();
        let planned = dir.clone();
        background_then(
            window,
            cx,
            move || load(&planned, audio),
            move |this, loaded, _, cx| {
                // A folder chosen while this one was reading wins.
                if this.dir.as_ref() != Some(&dir) {
                    return;
                }
                this.load = match loaded {
                    Ok(p) => Load::Ready(Box::new(p)),
                    Err(why) => Load::Refused(why),
                };
                cx.notify();
            },
        );
    }

    /// Writes the file; `overwrite` only from the Replace button.
    fn write(&mut self, overwrite: bool, cx: &mut Context<Self>) {
        let Load::Ready(planned) = &mut self.load else {
            return;
        };
        let outcome = match infofile::write(&planned.plan, overwrite) {
            Ok(path) => {
                planned.exists = true;
                Ok(format!("Wrote {}.", file_name(&path)))
            }
            Err(e) => Err(format!("{e:#}")),
        };
        cx.emit(StepDone(match &outcome {
            Ok(msg) => StepResult::Clean(msg.clone()),
            Err(e) => StepResult::Unclean(e.clone()),
        }));
        self.outcome = Some(outcome);
        cx.notify();
    }

    fn render_actions(&self, planned: &Planned, cx: &mut Context<Self>) -> impl IntoElement {
        let name = file_name(&planned.plan.path);
        let alert = match &self.outcome {
            Some(Ok(msg)) => Some(Alert::success("setlist-result", msg.clone()).title("Written")),
            Some(Err(e)) => Some(Alert::error("setlist-result", e.clone()).title("Not written")),
            None if planned.exists => Some(
                Alert::warning(
                    "setlist-exists",
                    format!(
                        "{name} is already in the folder. It is usually written by hand and \
                         can't be regenerated, so it is only replaced on purpose."
                    ),
                )
                .title("Already exists"),
            ),
            None => None,
        };
        let button = if planned.exists {
            Button::new("setlist-replace")
                .danger()
                .icon(IconName::ListMusic)
                .label(format!("Replace {name}"))
                .on_click(cx.listener(|this, _, _, cx| this.write(true, cx)))
        } else {
            Button::new("setlist-write")
                .primary()
                .icon(IconName::ListMusic)
                .label(format!("Write {name}"))
                .on_click(cx.listener(|this, _, _, cx| this.write(false, cx)))
        };
        v_flex()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(planned.plan.path.display().to_string()),
                    )
                    .child(
                        Button::new("setlist-reload")
                            .outline()
                            .icon(IconName::RefreshCw)
                            .label("Re-read tags")
                            .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
                    )
                    .child(button),
            )
            .children(alert)
    }
}

impl Render for Setlist {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let planned = match &self.load {
            Load::Refused(why) => {
                return Alert::warning("setlist-refused", why.clone())
                    .title("No setlist to write")
                    .into_any_element();
            }
            Load::Reading => {
                return card(cx, "Setlist")
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Reading the files' tags…"),
                    )
                    .into_any_element();
            }
            Load::Ready(p) => p,
        };

        let notes = &planned.plan.problems;
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .gap_4()
            .child(self.render_actions(planned, cx))
            .when(!notes.is_empty(), |this| {
                this.child(card(cx, "Notes").children(notes.iter().map(|p| {
                    div()
                        .text_sm()
                        .text_color(cx.theme().warning)
                        .child(p.to_string())
                })))
            })
            .child(
                card(cx, "As it will be written")
                    .flex_1()
                    .min_h(px(200.))
                    .child(
                        div()
                            .id("setlist-text")
                            .flex_1()
                            .min_h_0()
                            .overflow_scrollbar()
                            .font_family("monospace")
                            .text_sm()
                            .whitespace_nowrap()
                            .child(planned.text.clone()),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui's own `test` attribute in over the standard one.
    use super::{infofile, load};
    use crate::steps::tests::show;
    use lh_core::scan;

    /// A show of fixtures: the WAV is a note, not a track; the file is written once, and
    /// then only replaced when told to.
    #[test]
    fn load_plans_the_flacs_and_a_write_refuses_to_replace() {
        let dir = show(
            "setlist",
            &["cdda-aligned.flac", "hires-24bit.flac", "cdda-aligned.wav"],
        );
        let audio = scan::scan(&dir, false).unwrap().files;
        let planned = load(&dir, Ok(audio)).unwrap();
        assert_eq!(planned.plan.tracks.len(), 2);
        assert!(planned.text.contains("Total: "));
        assert!(!planned.exists);

        infofile::write(&planned.plan, false).unwrap();
        assert!(planned.plan.exists());
        assert!(infofile::write(&planned.plan, false).is_err());
        infofile::write(&planned.plan, true).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_refuses_a_folder_without_flacs() {
        let dir = show("setlist-wav", &["cdda-aligned.wav"]);
        let audio = scan::scan(&dir, false).unwrap().files;
        assert!(load(&dir, Ok(audio)).is_err());
        assert_eq!(load(&dir, Err("why".into())).err().as_deref(), Some("why"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
