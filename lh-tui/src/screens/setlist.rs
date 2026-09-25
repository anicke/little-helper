use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind};
use lh_cli::SetlistArgs;
use lh_core::AudioFile;
use lh_core::infofile::{self, InfoPlan};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};

// --- Setlist -----------------------------------------------------------------------------
//
// `lh setlist` as a screen (docs/info-file.md): the notes `plan` collected, then the text
// exactly as it will be written, scrollable. Like torrent info there is no `Queue` — one
// small file, written in-process. `a` writes; an existing file is only replaced with `F`,
// since it is usually hand-written and cannot be regenerated (§1).

pub(crate) fn run_setlist(args: SetlistArgs, theme: ThemeName) -> ExitCode {
    let folder = match scan_folder(&args.dir) {
        Ok(f) => f,
        Err(refusal) => return refusal.exit(),
    };
    for line in &folder.skipped {
        eprintln!("{line}");
    }
    let plan = match plan_setlist(&args.dir, &folder.files) {
        Ok(p) => p,
        Err(refusal) => return refusal.exit(),
    };

    let result = {
        let mut terminal = TerminalGuard::new();
        setlist_screen(&mut terminal, &plan, Theme::new(theme))
    };
    match result {
        Ok(None) | Ok(Some(Ok(_))) => ExitCode::SUCCESS,
        Ok(Some(Err(e))) => {
            eprintln!("lh-tui: {e:#}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Reads the tags; refuses a folder with no FLAC in it, the same as `lh setlist`.
pub(crate) fn plan_setlist(dir: &Path, files: &[AudioFile]) -> Result<InfoPlan, Refusal> {
    let plan = infofile::plan(dir, files).map_err(|e| {
        Refusal::new(
            2,
            format!("lh-tui: reading tags in {}: {e:#}", dir.display()),
        )
    })?;
    if plan.tracks.is_empty() {
        return Err(Refusal::new(
            1,
            format!("no FLAC files in {}", dir.display()),
        ));
    }
    Ok(plan)
}

/// `None` when the person left without writing; otherwise the write's outcome.
pub(crate) fn setlist_screen(
    terminal: &mut DefaultTerminal,
    plan: &InfoPlan,
    theme: Theme,
) -> io::Result<Option<lh_core::Result<PathBuf>>> {
    let text = infofile::render(plan);
    let exists = plan.exists();
    let mut scroll: u16 = 0;
    let mut refused = false;
    loop {
        terminal.draw(|frame| draw_setlist(frame, plan, &text, exists, refused, scroll, &theme))?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                if is_quit(&key) {
                    return Ok(None);
                }
                match key.code {
                    KeyCode::Char('a') if exists => refused = true,
                    KeyCode::Char('a') => return Ok(Some(infofile::write(plan, false))),
                    KeyCode::Char('F') if exists => {
                        return Ok(Some(infofile::write(plan, true)));
                    }
                    KeyCode::Down | KeyCode::Char('j') => scroll = scroll.saturating_add(1),
                    KeyCode::Up | KeyCode::Char('k') => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    KeyCode::Home => scroll = 0,
                    _ => {}
                }
            }
        }
    }
}

fn draw_setlist(
    frame: &mut Frame,
    plan: &InfoPlan,
    text: &str,
    exists: bool,
    refused: bool,
    scroll: u16,
    theme: &Theme,
) {
    let notes: Vec<Line> = plan
        .problems
        .iter()
        .map(|p| Line::styled(p.to_string(), theme.warn))
        .collect();
    let notes_height = if notes.is_empty() {
        0
    } else {
        notes.len() as u16 + 2
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(notes_height),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" setlist  "),
        Span::styled(plan.path.display().to_string(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    if !notes.is_empty() {
        frame.render_widget(
            Paragraph::new(notes).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" notes ")
                    .border_style(theme.dim),
            ),
            chunks[1],
        );
    }

    let name = file_name(&plan.path);
    let title = if exists {
        format!(" {name} (exists) ")
    } else {
        format!(" {name} ")
    };
    frame.render_widget(
        Paragraph::new(text).scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.dim),
        ),
        chunks[2],
    );

    let footer = if refused {
        Line::styled(
            format!(" {name} already exists   F replace it   q/esc quit "),
            theme.warn,
        )
    } else if exists {
        Line::styled(" ↑↓ scroll   F replace   q/esc quit ", theme.dim)
    } else {
        Line::styled(" ↑↓ scroll   a write   q/esc quit ", theme.dim)
    };
    frame.render_widget(Paragraph::new(footer), chunks[3]);
}
