//! Small pieces every page shares.

use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;
use lh_core::job::{Event, Queue};
use std::path::PathBuf;

use crate::steps::{FolderScan, scan_show};

/// A titled surface — gpui-kit has `GroupBox`, but a plain bordered card reads closer to
/// what the iced app is trying (and failing) to look like.
pub fn card(cx: &App, title: &'static str) -> Div {
    v_flex()
        .gap_3()
        .p_4()
        .rounded(cx.theme().radius_lg)
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().background)
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(cx.theme().muted_foreground)
                .child(title),
        )
}

/// Bridges a queue's blocking receiver onto gpui: waits on the background executor, hands
/// each event to `on_event` on the foreground. Ends once every sender is gone (the queue
/// dropped) or the view is.
pub fn bridge<T: Send + 'static, V: 'static>(
    queue: &Queue<T>,
    cx: &mut Context<V>,
    on_event: impl Fn(&mut V, Event<T>, &mut Context<V>) + 'static,
) {
    let rx = queue.events();
    cx.spawn(async move |this, cx| {
        loop {
            let rx = rx.clone();
            let Ok(event) = cx.background_spawn(async move { rx.recv() }).await else {
                break;
            };
            if this
                .update(cx, |this, cx| on_event(this, event, cx))
                .is_err()
            {
                break;
            }
        }
    })
    .detach();
}

/// Runs `work` on the background executor, then hands its result to `then` on the
/// foreground — a folder's probe or tag read never blocks the window.
pub fn background_then<V: 'static, R: Send + 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    work: impl FnOnce() -> R + Send + 'static,
    then: impl FnOnce(&mut V, R, &mut Window, &mut Context<V>) + 'static,
) {
    cx.spawn_in(window, async move |this, cx| {
        let result = cx.background_spawn(async move { work() }).await;
        this.update_in(cx, |this, window, cx| then(this, result, window, cx))
            .ok();
    })
    .detach();
}

/// Scans the show folder `dir` in the background, then hands the scan to `then`.
pub fn scan_then<V: 'static>(
    dir: PathBuf,
    window: &mut Window,
    cx: &mut Context<V>,
    then: impl FnOnce(&mut V, &PathBuf, FolderScan, &mut Window, &mut Context<V>) + 'static,
) {
    let scanned = dir.clone();
    background_then(
        window,
        cx,
        move || scan_show(&scanned),
        move |this, scan, window, cx| then(this, &dir, scan, window, cx),
    );
}
