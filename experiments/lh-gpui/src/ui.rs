//! Small pieces every page shares.

use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::*;

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
