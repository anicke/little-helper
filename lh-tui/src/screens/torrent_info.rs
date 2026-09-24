use std::io;
use std::process::ExitCode;
use std::time::Duration;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyEventKind};
use lh_core::display;
use lh_core::torrent::Metainfo;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::PathBuf;

// --- Torrent info ------------------------------------------------------------------------
//
// The one screen with no `Queue` at all (`docs/tui.md` §9): `Metainfo::read` is a single
// in-process parse, not a job worth submitting anywhere, so there is nothing to stream —
// the screen reads the file once before `ratatui::init()` and just redraws the same static
// content every 80ms tick until `q`/`Esc`/`Ctrl-C`, the same three keys every other screen
// uses to quit. Redrawing on every tick rather than once is what lets a terminal resize
// reflow the layout for free, same as every live screen already does.

pub(crate) fn run_torrent_info(file: PathBuf, list_files: bool, theme: ThemeName) -> ExitCode {
    let t = match Metainfo::read(&file) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e:#}", file.display());
            return ExitCode::from(2);
        }
    };

    let result = {
        let mut terminal = TerminalGuard::new();
        run_torrent_info_screen(&mut terminal, &t, list_files, Theme::new(theme))
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn run_torrent_info_screen(
    terminal: &mut DefaultTerminal,
    t: &Metainfo,
    list_files: bool,
    theme: Theme,
) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw_torrent_info(frame, t, list_files, &theme))?;

        if event::poll(Duration::from_millis(80))? {
            if let CtEvent::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    let quit = is_quit(&key);
                    if quit {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn draw_torrent_info(frame: &mut Frame, t: &Metainfo, list_files: bool, theme: &Theme) {
    let area = frame.area();
    let lines = torrent_info_lines(t, theme);
    let detail_height = lines.len() as u16 + 2;

    let chunks = if list_files {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(detail_height),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(3),
                Constraint::Length(1),
            ])
            .split(area)
    };

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" torrent info  "),
        Span::styled(t.name.clone(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" torrent ")
                .border_style(theme.dim),
        ),
        chunks[1],
    );

    if list_files {
        draw_torrent_info_files(frame, chunks[2], t, theme);
        draw_footer(frame, chunks[3], theme);
    } else {
        draw_footer(frame, chunks[2], theme);
    }
}

/// Mirrors `lh-cli`'s `cmd_torrent_info` line-by-line so the two front ends never drift.
pub(crate) fn torrent_info_lines<'a>(t: &Metainfo, theme: &'a Theme) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(format!("infohash     {}", t.info_hash_hex())),
        Line::from(format!(
            "pieces       {} x {}",
            t.pieces.len(),
            display::bytes(t.piece_length)
        )),
        Line::from(format!(
            "total        {} ({} bytes)",
            display::bytes(t.total_length),
            t.total_length
        )),
    ];
    let real = t.real_files().count();
    let pad = t.files.len() - real;
    lines.push(if pad > 0 {
        Line::from(format!("files        {real} ({pad} padding)"))
    } else {
        Line::from(format!("files        {real}"))
    });
    if t.private {
        lines.push(Line::styled(
            "private      yes (BEP 27; part of the infohash)",
            theme.warn,
        ));
    }
    if let Some(v) = &t.source {
        lines.push(Line::from(format!("source       {v}")));
    }
    if let Some(v) = &t.created_by {
        lines.push(Line::from(format!("created by   {v}")));
    }
    if let Some(ts) = t.creation_date {
        lines.push(Line::from(format!("created      {}", display::date(ts))));
    }
    if let Some(v) = &t.comment {
        for (i, line) in v.lines().enumerate() {
            lines.push(Line::from(format!(
                "{:<12} {line}",
                if i == 0 { "comment" } else { "" }
            )));
        }
    }
    for (i, tracker) in t.trackers().enumerate() {
        lines.push(Line::from(format!(
            "{:<12} {tracker}",
            if i == 0 { "trackers" } else { "" }
        )));
    }
    lines
}

pub(crate) fn draw_torrent_info_files(frame: &mut Frame, area: Rect, t: &Metainfo, theme: &Theme) {
    let rows = t.real_files().map(|f| {
        Row::new(vec![
            Cell::from(display::bytes(f.length)),
            Cell::from(f.display_path()),
        ])
    });

    let table = Table::new(rows, [Constraint::Length(11), Constraint::Percentage(100)])
        .header(Row::new(vec!["size", "file"]).style(theme.header))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" files ")
                .border_style(theme.dim),
        );

    frame.render_widget(table, area);
}
