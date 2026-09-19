use std::io;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEventKind, KeyModifiers};
use lh_cli::TorrentCreateArgs;
use lh_core::display;
use lh_core::job::{CancelToken, Event, Queue};
use lh_core::torrent::{
    CreateOpts, Created, Passkeys, PreviewFile, Resolved, Tracker, TrackerList, create,
    default_output, preview, resolve,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, List, ListItem, ListState, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::path::Path;

// --- Torrent create -----------------------------------------------------------------
//
// Unlike verify/checksum, `create` walks the whole payload as one sequential
// piece-hashing pass, not a batch of independent files (`docs/tui.md` §4) — so there is one
// job on a queue of one, and one row of progress to show, not a table. Its progress
// callback returns a `bool` the same way `lh-cli`'s own `cmd_torrent_create` uses it
// (`lh-cli/src/commands/torrent.rs`): `false` stops the hash within one piece, so `q`/`Esc`/`Ctrl-C`
// here waits for the job's own `Done` rather than breaking the draw loop immediately the
// way verify/checksum do — the wait is bounded by a single piece's hash time, and waiting
// for it means the screen reports what actually happened (cancelled vs. finished) instead
// of guessing.

pub(crate) enum CreateStage {
    /// Picking which trackers to announce to, before anything about the payload is
    /// touched — pre-selected from `--tracker` if any were given, but always confirmed
    /// on screen rather than assumed (TUI9: a TUI whose only tracker input is a
    /// command-line flag isn't much of a TUI).
    ChoosingTrackers,
    /// Walking the payload — after trackers are confirmed, before the first `Progress`
    /// event.
    Preparing,
    Hashing {
        done: u32,
        total: u32,
    },
    Done(Box<lh_core::Result<Created>>),
}

/// Which widget owns keystrokes on the tracker-picker stage. `None` is "nothing is being
/// edited," the same convention `TagFocus::None` uses on the tag screen, so a stray
/// `a`/`q` on the list itself confirms or quits rather than being typed into a field.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateFocus {
    None,
    List,
    Custom,
}

pub(crate) fn run_torrent_create(args: TorrentCreateArgs, theme: ThemeName) -> ExitCode {
    let source = match args.path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("lh-tui: reading {}: {e}", args.path.display());
            return ExitCode::from(2);
        }
    };

    // Tracker resolution (`lh_core::torrent::resolve`, same as `cmd_torrent_create`) now
    // happens on the picker stage inside the screen itself, seeded from `--tracker` —
    // an unknown id or a broken entry surfaces there as an inline error to fix, rather
    // than a preflight exit, since the picker gives a way to fix it without restarting.
    let list = match TrackerList::load() {
        Ok(l) => l,
        Err(e) => {
            eprintln!("lh-tui: reading the tracker list: {e:#}");
            return ExitCode::from(2);
        }
    };
    let keys = match Passkeys::load() {
        Ok(k) => k,
        Err(e) => {
            eprintln!("lh-tui: reading the passkey list: {e:#}");
            return ExitCode::from(2);
        }
    };

    let dst = match &args.output {
        Some(o) => o.clone(),
        None => match default_output(&source) {
            Some(p) => p,
            None => {
                eprintln!(
                    "lh-tui: {} has no parent directory to write a torrent beside",
                    source.display()
                );
                return ExitCode::from(2);
            }
        },
    };
    // Writing the .torrent inside the folder it describes adds a file to that folder, so
    // re-creating it later would produce a different infohash.
    if source.is_dir() && dst.parent().is_some_and(|p| p.starts_with(&source)) {
        eprintln!(
            "warning: writing the torrent inside {} means re-creating it later will not \
             produce the same infohash",
            source.display()
        );
    }

    let terminal = ratatui::init();
    let result = run_torrent_create_screen(
        terminal,
        &source,
        &dst,
        &args,
        &list,
        &keys,
        Theme::new(theme),
    );
    ratatui::restore();

    match result {
        Ok(Ok(made)) => {
            eprintln!("wrote {}", made.path.display());
            ExitCode::SUCCESS
        }
        Ok(Err(lh_core::Error::Cancelled)) => {
            eprintln!("cancelled before writing a torrent");
            ExitCode::from(1)
        }
        Ok(Err(e)) => {
            eprintln!("lh-tui: creating a torrent for {}: {e:#}", source.display());
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// Turns a confirmed tracker pick into the running job: resolves `picked` against the
/// list, builds `CreateOpts`, and starts the queue. On success returns what the rest of
/// the screen needs to show and to keep resolving further edits against.
pub(crate) struct Started {
    queue: Queue<lh_core::Result<Created>>,
    cancel: CancelToken,
}

pub(crate) fn start_create(
    source: &Path,
    dst: &Path,
    args: &TorrentCreateArgs,
    picked: &[String],
    list: &TrackerList,
    keys: &Passkeys,
) -> lh_core::Result<(Started, Resolved, bool, Option<String>)> {
    let chosen = resolve(picked, list, keys)?;
    let private = args.private || chosen.private;
    let source_tag = args.source.clone().or_else(|| chosen.source.clone());
    let opts = CreateOpts {
        announce: chosen.tiers.clone(),
        piece_length: args.piece_length,
        private,
        source: source_tag.clone(),
        comment: args.comment.clone(),
        include_all: args.include_all,
        overwrite: args.force,
        ..CreateOpts::default()
    };

    let queue: Queue<lh_core::Result<Created>> = Queue::with_workers(1);
    let cancel = queue.cancel_token();
    let job_source = source.to_path_buf();
    let job_dst = dst.to_path_buf();
    queue.submit("torrent create", move |progress| {
        create(&job_source, &job_dst, &opts, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });

    Ok((Started { queue, cancel }, chosen, private, source_tag))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_torrent_create_screen(
    mut terminal: DefaultTerminal,
    source: &Path,
    dst: &Path,
    args: &TorrentCreateArgs,
    list: &TrackerList,
    keys: &Passkeys,
    theme: Theme,
) -> io::Result<lh_core::Result<Created>> {
    let entries: Vec<Tracker> = list.iter().cloned().collect();
    let mut picked: Vec<String> = args.trackers.clone();
    let mut cursor = 0usize;
    let mut custom = Field::default();
    let mut focus = CreateFocus::None;
    let mut pick_error: Option<String> = None;

    // Walked once, up front: the tracker pick doesn't change what's on disk, so there is
    // nothing to re-walk for as the user picks. Shares `create`'s own file-collection logic
    // (`lh_core::torrent::preview`) so this can never show a list that `create` would then
    // include or exclude differently.
    let (preview_files, preview_excluded, preview_error): (
        Vec<PreviewFile>,
        usize,
        Option<String>,
    ) = match preview(source, args.include_all) {
        Ok(p) => (p.files, p.excluded.len(), None),
        Err(e) => (Vec::new(), 0, Some(format!("{e:#}"))),
    };

    let mut stage = CreateStage::ChoosingTrackers;
    let mut started: Option<Started> = None;
    let mut chosen = Resolved::default();
    let mut private = false;
    let mut source_tag: Option<String> = None;

    let mut start = Instant::now();
    let mut finished_at = None;
    let mut tick = 0usize;
    let mut want_quit = false;

    loop {
        if let Some(s) = &started {
            while let Ok(event) = s.queue.events().try_recv() {
                match event {
                    Event::Started { .. } => {}
                    Event::Progress { done, total, .. } => {
                        stage = CreateStage::Hashing { done, total };
                    }
                    Event::Finished { output, .. } => stage = CreateStage::Done(Box::new(output)),
                    Event::Cancelled { .. } => {
                        stage = CreateStage::Done(Box::new(Err(lh_core::Error::Cancelled)));
                    }
                }
            }
        }

        // Picking trackers is the user thinking, not the tool working — the clock
        // starts once creation actually kicks off (below), not when this screen opens.
        let elapsed = if matches!(stage, CreateStage::ChoosingTrackers) {
            0.0
        } else {
            header_elapsed(
                start,
                &mut finished_at,
                matches!(stage, CreateStage::Done(_)),
            )
        };
        terminal.draw(|frame| {
            draw_torrent_create(
                frame,
                source,
                dst,
                &entries,
                &picked,
                cursor,
                &custom,
                focus,
                pick_error.as_deref(),
                &preview_files,
                preview_excluded,
                preview_error.as_deref(),
                &chosen,
                private,
                &source_tag,
                &stage,
                elapsed,
                tick,
                &theme,
            )
        })?;

        if event::poll(Duration::from_millis(80))? {
            match event::read()? {
                CtEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.code == KeyCode::Char('c')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    match &stage {
                        CreateStage::ChoosingTrackers => {
                            if ctrl_c
                                || (focus == CreateFocus::None
                                    && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
                            {
                                // Nothing has started yet — quitting here is immediate,
                                // not a cancel-and-wait.
                                break;
                            }
                            match focus {
                                CreateFocus::None => match key.code {
                                    KeyCode::Tab => focus = CreateFocus::List,
                                    KeyCode::BackTab => focus = CreateFocus::Custom,
                                    KeyCode::Char('a') | KeyCode::Enter => {
                                        match start_create(source, dst, args, &picked, list, keys) {
                                            Ok((s, c, p, t)) => {
                                                chosen = c;
                                                private = p;
                                                source_tag = t;
                                                started = Some(s);
                                                pick_error = None;
                                                stage = CreateStage::Preparing;
                                                start = Instant::now();
                                            }
                                            Err(e) => pick_error = Some(format!("{e:#}")),
                                        }
                                    }
                                    _ => {}
                                },
                                CreateFocus::List => match key.code {
                                    KeyCode::Esc => focus = CreateFocus::None,
                                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                                    KeyCode::Down => {
                                        cursor = (cursor + 1).min(entries.len().saturating_sub(1));
                                    }
                                    KeyCode::Tab => focus = CreateFocus::Custom,
                                    KeyCode::BackTab => focus = CreateFocus::None,
                                    KeyCode::Char(' ') => {
                                        if let Some(t) = entries.get(cursor) {
                                            match picked.iter().position(|p| *p == t.id) {
                                                Some(i) => {
                                                    picked.remove(i);
                                                }
                                                None => picked.push(t.id.clone()),
                                            }
                                            pick_error = None;
                                        }
                                    }
                                    _ => {}
                                },
                                CreateFocus::Custom => match key.code {
                                    KeyCode::Esc => focus = CreateFocus::None,
                                    KeyCode::Tab => focus = CreateFocus::None,
                                    KeyCode::BackTab => focus = CreateFocus::List,
                                    KeyCode::Enter => {
                                        for spec in custom.value.split(',') {
                                            let spec = spec.trim();
                                            if !spec.is_empty() {
                                                picked.push(spec.to_string());
                                            }
                                        }
                                        custom = Field::default();
                                        pick_error = None;
                                    }
                                    // An unknown id or a typo'd URL, once added, has no row
                                    // in the list to toggle back off — backspace on an
                                    // already-empty field pops the most recent pick instead,
                                    // the same "backspace removes the last chip" convention
                                    // a comma-separated tag input uses.
                                    KeyCode::Backspace if custom.value.is_empty() => {
                                        picked.pop();
                                        pick_error = None;
                                    }
                                    other => {
                                        custom.on_key(other);
                                    }
                                },
                            }
                        }
                        CreateStage::Preparing
                        | CreateStage::Hashing { .. }
                        | CreateStage::Done(_) => {
                            let quit =
                                ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc);
                            if quit {
                                if let Some(s) = &started {
                                    s.cancel.cancel();
                                }
                                want_quit = true;
                            }
                        }
                    }
                }
                CtEvent::Paste(text)
                    if matches!(stage, CreateStage::ChoosingTrackers)
                        && focus == CreateFocus::Custom =>
                {
                    for c in text.chars().filter(|c| *c != '\n' && *c != '\r') {
                        custom.on_key(KeyCode::Char(c));
                    }
                }
                _ => {}
            }
        }
        if want_quit && matches!(stage, CreateStage::Done(_)) {
            break;
        }
        tick = tick.wrapping_add(1);
    }

    match stage {
        CreateStage::Done(result) => Ok(*result),
        _ => Ok(Err(lh_core::Error::Cancelled)),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_torrent_create(
    frame: &mut Frame,
    source: &Path,
    dst: &Path,
    entries: &[Tracker],
    picked: &[String],
    cursor: usize,
    custom: &Field,
    focus: CreateFocus,
    pick_error: Option<&str>,
    preview_files: &[PreviewFile],
    preview_excluded: usize,
    preview_error: Option<&str>,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
    stage: &CreateStage,
    elapsed: f32,
    tick: usize,
    theme: &Theme,
) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(area);

    let mode = if matches!(stage, CreateStage::ChoosingTrackers) {
        " torrent create — trackers  "
    } else {
        " torrent create  "
    };
    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(mode),
        Span::styled(source.display().to_string(), theme.dim),
        Span::raw(format!("   {elapsed:.1}s")),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        chunks[0],
    );

    if matches!(stage, CreateStage::ChoosingTrackers) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(chunks[1]);
        draw_tracker_picker(
            frame, cols[0], entries, picked, cursor, custom, focus, theme,
        );
        draw_file_preview(
            frame,
            cols[1],
            preview_files,
            preview_excluded,
            preview_error,
            theme,
        );
        let status = match pick_error {
            Some(e) => Line::styled(format!(" {e}"), theme.error),
            None if picked.is_empty() => {
                Line::styled(" no trackers picked yet (a trackerless torrent)", theme.dim)
            }
            None => Line::styled(format!(" picked: {}", picked.join(", ")), theme.dim),
        };
        frame.render_widget(
            Paragraph::new(status).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(theme.dim),
            ),
            chunks[2],
        );
    } else {
        let lines = create_lines(source, dst, chosen, private, source_tag, stage, theme);
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" torrent ")
                    .border_style(theme.dim),
            ),
            chunks[1],
        );
        draw_create_gauge(frame, chunks[2], stage, tick, theme);
    }

    let footer = match (stage, focus) {
        (CreateStage::ChoosingTrackers, CreateFocus::None) => {
            " tab select   space toggle   a/enter start   q/esc quit "
        }
        (CreateStage::ChoosingTrackers, CreateFocus::List) => {
            " ↑/↓ move   space toggle   tab custom   esc back "
        }
        (CreateStage::ChoosingTrackers, CreateFocus::Custom) => {
            " type id or URL, comma-separated   enter add   backspace-on-empty removes last   esc back "
        }
        _ => " q/esc quit ",
    };
    frame.render_widget(Paragraph::new(Line::styled(footer, theme.dim)), chunks[3]);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_tracker_picker(
    frame: &mut Frame,
    area: Rect,
    entries: &[Tracker],
    picked: &[String],
    cursor: usize,
    custom: &Field,
    focus: CreateFocus,
    theme: &Theme,
) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let is_picked = picked.contains(&t.id);
            let focused = focus == CreateFocus::List && i == cursor;
            let style = if focused {
                theme.accent
            } else {
                Style::default()
            };
            let mark = if is_picked { "[x]" } else { "[ ]" };
            let marker = if focused { ">" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{marker}{mark} "), theme.dim),
                Span::styled(format!("{:<14}", t.id), style),
                Span::styled(format!("{:<26}", t.name), style),
                Span::styled(t.health.label(), theme.dim),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if focus == CreateFocus::List {
        state.select(Some(cursor));
    }
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" known trackers ")
            .border_style(theme.dim),
    );
    frame.render_stateful_widget(list, rows[0], &mut state);

    let custom_focused = focus == CreateFocus::Custom;
    let custom_line = Line::from(vec![
        Span::raw("add: "),
        Span::styled(
            custom.display(custom_focused),
            if custom_focused {
                theme.accent
            } else {
                Style::default()
            },
        ),
    ]);
    frame.render_widget(
        Paragraph::new(custom_line).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" custom id or URL (comma-separated) ")
                .border_style(theme.dim),
        ),
        rows[1],
    );
}

/// What the tracker picker sits next to: what `create` would actually put in the torrent,
/// walked once up front (`preview`) since the tracker pick doesn't change it.
pub(crate) fn draw_file_preview(
    frame: &mut Frame,
    area: Rect,
    files: &[PreviewFile],
    excluded: usize,
    error: Option<&str>,
    theme: &Theme,
) {
    if let Some(e) = error {
        frame.render_widget(
            Paragraph::new(Line::styled(format!(" {e}"), theme.error)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" files ")
                    .border_style(theme.dim),
            ),
            area,
        );
        return;
    }

    let total: u64 = files.iter().map(|f| f.length).sum();
    let excluded_note = if excluded > 0 {
        format!(", {excluded} excluded")
    } else {
        String::new()
    };
    let title = format!(
        " files: {} ({}{excluded_note}) ",
        files.len(),
        display::bytes(total)
    );
    let items: Vec<ListItem> = files
        .iter()
        .map(|f| {
            ListItem::new(Line::from(vec![
                Span::raw(f.path.clone()),
                Span::styled(format!("  {}", display::bytes(f.length)), theme.dim),
            ]))
        })
        .collect();
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(theme.dim),
    );
    frame.render_widget(list, area);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn create_lines<'a>(
    source: &Path,
    dst: &Path,
    chosen: &Resolved,
    private: bool,
    source_tag: &Option<String>,
    stage: &CreateStage,
    theme: &'a Theme,
) -> Vec<Line<'a>> {
    let mut lines = vec![
        Line::from(format!("output     {}", dst.display())),
        Line::from(String::new()),
    ];
    if chosen.chosen.is_empty() {
        lines.push(Line::from("tracker    none (a trackerless torrent)"));
    }
    for (i, c) in chosen.chosen.iter().enumerate() {
        let label = if i == 0 { "tracker" } else { "" };
        let detail = match &c.tracker {
            Some(t) => format!("{}  {}  ({})", t.name, c.announce, t.health.label()),
            None => format!("{}  (given as a URL)", c.announce),
        };
        lines.push(Line::from(format!("{label:<10} {detail}")));
    }
    if private {
        lines.push(Line::from(
            "private    yes (BEP 27; part of the infohash)".to_string(),
        ));
    }
    if let Some(tag) = source_tag {
        lines.push(Line::from(format!(
            "source     {tag} (part of the infohash)"
        )));
    }
    lines.push(Line::from(String::new()));

    match stage {
        CreateStage::ChoosingTrackers => {
            unreachable!("draw_torrent_create only calls create_lines once trackers are chosen")
        }
        CreateStage::Preparing => {
            lines.push(Line::styled(
                format!("walking {}…", source.display()),
                theme.dim,
            ));
        }
        CreateStage::Hashing { .. } => {
            lines.push(Line::styled("hashing pieces…", theme.accent));
        }
        CreateStage::Done(result) => match result.as_ref() {
            Ok(made) => {
                lines.push(Line::styled(
                    format!("{} files", made.files.len()),
                    theme.ok,
                ));
                lines.push(Line::from(format!(
                    "size       {}",
                    display::bytes(made.total_length)
                )));
                lines.push(Line::from(format!(
                    "pieces     {} x {}",
                    made.pieces,
                    display::bytes(made.piece_length)
                )));
                lines.push(Line::from(format!("infohash   {}", made.info_hash_hex())));
                for (path, why) in &made.excluded {
                    let shown = path.strip_prefix(source).unwrap_or(path);
                    lines.push(Line::styled(
                        format!("excluded   {} ({})", shown.display(), why.reason()),
                        theme.warn,
                    ));
                }
            }
            Err(lh_core::Error::Cancelled) => {
                lines.push(Line::styled(
                    "cancelled before writing a torrent",
                    theme.warn,
                ));
            }
            Err(e) => {
                lines.push(Line::styled(format!("error: {e:#}"), theme.error));
            }
        },
    }
    lines
}

pub(crate) fn draw_create_gauge(
    frame: &mut Frame,
    area: Rect,
    stage: &CreateStage,
    tick: usize,
    theme: &Theme,
) {
    let spin = SPINNER[tick / 2 % SPINNER.len()];
    let (ratio, style, label) = match stage {
        CreateStage::ChoosingTrackers => {
            unreachable!(
                "draw_torrent_create only calls draw_create_gauge once trackers are chosen"
            )
        }
        CreateStage::Preparing => (0.0, theme.accent, format!("{spin} preparing")),
        CreateStage::Hashing { done, total } => {
            let ratio = if *total == 0 {
                0.0
            } else {
                f64::from(*done) / f64::from(*total)
            };
            (ratio, theme.accent, format!("{done}/{total} pieces"))
        }
        CreateStage::Done(result) => match result.as_ref() {
            Ok(_) => (1.0, theme.ok, "done".to_string()),
            Err(lh_core::Error::Cancelled) => (1.0, theme.warn, "cancelled".to_string()),
            Err(_) => (1.0, theme.error, "failed".to_string()),
        },
    };
    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        )
        .gauge_style(style)
        .ratio(ratio)
        .label(label);
    frame.render_widget(gauge, area);
}
