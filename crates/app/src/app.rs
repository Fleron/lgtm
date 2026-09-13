use crate::cached_prs::CachedPr;
use crate::chat::chat_scratch_root;
use crate::comments::comment_anchor;
use crate::composer::{Composer, ReviewDialog};
use crate::diff::{
    row_height, text_size, DEFAULT_TEXT_SIZE, FONT_PX, MAX_TEXT_SIZE, MIN_TEXT_SIZE,
    SPLIT_DIVIDER, SPLIT_GUTTER, UNIFIED_GUTTER, ViewMode,
};
use crate::items::{dir_name, ItemData, ItemState, ReviewItem, Source};
use crate::lsp_client::trace as lsp_trace;
use crate::palette::PaletteStep;
use crate::selection::{selection_text, RowCol, SelSide};
use crate::subscriptions::{load_subscribed_repos, SubscribedRepo};
use crate::{
    theme, ClearSelection, CloseItem, CopySelection, FocusTreeFilter, GoToBottom,
    GoToDefinition, GoToTop, NavBack, NavForward, NextFile, NextHunk, NextItem, OpenInput,
    OpenPalette, PrevFile, PrevHunk, PrevItem, Refresh, SubmitReview, ToggleChat,
    ToggleComments, ToggleMinimap, ToggleSidebar, ToggleView, ZoomIn, ZoomOut, ZoomReset, MONO,
};
use gpui::{
    div, font, point, prelude::*, px, ClipboardItem, Context, FocusHandle, IntoElement,
    Keystroke, MouseButton, MouseMoveEvent, PathPromptOptions, Pixels, Point, Render,
    ScrollHandle, ScrollStrategy, SharedString, Subscription, UniformListScrollHandle, Window,
};
use gpui_component::{
    input::{Escape as InputEscape, InputEvent, InputState},
    kbd::Kbd,
};
use std::sync::atomic::Ordering;

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

pub(crate) struct ReviewApp {
    pub(crate) items: Vec<ReviewItem>,
    pub(crate) active: usize,
    pub(crate) sidebar_visible: bool,
    pub(crate) open_input: gpui::Entity<InputState>,
    pub(crate) open_error: Option<SharedString>,
    /// Fuzzy filter over the active item's file tree (`/` focuses it).
    pub(crate) tree_filter_input: gpui::Entity<InputState>,
    pub(crate) focus_handle: FocusHandle,
    pub(crate) next_id: u64,
    pub(crate) palette: Option<PaletteStep>,
    pub(crate) palette_input: gpui::Entity<InputState>,
    /// Bumped on every palette transition; an in-flight PR-list fetch only
    /// lands if the generation it captured is still current.
    pub(crate) palette_gen: u64,
    pub(crate) palette_scroll: UniformListScrollHandle,
    /// Where the current selection drag started (side locked at mouse-down);
    /// None when no drag is in progress.
    pub(crate) drag_anchor: Option<(SelSide, RowCol)>,
    /// `m` toggles the minimap column for every item.
    pub(crate) minimap_visible: bool,
    /// `cmd-j` toggles the right-side chat panel (transcripts are per-item).
    pub(crate) chat_visible: bool,
    /// A minimap scrub drag is in progress (mouse went down on the minimap).
    pub(crate) minimap_scrub: bool,
    /// Advance width of one monospace cell at (MONO, text_size()), measured once.
    pub(crate) char_width: Option<Pixels>,
    /// Diff row + split half under the pointer where a hover "+" (new
    /// comment) affordance shows; None when the pointer isn't on a
    /// commentable line.
    pub(crate) hover_plus: Option<(usize, SelSide)>,
    pub(crate) composer: Option<Composer>,
    /// Bumped on every composer open/close; an in-flight post only reports
    /// back into the composer generation it was submitted from.
    pub(crate) composer_gen: u64,
    pub(crate) review: Option<ReviewDialog>,
    /// Bumped on every review-dialog open/close, same protocol as
    /// `composer_gen`.
    pub(crate) review_gen: u64,
    /// PRs with cached LSP worktrees on disk, listed in the sidebar to reopen or
    /// clean up. Filled by a background scan when the app starts.
    pub(crate) cached_prs: Vec<CachedPr>,
    /// GitHub repositories whose open PRs are kept in the sidebar feed.
    pub(crate) subscribed_repos: Vec<SubscribedRepo>,
    /// The feed refresh is guarded so a slow `gh pr list` cannot overlap the
    /// next scheduled refresh.
    pub(crate) subscribed_refreshing: bool,
    pub(crate) subscribed_scroll: ScrollHandle,
    pub(crate) _subscriptions: Vec<Subscription>,
}

impl ReviewApp {
    pub(crate) fn new(
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

    pub(crate) fn active_item(&self) -> Option<&ReviewItem> {
        self.items.get(self.active)
    }

    pub(crate) fn active_data(&self) -> Option<&ItemData> {
        match &self.items.get(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    pub(crate) fn active_data_mut(&mut self) -> Option<&mut ItemData> {
        match &mut self.items.get_mut(self.active)?.state {
            ItemState::Ready(data) => Some(data),
            _ => None,
        }
    }

    /// Advance width of one monospace cell, measured once via the text system
    /// (Menlo is monospace, so 'm' stands in for every glyph).
    pub(crate) fn char_width(&mut self, window: &Window) -> Pixels {
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
    pub(crate) fn pane_hit(
        &self,
        position: Point<Pixels>,
        char_width: Pixels,
        locked: Option<SelSide>,
    ) -> Option<(SelSide, RowCol)> {
        let (side, row, text_x) = self.pane_text_hit(position, locked)?;
        let col = (f32::from(text_x) / f32::from(char_width)).round().max(0.) as usize;
        Some((side, RowCol { row, col }))
    }

    pub(crate) fn pane_text_hit(
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
    pub(crate) fn prompt_open_folder(&mut self, cx: &mut Context<Self>) {
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
    pub(crate) fn render_plus(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
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
        let file_ix = data
            .file_rows
            .iter()
            .rposition(|&header| header <= row_ix)?;
        // A drag-selection (the same one cmd-c copies) whose near or far end
        // is this row becomes a multi-line comment spanning the selection,
        // as long as the other end lands in the same file and side and on a
        // different line (GitHub requires start_line strictly below line).
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
            let other_file_ix = data.file_rows.iter().rposition(|&header| header <= other_row)?;
            if other_file_ix != file_ix {
                return None;
            }
            let (other_side, other_line) = comment_anchor(&data.rows, other_row, side)?;
            (other_side == anchor_side && other_line != line).then_some(other_line)
        });
        let (line, start_line) = match other_end {
            Some(other) => (line.max(other), Some(line.min(other))),
            None => (line, None),
        };
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

