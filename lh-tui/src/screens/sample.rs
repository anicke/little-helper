use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use crate::*;
use crossterm::event::{self, Event as CtEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use lh_cli::SampleArgs;
use lh_core::job::{Event, Queue};
use lh_core::model::AudioFile;
use lh_core::sample::{
    self, Clip, Mode, Request, Sample, estimate_bytes, format_size, format_time, longest_fitting,
    parse_size, parse_time,
};
use lh_core::tag::{self, Field as TagField};
use lh_core::tools::Tool;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState};
use ratatui::{DefaultTerminal, Frame};

// --- Sample ------------------------------------------------------------------------------
//
// `lh sample` as a screen (docs/sample.md §4): pick a track, set where the clip starts, how
// long it is, the MP3 mode and the size limit, and encode it. Unlike `lh sample`, it takes
// the show folder. The size estimate is redrawn as the fields change, so the person sees a
// clip will not fit before asking for it. Like tag/rename, `q` only quits when no text
// field has focus. The one job runs on a `Queue`, so the screen keeps drawing and `q`
// can stop it.

pub(crate) fn run_sample(args: SampleArgs, theme: ThemeName) -> ExitCode {
    let (dir, preselect) = if args.path.is_dir() {
        (args.path.clone(), None)
    } else {
        let parent = args
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();
        (parent, Some(args.path.clone()))
    };
    let folder = match scan_folder(&dir) {
        Ok(f) => f,
        Err(refusal) => return refusal.exit(),
    };
    for line in &folder.skipped {
        eprintln!("{line}");
    }
    let encoder = match find_sample_encoder() {
        Ok(e) => e,
        Err(refusal) => return refusal.exit(),
    };
    let mut setup = SampleSetup::new(&folder.files, encoder);
    if let Some(file) = preselect {
        let wanted = file.canonicalize().ok();
        if let Some(i) = setup
            .tracks
            .iter()
            .position(|t| t.file.path.canonicalize().ok() == wanted)
        {
            setup.selected = i;
        }
    }
    setup.mode = args.mode;
    setup.max_bytes = args.max_size;
    setup.start = args.start;
    setup.length = args.length;
    setup.output = args.output;

    let result = {
        let mut terminal = TerminalGuard::new();
        sample_screen(
            &mut terminal,
            &dir.display().to_string(),
            setup,
            Theme::new(theme),
        )
    };
    match result {
        Ok(outcome) if outcome.failed.is_none() => ExitCode::SUCCESS,
        Ok(outcome) => {
            eprintln!("lh-tui: {}", outcome.failed.unwrap_or_default());
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("lh-tui: {e}");
            ExitCode::from(2)
        }
    }
}

/// `lame`, else `ffmpeg`, found before the screen opens so a missing encoder is said up
/// front rather than on the first `Enter`.
pub(crate) fn find_sample_encoder() -> Result<Tool, Refusal> {
    sample::find_encoder().map_err(|e| Refusal::new(2, format!("lh-tui: {e:#}")))
}

/// One track in the list: the file, its title from the tags when it has one, and its length.
pub(crate) struct Track {
    file: AudioFile,
    title: Option<String>,
    secs: Option<f64>,
}

/// What the screen opens with. `start`/`length`/`output` apply to the preselected track
/// only; moving to another track takes its own defaults.
pub(crate) struct SampleSetup {
    tracks: Vec<Track>,
    selected: usize,
    encoder: Tool,
    mode: Mode,
    max_bytes: u64,
    start: Option<f64>,
    length: Option<f64>,
    output: Option<PathBuf>,
}

impl SampleSetup {
    pub(crate) fn new(files: &[AudioFile], encoder: Tool) -> Self {
        let tracks = files
            .iter()
            .map(|f| Track {
                title: tag::read(&f.path)
                    .ok()
                    .and_then(|t| t.get(TagField::Title).map(str::to_string))
                    .filter(|t| !t.is_empty()),
                secs: f.stream_info.duration_secs(),
                file: f.clone(),
            })
            .collect();
        SampleSetup {
            tracks,
            selected: 0,
            encoder,
            mode: Mode::DEFAULT,
            max_bytes: sample::DEFAULT_MAX_BYTES,
            start: None,
            length: None,
            output: None,
        }
    }
}

/// What the person left with: every sample written, and the last attempt's error if the
/// last attempt failed.
pub(crate) struct SampleOutcome {
    pub(crate) written: Vec<PathBuf>,
    pub(crate) failed: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Tracks,
    Start,
    Length,
    Format,
    Limit,
}

impl Focus {
    const ORDER: [Focus; 5] = [
        Focus::Tracks,
        Focus::Start,
        Focus::Length,
        Focus::Format,
        Focus::Limit,
    ];

    fn step(self, forward: bool) -> Focus {
        let i = Self::ORDER.iter().position(|f| *f == self).unwrap_or(0);
        let n = Self::ORDER.len();
        Self::ORDER[if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        }]
    }
}

/// The line under the settings: what the last encode did, or is doing.
enum Status {
    Idle,
    Running {
        done: u32,
        total: u32,
        track: String,
    },
    Wrote(Sample),
    /// The output is already there; `F` replaces it.
    Exists(PathBuf),
    Failed(String),
    /// The fields do not make a clip; nothing was started.
    Invalid(String),
}

struct State {
    setup: SampleSetup,
    focus: Focus,
    start: Field,
    length: Field,
    limit: Field,
    mode: usize,
    /// Whether `setup.output` still applies: only to the track the screen opened on.
    output_for: Option<usize>,
    status: Status,
    written: Vec<PathBuf>,
}

/// The values the fields make, or which one does not parse.
struct Settings {
    clip: Result<Clip, &'static str>,
    max_bytes: Option<u64>,
    mode: Mode,
}

impl State {
    fn new(setup: SampleSetup) -> Self {
        let mode = Mode::PRESETS
            .iter()
            .position(|m| *m == setup.mode)
            .unwrap_or(0);
        let mut state = State {
            focus: Focus::Tracks,
            start: Field::default(),
            length: Field::default(),
            limit: Field::new(size_text(setup.max_bytes)),
            mode,
            output_for: setup.output.as_ref().map(|_| setup.selected),
            status: Status::Idle,
            written: Vec::new(),
            setup,
        };
        state.reset_clip();
        // `lh sample`'s rule: a length given alone is centred.
        if let Some(length) = state.setup.length {
            let clip = Clip::centred(state.track().secs, length);
            state.start = Field::new(format_time(clip.start));
            state.length = Field::new(secs_text(length));
        }
        if let Some(start) = state.setup.start {
            state.start = Field::new(format_time(start));
        }
        state
    }

    fn mode(&self) -> Mode {
        Mode::PRESETS[self.mode]
    }

    fn track(&self) -> &Track {
        &self.setup.tracks[self.setup.selected]
    }

    fn settings(&self) -> Settings {
        let start = parse_time(&self.start.value);
        let length = parse_time(&self.length.value).filter(|l| *l > 0.0);
        let clip = match (start, length) {
            (None, _) => Err("start is not a time like 1:30"),
            (_, None) => Err("length is not a time like 30 or 0:30"),
            (Some(start), Some(length)) => Ok(Clip { start, length }),
        };
        Settings {
            clip,
            max_bytes: parse_size(&self.limit.value),
            mode: self.mode(),
        }
    }

    /// The selected track's default clip at the current mode and limit.
    fn reset_clip(&mut self) {
        let max = parse_size(&self.limit.value).unwrap_or(sample::DEFAULT_MAX_BYTES);
        let clip = Clip::default_for(self.track().secs, self.mode(), max);
        self.start = Field::new(format_time(clip.start));
        self.length = Field::new(secs_text(clip.length));
    }

    fn output(&self) -> lh_core::Result<PathBuf> {
        match (&self.setup.output, self.output_for) {
            (Some(p), Some(i)) if i == self.setup.selected => Ok(p.clone()),
            _ => sample::default_output(&self.track().file.path),
        }
    }

    fn field(&mut self) -> Option<&mut Field> {
        match self.focus {
            Focus::Start => Some(&mut self.start),
            Focus::Length => Some(&mut self.length),
            Focus::Limit => Some(&mut self.limit),
            Focus::Tracks | Focus::Format => None,
        }
    }

    fn select(&mut self, index: usize) {
        if index != self.setup.selected {
            self.setup.selected = index;
            self.reset_clip();
        }
    }

    fn cycle_mode(&mut self, forward: bool) {
        let n = Mode::PRESETS.len();
        self.mode = if forward {
            (self.mode + 1) % n
        } else {
            (self.mode + n - 1) % n
        };
    }
}

/// The one encode in flight.
struct Job {
    queue: Queue<lh_core::Result<Sample>>,
}

pub(crate) fn sample_screen(
    terminal: &mut DefaultTerminal,
    target: &str,
    setup: SampleSetup,
    theme: Theme,
) -> io::Result<SampleOutcome> {
    let mut state = State::new(setup);
    let mut job: Option<Job> = None;
    let mut tick = 0usize;

    loop {
        if let Some(running) = &job {
            while let Ok(event) = running.queue.events().try_recv() {
                match event {
                    Event::Progress { done, total, .. } => {
                        if let Status::Running {
                            done: d, total: t, ..
                        } = &mut state.status
                        {
                            (*d, *t) = (done, total);
                        }
                    }
                    Event::Finished { output, .. } => {
                        state.status = match output {
                            Ok(sample) => {
                                state.written.push(sample.output.clone());
                                Status::Wrote(sample)
                            }
                            Err(lh_core::Error::OutputExists { path }) => Status::Exists(path),
                            Err(lh_core::Error::Cancelled) => {
                                Status::Failed("stopped; nothing was written".to_string())
                            }
                            // The path is the output field's already; the reason is what
                            // fits the line.
                            Err(lh_core::Error::SampleTooLarge { detail, .. }) => {
                                Status::Failed(detail)
                            }
                            Err(e) => Status::Failed(e.to_string()),
                        };
                        job = None;
                        break;
                    }
                    Event::Cancelled { .. } => {
                        state.status = Status::Failed("stopped; nothing was written".to_string());
                        job = None;
                        break;
                    }
                    Event::Started { .. } => {}
                }
            }
        }

        let spin = SPINNER[tick / 2 % SPINNER.len()];
        terminal.draw(|frame| draw_sample(frame, target, &state, spin, &theme))?;
        tick = tick.wrapping_add(1);

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let CtEvent::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        // While encoding, the only thing to do is stop it.
        if let Some(running) = &job {
            if is_quit(&key) {
                running.queue.cancel();
            }
            continue;
        }

        match on_key(&mut state, key) {
            Action::None => {}
            Action::Quit => break,
            Action::Encode { overwrite } => job = start(&mut state, overwrite),
        }
    }

    let failed = match &state.status {
        Status::Failed(e) => Some(e.clone()),
        _ => None,
    };
    Ok(SampleOutcome {
        written: state.written,
        failed,
    })
}

enum Action {
    None,
    Quit,
    Encode { overwrite: bool },
}

fn on_key(state: &mut State, key: KeyEvent) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    match key.code {
        KeyCode::Enter => return Action::Encode { overwrite: false },
        KeyCode::Tab => {
            state.focus = state.focus.step(true);
            return Action::None;
        }
        KeyCode::BackTab => {
            state.focus = state.focus.step(false);
            return Action::None;
        }
        _ => {}
    }

    if let Some(field) = state.field() {
        if key.code == KeyCode::Esc {
            state.focus = Focus::Tracks;
        } else {
            field.on_key(key.code);
        }
        return Action::None;
    }

    let last = state.setup.tracks.len() - 1;
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
        KeyCode::Char('F') if matches!(state.status, Status::Exists(_)) => {
            return Action::Encode { overwrite: true };
        }
        KeyCode::Left => state.cycle_mode(false),
        KeyCode::Right => state.cycle_mode(true),
        KeyCode::Char('d') => state.reset_clip(),
        KeyCode::Up | KeyCode::Char('k') if state.focus == Focus::Tracks => {
            state.select(state.setup.selected.saturating_sub(1));
        }
        KeyCode::Down | KeyCode::Char('j') if state.focus == Focus::Tracks => {
            state.select((state.setup.selected + 1).min(last));
        }
        KeyCode::Home if state.focus == Focus::Tracks => state.select(0),
        KeyCode::End if state.focus == Focus::Tracks => state.select(last),
        _ => {}
    }
    Action::None
}

/// Submits the encode, or says why the fields do not make one.
fn start(state: &mut State, overwrite: bool) -> Option<Job> {
    let settings = state.settings();
    let clip = match settings.clip {
        Ok(c) => c,
        Err(why) => {
            state.status = Status::Invalid(why.to_string());
            return None;
        }
    };
    let Some(max_bytes) = settings.max_bytes else {
        state.status = Status::Invalid("limit is not a size like 1M or 900K".to_string());
        return None;
    };
    let dst = match state.output() {
        Ok(d) => d,
        Err(e) => {
            state.status = Status::Failed(e.to_string());
            return None;
        }
    };
    let request = Request {
        clip,
        mode: settings.mode,
        max_bytes: Some(max_bytes),
        overwrite,
    };
    let src = state.track().file.path.clone();
    let track = state.track().file.file_name();
    let encoder = state.setup.encoder.clone();

    let queue: Queue<lh_core::Result<Sample>> = Queue::with_workers(1);
    queue.submit(track.clone(), move |progress| {
        sample::encode(&src, &dst, &encoder, &request, &mut |done, total| {
            progress.report(done, total);
            !progress.is_cancelled()
        })
    });
    state.status = Status::Running {
        done: 0,
        total: 0,
        track,
    };
    Some(Job { queue })
}

/// `1M`, `900K` or bytes, whichever the limit is a whole number of.
fn size_text(bytes: u64) -> String {
    if bytes % 1_000_000 == 0 {
        format!("{}M", bytes / 1_000_000)
    } else if bytes % 1000 == 0 {
        format!("{}K", bytes / 1000)
    } else {
        bytes.to_string()
    }
}

/// A length as the field shows it: `30`, or `12.5`.
fn secs_text(secs: f64) -> String {
    if secs.fract() == 0.0 {
        format!("{secs}")
    } else {
        format!("{secs:.1}")
    }
}

fn draw_sample(frame: &mut Frame, target: &str, state: &State, spin: char, theme: &Theme) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(14),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let header = Line::from(vec![
        Span::styled(" lh-tui ", theme.accent.bold()),
        Span::raw(" sample  "),
        Span::raw(format!("with {}  ", state.setup.encoder.id)),
        Span::styled(target.to_string(), theme.dim),
    ]);
    frame.render_widget(
        Paragraph::new(header).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        rows[0],
    );

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(30), Constraint::Length(52)])
        .split(rows[1]);
    draw_tracks(frame, body[0], state, theme);
    draw_settings(frame, body[1], state, theme);
    draw_status(frame, rows[2], state, spin, theme);

    let keys = match state.focus {
        _ if matches!(state.status, Status::Running { .. }) => " q/esc stop ",
        Focus::Tracks if matches!(state.status, Status::Exists(_)) => {
            " ↑↓ track   tab fields   ←→ format   enter encode   F replace   q/esc quit "
        }
        Focus::Tracks => {
            " ↑↓ track   tab fields   ←→ format   d default clip   enter encode   q/esc quit "
        }
        Focus::Format => " ←→ format   tab next   enter encode   esc back to tracks ",
        _ => " type to edit   tab next   enter encode   esc back to tracks ",
    };
    frame.render_widget(Paragraph::new(Line::styled(keys, theme.dim)), rows[3]);
}

fn draw_tracks(frame: &mut Frame, area: ratatui::layout::Rect, state: &State, theme: &Theme) {
    let focused = state.focus == Focus::Tracks;
    let table_rows = state.setup.tracks.iter().enumerate().map(|(i, t)| {
        let selected = i == state.setup.selected;
        let name = t.title.clone().unwrap_or_else(|| t.file.file_name());
        let style = match (selected, focused) {
            (true, true) => theme.accent.bold(),
            (true, false) => theme.accent,
            _ => ratatui::style::Style::default(),
        };
        Row::new(vec![
            Cell::from(if selected { ">" } else { " " }).style(theme.accent),
            Cell::from(name).style(style),
            Cell::from(t.secs.map(format_time).unwrap_or_else(|| "?".to_string())).style(theme.dim),
        ])
    });
    let table = Table::new(
        table_rows,
        [
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(8),
        ],
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" tracks ({}) ", state.setup.tracks.len()))
            .border_style(if focused { theme.accent } else { theme.dim }),
    );
    let mut table_state = TableState::default().with_selected(Some(state.setup.selected));
    frame.render_stateful_widget(table, area, &mut table_state);
}

fn draw_settings(frame: &mut Frame, area: ratatui::layout::Rect, state: &State, theme: &Theme) {
    let settings = state.settings();
    let track = state.track();
    let label = |text: &'static str, focus: Focus| {
        let style = if state.focus == focus {
            theme.accent.bold()
        } else {
            theme.dim
        };
        Span::styled(format!(" {text:<8}"), style)
    };
    let value = |field: &Field, focus: Focus, valid: bool| {
        let style = if !valid {
            theme.error
        } else if state.focus == focus {
            theme.accent
        } else {
            ratatui::style::Style::default()
        };
        Span::styled(field.display(state.focus == focus), style)
    };

    let start_ok = parse_time(&state.start.value).is_some();
    let length_ok = parse_time(&state.length.value).is_some_and(|l| l > 0.0);
    let mode = settings.mode;
    let format_text = if state.focus == Focus::Format {
        format!("◂ {mode} ▸")
    } else {
        mode.label()
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled(" track   ", theme.dim),
            Span::raw(track.file.file_name()),
        ]),
        Line::raw(""),
        Line::from(vec![
            label("start", Focus::Start),
            value(&state.start, Focus::Start, start_ok),
        ]),
        Line::from(vec![
            label("length", Focus::Length),
            value(&state.length, Focus::Length, length_ok),
            Span::styled(" s", theme.dim),
        ]),
        Line::from(vec![
            label("format", Focus::Format),
            Span::styled(
                format_text,
                if state.focus == Focus::Format {
                    theme.accent
                } else {
                    ratatui::style::Style::default()
                },
            ),
        ]),
        Line::from(vec![
            label("limit", Focus::Limit),
            value(&state.limit, Focus::Limit, settings.max_bytes.is_some()),
        ]),
        Line::raw(""),
    ];

    if let Ok(clip) = settings.clip {
        // What the encode will actually cut: a clip running past the end stops there.
        let end = track.secs.map_or(clip.end(), |t| clip.end().min(t));
        let length = (end - clip.start).max(0.0);
        let of = track
            .secs
            .map(|t| format!(" of {}", format_time(t)))
            .unwrap_or_default();
        let past_end = track.secs.is_some_and(|t| clip.start >= t);
        lines.push(Line::from(vec![
            Span::styled(" clip    ", theme.dim),
            if past_end {
                Span::styled("starts after the track ends", theme.error)
            } else {
                Span::raw(format!(
                    "{} – {}{of}",
                    format_time(clip.start),
                    format_time(end)
                ))
            },
        ]));

        let estimate = estimate_bytes(mode, length);
        let approx = if mode.is_exact() { "≈" } else { "~" };
        let size = match settings.max_bytes {
            Some(max) => {
                let over = estimate > max;
                let mut text =
                    format!("{approx} {} of {}", format_size(estimate), format_size(max));
                if over {
                    text.push_str("  over");
                } else if !mode.is_exact() {
                    text.push_str("  (checked after)");
                }
                Span::styled(text, if over { theme.error } else { theme.ok })
            }
            None => Span::raw(format!("{approx} {}", format_size(estimate))),
        };
        lines.push(Line::from(vec![Span::styled(" size    ", theme.dim), size]));
    }
    if let Some(max) = settings.max_bytes {
        lines.push(Line::from(vec![
            Span::styled(" fits    ", theme.dim),
            Span::raw(format!(
                "up to {} s at this format",
                longest_fitting(mode, max)
            )),
        ]));
    }
    let output = match state.output() {
        Ok(p) => file_name(&p),
        Err(e) => e.to_string(),
    };
    lines.push(Line::from(vec![
        Span::styled(" output  ", theme.dim),
        Span::raw(output),
    ]));

    let border = if state.focus == Focus::Tracks {
        theme.dim
    } else {
        theme.accent
    };
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" clip ")
                .border_style(border),
        ),
        area,
    );
}

fn draw_status(
    frame: &mut Frame,
    area: ratatui::layout::Rect,
    state: &State,
    spin: char,
    theme: &Theme,
) {
    let (text, style) = match &state.status {
        Status::Running { done, total, track } if *total > 0 => {
            let ratio = (f64::from(*done) / f64::from(*total)).min(1.0);
            let label = format!("cutting {track}  {}%", (ratio * 100.0) as u32);
            draw_gauge(frame, area, ratio, theme.accent, label, theme);
            return;
        }
        Status::Running { track, .. } => (
            format!("{spin} encoding {track} with {}", state.setup.encoder.id),
            theme.accent,
        ),
        Status::Idle => ("set the clip, then enter to encode".to_string(), theme.dim),
        Status::Wrote(s) => (
            format!(
                "wrote {} ({}, {} – {})",
                file_name(&s.output),
                format_size(s.bytes),
                format_time(s.clip.start),
                format_time(s.clip.end())
            ),
            theme.ok,
        ),
        Status::Exists(path) => (
            format!("{} already exists — F replaces it", file_name(path)),
            theme.warn,
        ),
        Status::Failed(e) => (e.clone(), theme.error),
        Status::Invalid(why) => (why.clone(), theme.warn),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(format!(" {text}"), style)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(theme.dim),
        ),
        area,
    );
}
