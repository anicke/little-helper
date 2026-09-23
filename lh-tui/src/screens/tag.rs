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
use lh_core::tag::{self, Tags};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};
use std::path::Path;

// --- Tag ---------------------------------------------------------------------------------

/// The six show-level fields a person edits directly, in the order shown on screen. The
/// other three of `tag::Field::ALL` are handled separately: `TITLE` is per-track (the
/// titles pane below), `TRACKNUMBER` and `TRACKTOTAL` are always derived from position and
/// count and have no field at all (docs/tagging.md §5).
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

/// The screen opens on `Viewing` — what the files carry now — and the person chooses to
/// edit from there. A finished write lands back in `Viewing`, showing the result, so
/// another round of edits is one `e` away rather than a relaunch.
pub(crate) enum TagStage {
    Viewing,
    Editing,
    Writing,
}

#[derive(Clone)]
pub(crate) enum WriteStatus {
    Pending,
    Running,
    Unchanged,
    Ok,
    Failed(String),
}

pub(crate) struct WriteRow {
    name: String,
    status: WriteStatus,
    /// What the file carries: the edit applied over its old tags while the write is
    /// pending, then what a fresh read found once it has landed. So the done screen shows
    /// the file, not the plan.
    tags: Tags,
}

/// Everything the tag screen opens with: the files, their tags as they stand, and the
/// seeded show fields and titles.
pub(crate) struct TagSetup {
    files: Vec<AudioFile>,
    /// How many files in the folder were left out for not being taggable — only FLAC
    /// carries Vorbis comments, so a WAV next to its converted FLAC is not a track.
    ignored: usize,
    before: Vec<Tags>,
    show: Tags,
    titles: Vec<String>,
}

pub(crate) fn run_tag(args: TagArgs, theme: ThemeName) -> ExitCode {
    let folder = match scan_folder(&args.dir) {
        Ok(f) => f,
        Err(refusal) => return refusal.exit(),
    };
    for line in &folder.skipped {
        eprintln!("{line}");
    }
    let setup = match prepare_tag(&args, folder.files) {
        Ok(s) => s,
        Err(refusal) => return refusal.exit(),
    };

    let result = {
        let mut terminal = TerminalGuard::with_paste();
        tag_screen(&mut terminal, &args.dir, setup, Theme::new(theme))
    };

    match result {
        // Leaving without applying anything is not a failure, same as today's `$?`.
        Ok(ok) => {
            if ok.unwrap_or(true) {
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

/// Reads every file's tags and seeds the edit from them, the folder name and `args` —
/// everything that can fail before the screen opens.
pub(crate) fn prepare_tag(args: &TagArgs, files: Vec<AudioFile>) -> Result<TagSetup, Refusal> {
    // Only taggable files are tracks: numbering, titles and the diff all run over them
    // alone, so a WAV left beside its converted FLAC doesn't shift every track number.
    let total = files.len();
    let files: Vec<AudioFile> = files
        .into_iter()
        .filter(|f| tag::is_taggable(f.format))
        .collect();
    let ignored = total - files.len();
    if files.is_empty() {
        return Err(Refusal::new(
            1,
            format!("no FLAC files to tag in {}", args.dir.display()),
        ));
    }

    // Read every file's existing tags up front — the "before" state the diff pane and
    // the apply stage's audio-unchanged postcondition are both built from
    // (docs/tagging.md §1 contract point 2).
    let mut before: Vec<Tags> = Vec::with_capacity(files.len());
    for f in &files {
        match tag::read(&f.path) {
            Ok(t) => before.push(t),
            Err(e) => {
                return Err(Refusal::new(
                    2,
                    format!("lh-tui: reading tags from {}: {e:#}", f.path.display()),
                ));
            }
        }
    }

    let show_name = args
        .dir
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ShowName::parse);

    // Seed the show-level fields from the first file that already carries any
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
            io::read_to_string(io::stdin())
                .map_err(|e| Refusal::new(2, format!("lh-tui: reading titles from stdin: {e}")))?
        } else {
            std::fs::read_to_string(path)
                .map_err(|e| Refusal::new(2, format!("lh-tui: reading {}: {e}", path.display())))?
        };
        let lines: Vec<String> = text.lines().map(str::to_string).collect();
        if lines.len() != files.len() {
            return Err(Refusal::new(
                2,
                format!(
                    "lh-tui: {} titles given but {} FLAC files in {}",
                    lines.len(),
                    files.len(),
                    args.dir.display()
                ),
            ));
        }
        titles = lines;
    }

    Ok(TagSetup {
        files,
        ignored,
        before,
        show,
        titles,
    })
}

/// `None` when the person left without writing; otherwise whether every write landed.
pub(crate) fn tag_screen(
    terminal: &mut DefaultTerminal,
    dir: &Path,
    setup: TagSetup,
    theme: Theme,
) -> io::Result<Option<bool>> {
    run_tag_screen(
        terminal,
        dir,
        setup.files,
        setup.ignored,
        setup.before,
        setup.show,
        setup.titles,
        theme,
    )
}

pub(crate) fn tag_edit(fields: &[Field; 6], titles: &[Field], track: usize, total: usize) -> Tags {
    let mut edit = Tags::default();
    for (field, value) in TAG_SHOW_FIELDS.iter().zip(fields.iter()) {
        edit.set(*field, value.edit_value());
    }
    edit.track_number = Some((track + 1).to_string());
    edit.track_total = Some(total.to_string());
    edit.title = titles.get(track).and_then(Field::edit_value);
    edit
}

/// Builds the write queue for the current edit, or `None` when there is nothing to
/// change — the interactive equivalent of `cmd_tag`'s "nothing to change" early return.
/// Returns the rows to show immediately (covering every file, including the ones that
/// never get a job) alongside them, since an `Unchanged` row's final status is already
/// known without running anything.
pub(crate) fn start_tag_write(
    files: &[AudioFile],
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
) -> (
    Option<Queue<lh_core::Result<Tags>>>,
    Vec<usize>,
    Vec<WriteRow>,
) {
    let mut rows = Vec::with_capacity(files.len());
    let mut edits = Vec::with_capacity(files.len());
    for (i, f) in files.iter().enumerate() {
        let edit = tag_edit(fields, titles, i, files.len());
        let status = if before[i].changes(&edit).is_empty() {
            WriteStatus::Unchanged
        } else {
            WriteStatus::Pending
        };
        rows.push(WriteRow {
            name: f.file_name(),
            status,
            tags: before[i].applied(&edit),
        });
        edits.push(edit);
    }

    if !rows
        .iter()
        .any(|r| matches!(r.status, WriteStatus::Pending))
    {
        return (None, Vec::new(), rows);
    }

    let queue: Queue<lh_core::Result<Tags>> = Queue::new();
    let mut submitted = Vec::new();
    for (i, f) in files.iter().enumerate() {
        if !matches!(rows[i].status, WriteStatus::Pending) {
            continue;
        }
        let path = f.path.clone();
        let edit = edits[i].clone();
        queue.submit(f.file_name(), move |_progress| -> lh_core::Result<Tags> {
            let audio_before = ffp(&path)?;
            tag::apply(&path, &edit)?;
            tag::assert_audio_unchanged(&path, audio_before)?;
            tag::read(&path)
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
    ignored: usize,
    mut before: Vec<Tags>,
    show_seed: Tags,
    title_seed: Vec<String>,
    theme: Theme,
) -> io::Result<Option<bool>> {
    let mut fields: [Field; 6] =
        TAG_SHOW_FIELDS.map(|f| Field::new(show_seed.get(f).unwrap_or_default()));
    let mut titles: Vec<Field> = title_seed.into_iter().map(Field::new).collect();
    let mut focus = TagFocus::None;
    let mut title_idx = 0usize;

    let mut stage = TagStage::Viewing;
    let mut rows: Vec<WriteRow> = files
        .iter()
        .zip(&before)
        .map(|(f, tags)| WriteRow {
            name: f.file_name(),
            status: WriteStatus::Pending,
            tags: tags.clone(),
        })
        .collect();
    let mut queue: Option<Queue<lh_core::Result<Tags>>> = None;
    let mut submitted_rows: Vec<usize> = Vec::new();
    let mut wrote = 0usize;
    let mut failed = 0usize;
    // `None` until a write has run; then whether every write so far landed.
    let mut applied: Option<bool> = None;

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
                            Ok(tags) => {
                                wrote += 1;
                                rows[row].tags = tags;
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
                // The files are the new "before": the next edit diffs against what they
                // carry now. A failed row shows what is actually on disk, not the plan.
                for &i in &submitted_rows {
                    if !matches!(rows[i].status, WriteStatus::Ok) {
                        rows[i].tags =
                            tag::read(&files[i].path).unwrap_or_else(|_| before[i].clone());
                    }
                    before[i] = rows[i].tags.clone();
                }
                applied = Some(applied.unwrap_or(true) && failed == 0);
                queue = None;
                stage = TagStage::Viewing;
            }
        }

        terminal.draw(|frame| {
            draw_tag(
                frame,
                dir,
                &files,
                ignored,
                &before,
                &fields,
                &titles,
                focus,
                title_idx,
                &rows,
                &stage,
                applied.is_some(),
                wrote,
                failed,
                tick,
                &theme,
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
                        TagStage::Viewing => match key.code {
                            KeyCode::Char('e') => stage = TagStage::Editing,
                            KeyCode::Char('q') | KeyCode::Esc => break,
                            _ => {}
                        },
                        TagStage::Editing => match key.code {
                            // Esc steps out one level: a field, then the editor. The edits
                            // are kept, so `e` picks up where this left off.
                            KeyCode::Esc if focus == TagFocus::None => stage = TagStage::Viewing,
                            KeyCode::Esc => focus = TagFocus::None,
                            KeyCode::Char('q') if focus == TagFocus::None => break,
                            KeyCode::Char('a') if focus == TagFocus::None => {
                                let (q, subs, initial_rows) =
                                    start_tag_write(&files, &before, &fields, &titles);
                                rows = initial_rows;
                                submitted_rows = subs;
                                wrote = 0;
                                failed = 0;
                                stage = if q.is_some() {
                                    TagStage::Writing
                                } else {
                                    applied = Some(applied.unwrap_or(true));
                                    TagStage::Viewing
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
                        TagStage::Writing => {
                            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                                if let Some(q) = &queue {
                                    q.cancel_token().cancel();
                                }
                                break;
                            }
                        }
                    }
                }
                // Pasting titles is the common reason to edit at all, so it works straight
                // from the overview too and opens the editor on the result.
                CtEvent::Paste(text)
                    if matches!(stage, TagStage::Viewing)
                        || (matches!(stage, TagStage::Editing)
                            && matches!(focus, TagFocus::None | TagFocus::Titles)) =>
                {
                    stage = TagStage::Editing;
                    titles = paste_titles(&text, files.len());
                    focus = TagFocus::Titles;
                    title_idx = title_idx.min(titles.len().saturating_sub(1));
                }
                _ => {}
            }
        }
        tick = tick.wrapping_add(1);
    }
    if matches!(stage, TagStage::Writing) {
        applied = Some(applied.unwrap_or(true) && failed == 0);
    }
    Ok(applied)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tag(
    frame: &mut Frame,
    dir: &Path,
    files: &[AudioFile],
    ignored: usize,
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    focus: TagFocus,
    title_idx: usize,
    rows: &[WriteRow],
    stage: &TagStage,
    applied: bool,
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
        TagStage::Viewing if applied => "tag (done)",
        TagStage::Viewing => "tag",
        TagStage::Editing => "tag (editing)",
        TagStage::Writing => "tag (writing)",
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
            frame, outer[1], files, ignored, before, fields, titles, focus, title_idx, theme,
        ),
        TagStage::Viewing | TagStage::Writing => {
            let show_status = applied || matches!(stage, TagStage::Writing);
            draw_tag_result(frame, outer[1], rows, ignored, show_status, tick, theme)
        }
    }

    draw_write_gauge(frame, outer[2], rows, stage, applied, wrote, failed, theme);

    let footer = match (stage, focus) {
        (TagStage::Viewing, _) => " e edit   paste titles   q/esc quit ",
        (TagStage::Editing, TagFocus::None) => {
            " tab fields   e titles   a apply   esc back   q quit "
        }
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
    ignored: usize,
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
        frame, cols[1], files, ignored, before, fields, titles, theme,
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
    ignored: usize,
    before: &[Tags],
    fields: &[Field; 6],
    titles: &[Field],
    theme: &Theme,
) {
    // Sized to the content so the file names are never clipped; the changes column takes
    // whatever is left.
    let number_width = format!("{0}/{0}", files.len()).len() as u16;
    let name_width = files
        .iter()
        .map(|f| f.file_name().chars().count())
        .max()
        .unwrap_or(0)
        .max("file".len()) as u16;
    let table_rows = files.iter().enumerate().map(|(i, f)| {
        let edit = tag_edit(fields, titles, i, files.len());
        let changes = before[i].changes(&edit);
        // The number and total this file will carry, always shown: both are derived and
        // never typed (docs/tagging.md §5), so this column is the only place to see them
        // when they already match. Accented when writing either would change the file.
        let number_style = if changes
            .iter()
            .any(|(f, ..)| matches!(f, tag::Field::TrackNumber | tag::Field::TrackTotal))
        {
            theme.accent
        } else {
            theme.dim
        };
        let number = Cell::from(format!(
            "{}/{}",
            edit.track_number.as_deref().unwrap_or_default(),
            edit.track_total.as_deref().unwrap_or_default()
        ))
        .style(number_style);
        if changes.is_empty() {
            Row::new(vec![
                number,
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
                number,
                Cell::from(f.file_name()),
                Cell::from("changed").style(theme.accent),
                Cell::from(detail),
            ])
        }
    });

    // Say what was left out rather than dropping it silently (Principle 5).
    let title = match ignored {
        0 => " diff ".to_string(),
        1 => " diff — 1 non-FLAC file not tagged ".to_string(),
        n => format!(" diff — {n} non-FLAC files not tagged "),
    };
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(number_width),
            Constraint::Length(name_width),
            Constraint::Length(10),
            Constraint::Min(0),
        ],
    )
    .header(Row::new(vec!["#", "file", "status", "changes"]).style(theme.header))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(theme.dim),
    );

    frame.render_widget(table, area);
}

/// What every file carries now, one row per file — the screen's opening overview, and
/// during and after a write the same table with a status column going
/// Pending → Running → OK/FAILED (docs/tagging.md §6). The show-level fields are the same
/// on every file, so they sit once in a pane above rather than repeating down the table;
/// a field whose value differs between files says so instead of picking one.
pub(crate) fn draw_tag_result(
    frame: &mut Frame,
    area: Rect,
    rows: &[WriteRow],
    ignored: usize,
    show_status: bool,
    tick: usize,
    theme: &Theme,
) {
    let split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(TAG_SHOW_FIELDS.len() as u16 + 2),
            Constraint::Min(3),
        ])
        .split(area);

    let show_lines: Vec<Line> = TAG_SHOW_FIELDS
        .iter()
        .map(|&field| {
            let first = rows.first().and_then(|r| r.tags.get(field));
            let value = if rows.iter().all(|r| r.tags.get(field) == first) {
                Span::raw(first.unwrap_or_default().to_string())
            } else {
                Span::styled("(differs between files)", theme.dim)
            };
            Line::from(vec![
                Span::styled(format!("{:<11}", field.key()), theme.dim),
                value,
            ])
        })
        .collect();
    frame.render_widget(
        Paragraph::new(show_lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" show ")
                .border_style(theme.dim),
        ),
        split[0],
    );

    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let number = |t: &Tags| {
        format!(
            "{}/{}",
            t.get(tag::Field::TrackNumber).unwrap_or("–"),
            t.get(tag::Field::TrackTotal).unwrap_or("–")
        )
    };
    let number_width = rows
        .iter()
        .map(|r| number(&r.tags).chars().count())
        .max()
        .unwrap_or(0)
        .max(1) as u16;
    let title_width = rows
        .iter()
        .map(|r| r.tags.title.as_deref().unwrap_or_default().chars().count())
        .max()
        .unwrap_or(0)
        .clamp("title".len(), 48) as u16;
    let name_width = rows
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("file".len()) as u16;
    let table_rows = rows.iter().map(|row| {
        let (label, style, detail) = write_row_cells(&row.status, spin, theme);
        let mut cells = vec![
            Cell::from(number(&row.tags)),
            Cell::from(row.tags.title.clone().unwrap_or_default()),
            Cell::from(row.name.clone()).style(theme.dim),
        ];
        if show_status {
            cells.insert(0, Cell::from(label).style(style));
            cells.push(Cell::from(detail).style(theme.error));
        }
        Row::new(cells)
    });
    let mut widths = vec![
        Constraint::Length(number_width),
        Constraint::Length(title_width),
        Constraint::Length(name_width),
    ];
    let mut header = vec!["#", "title", "file"];
    if show_status {
        widths.insert(0, Constraint::Length(10));
        widths.push(Constraint::Min(0));
        header.insert(0, "status");
        header.push("detail");
    }
    // Say what was left out rather than dropping it silently (Principle 5).
    let title = match ignored {
        0 => " files ".to_string(),
        1 => " files — 1 non-FLAC file not tagged ".to_string(),
        n => format!(" files — {n} non-FLAC files not tagged "),
    };
    let table = Table::new(table_rows, widths)
        .header(Row::new(header).style(theme.header))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(theme.dim),
        );
    frame.render_widget(table, split[1]);
}

pub(crate) fn write_row_cells(
    status: &WriteStatus,
    spin: char,
    theme: &Theme,
) -> (String, Style, String) {
    match status {
        WriteStatus::Pending => ("pending".to_string(), theme.dim, String::new()),
        WriteStatus::Running => (format!("{spin} running"), theme.accent, String::new()),
        WriteStatus::Unchanged => ("unchanged".to_string(), theme.dim, String::new()),
        WriteStatus::Ok => ("OK".to_string(), theme.ok, String::new()),
        WriteStatus::Failed(e) => ("FAILED".to_string(), theme.error, e.clone()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_write_gauge(
    frame: &mut Frame,
    area: Rect,
    rows: &[WriteRow],
    stage: &TagStage,
    applied: bool,
    wrote: usize,
    failed: usize,
    theme: &Theme,
) {
    let total = rows.len();
    let (ratio, style, label) = match stage {
        TagStage::Viewing if !applied => (
            0.0,
            theme.accent,
            format!("{total} files — press e to edit"),
        ),
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
