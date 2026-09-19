//! The GUI's look: which built-in theme follows the desktop's light/dark preference, and
//! the container, button and text styles the views share. Every colour is read from the
//! theme's extended palette rather than written as a literal, so a style never needs a
//! second definition for the other theme.
//!
//! Also the bundled fonts: Inter for all text, so the window reads the same on every
//! desktop instead of inheriting whatever sans the system resolves, and Lucide (via
//! `iced_fonts`) for icons. Inter is SIL OFL 1.1 — `fonts/Inter-OFL.txt` has to ship
//! alongside the binary (M4 packaging); Lucide is ISC and compiled in by `iced_fonts`.

use iced::font::Weight;
use iced::theme::Mode;
use iced::widget::{button, container, text};
use iced::{Background, Border, Font, Theme};

/// Every font the app loads at startup, in the form `iced::application(..).font()` takes.
pub(crate) const FONTS: [&[u8]; 3] = [
    include_bytes!("../fonts/Inter-Regular.ttf"),
    include_bytes!("../fonts/Inter-SemiBold.ttf"),
    iced_fonts::LUCIDE_FONT_BYTES,
];

/// The default font for all text.
pub(crate) const INTER: Font = Font::with_name("Inter");

/// Headings and labels: rail group headers, table column headers.
pub(crate) const INTER_SEMIBOLD: Font = Font {
    weight: Weight::Semibold,
    ..INTER
};

/// Tokyo Night Storm on a dark desktop, Catppuccin Latte otherwise — including
/// `Mode::None`, which is what a desktop that states no preference reports. Both were
/// picked from a side-by-side of every `Theme::ALL` entry on the real window: they had the
/// clearest primary buttons of the dark and light sets respectively.
pub(crate) fn theme_for(mode: Mode) -> Theme {
    match mode {
        Mode::Dark => Theme::TokyoNightStorm,
        Mode::Light | Mode::None => Theme::CatppuccinLatte,
    }
}

const RADIUS: f32 = 6.0;

/// The rail and the dock: one step off the window background, so the three regions
/// (`docs/gui-shell.md` §4) read as regions without needing a border each.
pub(crate) fn surface(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.weakest.color.into()),
        text_color: Some(palette.background.weakest.text),
        ..container::Style::default()
    }
}

/// A bordered panel on the window background — the file table's frame.
pub(crate) fn card(theme: &Theme) -> container::Style {
    let palette = theme.extended_palette();
    container::Style {
        background: Some(palette.background.base.color.into()),
        border: Border {
            width: 1.0,
            radius: RADIUS.into(),
            color: palette.background.weak.color,
        },
        ..container::Style::default()
    }
}

/// One of a set of mutually exclusive choices — a rail row, a dock tab, a checksum kind.
/// The selected one takes the theme's accent; the rest have no chrome until hovered, so a
/// row of them reads as a list rather than as a row of buttons.
pub(crate) fn choice(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let palette = theme.extended_palette();
        let (background, text_color) = if selected {
            (Some(palette.primary.weak.color), palette.primary.weak.text)
        } else if matches!(status, button::Status::Hovered | button::Status::Pressed) {
            (
                Some(palette.background.weak.color),
                palette.background.weak.text,
            )
        } else {
            (None, palette.background.base.text)
        };
        button::Style {
            background: background.map(Background::Color),
            text_color,
            border: Border {
                radius: RADIUS.into(),
                ..Border::default()
            },
            ..button::Style::default()
        }
    }
}

/// Labels that name or group other content — rail group headers, table column headers —
/// rather than being content themselves.
pub(crate) fn muted(theme: &Theme) -> text::Style {
    text::Style {
        color: Some(
            theme
                .extended_palette()
                .background
                .base
                .text
                .scale_alpha(0.6),
        ),
    }
}
