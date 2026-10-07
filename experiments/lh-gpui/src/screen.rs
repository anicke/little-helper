//! What the workspace needs from a ported screen that keeps its own view: whether it is
//! busy, and to hear about a new show folder. Batch screens are not these — the workspace
//! makes a fresh `Batch` per run — but every screen, batch or not, reports how it went
//! through the one `StepDone` event.

use gpui_kit::*;
use std::path::Path;

use crate::steps::{Audio, StepDone};

pub trait Screen: Render + EventEmitter<StepDone> {
    /// Whether a job is in flight, which keeps the show folder from changing under it.
    fn running(&self) -> bool;

    /// A new show folder, once it has been scanned: its audio files, or why it has none.
    fn use_folder(
        &mut self,
        dir: &Path,
        audio: &Audio,
        window: &mut Window,
        cx: &mut Context<Self>,
    );
}

/// A screen of any type, as the workspace holds it.
pub trait AnyScreen {
    fn running(&self, cx: &App) -> bool;
    fn use_folder(&self, dir: &Path, audio: &Audio, window: &mut Window, cx: &mut App);
    fn view(&self) -> AnyView;
}

impl<T: Screen> AnyScreen for Entity<T> {
    fn running(&self, cx: &App) -> bool {
        self.read(cx).running()
    }

    fn use_folder(&self, dir: &Path, audio: &Audio, window: &mut Window, cx: &mut App) {
        self.update(cx, |s, cx| s.use_folder(dir, audio, window, cx));
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }
}
