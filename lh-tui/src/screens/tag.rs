use std::io;
use std::process::ExitCode;
use std::time::Duration;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::TagArgs;
use lh_core::checksum::ffp;
use lh_core::etree::ShowName;
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::scan;
use lh_core::tag::{self, Tags};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::Path;

// --- Tag ---------------------------------------------------------------------------------

/// The six show-level fields a person edits directly, in the order shown on screen. The
/// other two of `tag::Field::ALL` are handled separately: `TITLE` is per-track (the
/// titles pane below), `TRACKNUMBER` is always derived from position and has no field at
/// all (docs/tagging.md §5).
pub(crate) const TAG_SHOW_FIELDS: [tag::Field; 6] = [
    tag::Field::Artist,
    tag::Field::Album,
    tag::Field::Date,
    tag::Field::Genre,
    tag::Field::Comment,
    tag::Field::Location,
];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TagFocus {
    None,
    Show(usize),
    Titles,
}

pub(crate) enum TagStage {
    Editing,
    Writing,
    Done,
}

#[derive(Clone)]
pub(crate) enum WriteStatus {
    Pending,
    Running,
    NotApplicable,
    Unchanged,
    Ok,
    Failed(String),
}

pub(crate) struct WriteRow {
    name: String,
    status: WriteStatus,
}

pub(crate) fn run_tag(args: TagArgs, theme: ThemeName) -> ExitCode {
    if !args.dir.is_dir() {
        eprintln!("lh-tui: {} is not a directory", args.dir.display());
        return ExitCode::from(2);
    }
    let set = match scan::scan(&args.dir, false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("lh-tui: scanning {}: {e:#}", args.dir.display());
            return ExitCode::from(2);
        }
    };
    for (skipped, why) in &set.skipped {
        eprintln!("skipped {}: {why}", skipped.display());
    }
    if set.files.is_empty() {
        eprintln!("no audio files found in {}", args.dir.display());
        return ExitCode::from(1);
    }

    // Read every taggable file's existing tags up front — the "before" state the diff
    // pane and the apply stage's audio-unchanged postcondition are both built from
    // (docs/tagging.md §1 contract point 2).
    let mut before: Vec<Tags> = Vec::with_capacity(set.files.len());
    for f in &set.files {
        if tag::is_taggable(f.format) {
            match tag::read(&f.path) {
                Ok(t) => before.push(t),
                Err(e) => {
                    eprintln!("lh-tui: reading tags from {}: {e:#}", f.path.display());
                    return ExitCode::from(2);
                }
            }
        } else {
            before.push(Tags::default());
        }
    }

    let show_name = args
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ShowName::parse);

    // Seed the show-level fields from the first taggable file that already carries any
    // tags, then from the folder name's own date where that leaves it blank
    // (docs/tagging.md §6).
    let mut show = before
        .iter()
        .find(|t| !t.is_empty())
        .cloned()
        .unwrap_or_default();
    if show.date.is_none() {
        if let Some(sn) = &show_name {
            show.date = Some(sn.date.render_iso());
        }
    }
    if let Some(v) = &args.artist {
        show.artist = Some(v.clone());
    }
    if let Some(v) = &args.album {
        show.album = Some(v.clone());
    }
    if let Some(v) = &args.date {
        show.date = Some(v.clone());
    }
    if let Some(v) = &args.genre {
        show.genre = Some(v.clone());
    }
    if let Some(v) = &args.comment {
        show.comment = Some(v.clone());
    }
    if let Some(v) = &args.location {
        show.location = Some(v.clone());
    }

    let mut titles: Vec<String> = before
        .iter()
        .map(|t| t.title.clone().unwrap_or_default())
        .collect();
    if let Some(path) = &args.titles {
        let text = if path == Path::new("-") {
            match io::read_to_string(io::stdin()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("lh-tui: reading titles from stdin: {e}");
                    return ExitCode::from(2);
                }
            }
        } else {
            match std::fs::read_to_string(path) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("lh-tui: reading {}: {e}", path.display());
                    return ExitCode::from(2);
                }
            }
        };
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() != set.files.len() {
            eprintln!(
                "lh-tui: {} titles given but {} files in {}",
                lines.len(),
                set.files.len(),
                args.dir.display()
            );
            return ExitCode::from(2);
        }
        titles = lines;
    }

    let result = {
        let mut terminal = TerminalGuard::with_paste();
        run_tag_screen(
            &mut terminal,
            &args.dir,
            set.files,
            before,
            show,
            titles,
            Theme::new(theme),
        )
    };

    match result {
        Ok(ok) => {
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

pub(crate) fn tag_edit(fields: &[Field; 6], titles: &[Field], track: usize) -> Tags {
    let mut edit = Tags::default();
    for (field, value) in TAG_SHOW_FIELDS.iter().zip(fields.iter()) {
        edit.set(*field, value.edit_value());
    }
    edit.track_number = Some((track + 1).to_string());
    edit.title = titles.get(track).and_then(Field::edit_value);
    edit
}

/// Builds the write queue for the current edit, or `None` when there is nothing taggable
/// to change — the interactive equivalent of `cmd_tag`'s "nothing to change" early return.
/// Returns the rows to show immediately (covering every file, including the ones that
/// never get a job) alongside them, since a row's final status for `NotApplicable` and
/// `Unchanged` is already known without running anything.
pub(crate) fn start_tag_write(
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
) -> (
    Option<Queue<lh_core::Result<()>>>,
    Vec<usize>,
    Vec<WriteRow>,
) {
    let mut rows = Vec::with_capacity(files.len());
    let mut edits = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let edit = tag_edit(fields, titles, i);
        let status = if !taggable[i] {
            WriteStatus::NotApplicable
        } else if before[i].changes(&edit).is_empty() {
            WriteStatus::Unchanged
        } else {
            WriteStatus::Pending
        };
        rows.push(WriteRow {
            name: f.file_name(),
            status,
        });
        edits.push(edit);
    }

    if !rows
        .iter()
        .any(|r| matches!(r.status, WriteStatus::Pending))
    {
        return (None, Vec::new(), rows);
    }

    let queue: Queue<lh_core::Result<()>> = Queue::new();
    let mut submitted = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if !matches!(rows[i].status, WriteStatus::Pending) {
            continue;
        }
        let path = f.path.clone();
        let edit = edits[i].clone();
        queue.submit(f.file_name(), move |_progress| -> lh_core::Result<()> {
            let audio_before = ffp(&path)?;
            tag::apply(&path, &edit)?;
            tag::assert_audio_unchanged(&path, audio_before)?;
            Ok(())
        });
        submitted.push(i);
    }
    (Some(queue), submitted, rows)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_tag_screen(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    files: Vec<AudioFile>,
    before: Vec<Tags>,
    show_seed: Tags,
    title_seed: Vec<String>,
    theme: Theme,
) -> io::Result<bool> {
    let taggable: Vec<bool> = files.iter().map(|f| tag::is_taggable(f.format)).collect();
    let mut fields: [Field; 6] =
        TAG_SHOW_FIELDS.map(|f| Field::new(show_seed.get(f).unwrap_or_default()));
    let mut titles: Vec<Field> = title_seed.into_iter().map(Field::new).collect();
    let mut focus = TagFocus::None;
    let mut title_idx = 0usize;

    let mut stage = TagStage::Editing;
    let mut rows: Vec<WriteRow> = files
        .iter()
        .map(|f| WriteRow {
            name: f.file_name(),
            status: WriteStatus::Pending,
        })
        .collect();
    let mut queue: Option<Queue<lh_core::Result<()>>> = None;
    let mut submitted_rows: Vec<usize> = Vec::new();
    let mut wrote = 0usize;
    let mut failed = 0usize;

    let mut tick = 0usize;

    loop {
        if let Some(q) = &queue {
            while let Ok(event) = q.events().try_recv() {
                match event {
                    Event::Started { id, .. } => {
                        rows[submitted_rows[id.index()]].status = WriteStatus::Running;
                    }
                    Event::Progress { .. } => {}
                    Event::Finished { id, output, .. } => {
                        let row = submitted_rows[id.index()];
                        rows[row].status = match output {
                            Ok(()) => {
                                wrote += 1;
                                WriteStatus::Ok
                            }
                            Err(e) => {
                                failed += 1;
                                WriteStatus::Failed(e.to_string())
                            }
                        };
                    }
                    Event::Cancelled { id, .. } => {
                        failed += 1;
                        rows[submitted_rows[id.index()]].status =
                            WriteStatus::Failed("cancelled".to_string());
                    }
                }
            }
            if matches!(stage, TagStage::Writing)
                && rows
                    .iter()
                    .all(|r| !matches!(r.status, WriteStatus::Pending | WriteStatus::Running))
            {
                stage = TagStage::Done;
            }
        }

        terminal.draw(|frame| {
            draw_tag(
                frame, dir, &files, &taggable, &before, &fields, &titles, focus, title_idx, &rows,
                &stage, wrote, failed, tick, &theme,
            )
        })?;

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        if let Some(q) = &queue {
                            q.cancel_token().cancel();
                        }
                        break;
                    }
                    match stage {
                        TagStage::Editing => match key.code {
                            KeyCode::Esc => focus = TagFocus::None,
                            KeyCode::Char('q') if focus == TagFocus::None => break,
                            KeyCode::Char('a') if focus == TagFocus::None => {
                                let (q, subs, initial_rows) =
                                    start_tag_write(&files, &taggable, &before, &fields, &titles);
                                rows = initial_rows;
                                submitted_rows = subs;
                                stage = if q.is_some() {
                                    TagStage::Writing
                                } else {
                                    TagStage::Done
                                };
                                queue = q;
                            }
                            KeyCode::Char('e') if focus == TagFocus::None => {
                                focus = TagFocus::Titles;
                                title_idx = title_idx.min(titles.len().saturating_sub(1));
                            }
                            KeyCode::Tab => {
                                focus = match focus {
                                    TagFocus::None | TagFocus::Titles => TagFocus::Show(0),
                                    TagFocus::Show(i) => {
                                        TagFocus::Show((i + 1) % TAG_SHOW_FIELDS.len())
                                    }
                                };
                            }
                            KeyCode::BackTab => {
                                focus = match focus {
                                    TagFocus::None | TagFocus::Titles => {
                                        TagFocus::Show(TAG_SHOW_FIELDS.len() - 1)
                                    }
                                    TagFocus::Show(0) => TagFocus::Show(TAG_SHOW_FIELDS.len() - 1),
                                    TagFocus::Show(i) => TagFocus::Show(i - 1),
                                };
                            }
                            other => match focus {
                                TagFocus::Show(i) => {
                                    fields[i].on_key(other);
                                }
                                TagFocus::Titles => match other {
                                    KeyCode::Up => title_idx = title_idx.saturating_sub(1),
                                    KeyCode::Down => {
                                        title_idx =
                                            (title_idx + 1).min(titles.len().saturating_sub(1));
                                    }
                                    _ => {
                                        if let Some(t) = titles.get_mut(title_idx) {
                                            t.on_key(other);
                                        }
                                    }
                                },
                                TagFocus::None => {}
                            },
                        },
                        TagStage::Writing | TagStage::Done => {
                            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                                if let Some(q) = &queue {
                                    q.cancel_token().cancel();
                                }
                                break;
                            }
                        }
                    }
                }
                CtEvent::Paste(text)
                    if matches!(stage, TagStage::Editing) && focus == TagFocus::Titles =>
                {
                    titles = paste_titles(&text, files.len());
                    title_idx = title_idx.min(titles.len().saturating_sub(1));
                }
                _ => {}
            }
        }
        tick = tick.wrapping_add(1);
    }
    Ok(failed == 0)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tag(
    frame: &mut Frame,
    dir: &Path,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    rows: &[WriteRow],
    stage: &TagStage,
    wrote: usize,
    failed: usize,
    tick: usize,
    theme: &Theme,
) {
    let area = frame.area();
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = match stage {
        TagStage::Editing => "tag",
        TagStage::Writing => "tag (writing)",
        TagStage::Done => "tag (done)",
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(format!(" {mode}  ")),
        Span::styled(dir.display().to_string(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        outer[0],
    );

    match stage {
        TagStage::Editing => draw_tag_editor(
            frame, outer[1], files, taggable, before, fields, titles, focus, title_idx, theme,
        ),
        TagStage::Writing | TagStage::Done => draw_write_table(frame, outer[1], rows, tick, theme),
    }

    draw_write_gauge(frame, outer[2], rows, stage, wrote, failed, theme);

    let footer = match (stage, focus) {
        (TagStage::Editing, TagFocus::None) => " tab fields   e titles   a apply   q/esc quit ",
        (TagStage::Editing, TagFocus::Titles) => {
            " ↑/↓ select line   paste replaces all   esc leave "
        }
        (TagStage::Editing, _) => " esc leave field   type to edit ",
        _ => " q/esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), outer[3]);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tag_editor(
    frame: &mut Frame,
    area: Rect,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    theme: &Theme,
) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(area);
    let left = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5)])
        .split(cols[0]);

    draw_show_fields(frame, left[0], fields, focus, theme);
    draw_titles(frame, left[1], titles, focus, title_idx, theme);
    draw_tag_diff(
        frame, cols[1], files, taggable, before, fields, titles, theme,
    );
}

pub(crate) fn draw_show_fields(
    frame: &mut Frame,
    area: Rect,
    fields: &[Field; 6],
    focus: TagFocus,
    theme: &Theme,
) {
    let lines: Vec<Line> = TAG_SHOW_FIELDS
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let focused = focus == TagFocus::Show(i);
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{:<11}", f.key()), theme.header),
                Span::styled(fields[i].display(focused), style),
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" show ")
                .border_style(theme.dim),
        ),
        area,
    );
}

pub(crate) fn draw_titles(
    frame: &mut Frame,
    area: Rect,
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    theme: &Theme,
) {
    let items: Vec<ListItem> = titles
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let focused = focus == TagFocus::Titles && i == title_idx;
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            let marker = if focused { ">" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{:>3}  ", i + 1), theme.dim),
                Span::styled(t.display(focused), style),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if focus == TagFocus::Titles {
        state.select(Some(title_idx));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" titles (e to edit, paste to replace) ")
            .border_style(theme.dim),
    );
    frame.render_stateful_widget(list, area, &mut state);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tag_diff(
    frame: &mut Frame,
    area: Rect,
    files: &[AudioFile],
    taggable: &[bool],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    theme: &Theme,
) {
    let table_rows = files.iter().enumerate().map(|(i, f)| {
        if !taggable[i] {
            return Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("N/A").style(theme.dim),
                Cell::from("no Vorbis comments").style(theme.dim),
            ]);
        }
        let edit = tag_edit(fields, titles, i);
        let changes = before[i].changes(&edit);
        if changes.is_empty() {
            Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("unchanged").style(theme.dim),
                Cell::from(""),
            ])
        } else {
            let detail = changes
                .iter()
                .map(|(field, old, new)| {
                    format!("{}: {:?} -> {new:?}", field.key(), old.unwrap_or(""))
                })
                .collect::<Vec<_>>()
                .join(", ");
            Row::new(vec![
                Cell::from(f.file_name()),
                Cell::from("changed").style(theme.accent),
                Cell::from(detail),
            ])
        }
    });

    let table = Table::new(
        table_rows,
        [
            Constraint::Percentage(30),
            Constraint::Length(10),
            Constraint::Percentage(60),
        ],
    )
    .header(Row::new(vec!["file", "status", "changes"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" diff ")
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

/// Shared by the tag and rename apply stages: both settle into "one row per file, status
/// going Pending → Running → OK/FAILED" (docs/tagging.md §6), just with a different
/// `WriteStatus` producer behind it.
pub(crate) fn draw_write_table(
    frame: &mut Frame,
    area: Rect,
    rows: &[WriteRow],
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let table_rows = rows.iter().map(|row| {
        let (label, style, detail) = write_row_cells(&row.status, spin, theme);
        Row::new(vec![
            Cell::from(label).style(style),
            Cell::from(row.name.clone()),
            Cell::from(detail).style(theme.dim),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(45),
            Constraint::Percentage(45),
        ],
    )
    .header(Row::new(vec!["status", "file", "detail"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" files ")
            .border_style(theme.dim),
    );
    frame.render_widget(table, area);
}

pub(crate) fn write_row_cells(
    status: &WriteStatus,
    spin: char,
    theme: &Theme,
) -> (String, Style, String) {
    match status {
        WriteStatus::Pending => ("pending".to_string(), theme.dim, String::new()),
        WriteStatus::Running => (format!("{spin} running"), theme.accent, String::new()),
        WriteStatus::NotApplicable => (
            "N/A".to_string(),
            theme.dim,
            "no Vorbis comments".to_string(),
        ),
        WriteStatus::Unchanged => ("unchanged".to_string(), theme.dim, String::new()),
        WriteStatus::Ok => ("OK".to_string(), theme.ok, String::new()),
        WriteStatus::Failed(e) => ("FAILED".to_string(), theme.error, e.clone()),
    }
}

pub(crate) fn draw_write_gauge(
    frame: &mut Frame,
    area: Rect,
    rows: &[WriteRow],
    stage: &TagStage,
    wrote: usize,
    failed: usize,
    theme: &Theme,
) {
    let total = rows.len();
    let (ratio, style, label) = match stage {
        TagStage::Editing => (
            0.0,
            theme.accent,
            format!("{total} files — press a to write"),
        ),
        _ => {
            let done = rows
                .iter()
                .filter(|r| !matches!(r.status, WriteStatus::Pending | WriteStatus::Running))
                .count();
            let ratio = if total == 0 {
                1.0
            } else {
                done as f64 / total as f64
            };
            let style = if failed > 0 {
                theme.error
            } else if done == total {
                theme.ok
            } else {
                theme.accent
            };
            (
                ratio,
                style,
                format!("{done}/{total} wrote:{wrote} failed:{failed}"),
            )
        }
    };
    draw_gauge(frame, area, ratio, style, label, theme);
}
