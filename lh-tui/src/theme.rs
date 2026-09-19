use ratatui::style::{Color, Modifier, Style};

/// Color themes this UI can draw with. `Default` relies on the terminal's own ANSI
/// palette (see `Theme::new`'s doc comment); the rest are fixed RGB palettes already
/// shipped by name in plenty of other Go/Rust TUIs (bottom, yazi, lazygit, bat, ...),
/// so `--theme nord` etc. reads the same set of hues here as it does there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ThemeName {
    Default,
    CatppuccinMocha,
    CatppuccinLatte,
    Nord,
    Dracula,
    Gruvbox,
    TokyoNight,
}

/// Every color/style this UI uses, named once. Deliberately avoids inverted
/// backgrounds (`.bg(Color::Cyan)`, `.bg(Color::DarkGray)`) — those assume a dark
/// terminal and clash on a light one, since ratatui's named colors map to the
/// terminal's own ANSI palette rather than fixed RGB. Bold/underline read as
/// "header" or "accent" regardless of the terminal's background.
pub(crate) struct Theme {
    pub(crate) accent: Style,
    pub(crate) ok: Style,
    pub(crate) warn: Style,
    pub(crate) error: Style,
    pub(crate) dim: Style,
    pub(crate) header: Style,
}

impl Theme {
    pub(crate) fn new(name: ThemeName) -> Self {
        match name {
            ThemeName::Default => Theme {
                accent: Style::default().fg(Color::Cyan),
                ok: Style::default().fg(Color::Green).bold(),
                warn: Style::default().fg(Color::Yellow),
                error: Style::default().fg(Color::Red).bold(),
                dim: Style::default().fg(Color::DarkGray),
                header: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            },
            // Mauve, green, yellow, red, overlay0.
            ThemeName::CatppuccinMocha => {
                Theme::palette(0xcba6f7, 0xa6e3a1, 0xf9e2af, 0xf38ba8, 0x6c7086)
            }
            // Mauve, green, yellow, red, overlay0 (Latte's light-background variants).
            ThemeName::CatppuccinLatte => {
                Theme::palette(0x8839ef, 0x40a02b, 0xdf8e1d, 0xd20f39, 0x9ca0b0)
            }
            // Frost cyan (nord8), aurora green (nord14)/yellow (nord13)/red (nord11), nord3.
            ThemeName::Nord => Theme::palette(0x88c0d0, 0xa3be8c, 0xebcb8b, 0xbf616a, 0x4c566a),
            // Purple, green, yellow, red, comment.
            ThemeName::Dracula => Theme::palette(0xbd93f9, 0x50fa7b, 0xf1fa8c, 0xff5555, 0x6272a4),
            // Bright purple, green, yellow, red, gray.
            ThemeName::Gruvbox => Theme::palette(0xd3869b, 0xb8bb26, 0xfabd2f, 0xfb4934, 0x928374),
            // Purple, green, yellow, red (magenta/pink role omitted), comment.
            ThemeName::TokyoNight => {
                Theme::palette(0xbb9af7, 0x9ece6a, 0xe0af68, 0xf7768e, 0x565f89)
            }
        }
    }

    /// Builds a theme from five packed-RGB hex colors, one per role — the same five every
    /// named palette above maps its own hues onto. `header` stays modifier-only regardless
    /// of theme: nothing here ever sets a background, so bold+underline is what stays
    /// legible no matter what the terminal's own background is.
    pub(crate) fn palette(accent: u32, ok: u32, warn: u32, error: u32, dim: u32) -> Self {
        Theme {
            accent: Style::default().fg(rgb(accent)),
            ok: Style::default().fg(rgb(ok)).bold(),
            warn: Style::default().fg(rgb(warn)),
            error: Style::default().fg(rgb(error)).bold(),
            dim: Style::default().fg(rgb(dim)),
            header: Style::default().add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        }
    }
}

pub(crate) fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
