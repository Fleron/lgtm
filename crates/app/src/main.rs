mod cached_prs;
mod chat;
mod comments;
mod composer;
mod diff;
mod items;
mod lsp;
mod lsp_client;
mod minimap;
mod palette;
mod selection;
mod subscriptions;
mod theme;
mod titlebar;
mod sidebar;
mod tree;

use gpui::{
    actions, div, font, point, prelude::*, px, size, App,
    Application, Bounds, ClipboardItem, Context, FocusHandle, KeyBinding,
    Keystroke, MouseButton, MouseMoveEvent,
    PathPromptOptions, Pixels, Point, ScrollHandle, ScrollStrategy,
    SharedString, Subscription, Task, TitlebarOptions, UniformListScrollHandle,
    Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    input::{CompletionProvider, Escape as InputEscape, InputEvent, InputState},
    kbd::Kbd,
    Root, Rope, RopeExt as _, TitleBar,
};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::Ordering;

use lsp_client::trace as lsp_trace;

use comments::comment_anchor;
use selection::{selection_text, RowCol, SelSide};

use cached_prs::{git_env, CachedPr};
use subscriptions::{load_subscribed_repos, SubscribedRepo};

pub(crate) use diff::{
    line_content, row_height, text_size, Cell, CardEdge, LineKind, Row,
    ViewMode, DEFAULT_TEXT_SIZE, FONT_PX, MAX_SOURCE_HIGHLIGHT_BYTES, MAX_SYNTAX_LINE_BYTES,
    MAX_TEXT_SIZE, MIN_TEXT_SIZE, SPLIT_DIVIDER, UNIFIED_GUTTER, SPLIT_GUTTER,
};

pub(crate) use items::{dir_name, ItemData, ItemState, ReviewItem, Source};
pub(crate) use palette::{PaletteStep, SIDEBAR_MAX_LIST_HEIGHT};

pub(crate) use chat::{chat_scratch_root, local_review_prompt, ChatState};
pub(crate) use composer::{Composer, ReviewDialog};
pub(crate) use lsp::{HoverState, LspHandle, NavLocation, SourceViewState, SymbolTarget};

pub(crate) const MONO: &str = "Menlo";

actions!(
    lgtm,
    [
        NextFile,
        PrevFile,
        NextHunk,
        PrevHunk,
        GoToTop,
        GoToBottom,
        ToggleView,
        Quit,
        ToggleSidebar,
        OpenInput,
        CloseItem,
        NextItem,
        PrevItem,
        Refresh,
        OpenPalette,
        PaletteUp,
        PaletteDown,
        PaletteBack,
        ClearSelection,
        CopySelection,
        FocusTreeFilter,
        ToggleMinimap,
        ToggleComments,
        ToggleChat,
        SubmitReview,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        GoToDefinition,
        NavBack,
        NavForward
    ]
);

fn main() {
    // Before any thread or child exists: set_var is process-global and not
    // thread-safe, and every subprocess needs to inherit this.
    for (key, value) in git_env() {
        std::env::set_var(key, value);
    }
    // Keep an SSH remote from stalling on a passphrase prompt for the same
    // reason — but never clobber a wrapper the user configured themselves
    // (custom identity, proxy command), which would break their setup.
    if std::env::var_os("GIT_SSH_COMMAND").is_none() {
        std::env::set_var("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [mode, root] = args.as_slice() {
        if mode == "--bifrost-lsp-server" {
            if let Err(err) = brokk_bifrost::lsp::run_lsp_stdio_server(PathBuf::from(root)) {
                eprintln!("lgtm: embedded Bifrost LSP failed: {err}");
                std::process::exit(1);
            }
            return;
        }
    }
    let mut sources = Vec::new();
    let mut errors = Vec::new();
    if args.is_empty() {
        // No args: review the repo we're standing in, or open empty.
        if let Ok(src) = git::resolve_local(Path::new(".")) {
            sources.push(Source::Local(src));
        }
    } else {
        for arg in &args {
            let parsed = if Path::new(arg).is_dir() {
                git::resolve_local(Path::new(arg)).map(Source::Local)
            } else {
                gh::resolve_pr_arg(arg).map(Source::Pr)
            };
            match parsed {
                Ok(source) => sources.push(source),
                Err(err) => {
                    eprintln!("error: {arg}: {err:#}");
                    errors.push(format!("{arg}: {err:#}"));
                }
            }
        }
    }

    Application::new()
        .with_assets(gpui_component_assets::Assets)
        .run(move |cx: &mut App| {
            gpui_component::init(cx);
            theme::apply_ui_theme(cx);
            cx.bind_keys([
                KeyBinding::new("]", NextFile, Some("ReviewApp")),
                KeyBinding::new("[", PrevFile, Some("ReviewApp")),
                KeyBinding::new("n", NextHunk, Some("ReviewApp")),
                KeyBinding::new("p", PrevHunk, Some("ReviewApp")),
                KeyBinding::new("home", GoToTop, Some("ReviewApp")),
                KeyBinding::new("end", GoToBottom, Some("ReviewApp")),
                KeyBinding::new("v", ToggleView, Some("ReviewApp")),
                KeyBinding::new("m", ToggleMinimap, Some("ReviewApp")),
                KeyBinding::new("c", ToggleComments, Some("ReviewApp")),
                KeyBinding::new("r", Refresh, Some("ReviewApp")),
                // Finish the review: approve / request changes / comment.
                KeyBinding::new("cmd-enter", SubmitReview, Some("ReviewApp")),
                // Only while the diff pane has focus; typing `/` in any input
                // stays a plain character.
                KeyBinding::new("/", FocusTreeFilter, Some("ReviewApp")),
                // Selection: escape/cmd-c only fire while the diff pane has
                // focus; with the palette open its input has focus, so the
                // palette's own escape routing wins by construction.
                KeyBinding::new("escape", ClearSelection, Some("ReviewApp")),
                KeyBinding::new("cmd-c", CopySelection, Some("ReviewApp")),
                KeyBinding::new("f12", GoToDefinition, Some("ReviewApp")),
                KeyBinding::new("ctrl-tab", NextItem, Some("ReviewApp")),
                KeyBinding::new("ctrl-shift-tab", PrevItem, Some("ReviewApp")),
                KeyBinding::new("cmd-left", NavBack, Some("ReviewApp")),
                KeyBinding::new("cmd-right", NavForward, Some("ReviewApp")),
                KeyBinding::new("alt-left", NavBack, Some("ReviewApp")),
                KeyBinding::new("alt-right", NavForward, Some("ReviewApp")),
                // Zoom. Bind both "cmd-=" and "cmd-+" so it fires with or
                // without shift, the way browsers and editors behave.
                KeyBinding::new("cmd-=", ZoomIn, None),
                KeyBinding::new("cmd-+", ZoomIn, None),
                KeyBinding::new("cmd--", ZoomOut, None),
                KeyBinding::new("cmd-0", ZoomReset, None),
                // Global (None context): must work while the open input is focused.
                KeyBinding::new("cmd-b", ToggleSidebar, None),
                KeyBinding::new("cmd-j", ToggleChat, None),
                KeyBinding::new("cmd-t", OpenInput, None),
                KeyBinding::new("cmd-w", CloseItem, None),
                KeyBinding::new("cmd-k", OpenPalette, None),
                KeyBinding::new("cmd-q", Quit, None),
                // Palette navigation. The `Palette > Input` variants are bound
                // after gpui_component::init, so at the input's dispatch depth
                // they take precedence over the Input's own up/down (which a
                // single-line input consumes without propagating).
                KeyBinding::new("up", PaletteUp, Some("Palette")),
                KeyBinding::new("down", PaletteDown, Some("Palette")),
                KeyBinding::new("escape", PaletteBack, Some("Palette")),
                KeyBinding::new("up", PaletteUp, Some("Palette > Input")),
                KeyBinding::new("down", PaletteDown, Some("Palette > Input")),
            ]);
            cx.on_action(|_: &Quit, cx| cx.quit());
            // One window is the whole app: closing it quits the process.
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();

            let bounds = Bounds::centered(None, size(px(1280.), px(860.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("lgtm".into()),
                        ..TitleBar::title_bar_options()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| ReviewApp::new(sources, errors, window, cx));
                    window.focus(&view.read(cx).focus_handle);
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .unwrap();
            cx.activate(true);
        });
}

pub(crate) fn centered_message(text: SharedString, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_color(color)
        .child(text)
        .into_any_element()
}

pub(crate) fn app_title(detail: Option<String>) -> gpui::AnyElement {
    let mut title = div()
        .flex()
        .items_center()
        .gap_2()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .font_weight(gpui::FontWeight::BOLD)
                .child(SharedString::from("lgtm")),
        );
    if let Some(detail) = detail {
        title = title.child(
            div()
                .text_color(theme::subtext())
                .truncate()
                .child(SharedString::from(detail)),
        );
    }
    title.into_any_element()
}

/// Icon-only review + CI summary for a sidebar PR row: a warning triangle

// --- @-mention autocomplete ------------------------------------------------

/// Most completion items to offer at once.
const MENTION_LIMIT: usize = 50;

/// `@`-mention autocomplete for the comment composer, backed by the item's
/// shared, live-updating pool of mentionable users (seeded with PR
/// participants, then filled from the repo's mentionable set in the
/// background). Reads the pool fresh on every keystroke.
pub(crate) struct MentionProvider {
    pub(crate) users: Rc<RefCell<Vec<gh::Mention>>>,
}

/// If the cursor sits inside an `@mention` token, return the byte offset of the
/// `@` and the (possibly empty) login text typed after it. The `@` must begin a
/// word — preceded by whitespace or the start of the text — matching GitHub's
/// own mention rules, so `foo@bar` never triggers.
fn mention_prefix(text: &Rope, offset: usize) -> Option<(usize, String)> {
    let s = text.to_string();
    let offset = offset.min(s.len());
    let before = &s[..offset];
    // GitHub logins are alphanumeric plus hyphen; walk back over that run.
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_ascii_alphanumeric() || *c == '-')
        .last()
        .map(|(i, _)| i)
        .unwrap_or(offset);
    if start == 0 || before.as_bytes()[start - 1] != b'@' {
        return None;
    }
    let at = start - 1;
    if at > 0 && !before[..at].chars().next_back().unwrap().is_whitespace() {
        return None;
    }
    Some((at, before[start..offset].to_string()))
}

/// Rank of `user` against `query` (matched case-insensitively), lower = better;
/// None = no match. Login prefix beats name prefix beats substring matches.
fn mention_rank(user: &gh::Mention, query: &str) -> Option<u8> {
    if query.is_empty() {
        return Some(0);
    }
    let query = &query.to_ascii_lowercase();
    let login = user.login.to_ascii_lowercase();
    let name = user.name.as_ref().map(|n| n.to_ascii_lowercase());
    if login.starts_with(query) {
        Some(0)
    } else if name
        .as_deref()
        .is_some_and(|n| n.split_whitespace().any(|w| w.starts_with(query)))
    {
        Some(1)
    } else if login.contains(query) {
        Some(2)
    } else if name.as_deref().is_some_and(|n| n.contains(query)) {
        Some(3)
    } else {
        None
    }
}

impl CompletionProvider for MentionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        _cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        let empty = Task::ready(Ok(lsp_types::CompletionResponse::Array(vec![])));
        let Some((at, prefix)) = mention_prefix(text, offset) else {
            return empty;
        };
        // The edit replaces `@prefix` (the token so far) with `@login `.
        let range = lsp_types::Range {
            start: text.offset_to_position(at),
            end: text.offset_to_position(offset),
        };
        let users = self.users.borrow();
        let mut ranked: Vec<(u8, &gh::Mention)> = users
            .iter()
            .filter_map(|u| mention_rank(u, &prefix).map(|r| (r, u)))
            .collect();
        // Stable sort keeps GitHub's alphabetical order within each rank.
        ranked.sort_by_key(|(rank, _)| *rank);
        let items = ranked
            .into_iter()
            .take(MENTION_LIMIT)
            .map(|(_, u)| lsp_types::CompletionItem {
                label: u.login.clone(),
                filter_text: Some(u.login.clone()),
                detail: u.name.clone(),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range,
                    new_text: format!("@{} ", u.login),
                })),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        Task::ready(Ok(lsp_types::CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        _new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        // Cheap to always run; `completions` returns nothing outside a mention.
        true
    }
}

/// Seed `cell` with everyone already visible on the PR — the author and every
/// comment author — so autocomplete has relevant names before the full
/// mentionable-user fetch returns. Additive: never drops fetched entries.
pub(crate) fn seed_mentions(cell: &Rc<RefCell<Vec<gh::Mention>>>, data: &ItemData) {
    let mut pool = cell.borrow_mut();
    let mut add = |login: &str| {
        if !login.is_empty() && !pool.iter().any(|m| m.login.eq_ignore_ascii_case(login)) {
            pool.push(gh::Mention {
                login: login.to_string(),
                name: None,
            });
        }
    };
    if let Some(meta) = &data.pr_meta {
        add(&meta.author.login);
    }
    if let Some(index) = &data.comments {
        for anchors in index.threads.values() {
            for threads in anchors.values() {
                for thread in threads {
                    add(&thread.root.user.login);
                    for reply in &thread.replies {
                        add(&reply.user.login);
                    }
                }
            }
        }
    }
}

struct ReviewApp {
    items: Vec<ReviewItem>,
    active: usize,
    sidebar_visible: bool,
    open_input: gpui::Entity<InputState>,
    open_error: Option<SharedString>,
    /// Fuzzy filter over the active item's file tree (`/` focuses it).
    tree_filter_input: gpui::Entity<InputState>,
    focus_handle: FocusHandle,
    next_id: u64,
    palette: Option<PaletteStep>,
    palette_input: gpui::Entity<InputState>,
    /// Bumped on every palette transition; an in-flight PR-list fetch only
    /// lands if the generation it captured is still current.
    palette_gen: u64,
    palette_scroll: UniformListScrollHandle,
    /// Where the current selection drag started (side locked at mouse-down);
    /// None when no drag is in progress.
    drag_anchor: Option<(SelSide, RowCol)>,
    /// `m` toggles the minimap column for every item.
    minimap_visible: bool,
    /// `cmd-j` toggles the right-side chat panel (transcripts are per-item).
    chat_visible: bool,
    /// A minimap scrub drag is in progress (mouse went down on the minimap).
    minimap_scrub: bool,
    /// Advance width of one monospace cell at (MONO, text_size()), measured once.
    char_width: Option<Pixels>,
    /// Diff row + split half under the pointer where a hover "+" (new
    /// comment) affordance shows; None when the pointer isn't on a
    /// commentable line.
    hover_plus: Option<(usize, SelSide)>,
    composer: Option<Composer>,
    /// Bumped on every composer open/close; an in-flight post only reports
    /// back into the composer generation it was submitted from.
    composer_gen: u64,
    review: Option<ReviewDialog>,
    /// Bumped on every review-dialog open/close, same protocol as
    /// `composer_gen`.
    review_gen: u64,
    /// PRs with cached LSP worktrees on disk, listed in the sidebar to reopen or
    /// clean up. Filled by a background scan when the app starts.
    cached_prs: Vec<CachedPr>,
    /// GitHub repositories whose open PRs are kept in the sidebar feed.
    subscribed_repos: Vec<SubscribedRepo>,
    /// The feed refresh is guarded so a slow `gh pr list` cannot overlap the
    /// next scheduled refresh.
    subscribed_refreshing: bool,
    subscribed_scroll: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl ReviewApp {
    fn new(
        sources: Vec<Source>,
        errors: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let open_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("owner/repo#123, PR URL, or path"));
        let palette_input = cx.new(|cx| InputState::new(window, cx));
        let tree_filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("filter files…"));
        let _subscriptions = vec![
            // Best-effort cleanup of every item's chat scratch dir on quit
            // (close_item handles the per-item case).
            cx.on_app_quit(|this: &mut Self, _cx| {
                for item in &this.items {
                    let _ = std::fs::remove_dir_all(chat_scratch_root(item.id));
                }
                async {}
            }),
            cx.subscribe_in(
                &open_input,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.submit_open(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &palette_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.palette_confirm(window, cx),
                    InputEvent::Change => this.palette_query_changed(cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &tree_filter_input,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.tree_filter_confirm(window, cx),
                    InputEvent::Change => {
                        // The match list changes shape; start it at the top.
                        if let Some(data) = this.active_data() {
                            data.tree_scroll.scroll_to_item(0, ScrollStrategy::Top);
                        }
                        cx.notify();
                    }
                    _ => {}
                },
            ),
        ];
        let subscribed_repos = load_subscribed_repos();
        let mut this = Self {
            items: Vec::new(),
            active: 0,
            sidebar_visible: !errors.is_empty()
                || sources.len() != 1
                || !subscribed_repos.is_empty(),
            open_input,
            open_error: errors.first().cloned().map(SharedString::from),
            tree_filter_input,
            focus_handle: cx.focus_handle(),
            next_id: 0,
            palette: None,
            palette_input,
            palette_gen: 0,
            palette_scroll: UniformListScrollHandle::new(),
            drag_anchor: None,
            minimap_visible: true,
            chat_visible: false,
            minimap_scrub: false,
            char_width: None,
            hover_plus: None,
            composer: None,
            composer_gen: 0,
            review: None,
            review_gen: 0,
            cached_prs: Vec::new(),
            subscribed_repos,
            subscribed_refreshing: false,
            subscribed_scroll: ScrollHandle::new(),
            _subscriptions,
        };
        this.refresh_cached_prs(cx);
        this.start_subscribed_pr_poll(cx);
        for source in sources {
            this.open_item(source, cx);
        }
        this.active = 0;
        this
    }

    /// Step the diff font size (cmd-+ / cmd-- / cmd-0). Row height follows the
    /// font, so every item's scroll offset is rescaled to keep the same line at
    /// the top — otherwise the pixel offset would silently mean a different row.
    fn zoom(&mut self, delta: f32, reset: bool, cx: &mut Context<Self>) {
        let old_rh = row_height();
        let next = if reset {
            DEFAULT_TEXT_SIZE
        } else {
            (text_size() + delta).clamp(MIN_TEXT_SIZE, MAX_TEXT_SIZE)
        };
        if next == text_size() {
            return; // Already at the bound; nothing to redraw.
        }
        FONT_PX.store(next as u32, Ordering::Relaxed);
        let new_rh = row_height();
        // The cached advance width was measured at the old size.
        self.char_width = None;
        for item in &mut self.items {
            let ItemState::Ready(data) = &mut item.state else {
                continue;
            };
            let offset = data.scroll.0.borrow().base_handle.offset();
            let top_row = (-f32::from(offset.y) / old_rh).max(0.);
            data.scroll
                .0
                .borrow()
                .base_handle
                .set_offset(point(offset.x, px(-(top_row * new_rh))));
            // Minimap geometry is height-derived; drop the memoized layout.
            data.minimap_cache.replace(None);
        }
        cx.notify();
    }

    fn active_item(&self) -> Option<&ReviewItem> {
        self.items.get(self.active)
    }

    fn active_data(&self) -> Option<&ItemData> {
        match &self.items.get(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    fn active_data_mut(&mut self) -> Option<&mut ItemData> {
        match &mut self.items.get_mut(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    /// Advance width of one monospace cell, measured once via the text system
    /// (Menlo is monospace, so 'm' stands in for every glyph).
    fn char_width(&mut self, window: &Window) -> Pixels {
        *self.char_width.get_or_insert_with(|| {
            let text_system = window.text_system();
            let font_id = text_system.resolve_font(&font(MONO));
            text_system
                .em_advance(font_id, px(text_size()))
                .unwrap_or(px(text_size() * 0.6))
        })
    }

    /// Window position → (side, row/col) in the active diff. Row from the
    /// uniform_list's scroll offset and painted bounds (both kept fresh each
    /// frame on the tracked scroll handle); col from monospace arithmetic.
    /// `locked` pins a split drag to the side where it started. Pure mouse
    /// math — verified manually, not unit-tested.
    fn pane_hit(
        &self,
        position: Point<Pixels>,
        char_width: Pixels,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, RowCol)> {
        let (side, row, text_x) = self.pane_text_hit(position, locked)?;
        let col = (f32::from(text_x) / f32::from(char_width)).round().max(0.) as usize;
        Some((side, RowCol { row, col }))
    }

    fn pane_text_hit(
        &self,
        position: Point<Pixels>,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, usize, Pixels)> {
        let data = self.active_data()?;
        if data.rows.is_empty() {
            return None;
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        // offset.y is negative when scrolled down.
        let y = f32::from(position.y - bounds.top() - offset.y);
        let row = ((y / row_height()).floor().max(0.) as usize).min(data.rows.len() - 1);
        let rel_x = f32::from(position.x - bounds.left());
        let (side, text_x) = match data.mode {
            // offset.x is negative when scrolled right (unified mode only;
            // split uses FitList and never scrolls horizontally).
            ViewMode::Unified => (
                SelSide::Unified,
                rel_x - f32::from(offset.x) - UNIFIED_GUTTER,
            ),
            ViewMode::Split => {
                let half = (f32::from(bounds.size.width) - SPLIT_DIVIDER) / 2.;
                let side = locked.unwrap_or(if rel_x < half + SPLIT_DIVIDER / 2. {
                    SelSide::Left
                } else {
                    SelSide::Right
                });
                let cell_x = match side {
                    SelSide::Right => rel_x - half - SPLIT_DIVIDER,
                    _ => rel_x,
                };
                (side, cell_x - SPLIT_GUTTER)
            }
        };
        Some((side, row, px(text_x)))
    }

    /// Native directory picker. The chosen path becomes a Local item through
    /// the normal add-item path; fetch_item re-resolves it on the background
    /// executor, so a non-repo directory surfaces as a Failed item.
    fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |app, cx| {
                let source = Source::Local(git::LocalSource {
                    branch: dir_name(&path),
                    base_ref: None,
                    base_label: "…".to_string(),
                    base_oid: None,
                    repo_root: path,
                });
                app.open_item(source, cx);
            })
            .ok();
        })
        .detach();
    }

    /// The hover "+" affordance: a small blue box at the far left of the
    /// hovered line (its half, in split mode), absolutely positioned over the
    /// list like the minimap viewport. Clicking opens the composer for that
    /// row's anchor.
    fn render_plus(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let (row_ix, side) = self.hover_plus?;
        if self.palette.is_some() {
            return None;
        }
        let item = self.active_item()?;
        let ItemState::Ready(data) = &item.state else {
            return None;
        };
        if matches!(&item.source, Source::Pr(_)) {
            if data
                .pr_meta
                .as_ref()
                .is_none_or(|meta| meta.head_ref_oid.is_empty())
            {
                return None;
            }
        }
        let (anchor_side, line) = comment_anchor(&data.rows, row_ix, side)?;
        // A drag-selection (the same one cmd-c copies) whose near or far end
        // is this row becomes a multi-line comment spanning the selection.
        let other_end = data.selection.and_then(|sel| {
            if sel.side != side {
                return None;
            }
            let (lo, hi) = sel.ordered();
            if lo.row == hi.row {
                return None;
            }
            let other_row = if row_ix == lo.row {
                Some(hi.row)
            } else if row_ix == hi.row {
                Some(lo.row)
            } else {
                None
            }?;
            let (other_side, other_line) = comment_anchor(&data.rows, other_row, side)?;
            (other_side == anchor_side).then_some(other_line)
        });
        let (line, start_line) = match other_end {
            Some(other) => (line.max(other), Some(line.min(other))),
            None => (line, None),
        };
        let file_ix = data
            .file_rows
            .iter()
            .rposition(|&header| header <= row_ix)?;
        let path = data.diff.files[file_ix].display_path().to_string();
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        // Pane-relative y of the hovered row; skip when scrolled out of view.
        let y = row_ix as f32 * row_height() + f32::from(offset.y);
        if y < 0. || y + row_height() > f32::from(bounds.size.height) {
            return None;
        }
        let x = match (data.mode, side) {
            (ViewMode::Split, SelSide::Right) => {
                (f32::from(bounds.size.width) - SPLIT_DIVIDER) / 2. + SPLIT_DIVIDER + 2.
            }
            _ => 2.,
        };
        let entity = cx.entity();
        Some(
            div()
                .absolute()
                .left(px(x))
                .top(px(y + (row_height() - 16.) / 2.))
                .w(px(16.))
                .h(px(16.))
                .rounded_sm()
                .bg(theme::blue())
                .flex()
                .items_center()
                .justify_center()
                .text_color(gpui::white())
                .text_size(px(13.))
                .cursor_pointer()
                .child(SharedString::from("+"))
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    let path = path.clone();
                    entity.update(cx, |this, cx| {
                        this.open_composer(
                            None, path, anchor_side, line, start_line, row_ix, window, cx,
                        );
                    });
                })
                .into_any_element(),
        )
    }

    fn render_footer(&self) -> impl IntoElement {
        let hint = |keys: &[&str], label: &'static str| {
            let mut hint = div().flex().items_center().gap_1();
            for key in keys {
                hint = hint.child(Kbd::new(Keystroke::parse(key).unwrap()));
            }
            hint.child(
                div()
                    .text_color(theme::overlay0())
                    .child(SharedString::from(label)),
            )
        };
        div()
            .h(px(28.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_4()
            .px_3()
            .bg(theme::mantle())
            .border_t_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(hint(&["]", "["], "files"))
            .child(hint(&["n", "p"], "hunks"))
            .child(hint(&["v"], "unified/split"))
            .child(hint(&["m"], "minimap"))
            .child(hint(&["c"], "comments"))
            .child(hint(&["/"], "filter files"))
            .child(hint(&["home", "end"], "top/bottom"))
            .child(hint(&["cmd-k"], "palette"))
            .child(hint(&["cmd-t"], "open"))
            .child(hint(&["cmd-b"], "sidebar"))
            .child(hint(&["cmd-j"], "chat"))
            .child(hint(&["r"], "refresh"))
            .child(hint(&["cmd-enter"], "review"))
    }
}

impl Render for ReviewApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.resync_comment_wrap(window);
        let pane: gpui::AnyElement = self.render_pane(window, cx);
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::base())
            .text_color(theme::text())
            .on_action(cx.listener(|this, _: &OpenPalette, window, cx| {
                if this.palette.is_some() {
                    this.close_palette(window, cx);
                } else {
                    this.open_palette(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevFile, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.file_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &NextHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_next(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &PrevHunk, _, cx| {
                let targets = this
                    .active_data()
                    .map(|d| d.hunk_rows.clone())
                    .unwrap_or_default();
                this.jump_prev(&targets, cx)
            }))
            .on_action(cx.listener(|this, _: &GoToTop, _, cx| this.jump(0, cx)))
            .on_action(cx.listener(|this, _: &GoToBottom, _, cx| {
                if let Some(last) = this.active_data().map(|d| d.rows.len().saturating_sub(1)) {
                    this.jump(last, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleView, _, cx| this.toggle_view(cx)))
            .on_action(cx.listener(|this, _: &ToggleMinimap, _, cx| {
                this.minimap_visible = !this.minimap_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleComments, _, cx| this.toggle_comments(cx)))
            .on_action(cx.listener(|this, _: &SubmitReview, window, cx| {
                // Backstop: with the dialog open, focus sits in its input,
                // whose own secondary-enter handles submission.
                if this.review.is_some() {
                    this.submit_review(window, cx);
                } else if matches!(
                    this.active_item().map(|item| &item.source),
                    Some(Source::Local(_))
                ) {
                    this.copy_local_review_prompt(cx);
                } else {
                    this.open_review(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleChat, window, cx| this.toggle_chat(window, cx)))
            .on_action(cx.listener(|this, _: &GoToDefinition, _, cx| this.go_to_last_symbol(cx)))
            .on_action(cx.listener(|this, _: &NavBack, _, cx| this.nav_back(cx)))
            .on_action(cx.listener(|this, _: &NavForward, _, cx| this.nav_forward(cx)))
            // Hover tracking for the "+" affordance lives on the root so the
            // affordance clears when the pointer leaves the diff list.
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                if event.dragging() {
                    return;
                }
                let hover = this.hover_target(event.position, window);
                if this.hover_plus != hover {
                    this.hover_plus = hover;
                    cx.notify();
                }
                let symbol = this.symbol_target_at(event.position, window);
                let existing = this
                    .active_data()
                    .and_then(|data| data.last_symbol_target.clone());
                match (symbol, existing) {
                    (Some(target), Some(existing)) if target == existing => {}
                    (Some(target), _) => this.request_hover(target, event.position, cx),
                    (None, Some(_)) => {
                        if let Some(data) = this.active_data_mut() {
                            data.hover_gen += 1;
                            lsp_trace(format_args!("ui hover cleared gen={}", data.hover_gen));
                            if let Some(cancel) = data.hover_cancel.take() {
                                cancel.store(true, Ordering::Release);
                            }
                            data.last_symbol_target = None;
                            data.hover = None;
                            cx.notify();
                        }
                    }
                    (None, None) => {}
                }
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            // Bound in the "ReviewApp" context: these only fire while the
            // diff pane has focus. With the palette open, focus sits in the
            // palette input, so its escape routing (PaletteBack) wins.
            .on_action(cx.listener(|this, _: &ClearSelection, window, cx| {
                // With the composer open but focus back on the diff pane,
                // escape closes the composer before touching the selection.
                if this.composer.is_some() {
                    this.close_composer(window, cx);
                    return;
                }
                // Next in line: a streaming chat run — escape stops it.
                if this.cancel_chat(cx) {
                    return;
                }
                if let Some(data) = this.active_data_mut() {
                    if data.hover.take().is_some() {
                        cx.notify();
                        return;
                    }
                    if data.source_view.take().is_some() {
                        cx.notify();
                        return;
                    }
                }
                if let Some(data) = this.active_data_mut() {
                    if data.selection.take().is_some() {
                        cx.notify();
                    }
                }
            }))
            .on_action(cx.listener(|this, _: &CopySelection, _, cx| {
                let Some(data) = this.active_data() else {
                    return;
                };
                let Some(sel) = data.selection else {
                    return;
                };
                let text = selection_text(&sel, &data.rows);
                if !text.is_empty() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_visible = !this.sidebar_visible;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(-1., false, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(0., true, cx)))
            .on_action(cx.listener(|this, _: &FocusTreeFilter, window, cx| {
                this.sidebar_visible = true;
                this.tree_filter_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenInput, window, cx| {
                // The sidebar input can't take focus under the palette.
                this.palette = None;
                this.palette_gen += 1;
                this.sidebar_visible = true;
                this.open_input
                    .update(cx, |state, cx| state.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseItem, _, cx| {
                let active = this.active;
                this.close_item(active, cx)
            }))
            .on_action(cx.listener(|this, _: &NextItem, _, cx| this.cycle_items(1, cx)))
            .on_action(cx.listener(|this, _: &PrevItem, _, cx| this.cycle_items(-1, cx)))
            // The open input propagates Escape when it has nothing of its own
            // to dismiss: hand focus back to the diff.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }))
            .child(self.render_titlebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .when(self.sidebar_visible, |main| {
                        main.child(self.render_sidebar(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .key_context("ReviewApp")
                            .track_focus(&self.focus_handle)
                            .child(pane),
                    )
                    // Chat panel: a sibling of the diff pane, outside the
                    // "ReviewApp" key context so typing in its input never
                    // triggers diff keys — the same isolation the palette
                    // and composer inputs rely on.
                    .when(self.chat_visible, |main| {
                        main.child(self.render_chat(window, cx))
                    }),
            )
            .child(self.render_footer())
            // Root-level so the composer's input escapes the "ReviewApp" key
            // context (plain letters must stay text, like the palette input).
            .when(self.composer.is_some(), |root| {
                root.child(self.render_composer(cx))
            })
            .when(self.review.is_some(), |root| {
                root.child(self.render_review(cx))
            })
            .when(self.palette.is_some(), |root| {
                root.child(self.render_palette(cx))
            })
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;
    use diff_core::{DiffRow, FileDiff, FileStatus, Hunk, PrDiff};
    use gpui::HighlightStyle;
    use std::ops::Range;
    use syntax::Token;

    pub(crate) fn ctx(old_no: u32, new_no: u32, text: &str) -> DiffRow {
        DiffRow::Context {
            old_no,
            new_no,
            text: text.to_string(),
        }
    }

    pub(crate) fn add(new_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Added {
            new_no,
            text: text.to_string(),
            intra,
        }
    }

    pub(crate) fn rem(old_no: u32, text: &str, intra: Vec<Range<usize>>) -> DiffRow {
        DiffRow::Removed {
            old_no,
            text: text.to_string(),
            intra,
        }
    }

    pub(crate) fn hunk(old_start: u32, new_start: u32, rows: Vec<DiffRow>) -> Hunk {
        Hunk {
            old_start,
            old_count: 0,
            new_start,
            new_count: 0,
            section: String::new(),
            rows,
        }
    }

    pub(crate) fn sample_diff() -> PrDiff {
        PrDiff {
            files: vec![
                FileDiff {
                    old_path: Some("a.rs".into()),
                    new_path: Some("a.rs".into()),
                    status: FileStatus::Modified,
                    hunks: vec![
                        // Equal-count modified run, flanked by context.
                        hunk(
                            1,
                            1,
                            vec![
                                ctx(1, 1, "ctx"),
                                rem(2, "old1", vec![0..3]),
                                rem(3, "old2", Vec::new()),
                                add(2, "new1", vec![0..3]),
                                add(3, "new2", Vec::new()),
                                ctx(4, 4, "tail"),
                            ],
                        ),
                        // Unequal run (2 removed, 1 added) + a lone added run.
                        hunk(
                            10,
                            10,
                            vec![
                                rem(10, "r1", Vec::new()),
                                rem(11, "r2", Vec::new()),
                                add(10, "a1", Vec::new()),
                                ctx(12, 11, "c"),
                                add(12, "lone", Vec::new()),
                            ],
                        ),
                    ],
                    additions: 4,
                    deletions: 4,
                },
                FileDiff {
                    old_path: Some("b.png".into()),
                    new_path: Some("b.png".into()),
                    status: FileStatus::Binary,
                    hunks: Vec::new(),
                    additions: 0,
                    deletions: 0,
                },
            ],
        }
    }

    pub(crate) fn cell(cell: &Option<Cell>) -> (u32, LineKind, &str, &[Range<usize>]) {
        let cell = cell.as_ref().expect("expected a cell");
        (cell.no, cell.kind, cell.text.as_ref(), &cell.intra)
    }

    pub(crate) fn style(token: Token) -> HighlightStyle {
        theme::token_style(token)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn rc(
        id: u64,
        path: &str,
        side: Option<&str>,
        line: Option<u64>,
        body: &str,
        author: &str,
        created_at: &str,
        reply_to: Option<u64>,
    ) -> gh::ReviewComment {
        gh::ReviewComment {
            id,
            path: path.to_string(),
            line,
            side: side.map(str::to_string),
            start_line: None,
            body: body.to_string(),
            user: gh::Author {
                login: author.to_string(),
            },
            created_at: created_at.to_string(),
            in_reply_to_id: reply_to,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use chat::{
        chat_prompt, local_chat_header, materialize_files, pr_chat_header, scratch_path,
        truncate_str, MAX_CHAT_PATCH_BYTES, MAX_EXPLORE_FILE_BYTES,
    };
    use comments::{group_comments, CommentIndex, CommentSide, LocalReview, COMMENT_WRAP_CHARS};
    use lsp::identifier_start_at;
    use palette::{filter_prs, filtered_sources};
    use diff::{
        build_rows, gap_span, hunk_syntax, is_comment_row, merge_highlights,
        nth_noncomment_row, row_height_for, FileUpgrade,
    };
    use diff_core::{diff_texts, FileDiff, FileStatus, PrDiff};
    use gpui::HighlightStyle;
    use minimap::{minimap_rows, MinimapKind, MinimapRow};
    use selection::row_side_text;
    use std::collections::HashMap;
    use test_util::*;

    fn mention(login: &str, name: Option<&str>) -> gh::Mention {
        gh::Mention {
            login: login.to_string(),
            name: name.map(str::to_string),
        }
    }

    #[test]
    fn mention_prefix_detects_at_tokens_at_word_boundaries() {
        let at = |s: &str, off: usize| mention_prefix(&Rope::from(s), off);
        // Bare `@` with the cursor right after it: empty prefix.
        assert_eq!(at("hi @", 4), Some((3, String::new())));
        // Mid-token cursor returns only what's typed so far.
        assert_eq!(at("hi @oct", 7), Some((3, "oct".to_string())));
        assert_eq!(at("hi @oct", 5), Some((3, "o".to_string())));
        // Start of text counts as a boundary.
        assert_eq!(at("@oct", 4), Some((0, "oct".to_string())));
        // Hyphens are valid login characters.
        assert_eq!(at("@foo-bar", 8), Some((0, "foo-bar".to_string())));
        // Not a boundary (looks like an email) — no completion.
        assert_eq!(at("foo@bar", 7), None);
        // No `@` at all.
        assert_eq!(at("hello", 5), None);
        // Cursor before the `@`.
        assert_eq!(at("hi @oct", 3), None);
    }

    #[test]
    fn mention_rank_orders_login_prefix_first() {
        let octocat = mention("octocat", Some("The Octocat"));
        // Login prefix is the strongest match.
        assert_eq!(mention_rank(&octocat, "oct"), Some(0));
        // Empty query matches everything at the top rank.
        assert_eq!(mention_rank(&octocat, ""), Some(0));
        // Name-word prefix beats a login substring.
        assert_eq!(mention_rank(&mention("xyz", Some("Bob Jones")), "bob"), Some(1));
        assert_eq!(mention_rank(&mention("abobc", None), "bob"), Some(2));
        // Case-insensitive, and no match returns None.
        assert_eq!(mention_rank(&octocat, "OCT"), Some(0));
        assert_eq!(mention_rank(&octocat, "zzz"), None);
    }

    #[test]
    fn identifier_hover_hit_snaps_to_the_token_start() {
        let text = "    if cfg.font.mono_family.is_empty() {";
        assert_eq!(identifier_start_at(text, 7), Some(7));
        assert_eq!(identifier_start_at(text, 9), Some(7));
        assert_eq!(identifier_start_at(text, 10), Some(7));
        assert_eq!(identifier_start_at(text, 11), Some(11));
        assert_eq!(identifier_start_at(text, 14), Some(11));
        assert_eq!(identifier_start_at(text, 15), Some(11));
        assert_eq!(identifier_start_at(text, 6), None);
    }

    #[test]
    fn split_context_fills_both_cells() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        // rows[0] = FileHeader, rows[1] = HunkHeader, rows[2] = first context.
        match &rows[2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (1, LineKind::Context, "ctx", &[][..]));
                assert_eq!(cell(right), (1, LineKind::Context, "ctx", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_pairs_equal_runs_positionally() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        match &rows[3] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (2, LineKind::Removed, "old1", &[0..3][..]));
                assert_eq!(cell(right), (2, LineKind::Added, "new1", &[0..3][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[4] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (3, LineKind::Removed, "old2", &[][..]));
                assert_eq!(cell(right), (3, LineKind::Added, "new2", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        // Equal run + 2 context rows: 4 split lines for a 6-row hunk.
        match &rows[5] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left).2, "tail");
                assert_eq!(cell(right).2, "tail");
            }
            _ => panic!("expected split line"),
        }
    }

    #[test]
    fn split_unequal_and_lone_runs_are_one_sided() {
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let h2 = hunk_rows[1];
        // 2 removed / 1 added: first row paired, second left-only.
        match &rows[h2 + 1] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (10, LineKind::Removed, "r1", &[][..]));
                assert_eq!(cell(right), (10, LineKind::Added, "a1", &[][..]));
            }
            _ => panic!("expected split line"),
        }
        match &rows[h2 + 2] {
            Row::SplitLine { left, right } => {
                assert_eq!(cell(left), (11, LineKind::Removed, "r2", &[][..]));
                assert!(right.is_none());
            }
            _ => panic!("expected split line"),
        }
        // Lone added run after context: right-only.
        match &rows[h2 + 4] {
            Row::SplitLine { left, right } => {
                assert!(left.is_none());
                assert_eq!(cell(right), (12, LineKind::Added, "lone", &[][..]));
            }
            _ => panic!("expected split line"),
        }
    }

    /// A 20-line file with line 10 changed, re-diffed with context 3: one
    /// hunk covering lines 7..=13, hidden gaps of 6 lines above and 7 below.
    fn upgraded_diff() -> (PrDiff, HashMap<usize, FileUpgrade>) {
        let old: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        let new = old.replace("line 10\n", "line ten\n");
        let hunks = diff_texts(&old, &new, 3);
        let new_lines: Vec<SharedString> = new.lines().map(|l| l.to_string().into()).collect();
        let n = new_lines.len();
        let file = FileDiff {
            old_path: Some("a.txt".into()),
            new_path: Some("a.txt".into()),
            status: FileStatus::Modified,
            hunks,
            additions: 1,
            deletions: 1,
        };
        let upgrade = FileUpgrade {
            new_lines,
            old_spans: vec![Vec::new(); 20],
            new_spans: vec![Vec::new(); n],
            expanded: HashSet::new(),
        };
        (PrDiff { files: vec![file] }, HashMap::from([(0, upgrade)]))
    }

    #[test]
    fn gap_span_math() {
        let (diff, upgrades) = upgraded_diff();
        let hunks = &diff.files[0].hunks;
        let total = upgrades[&0].new_lines.len() as u32;
        assert_eq!(gap_span(hunks, 0, total), (1, 1, 6)); // lines 1..=6 hidden
        assert_eq!(gap_span(hunks, 1, total), (14, 14, 7)); // lines 14..=20

        // Zero hunks (e.g. a pure CRLF flip): the whole file is one gap.
        assert_eq!(gap_span(&[], 0, total), (1, 1, 20));
        // Added file: single hunk covers everything, both gaps empty.
        let added = diff_texts("", "a\nb\n", 3);
        assert_eq!(gap_span(&added, 0, 2).2, 0);
        assert_eq!(gap_span(&added, 1, 2).2, 0);
        // Deleted file: new side empty, no gaps and no underflow.
        let deleted = diff_texts("a\nb\n", "", 3);
        assert_eq!(gap_span(&deleted, 0, 0).2, 0);
        assert_eq!(gap_span(&deleted, 1, 0).2, 0);
    }

    fn row_name(row: &Row) -> &'static str {
        match row {
            Row::Spacer => "Spacer",
            Row::FileHeader { .. } => "FileHeader",
            Row::HunkHeader { .. } => "HunkHeader",
            Row::Binary => "Binary",
            Row::Gap { .. } => "Gap",
            Row::Line { .. } => "Line",
            Row::SplitLine { .. } => "SplitLine",
            Row::CommentHeader { .. } => "CommentHeader",
            Row::CommentBody { .. } => "CommentBody",
            Row::CommentActions { .. } => "CommentActions",
        }
    }

    #[test]
    fn upgraded_file_gets_gap_rows_and_marked_headers() {
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, hunk_rows) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            // FileHeader, Gap(6), HunkHeader, 7 hunk rows, Gap(7).
            match &rows[1] {
                Row::Gap {
                    file_ix,
                    gap_ix,
                    hidden,
                } => {
                    assert_eq!((*file_ix, *gap_ix, *hidden), (0, 0, 6));
                }
                other => panic!("expected leading gap, got {}", row_name(other)),
            }
            assert_eq!(hunk_rows, vec![2]);
            assert!(matches!(rows[2], Row::HunkHeader { upgraded: true, .. }));
            match rows.last().unwrap() {
                Row::Gap { gap_ix, hidden, .. } => assert_eq!((*gap_ix, *hidden), (1, 7)),
                other => panic!("expected trailing gap, got {}", row_name(other)),
            }
            // Gap rows are selectable-through, like headers.
            assert!(row_side_text(&rows[1], SelSide::Unified).is_none());
            assert!(row_side_text(&rows[1], SelSide::Left).is_none());
        }
        // Un-upgraded build of the same diff has no gap rows.
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        assert!(matches!(
            rows[1],
            Row::HunkHeader {
                upgraded: false,
                ..
            }
        ));
    }

    #[test]
    fn expanded_gap_synthesizes_context_rows_with_correct_numbers() {
        let (diff, mut upgrades) = upgraded_diff();
        upgrades.get_mut(&0).unwrap().expanded.insert(0);

        let (rows, _, hunk_rows) = build_rows(&diff, ViewMode::Unified, &upgrades, None, true, COMMENT_WRAP_CHARS);
        // Leading gap expanded into 6 context rows before the hunk header.
        assert_eq!(hunk_rows, vec![7]); // FileHeader + 6 context rows
        for (j, row) in rows[1..7].iter().enumerate() {
            match row {
                Row::Line {
                    old_no,
                    new_no,
                    kind,
                    text,
                    ..
                } => {
                    assert_eq!(*kind, LineKind::Context);
                    assert_eq!(*old_no, Some(j as u32 + 1));
                    assert_eq!(*new_no, Some(j as u32 + 1));
                    assert_eq!(text.as_ref(), format!("line {}", j + 1));
                }
                other => panic!("expected context line, got {}", row_name(other)),
            }
        }
        // Trailing gap still collapsed.
        assert!(matches!(rows.last(), Some(Row::Gap { gap_ix: 1, .. })));

        // Split mode: same expansion as two-cell context rows.
        let (rows, _, _) = build_rows(&diff, ViewMode::Split, &upgrades, None, true, COMMENT_WRAP_CHARS);
        match &rows[1] {
            Row::SplitLine { left, right } => {
                let (l, r) = (left.as_ref().unwrap(), right.as_ref().unwrap());
                assert_eq!((l.no, r.no), (1, 1));
                assert_eq!(l.kind, LineKind::Context);
                assert_eq!(l.text.as_ref(), "line 1");
                assert_eq!(r.text.as_ref(), "line 1");
            }
            other => panic!("expected split context, got {}", row_name(other)),
        }

        // Expanding the trailing gap too: numbering continues past the hunk.
        upgrades.get_mut(&0).unwrap().expanded.insert(1);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, None, true, COMMENT_WRAP_CHARS);
        assert!(!rows.iter().any(|row| matches!(row, Row::Gap { .. })));
        match rows.last().unwrap() {
            Row::Line {
                old_no,
                new_no,
                text,
                ..
            } => {
                assert_eq!((*old_no, *new_no), (Some(20), Some(20)));
                assert_eq!(text.as_ref(), "line 20");
            }
            other => panic!("expected context line, got {}", row_name(other)),
        }
    }

    use syntax::Token;

    fn style_bg(token: Option<Token>) -> HighlightStyle {
        let mut style = token.map(style).unwrap_or_default();
        style.background_color = Some(theme::added_word_bg().into());
        style
    }

    #[test]
    fn merge_syntax_only() {
        let syntax = [(0..2, Token::Keyword), (3..7, Token::Function)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![
                (0..2, style(Token::Keyword)),
                (3..7, style(Token::Function))
            ]
        );
    }

    #[test]
    fn merge_intra_only() {
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[2..5], bg, None),
            vec![(2..5, style_bg(None))]
        );
    }

    #[test]
    fn merge_partial_overlap() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..6, Token::String)];
        assert_eq!(
            merge_highlights(&syntax, &[4..8], bg, None),
            vec![
                (0..4, style(Token::String)),
                (4..6, style_bg(Some(Token::String))),
                (6..8, style_bg(None)),
            ]
        );
    }

    #[test]
    fn merge_intra_spanning_multiple_tokens() {
        let bg = Some(theme::added_word_bg());
        let syntax = [(0..3, Token::Keyword), (5..8, Token::Number)];
        assert_eq!(
            merge_highlights(&syntax, &[1..7], bg, None),
            vec![
                (0..1, style(Token::Keyword)),
                (1..3, style_bg(Some(Token::Keyword))),
                (3..5, style_bg(None)),
                (5..7, style_bg(Some(Token::Number))),
                (7..8, style(Token::Number)),
            ]
        );
    }

    #[test]
    fn merge_adjacent_ranges() {
        // Same style across a shared boundary coalesces; different styles
        // stay split exactly at the boundary.
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..4, style(Token::Keyword))]
        );
        let syntax = [(0..2, Token::Keyword), (2..4, Token::Type)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, None),
            vec![(0..2, style(Token::Keyword)), (2..4, style(Token::Type))]
        );
        let bg = Some(theme::added_word_bg());
        assert_eq!(
            merge_highlights(&[], &[0..2, 2..4], bg, None),
            vec![(0..4, style_bg(None))]
        );
    }

    #[test]
    fn hunk_syntax_takes_spans_from_the_right_side() {
        let lang = syntax::language_for_path("x.rs");
        let rows = vec![
            ctx(1, 1, "fn f() {"),
            rem(2, "// gone", Vec::new()),
            add(2, "    let b = 2;", Vec::new()),
            ctx(3, 3, "}"),
        ];
        let spans = hunk_syntax(lang, &rows);
        assert_eq!(spans.len(), 4);
        // Context: from the new side.
        assert!(spans[0].contains(&(0..2, Token::Keyword)));
        // Removed: highlighted as part of old_source — a comment.
        assert_eq!(spans[1], vec![(0..7, Token::Comment)]);
        // Added: highlighted as part of new_source — `let` keyword.
        assert!(spans[2].contains(&(4..7, Token::Keyword)));
    }

    #[test]
    fn hunk_syntax_guardrails() {
        let rows = vec![ctx(1, 1, "fn f() {}")];
        // No language → no spans.
        assert_eq!(hunk_syntax(None, &rows), vec![Vec::new()]);
        // Over-long line stays plain even when the hunk is highlighted.
        let lang = syntax::language_for_path("x.rs");
        let long = format!("// {}", "x".repeat(5000));
        let rows = vec![ctx(1, 1, "fn f() {}"), ctx(2, 2, &long)];
        let spans = hunk_syntax(lang, &rows);
        assert!(!spans[0].is_empty());
        assert!(spans[1].is_empty());
    }

    fn pr(number: u64, title: &str, author: &str, branch: &str) -> gh::PrSummary {
        gh::PrSummary {
            number,
            title: title.to_string(),
            author: gh::Author {
                login: author.to_string(),
            },
            state: "OPEN".to_string(),
            is_draft: false,
            head_ref_name: branch.to_string(),
            updated_at: "2026-07-01T00:00:00Z".to_string(),
            review_decision: String::new(),
            status_check_rollup: Vec::new(),
        }
    }

    #[test]
    fn pr_filter_empty_query_keeps_original_order() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
            pr(2, "docs", "carol", "docs"),
        ];
        assert_eq!(filter_prs(&all, ""), vec![0, 1, 2]);
        assert_eq!(filter_prs(&all, "   "), vec![0, 1, 2]);
    }

    #[test]
    fn pr_filter_matches_number_title_author_and_branch() {
        let all = vec![
            pr(3, "fix crash", "alice", "fix-crash"),
            pr(1, "add feature", "bob", "feat"),
        ];
        assert_eq!(filter_prs(&all, "#3"), vec![0]);
        assert_eq!(filter_prs(&all, "bob"), vec![1]);
        assert_eq!(filter_prs(&all, "crash"), vec![0]);
        assert!(filter_prs(&all, "zzzqqq").is_empty());
    }

    #[test]
    fn source_options_filter_by_substring() {
        assert_eq!(filtered_sources(""), vec![0, 1, 2]);
        assert_eq!(filtered_sources("pull"), vec![0]);
        assert_eq!(filtered_sources("subscribe"), vec![1]);
        assert_eq!(filtered_sources("FOLDER"), vec![2]);
        assert_eq!(filtered_sources("open"), vec![0, 2]);
        assert!(filtered_sources("nope").is_empty());
    }

    #[test]
    fn header_indices_are_correct_in_both_modes() {
        let diff = sample_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, file_rows, hunk_rows) = build_rows(&diff, mode, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
            assert_eq!(file_rows.len(), 2);
            assert_eq!(hunk_rows.len(), 2);
            for &ix in &file_rows {
                assert!(matches!(rows[ix], Row::FileHeader { .. }));
            }
            for &ix in &hunk_rows {
                assert!(matches!(rows[ix], Row::HunkHeader { .. }));
            }
            // Binary file: header immediately followed by the binary row.
            assert!(matches!(rows[file_rows[1] + 1], Row::Binary));
        }
        // Unified emits one row per diff row; split collapses the equal run.
        let (unified, _, _) = build_rows(&diff, ViewMode::Unified, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let (split, _, _) = build_rows(&diff, ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let unified_lines = unified
            .iter()
            .filter(|r| matches!(r, Row::Line { .. }))
            .count();
        let split_lines = split
            .iter()
            .filter(|r| matches!(r, Row::SplitLine { .. }))
            .count();
        assert_eq!(unified_lines, 11);
        assert_eq!(split_lines, 8); // 4 (hunk 1) + 4 (hunk 2)
    }

    // --- Minimap -----------------------------------------------------------

    fn mrow(kind: MinimapKind, len_frac: f32) -> MinimapRow {
        MinimapRow { kind, len_frac }
    }

    #[test]
    fn minimap_rows_unified_kinds_and_fracs() {
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // FileHeader and HunkHeader both map to Header ticks.
        assert_eq!(mm[0], mrow(MinimapKind::Header, 1.));
        assert_eq!(mm[1], mrow(MinimapKind::Header, 1.));
        // Line rows: kind from the row, frac = chars / MAX_MINIMAP_CHARS.
        assert_eq!(mm[2], mrow(MinimapKind::Context, 3. / 160.)); // "ctx"
        assert_eq!(mm[3], mrow(MinimapKind::Removed, 4. / 160.)); // "old1"
        assert_eq!(mm[5], mrow(MinimapKind::Added, 4. / 160.)); // "new1"
                                                                // Spacer and Binary rows are blank.
        assert_eq!(mm[14], mrow(MinimapKind::Blank, 0.));
        assert_eq!(mm[16], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn minimap_rows_split_pairs_and_gaps() {
        let (rows, _, _) = build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        let mm = minimap_rows(&rows);
        assert_eq!(mm.len(), rows.len());
        // Context pair: both halves, no change flags.
        assert_eq!(
            mm[2].kind,
            MinimapKind::SplitPair {
                left_frac: 3. / 160.,
                right_frac: 3. / 160.,
                left: false,
                right: false,
            }
        );
        // Paired removed/added ("old1" / "new1").
        assert_eq!(
            mm[3].kind,
            MinimapKind::SplitPair {
                left_frac: 4. / 160.,
                right_frac: 4. / 160.,
                left: true,
                right: true,
            }
        );
        assert_eq!(mm[3].len_frac, 4. / 160.);
        // One-sided rows: the absent half has a zero fraction and no flag.
        let h2 = 6; // second HunkHeader (see split tests above)
        assert_eq!(
            mm[h2 + 2].kind,
            MinimapKind::SplitPair {
                left_frac: 2. / 160., // "r2"
                right_frac: 0.,
                left: true,
                right: false,
            }
        );
        assert_eq!(
            mm[h2 + 4].kind,
            MinimapKind::SplitPair {
                left_frac: 0.,
                right_frac: 4. / 160., // "lone"
                left: false,
                right: true,
            }
        );

        // Gap rows map to Gap in both modes.
        let (diff, upgrades) = upgraded_diff();
        for mode in [ViewMode::Unified, ViewMode::Split] {
            let (rows, _, _) = build_rows(&diff, mode, &upgrades, None, true, COMMENT_WRAP_CHARS);
            let mm = minimap_rows(&rows);
            assert_eq!(mm[1], mrow(MinimapKind::Gap, 1.));
        }
    }

    // --- Review comments ----------------------------------------------------

    /// Comments for sample_diff's a.rs: a RIGHT thread (with one reply) on
    /// added line 2 ("new1"), a LEFT thread on removed line 2 ("old1"), and
    /// one outdated comment.
    fn sample_comments() -> CommentIndex {
        group_comments(vec![
            rc(
                1,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "on new1",
                "alice",
                "2026-01-01T00:00:00Z",
                None,
            ),
            rc(
                2,
                "a.rs",
                Some("RIGHT"),
                Some(2),
                "reply",
                "bob",
                "2026-01-02T00:00:00Z",
                Some(1),
            ),
            rc(
                3,
                "a.rs",
                Some("LEFT"),
                Some(2),
                "on old1",
                "carol",
                "2026-01-03T00:00:00Z",
                None,
            ),
            rc(
                4,
                "a.rs",
                None,
                None,
                "outdated",
                "dave",
                "2026-01-04T00:00:00Z",
                None,
            ),
        ])
    }

    #[test]
    fn unified_rows_insert_threads_beneath_their_anchor() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows: FileHeader, HunkHeader, ctx, rem old1 (old_no 2) + LEFT
        // thread, rem old2, add new1 (new_no 2) + RIGHT thread, …
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[..12],
            &[
                "FileHeader",
                "HunkHeader",
                "Line",          // ctx 1/1
                "Line",          // rem old1 (old 2)
                "CommentHeader", // carol on old1
                "CommentBody",
                "CommentActions",
                "Line",          // rem old2
                "Line",          // add new1 (new 2)
                "CommentHeader", // alice
                "CommentBody",
                "CommentHeader", // bob's reply
            ]
        );
        assert_eq!(names[12], "CommentBody");
        assert_eq!(names[13], "CommentActions");
        // The LEFT thread is carol's; the reply flag follows position.
        match &rows[4] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "carol");
                assert!(!is_reply);
            }
            other => panic!("expected comment header, got {}", row_name(other)),
        }
        match &rows[11] {
            Row::CommentHeader {
                author, is_reply, ..
            } => {
                assert_eq!(author.as_ref(), "bob");
                assert!(is_reply);
            }
            other => panic!("expected reply header, got {}", row_name(other)),
        }
        // Actions row targets the thread root and carries the anchor.
        match &rows[13] {
            Row::CommentActions {
                root_id,
                path,
                side,
                line,
                ..
            } => {
                assert_eq!(*root_id, 1);
                assert_eq!(path.as_ref(), "a.rs");
                assert_eq!((*side, *line), (CommentSide::Right, 2));
            }
            other => panic!("expected actions row, got {}", row_name(other)),
        }
        // File header carries the counts (3 anchored, 1 outdated).
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // Comment rows are selectable-through and blank in the minimap.
        assert!(row_side_text(&rows[4], SelSide::Unified).is_none());
        assert_eq!(minimap_rows(&rows)[4], mrow(MinimapKind::Blank, 0.));
    }

    #[test]
    fn split_rows_anchor_threads_by_cell() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[3] pairs old1/new1 (both line 2): LEFT thread then RIGHT
        // thread directly beneath it.
        let names: Vec<&str> = rows.iter().map(row_name).collect();
        assert_eq!(
            &names[2..11],
            &[
                "SplitLine",     // ctx
                "SplitLine",     // old1 | new1
                "CommentHeader", // carol (LEFT)
                "CommentBody",
                "CommentActions",
                "CommentHeader", // alice (RIGHT)
                "CommentBody",
                "CommentHeader", // bob reply
                "CommentBody",
            ]
        );
        assert_eq!(names[11], "CommentActions");
        assert_eq!(names[12], "SplitLine"); // old2 | new2
    }

    /// The half a comment row renders in, or None for a full-width card.
    fn row_half(row: &Row) -> Option<Option<CommentSide>> {
        match row {
            Row::CommentHeader { half, .. }
            | Row::CommentBody { half, .. }
            | Row::CommentActions { half, .. } => Some(*half),
            _ => None,
        }
    }

    #[test]
    fn split_comments_render_in_the_half_they_were_left_on() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Split,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // rows[4..7] are carol's LEFT thread, rows[7..12] the RIGHT one; each
        // row of a thread carries the side it was anchored to.
        let halves: Vec<Option<Option<CommentSide>>> =
            rows[4..12].iter().map(row_half).collect();
        assert_eq!(
            halves,
            vec![
                Some(Some(CommentSide::Left)),  // carol header
                Some(Some(CommentSide::Left)),  // body
                Some(Some(CommentSide::Left)),  // actions
                Some(Some(CommentSide::Right)), // alice header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // bob reply header
                Some(Some(CommentSide::Right)), // body
                Some(Some(CommentSide::Right)), // actions
            ]
        );
    }

    #[test]
    fn only_a_threads_first_row_draws_the_card_top() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
            COMMENT_WRAP_CHARS,
        );
        // Exactly one top edge per thread, and it's the root's header — a
        // reply's header sits mid-card and must not cap it.
        let mut tops = 0;
        let mut threads = 0;
        for row in &rows {
            match row {
                Row::CommentHeader { top, is_reply, .. } => {
                    if *top {
                        tops += 1;
                        assert!(!is_reply, "a reply header must not open the card");
                    }
                }
                // The actions row always closes a thread, so it counts them.
                Row::CommentActions { .. } => threads += 1,
                _ => {}
            }
        }
        assert!(threads > 0, "fixture should have threads");
        assert_eq!(tops, threads, "one top edge per thread");
    }

    #[test]
    fn zoom_keybindings_parse() {
        // These strings are only parsed when the app boots, so a typo would be
        // a runtime panic rather than a compile error.
        for keys in ["cmd-=", "cmd-+", "cmd--", "cmd-0"] {
            assert!(
                Keystroke::parse(keys).is_ok(),
                "{keys:?} should be a valid keystroke"
            );
        }
    }

    #[test]
    fn row_height_follows_the_font_size() {
        // The default must reproduce the pre-zoom geometry exactly.
        assert_eq!(DEFAULT_TEXT_SIZE, 13.0);
        assert_eq!(row_height_for(DEFAULT_TEXT_SIZE), 22.0);
        // Rows grow and shrink with the text, always leaving headroom so
        // glyphs can't outgrow their row at either bound.
        assert_eq!(row_height_for(20.), 34.0);
        assert_eq!(row_height_for(8.), 14.0);
        for size in [MIN_TEXT_SIZE, DEFAULT_TEXT_SIZE, MAX_TEXT_SIZE] {
            assert!(
                row_height_for(size) > size,
                "row must be taller than {size}px text"
            );
        }
    }

    #[test]
    fn comment_bodies_wrap_to_the_requested_column_width() {
        // One long prose line plus an unbreakable token (a URL), the two ways a
        // body runs past the card.
        let body = format!("{} https://example.com/{}", "word ".repeat(60), "x".repeat(200));
        let index = group_comments(vec![rc(
            1,
            "a.rs",
            Some("RIGHT"),
            Some(2),
            &body,
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        for (name, mode, cap) in [
            ("unified", ViewMode::Unified, COMMENT_WRAP_CHARS),
            ("split", ViewMode::Split, 40),
        ] {
            let (rows, _, _) =
                build_rows(&sample_diff(), mode, &HashMap::new(), Some(&index), true, cap);
            let bodies: Vec<&SharedString> = rows
                .iter()
                .filter_map(|row| match row {
                    Row::CommentBody { line, .. } => Some(line),
                    _ => None,
                })
                .collect();
            assert!(!bodies.is_empty(), "{name} should have body rows");
            for line in bodies {
                assert!(
                    line.chars().count() <= cap,
                    "{name}: {:?} exceeds {cap} cols",
                    line.as_ref()
                );
            }
        }
    }

    #[test]
    fn unified_comments_span_the_full_width() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Unified has one column, so no comment row is confined to a half.
        assert!(rows.iter().filter_map(row_half).all(|half| half.is_none()));
        // ...and the diff really does contain comment rows to check.
        assert!(rows.iter().any(is_comment_row));
    }

    #[test]
    fn hidden_comments_keep_header_counts() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            false,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (3, 1));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
        // And with no index at all (local items): zero counts, no rows.
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        assert!(!rows.iter().any(is_comment_row));
        match &rows[0] {
            Row::FileHeader {
                comments, outdated, ..
            } => {
                assert_eq!((*comments, *outdated), (0, 0));
            }
            other => panic!("expected file header, got {}", row_name(other)),
        }
    }

    #[test]
    fn expanded_gap_context_rows_host_threads() {
        let (diff, mut upgrades) = upgraded_diff();
        // a.txt line 3 lives in the leading gap (hunk covers 7..=13).
        let index = group_comments(vec![rc(
            9,
            "a.txt",
            Some("RIGHT"),
            Some(3),
            "gap comment",
            "alice",
            "2026-01-01T00:00:00Z",
            None,
        )]);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, Some(&index), true, COMMENT_WRAP_CHARS);
        // Collapsed gap: the thread has no anchor row and stays hidden.
        assert!(!rows.iter().any(is_comment_row));
        upgrades.get_mut(&0).unwrap().expanded.insert(0);
        let (rows, _, _) = build_rows(&diff, ViewMode::Unified, &upgrades, Some(&index), true, COMMENT_WRAP_CHARS);
        // FileHeader, ctx 1, ctx 2, ctx 3, then the thread.
        assert_eq!(row_name(&rows[3]), "Line");
        assert_eq!(row_name(&rows[4]), "CommentHeader");
        assert_eq!(row_name(&rows[5]), "CommentBody");
        assert_eq!(row_name(&rows[6]), "CommentActions");
    }

    #[test]
    fn comment_anchor_resolves_sides_and_skips_non_lines() {
        let index = sample_comments();
        let (rows, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Headers and comment rows anchor nothing.
        assert_eq!(comment_anchor(&rows, 0, SelSide::Unified), None);
        assert_eq!(comment_anchor(&rows, 4, SelSide::Unified), None);
        // Context row (1,1) → RIGHT 1; removed old1 → LEFT 2; added new1 → RIGHT 2.
        assert_eq!(
            comment_anchor(&rows, 2, SelSide::Unified),
            Some((CommentSide::Right, 1))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Unified),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 8, SelSide::Unified),
            Some((CommentSide::Right, 2))
        );
        // Split: the half under the pointer decides; absent cells refuse.
        let (rows, _, hunk_rows) =
            build_rows(&sample_diff(), ViewMode::Split, &HashMap::new(), None, true, COMMENT_WRAP_CHARS);
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Left),
            Some((CommentSide::Left, 2))
        );
        assert_eq!(
            comment_anchor(&rows, 3, SelSide::Right),
            Some((CommentSide::Right, 2))
        );
        // Second hunk: r2 has no right cell (see split tests above).
        let h2 = hunk_rows[1];
        assert_eq!(comment_anchor(&rows, h2 + 2, SelSide::Right), None);
        assert_eq!(
            comment_anchor(&rows, h2 + 2, SelSide::Left),
            Some((CommentSide::Left, 11))
        );
    }

    #[test]
    fn nth_noncomment_row_maps_positions_across_comment_insertions() {
        let index = sample_comments();
        let (plain, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            None,
            true,
                COMMENT_WRAP_CHARS,
        );
        let (with, _, _) = build_rows(
            &sample_diff(),
            ViewMode::Unified,
            &HashMap::new(),
            Some(&index),
            true,
                COMMENT_WRAP_CHARS,
        );
        // Every plain row maps to the same row content with comments shown.
        for (n, row) in plain.iter().enumerate() {
            let ix = nth_noncomment_row(&with, n);
            assert_eq!(row_name(&with[ix]), row_name(row), "row {n}");
        }
        // n beyond the end clamps to the last row.
        assert_eq!(nth_noncomment_row(&plain, 999), plain.len() - 1);
    }

    // --- Chat with Claude ----------------------------------------------------

    #[test]
    fn chat_prompt_first_message_carries_header_patch_and_selection() {
        let prompt = chat_prompt(
            Some(("HEADER", "diff --git a/x b/x\n+new line\n")),
            Some("Selected text (x:1-1, RIGHT (new) side):\n```\nnew line\n```\n"),
            "why?",
        );
        assert!(prompt.starts_with("HEADER\n\nThe unified diff under review:\n```diff\n"));
        assert!(prompt.contains("+new line\n```\n"));
        assert!(!prompt.contains("truncated"));
        assert!(prompt.contains("Selected text (x:1-1"));
        assert!(prompt.ends_with("why?"));
        // Later turns: no context block, just selection (if any) + question.
        let followup = chat_prompt(None, None, "and this?");
        assert_eq!(followup, "and this?");
    }

    #[test]
    fn chat_prompt_truncates_patch_at_cap_on_char_boundary() {
        // 'a' then 2-byte 'é's: char boundaries sit at odd offsets, so the
        // even cap falls mid-char and must be shaved back to a boundary.
        let patch = format!("a{}", "é".repeat(MAX_CHAT_PATCH_BYTES));
        let prompt = chat_prompt(Some(("H", &patch)), None, "q");
        assert!(prompt.contains("(patch truncated at 200KB"));
        let (cut, truncated) = truncate_str(&patch, MAX_CHAT_PATCH_BYTES);
        assert!(truncated);
        assert_eq!(cut.len(), MAX_CHAT_PATCH_BYTES - 1); // boundary shaved one byte
        assert!(cut.is_char_boundary(cut.len()));
        let (all, truncated) = truncate_str("abc", 10);
        assert_eq!((all, truncated), ("abc", false));
    }

    #[test]
    fn chat_headers_describe_the_item() {
        let meta = gh::PrMeta {
            number: 7,
            title: "Fix the frobnicator".into(),
            author: gh::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            is_draft: false,
            url: "https://github.com/o/r/pull/7".into(),
            body: "It was broken.\n".into(),
            base_ref_name: "main".into(),
            head_ref_name: "fix".into(),
            base_ref_oid: String::new(),
            head_ref_oid: String::new(),
            additions: 1,
            deletions: 2,
            changed_files: 3,
            review_decision: String::new(),
            status_check_rollup: Vec::new(),
        };
        let header = pr_chat_header(&meta);
        assert!(header.contains("\"Fix the frobnicator\""));
        assert!(header.contains("https://github.com/o/r/pull/7"));
        assert!(header.contains("alice"));
        assert!(header.contains("OPEN"));
        assert!(header.contains("main ← fix"));
        assert!(header.contains("It was broken."));
        // Empty body → explicit placeholder, so the model doesn't guess.
        let meta = gh::PrMeta {
            body: "  \n".into(),
            ..meta
        };
        assert!(pr_chat_header(&meta).contains("(no description)"));

        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "origin/main".into(),
            base_oid: None,
        };
        let header = local_chat_header(&src);
        assert!(header.contains("myrepo"));
        assert!(header.contains("feature"));
        assert!(header.contains("origin/main"));
    }

    #[test]
    fn local_review_prompt_matches_difit_comment_format() {
        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "upstream/main".into(),
            base_oid: None,
        };
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "why remove this?\nplease explain".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "because this path handles nil".to_string(),
        );
        review.add_comment(
            None,
            "src/main.rs".to_string(),
            CommentSide::Left,
            12,
            None,
            "second thread".to_string(),
        );

        assert_eq!(
            local_review_prompt(&src, &review),
            "Diff: upstream/main ← feature. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\nsrc/lib.rs:R7\nwhy remove this?\nplease explain\nReply 1 (you)\nbecause this path handles nil\n=====\nsrc/main.rs:L12\nsecond thread"
        );
    }

    #[test]
    fn scratch_paths_stay_inside_the_root() {
        let root = Path::new("/tmp/lgtm-chat-1-2");
        assert_eq!(
            scratch_path(root, "src/main.rs"),
            Some(root.join("src/main.rs"))
        );
        assert_eq!(
            scratch_path(root, "deep/a/b/c.txt"),
            Some(root.join("deep/a/b/c.txt"))
        );
        // Escapes and non-normal components are refused.
        assert_eq!(scratch_path(root, "../evil"), None);
        assert_eq!(scratch_path(root, "a/../../evil"), None);
        assert_eq!(scratch_path(root, "/abs/path"), None);
        assert_eq!(scratch_path(root, "./x"), None);
        assert_eq!(scratch_path(root, ""), None);
    }

    #[test]
    fn materialize_writes_files_and_skips_oversized_and_unsafe() {
        let root = std::env::temp_dir().join(format!(
            "lgtm-chat-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let files = vec![
            ("src/lib.rs".to_string(), "fn a() {}\n".to_string()),
            ("../escape.rs".to_string(), "nope\n".to_string()),
            ("big.rs".to_string(), "x".repeat(MAX_EXPLORE_FILE_BYTES + 1)),
        ];
        let dir = materialize_files(&root, &files).unwrap();
        assert_eq!(dir, root);
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert!(!root.join("big.rs").exists());
        assert!(!root.parent().unwrap().join("escape.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merge_selection_wins_over_intra() {
        let bg = Some(theme::added_word_bg());
        let sel_style = |token: Option<Token>| {
            let mut style = token.map(style).unwrap_or_default();
            style.background_color = Some(theme::selection_bg().into());
            style
        };
        // Intra 2..6, selection 4..8: the overlap 4..6 paints selection bg.
        assert_eq!(
            merge_highlights(&[], &[2..6], bg, Some(4..8)),
            vec![(2..4, style_bg(None)), (4..8, sel_style(None))]
        );
        // Selection over a syntax token keeps the token foreground.
        let syntax = [(0..4, Token::Keyword)];
        assert_eq!(
            merge_highlights(&syntax, &[], None, Some(2..6)),
            vec![
                (0..2, style(Token::Keyword)),
                (2..4, sel_style(Some(Token::Keyword))),
                (4..6, sel_style(None)),
            ]
        );
    }
}
