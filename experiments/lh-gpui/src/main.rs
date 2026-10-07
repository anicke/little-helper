//! Experiment: Lossless Little Helper's screens on gpui-kit, to judge how they look and
//! behave next to the iced GUI and the TUI. The shell is `lh-tui <folder>`'s workspace
//! (`lh-tui/src/screens/workspace.rs`) as a window: one show folder, and a sidebar of every
//! screen grouped the same way — Prepare, Inspect, Checksums, Torrent — each running on
//! that folder with the defaults the workspace uses.
//!
//! Ported so far: the batch screens (verify, sbe, convert → FLAC/WAV, check checksums,
//! create ffp/md5/st5) through one shared view (`batch.rs`), rename, torrent info and
//! torrent check. The other editor screens (tag, setlist, sample, sbe fix, torrent create)
//! are listed but point at their `lh-tui` command for now. Every job still runs through
//! `lh_core::job::Queue`, so progress and Cancel behave the way the real apps' do.

mod batch;
mod rename;
mod screen;
mod steps;
mod torrent_check;
mod torrent_info;
mod ui;

use gpui_kit::assets::{Assets, IconName, icon_assets};
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::sidebar::{Sidebar, SidebarGroup, SidebarMenu, SidebarMenuItem};
use gpui_kit::component::table::{DataTable, TableState};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, Theme, TitleBar, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

use batch::Batch;
use rename::Rename;
use screen::{AnyScreen, Screen as _};
use steps::{Item, Step, StepDone, StepResult, items};
use torrent_check::{TorrentCheck, TorrentLoaded};
use torrent_info::Files;
use ui::{card, scan_then};

// gpui-kit's `Assets` embeds only the icons its own components use; these are the extra
// ones the sidebar and buttons draw.
icon_assets!(
    AppIcons,
    [
        AudioWaveform,
        FileInput,
        FileMusic,
        FileSearch,
        Hash,
        ListChecks,
        ListMusic,
        PackageCheck,
        PackagePlus,
        PencilLine,
        Play,
        Ruler,
        Scissors,
        ShieldCheck,
        Tag,
        Wrench,
    ]
);

struct AppAssets;

impl AssetSource for AppAssets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        if let Some(bytes) = AppIcons.load(path)? {
            return Ok(Some(bytes));
        }
        Assets.load(path)
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let mut paths = Assets.list(path)?;
        paths.extend(AppIcons.list(path)?);
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn index_of(step: Step) -> usize {
    items()
        .position(|i| i.step == step)
        .expect("every step is in the menu")
}

struct Workspace {
    folder: Option<PathBuf>,
    /// While a new folder is being scanned; the screens hear about it once that is done.
    scanning: bool,
    /// What is in the folder, by format; rescanned after a run that can change it.
    summary: SharedString,
    /// The open screen, as an index into `items()`.
    selected: usize,
    results: Vec<StepResult>,
    /// Each batch screen's own view, made the first time it runs, so its last table stays
    /// put while another screen is open.
    batches: Vec<Option<Entity<Batch>>>,
    /// The screens that keep their own view, each told about a new folder.
    screens: Vec<(Step, Box<dyn AnyScreen>)>,
    /// Also in `screens`; Torrent info reads its torrent.
    torrent_check: Entity<TorrentCheck>,
    torrent_files: Entity<TableState<Files>>,
}

impl Workspace {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let count = items().count();
        let torrent_check = cx.new(|cx| TorrentCheck::new(window, cx));
        let torrent_files = cx.new(|cx| TableState::new(Files::new(), window, cx));
        let rename = cx.new(|cx| Rename::new(window, cx));

        cx.subscribe(&torrent_check, |this, check, _: &TorrentLoaded, cx| {
            let meta = check.read(cx).meta.clone();
            this.torrent_files.update(cx, |t, cx| {
                t.delegate_mut().load(meta.as_ref());
                t.refresh(cx);
            });
            cx.notify();
        })
        .detach();

        let mut this = Workspace {
            folder: None,
            scanning: false,
            summary: SharedString::default(),
            selected: index_of(Step::Verify),
            results: vec![StepResult::NotRun; count],
            batches: (0..count).map(|_| None).collect(),
            screens: Vec::new(),
            torrent_check: torrent_check.clone(),
            torrent_files,
        };
        this.add_screen(Step::Rename, rename, window, cx);
        this.add_screen(Step::TorrentCheck, torrent_check, window, cx);
        this
    }

    fn add_screen<T: screen::Screen>(
        &mut self,
        step: Step,
        screen: Entity<T>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.watch(step, &screen, window, cx);
        self.screens.push((step, Box::new(screen)));
    }

    /// Records how each of `entity`'s runs went, and rescans the folder summary after a
    /// run that can change it.
    fn watch<T: EventEmitter<StepDone>>(
        &self,
        step: Step,
        entity: &Entity<T>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            entity,
            window,
            move |this, _, StepDone(result), window, cx| {
                this.results[index_of(step)] = result.clone();
                if step.changes_folder() {
                    this.rescan(window, cx);
                }
                cx.notify();
            },
        )
        .detach();
    }

    fn screen(&self, step: Step) -> Option<&dyn AnyScreen> {
        self.screens
            .iter()
            .find(|(s, _)| *s == step)
            .map(|(_, screen)| screen.as_ref())
    }

    fn busy(&self, cx: &App) -> bool {
        self.batches.iter().flatten().any(|b| b.read(cx).running())
            || self.screens.iter().any(|(_, s)| s.running(cx))
    }

    /// A new show folder: every screen's last result was about the old one, so they go.
    /// The screens hear about it once it has been scanned.
    fn set_folder(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Resolved once, so `<folder>` in a checksum or torrent name is the folder's real
        // name even when it was given as `.`.
        let Ok(dir) = dir.canonicalize() else {
            return;
        };
        if !dir.is_dir() || self.busy(cx) {
            return;
        }
        self.results.fill(StepResult::NotRun);
        self.batches.fill(None);
        self.scanning = true;
        self.summary = "Scanning…".into();
        self.folder = Some(dir.clone());
        scan_then(dir, window, cx, |this, dir, scan, window, cx| {
            // A folder chosen while this one was scanning wins.
            if this.folder.as_ref() != Some(dir) {
                return;
            }
            for (_, screen) in &this.screens {
                screen.use_folder(dir, &scan.audio, window, cx);
            }
            this.summary = scan.summary.into();
            this.scanning = false;
            cx.notify();
        });
        cx.notify();
    }

    /// Refreshes the summary after a run changed what is in the folder.
    fn rescan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.folder.clone() else {
            return;
        };
        scan_then(dir, window, cx, |this, dir, scan, _, cx| {
            if this.folder.as_ref() == Some(dir) {
                this.summary = scan.summary.into();
                cx.notify();
            }
        });
    }

    /// A path from the command line or a drop: a folder becomes the show folder, a
    /// `.torrent` the torrent to check.
    fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if path.is_dir() {
            self.set_folder(path, window, cx);
        } else if !self.torrent_check.read(cx).running() {
            self.torrent_check
                .update(cx, |c, cx| c.pick_torrent(path, window, cx));
            self.selected = index_of(Step::TorrentCheck);
            cx.notify();
        }
    }

    fn browse_folder(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose show folder".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(mut paths))) = paths.await
                && let Some(path) = paths.pop()
            {
                this.update_in(cx, |this, window, cx| this.set_folder(path, window, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn run_batch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.folder.clone() else {
            return;
        };
        let step = items().nth(index).expect("index comes from items()").step;
        let prepared = match steps::prepare(step, &dir) {
            Ok(p) => p,
            Err(why) => {
                self.results[index] = StepResult::Refused(why);
                cx.notify();
                return;
            }
        };
        self.results[index] = StepResult::NotRun;
        let batch = match &self.batches[index] {
            Some(b) => b.clone(),
            None => {
                let b = cx.new(|cx| Batch::new(window, cx));
                self.watch(step, &b, window, cx);
                self.batches[index] = Some(b.clone());
                b
            }
        };
        batch.update(cx, |b, cx| b.start(prepared, cx));
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.busy(cx);
        let header = v_flex()
            .w_full()
            .gap_2()
            .p_2()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().muted_foreground)
                    .child("SHOW FOLDER"),
            )
            .child(match &self.folder {
                Some(dir) => v_flex()
                    .gap_0p5()
                    .child(
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .truncate()
                            .child(steps::file_name(dir)),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(self.summary.clone()),
                    ),
                None => v_flex().child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("None yet — browse, or drop a folder on the window."),
                ),
            })
            .child(
                Button::new("browse-show")
                    .outline()
                    .small()
                    .w_full()
                    .icon(IconName::FolderOpen)
                    .label("Choose folder…")
                    .disabled(busy)
                    .on_click(cx.listener(Self::browse_folder)),
            );

        let mut index = 0;
        let groups = steps::MENU.iter().map(|(title, group)| {
            let menu = SidebarMenu::new().children(group.iter().map(|item| {
                let i = index;
                index += 1;
                self.menu_item(i, item, cx)
            }));
            SidebarGroup::new(*title).child(menu)
        });
        let groups: Vec<_> = groups.collect();

        Sidebar::new("screens")
            .collapsible(false)
            .header(header)
            .children(groups)
    }

    fn menu_item(
        &self,
        index: usize,
        item: &'static Item,
        cx: &mut Context<Self>,
    ) -> SidebarMenuItem {
        enum Mark {
            Tui,
            Result(IconName, Hsla),
        }
        let theme = cx.theme();
        let mark = match (&self.results[index], item.tui_only) {
            (_, Some(_)) => Some(Mark::Tui),
            (StepResult::NotRun, _) => None,
            (StepResult::Clean(_), _) => Some(Mark::Result(IconName::CircleCheck, theme.success)),
            (StepResult::Unclean(_), _) => Some(Mark::Result(IconName::CircleX, theme.danger)),
            (StepResult::Refused(_), _) => {
                Some(Mark::Result(IconName::TriangleAlert, theme.warning))
            }
        };
        SidebarMenuItem::new(item.label)
            .icon(item.icon)
            .active(index == self.selected)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = index;
                cx.notify();
            }))
            .when_some(mark, |this, mark| {
                this.suffix(move |_, _| match &mark {
                    Mark::Tui => Tag::secondary().small().child("TUI").into_any_element(),
                    Mark::Result(icon, color) => div()
                        .text_color(*color)
                        .child(Icon::new(*icon).small())
                        .into_any_element(),
                })
            })
    }

    fn render_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let index = self.selected;
        let item = items().nth(index).expect("selected comes from items()");

        let header = h_flex()
            .gap_3()
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(Icon::new(item.icon).large()),
            )
            .child(
                v_flex()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(item.label),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(item.about),
                    ),
            );

        let body: AnyElement = if let Some(command) = item.tui_only {
            self.render_tui_only(command, cx).into_any_element()
        } else if item.step.needs_folder() && self.folder.is_none() {
            self.render_no_folder(cx).into_any_element()
        } else if let Some(screen) = self.screen(item.step) {
            if item.step.needs_folder() && self.scanning {
                card(cx, "Show folder")
                    .child(
                        div()
                            .text_color(cx.theme().muted_foreground)
                            .child("Scanning the folder…"),
                    )
                    .into_any_element()
            } else {
                screen.view().into_any_element()
            }
        } else if item.step == Step::TorrentInfo {
            self.render_torrent_info(cx).into_any_element()
        } else {
            self.render_batch_page(index, item, window, cx)
                .into_any_element()
        };

        v_flex()
            .id("page")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap_4()
            .p_6()
            .bg(cx.theme().muted.opacity(0.4))
            .text_color(cx.theme().foreground)
            .child(header)
            .child(body)
    }

    fn render_tui_only(&self, command: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let folder = self
            .folder
            .as_deref()
            .map(shell_quote)
            .unwrap_or_else(|| "<folder>".into());
        card(cx, "Not in the gpui trial yet")
            .child(
                div()
                    .text_sm()
                    .child("This screen hasn't been ported. For now, run it from a terminal:"),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().muted)
                    .font_family("monospace")
                    .text_sm()
                    .child(format!("lh-tui {command} {folder}")),
            )
    }

    fn render_no_folder(&self, cx: &mut Context<Self>) -> impl IntoElement {
        card(cx, "Show folder").child(
            h_flex()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .text_color(cx.theme().muted_foreground)
                        .child("Choose a show folder to run this on — or drop one on the window."),
                )
                .child(
                    Button::new("browse-empty")
                        .outline()
                        .icon(IconName::FolderOpen)
                        .label("Choose folder…")
                        .on_click(cx.listener(Self::browse_folder)),
                ),
        )
    }

    fn render_batch_page(
        &mut self,
        index: usize,
        item: &'static Item,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(dir) = self.folder.clone() else {
            unreachable!("render_page shows no batch page without a folder");
        };
        let batch = self.batches[index].clone();
        let running = batch.as_ref().is_some_and(|b| b.read(cx).running());
        let has_rows = batch.as_ref().is_some_and(|b| b.read(cx).has_rows(cx));

        let alert = match &self.results[index] {
            StepResult::NotRun => None,
            StepResult::Clean(msg) => Some(Alert::success("result", msg.clone()).title("Clean")),
            StepResult::Unclean(msg) => {
                Some(Alert::error("result", msg.clone()).title("Not clean"))
            }
            StepResult::Refused(msg) => {
                Some(Alert::warning("result", msg.clone()).title("Can't run this here"))
            }
        };

        v_flex()
            .flex_1()
            .min_h_0()
            .gap_4()
            .child(
                card(cx, "Show folder").child(
                    h_flex()
                        .gap_3()
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().truncate().child(dir.display().to_string()))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(self.summary.clone()),
                                ),
                        )
                        .child(
                            Button::new("run")
                                .primary()
                                .icon(IconName::Play)
                                .label(item.label)
                                .loading(running)
                                .disabled(running)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.run_batch(index, window, cx)
                                })),
                        ),
                ),
            )
            .children(alert)
            .when_some(batch.filter(|_| has_rows), |this, batch| this.child(batch))
            .into_any_element()
    }

    fn render_torrent_info(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let check = self.torrent_check.read(cx);
        let browse = Button::new("browse-torrent-info")
            .outline()
            .icon(IconName::FileInput)
            .label("Choose .torrent…")
            .disabled(check.running())
            .on_click(cx.listener(|this, ev, window, cx| {
                this.torrent_check
                    .update(cx, |c, cx| c.browse_torrent(ev, window, cx))
            }));

        let Some(meta) = &check.meta else {
            let why = match &self.folder {
                Some(dir) => match steps::find_torrent(dir) {
                    Err(why) => why,
                    Ok(_) => "The folder's torrent couldn't be read.".into(),
                },
                None => "No show folder or torrent chosen yet.".into(),
            };
            return card(cx, "Torrent")
                .child(
                    h_flex()
                        .gap_3()
                        .child(
                            div()
                                .flex_1()
                                .text_color(cx.theme().muted_foreground)
                                .child(why),
                        )
                        .child(browse),
                )
                .into_any_element();
        };

        let path = check
            .torrent_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        v_flex()
            .flex_1()
            .min_h_0()
            .gap_4()
            .child(
                card(cx, "Torrent")
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().flex_1().min_w_0().truncate().child(path))
                            .child(browse),
                    )
                    .child(torrent_info::details(meta)),
            )
            .child(
                div().flex_1().min_h(px(160.)).child(
                    DataTable::new(&self.torrent_files)
                        .stripe(true)
                        .bordered(true),
                ),
            )
            .into_any_element()
    }
}

/// A path as it would be typed into a shell: quoted when it has anything but the plain
/// characters show folders are usually named with.
fn shell_quote(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+,".contains(c))
    {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Client-side decorations: GNOME's Wayland compositor draws no title bar for
        // non-GTK apps, so the window draws its own (drag, double-click, min/max/close).
        v_flex()
            .id("workspace")
            .size_full()
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                if let Some(path) = paths.paths().first() {
                    this.open_path(path.clone(), window, cx);
                }
            }))
            .drag_over::<ExternalPaths>(|s, _, _, cx| s.bg(cx.theme().drop_target))
            .child(
                TitleBar::new().child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Lossless Little Helper"),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_stretch()
                    .child(self.render_sidebar(cx))
                    .child(self.render_page(window, cx)),
            )
    }
}

fn main() {
    application().with_assets(AppAssets).run(|cx| {
        init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1180.), px(800.)), cx)),
            window_min_size: Some(size(px(900.), px(600.))),
            window_decorations: Some(WindowDecorations::Client),
            ..TitleBar::window_options()
        };
        open_window(options, cx, |window, cx| {
            // Follow the desktop's light/dark preference, as the iced app does.
            Theme::sync_system_appearance(Some(window), cx);
            window
                .observe_window_appearance(|window, cx| {
                    Theme::sync_system_appearance(Some(window), cx)
                })
                .detach();
            cx.new(|cx| {
                let mut view = Workspace::new(window, cx);
                // `lh-gpui <folder>` opens on that show folder; `lh-gpui some.torrent` on
                // Torrent check with it loaded.
                if let Some(path) = std::env::args_os().nth(1) {
                    view.open_path(path.into(), window, cx);
                }
                view
            })
        })
        .expect("failed to open window");
        cx.activate(true);
    });
}
