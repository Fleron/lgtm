use super::{
    build_rows, kind_style, line_content, row_height, text_size, CardEdge, Cell, Row, ViewMode,
};
use crate::comments::{comment_row, comment_wrap_cols};
use crate::items::ItemState;
use crate::minimap::{minimap_scale, MinimapColor, MinimapLane, MINIMAP_GAP, MINIMAP_PAD, MINIMAP_WIDTH};
use crate::selection::{row_selection_range, SelSide, Selection};
use crate::tree::status_style;
use crate::{centered_message, theme, ReviewApp, MONO};
use gpui::{
    canvas, div, fill, point, prelude::*, px, size, uniform_list, Bounds, Context, Hsla,
    ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, ScrollStrategy, SharedString, Window,
};
use gpui_component::{scroll::Scrollbar, tag::Tag, Sizable as _};
use std::ops::Range;

/// `selection` is this row's selected byte range (side + non-empty range),
/// computed by the caller via `row_selection_range`; its side always matches
/// the row shape (Unified for Line rows, Left/Right for SplitLine rows).
/// `entity` is used by Gap rows (click expands) and CommentActions rows
/// (click opens the reply composer); `ix` is the display-row index.
fn render_row(
    ix: usize,
    row: &Row,
    selection: Option<(SelSide, Range<usize>)>,
    entity: &gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let row_height = px(row_height());
    match row {
        Row::CommentHeader {
            author,
            when,
            is_reply,
            half,
            top,
        } => {
            let mut inner = div().flex().items_center().gap_2().min_w_0();
            if *is_reply {
                inner = inner.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from("↳")),
                );
            }
            comment_row(
                inner
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(theme::text())
                            .child(author.clone()),
                    )
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .text_size(px(11.))
                            .child(when.clone()),
                    )
                    .into_any_element(),
                *half,
                true,
                if *top { CardEdge::Top } else { CardEdge::Middle },
            )
        }
        Row::CommentBody { line, half } => comment_row(
            div()
                .whitespace_nowrap()
                .text_color(theme::subtext())
                .child(line.clone())
                .into_any_element(),
            *half,
            false,
            CardEdge::Middle,
        ),
        Row::CommentActions {
            root_id,
            path,
            side,
            line,
            half,
        } => {
            let (root_id, side, line) = (*root_id, *side, *line);
            let path = path.clone();
            let entity = entity.clone();
            comment_row(
                div()
                    .id(("reply", root_id as usize))
                    .cursor_pointer()
                    .text_color(theme::blue())
                    .hover(|style| style.opacity(0.8))
                    .child(SharedString::from("↳ reply"))
                    .on_click(move |_, window, cx| {
                        entity.update(cx, |this, cx| {
                            this.open_composer(
                                Some(root_id),
                                path.to_string(),
                                side,
                                line,
                                None,
                                ix,
                                window,
                                cx,
                            );
                        });
                    })
                    .into_any_element(),
                *half,
                false,
                CardEdge::Bottom,
            )
        }
        Row::Spacer => div().h(row_height).into_any_element(),
        Row::Gap {
            file_ix,
            gap_ix,
            hidden,
        } => {
            let (file_ix, gap_ix) = (*file_ix, *gap_ix);
            let entity = entity.clone();
            let noun = if *hidden == 1 { "line" } else { "lines" };
            div()
                .id(("gap", (file_ix << 20) | gap_ix))
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .bg(theme::crust())
                .hover(|style| style.bg(theme::surface0()))
                .cursor_pointer()
                .text_color(theme::overlay0())
                .child(SharedString::from(format!("⋯ {hidden} hidden {noun}")))
                .on_click(move |_, _, cx| {
                    entity.update(cx, |this, cx| this.expand_gap(file_ix, gap_ix, cx));
                })
                .into_any_element()
        }
        Row::FileHeader {
            path,
            old_path,
            status,
            additions,
            deletions,
            comments,
            outdated,
        } => {
            let (status_label, status_color) = status_style(*status);
            let status: Hsla = status_color.into();
            let mut header = div()
                .h(row_height)
                .w_full()
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .bg(theme::mantle())
                .child(
                    Tag::custom(status.opacity(0.15), status, status.opacity(0.4))
                        .small()
                        .child(SharedString::from(status_label)),
                )
                .child(
                    div()
                        .text_color(theme::text())
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(path.clone()),
                );
            if let Some(old_path) = old_path {
                header = header.child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!("← {old_path}"))),
                );
            }
            if *comments > 0 || *outdated > 0 {
                let mut note = String::new();
                if *comments > 0 {
                    note = format!(
                        "{comments} comment{}",
                        if *comments == 1 { "" } else { "s" }
                    );
                }
                if *outdated > 0 {
                    if !note.is_empty() {
                        note.push_str(" · ");
                    }
                    note.push_str(&format!("{outdated} outdated"));
                }
                header = header.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::overlay0())
                        .child(SharedString::from(note)),
                );
            }
            header
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{additions}"))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{deletions}"))),
                )
                .into_any_element()
        }
        Row::HunkHeader { label, upgraded } => div()
            .h(row_height)
            .w_full()
            .flex()
            .items_center()
            .px_3()
            .bg(theme::crust())
            // Subtle upgrade indicator: full-content re-diffed hunks get a
            // faint blue label instead of the plain overlay color.
            .text_color(if *upgraded {
                Hsla::from(theme::blue()).opacity(0.55)
            } else {
                theme::overlay0().into()
            })
            .child(label.clone())
            .into_any_element(),
        Row::Binary => div()
            .h(row_height)
            .flex()
            .items_center()
            .px_3()
            .text_color(theme::overlay0())
            .child(SharedString::from("binary file changed"))
            .into_any_element(),
        Row::Line {
            old_no,
            new_no,
            kind,
            text,
            intra,
            syntax,
        } => {
            let (row_bg, word_bg, marker, marker_color) = kind_style(*kind);
            let number = |no: Option<u32>| {
                div()
                    .w(px(44.))
                    .flex_shrink_0()
                    .text_color(theme::overlay0())
                    .flex()
                    .justify_end()
                    .child(SharedString::from(
                        no.map(|no| no.to_string()).unwrap_or_default(),
                    ))
            };
            let mut line = div().h(row_height).flex().items_center();
            if let Some(bg) = row_bg {
                line = line.bg(bg);
            }
            line.child(number(*old_no))
                .child(number(*new_no))
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(div().whitespace_nowrap().child(line_content(
                    text,
                    syntax,
                    intra,
                    word_bg,
                    selection.map(|(_, range)| range),
                )))
                .into_any_element()
        }
        Row::SplitLine { left, right } => {
            let (left_sel, right_sel) = match selection {
                Some((SelSide::Left, range)) => (Some(range), None),
                Some((SelSide::Right, range)) => (None, Some(range)),
                _ => (None, None),
            };
            let cell = |cell: &Option<Cell>, sel: Option<Range<usize>>| {
                let base = div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .h_full()
                    .flex()
                    .items_center();
                let Some(cell) = cell else {
                    return base.bg(theme::void_cell_bg());
                };
                let (row_bg, word_bg, marker, marker_color) = kind_style(cell.kind);
                let mut side = base;
                if let Some(bg) = row_bg {
                    side = side.bg(bg);
                }
                side.child(
                    div()
                        .w(px(44.))
                        .flex_shrink_0()
                        .text_color(theme::overlay0())
                        .flex()
                        .justify_end()
                        .child(SharedString::from(cell.no.to_string())),
                )
                .child(
                    div()
                        .w(px(28.))
                        .flex_shrink_0()
                        .flex()
                        .justify_center()
                        .text_color(marker_color)
                        .child(SharedString::from(marker)),
                )
                .child(div().whitespace_nowrap().child(line_content(
                    &cell.text,
                    &cell.syntax,
                    &cell.intra,
                    word_bg,
                    sel,
                )))
            };
            // w_full is load-bearing: without a definite row width the row
            // sizes to fit-content and the flex_1 halves collapse to their
            // text width, putting the divider at a different x every row.
            div()
                .h(row_height)
                .w_full()
                .flex()
                .child(cell(left, left_sel))
                .child(
                    div()
                        .w(px(6.))
                        .flex_shrink_0()
                        .h_full()
                        .bg(theme::crust())
                        .border_l_1()
                        .border_r_1()
                        .border_color(theme::surface0()),
                )
                .child(cell(right, right_sel))
                .into_any_element()
        }
    }
}

impl ReviewApp {
    /// Re-wrap comment bodies to the pane's current width. Called each render:
    /// the bounds come from the last paint, so a resize (or toggling the
    /// sidebar, chat, or view mode) settles on the following frame. Rebuilding
    /// only when the column count actually changes keeps this off the hot path
    /// — a few pixels of drag usually map to the same width in columns.
    pub(crate) fn resync_comment_wrap(&mut self, window: &Window) {
        let char_width = f32::from(self.char_width(window));
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Nothing to re-wrap without visible threads.
        let has_threads = data.comments_visible
            && data
                .comments
                .as_ref()
                .is_some_and(|index| !index.threads.is_empty());
        let list_width = f32::from(data.scroll.0.borrow().base_handle.bounds().size.width);
        if list_width <= 0. {
            return; // Not painted yet; keep the default until it is.
        }
        let wrap = comment_wrap_cols(list_width, data.mode, char_width);
        if wrap == data.comment_wrap {
            return;
        }
        data.comment_wrap = wrap;
        if has_threads {
            data.rebuild_rows_anchored();
        }
    }

    /// Reveal all hidden lines of one gap in an upgraded file, keeping the
    /// viewport stable: when the expansion happens above the visible top row,
    /// the scroll offset shifts down by exactly the inserted height.
    fn expand_gap(&mut self, file_ix: usize, gap_ix: usize, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let Some(upgrade) = data.upgrades.get_mut(&file_ix) else {
            return;
        };
        if !upgrade.expanded.insert(gap_ix) {
            return;
        }
        let gap_row = data.rows.iter().position(|row| {
            matches!(row, Row::Gap { file_ix: f, gap_ix: g, .. } if *f == file_ix && *g == gap_ix)
        });
        let old_len = data.rows.len();
        data.set_rows(build_rows(
            &data.diff,
            data.mode,
            &data.upgrades,
            data.comments.as_ref(),
            data.comments_visible,
            data.comment_wrap,
        ));
        // The gap row is replaced by its hidden context rows (plus any
        // comment threads anchored inside them): the row-count delta is
        // exactly what got inserted.
        let inserted = data.rows.len().saturating_sub(old_len);
        data.selection = None;
        if let Some(gap_row) = gap_row {
            if data.cursor > gap_row {
                data.cursor += inserted;
            }
            let scroll = data.scroll.0.borrow();
            let offset = scroll.base_handle.offset();
            // offset.y is negative when scrolled down.
            let top_row = (f32::from(-offset.y) / row_height()).floor() as usize;
            if gap_row < top_row {
                scroll
                    .base_handle
                    .set_offset(point(offset.x, offset.y - px(inserted as f32 * row_height())));
            }
        }
        cx.notify();
    }

    pub(crate) fn toggle_view(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        // Best-effort position preservation: stay on the same file.
        let file_pos = data.file_rows.iter().rposition(|&ix| ix <= data.cursor);
        data.selection = None;
        data.mode = match data.mode {
            ViewMode::Unified => ViewMode::Split,
            ViewMode::Split => ViewMode::Unified,
        };
        data.set_rows(build_rows(
            &data.diff,
            data.mode,
            &data.upgrades,
            data.comments.as_ref(),
            data.comments_visible,
            data.comment_wrap,
        ));
        let target = file_pos
            .and_then(|pos| data.file_rows.get(pos).copied())
            .unwrap_or(0);
        self.jump(target, cx);
    }

    pub(crate) fn jump(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(data) = self.active_data_mut() {
            data.cursor = ix;
            data.scroll.scroll_to_item_strict(ix, ScrollStrategy::Top);
        }
        cx.notify();
    }

    pub(crate) fn jump_next(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().find(|&&ix| ix > cursor) {
            self.jump(ix, cx);
        }
    }

    pub(crate) fn jump_prev(&mut self, targets: &[usize], cx: &mut Context<Self>) {
        let Some(cursor) = self.active_data().map(|data| data.cursor) else {
            return;
        };
        if let Some(&ix) = targets.iter().rev().find(|&&ix| ix < cursor) {
            self.jump(ix, cx);
        }
    }

    /// Scrub the diff to the row under a minimap mouse position: invert the
    /// minimap scale (downsample-aware) to a fractional row, then center it
    /// by setting the scroll offset directly. Pure mouse math — verified
    /// manually, like the selection hit test.
    fn minimap_scrub_to(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(data) = self.active_data() else {
            return;
        };
        let total = data.rows.len();
        if total == 0 {
            return;
        }
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let pane_h = f32::from(bounds.size.height);
        if pane_h <= 0. {
            return;
        }
        // The minimap column is the same height as the list, so its y space
        // starts at the list's top.
        let (slot_h, group) = minimap_scale(total, pane_h);
        let px_per_row = slot_h / group as f32;
        let y = f32::from(position.y - bounds.top());
        let row = (y / px_per_row).clamp(0., (total - 1) as f32);
        let target = row * row_height() - (pane_h - row_height()) / 2.;
        let max_scroll = (total as f32 * row_height() - pane_h).max(0.);
        data.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(offset.x, px(-target.clamp(0., max_scroll))));
        cx.notify();
    }

    /// `c`: show/hide the comment rows of the active item, keeping the
    /// viewport anchored at the first visible diff row.
    pub(crate) fn toggle_comments(&mut self, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.comments.is_none() {
            return;
        }
        data.comments_visible = !data.comments_visible;
        data.rebuild_rows_anchored();
        cx.notify();
    }

    /// The minimap column: precomputed, coalesced quad runs plus one
    /// per-frame viewport rectangle, painted straight into a canvas (no text,
    /// no per-row elements). Mouse-downs stop propagation here so the pane's
    /// selection listeners never see them.
    fn render_minimap(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        div()
            .w(px(MINIMAP_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(Hsla::from(theme::crust()).opacity(0.5))
            .border_l_1()
            .border_color(theme::surface0())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    window.focus(&this.focus_handle);
                    this.minimap_scrub = true;
                    this.minimap_scrub_to(event.position, cx);
                }),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        let this = entity.read(cx);
                        let Some(data) = this.active_data() else {
                            return;
                        };
                        let total = data.rows.len();
                        let pane_h = f32::from(bounds.size.height);
                        if total == 0 || pane_h <= 0. {
                            return;
                        }
                        let layout = data.minimap_layout(pane_h);
                        let (x0, y0) = (f32::from(bounds.left()), f32::from(bounds.top()));
                        let usable = f32::from(bounds.size.width) - 2. * MINIMAP_PAD;
                        let half = (usable - MINIMAP_GAP) / 2.;
                        for run in &layout.runs {
                            let (x, w) = match run.lane {
                                MinimapLane::Full => (0., usable * run.frac),
                                MinimapLane::Left => (0., half * run.frac),
                                MinimapLane::Right => (half + MINIMAP_GAP, half * run.frac),
                            };
                            let y = run.start as f32 * layout.slot_h;
                            let h = if run.tick {
                                1.
                            } else {
                                (run.end - run.start) as f32 * layout.slot_h
                            };
                            // Full alpha-ish tints: these are 1-3px bars and
                            // need punch, unlike the row backgrounds.
                            let color: Hsla = match run.color {
                                MinimapColor::Added => Hsla::from(theme::green()).opacity(0.8),
                                MinimapColor::Removed => Hsla::from(theme::red()).opacity(0.8),
                                MinimapColor::Context => {
                                    Hsla::from(theme::overlay0()).opacity(0.35)
                                }
                                MinimapColor::Header => Hsla::from(theme::blue()).opacity(0.5),
                                MinimapColor::Gap => Hsla::from(theme::overlay0()).opacity(0.2),
                            };
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(px(x0 + MINIMAP_PAD + x), px(y0 + y)),
                                    size(px(w.max(1.)), px(h)),
                                ),
                                color,
                            ));
                        }
                        // Viewport indicator — the only per-frame math.
                        let offset_y = f32::from(-data.scroll.0.borrow().base_handle.offset().y);
                        let px_per_row = layout.slot_h / layout.group as f32;
                        let top_row = offset_y / row_height();
                        let visible = (pane_h / row_height()).min(total as f32 - top_row);
                        let vy = top_row * px_per_row;
                        let vh = (visible * px_per_row).max(3.);
                        window.paint_quad(
                            fill(
                                Bounds::new(
                                    point(bounds.left(), px(y0 + vy)),
                                    size(bounds.size.width, px(vh)),
                                ),
                                Hsla::from(theme::text()).opacity(0.08),
                            )
                            .border_widths(1.)
                            .border_color(Hsla::from(theme::overlay0()).opacity(0.4)),
                        );
                    },
                )
                .size_full(),
            )
            .into_any_element()
    }

    pub(crate) fn render_pane(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        let entity = cx.entity();
        let pane: gpui::AnyElement = match self.active_item() {
            None => centered_message("⌘T to open a PR or path".into(), theme::overlay0()),
            Some(item) => match &item.state {
                ItemState::Loading => centered_message("loading…".into(), theme::overlay0()),
                ItemState::Failed(msg) => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .p_8()
                    .child(
                        div()
                            .max_w(px(720.))
                            .text_color(theme::red())
                            .child(SharedString::from(msg.clone())),
                    )
                    .into_any_element(),
                ItemState::Ready(data) if data.source_view.is_some() => {
                    self.render_source_view(data.source_view.as_ref().unwrap(), cx)
                }
                ItemState::Ready(data) => div()
                    .size_full()
                    .relative()
                    .flex()
                    .font_family(MONO)
                    .text_size(px(text_size()))
                    .line_height(px(row_height()))
                    // Selection mouse listeners live on the diff pane only.
                    // While the palette is open its occluding backdrop keeps
                    // this hitbox from being hovered, so none of these fire;
                    // the palette.is_none() guard documents (and backstops)
                    // that. The Scrollbar stops propagation of its own
                    // mouse-downs and thumb drags before they reach us.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            if this.palette.is_some() {
                                return;
                            }
                            if event.modifiers.secondary() {
                                if let Some(target) = this.symbol_target_at(event.position, window)
                                {
                                    this.request_definition(target, cx);
                                    return;
                                }
                            }
                            window.focus(&this.focus_handle);
                            let char_width = this.char_width(window);
                            this.drag_anchor = this.pane_hit(event.position, char_width, None);
                            // A plain click clears; a selection only appears
                            // once the drag covers ≥ 1 char.
                            if let Some(data) = this.active_data_mut() {
                                if data.selection.take().is_some() {
                                    cx.notify();
                                }
                            }
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                        if !event.dragging() {
                            return;
                        }
                        // A scrub drag that started on the minimap keeps
                        // scrubbing wherever the pointer goes; it never
                        // becomes a text selection (drag_anchor stays None).
                        if this.minimap_scrub {
                            this.minimap_scrub_to(event.position, cx);
                            return;
                        }
                        let Some((side, anchor)) = this.drag_anchor else {
                            return;
                        };
                        let char_width = this.char_width(window);
                        let Some((_, head)) = this.pane_hit(event.position, char_width, Some(side))
                        else {
                            return;
                        };
                        let selection =
                            (head != anchor).then_some(Selection { side, anchor, head });
                        if let Some(data) = this.active_data_mut() {
                            if data.selection != selection {
                                data.selection = selection;
                                cx.notify();
                            }
                        }
                    }))
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                            this.minimap_scrub = false;
                        }),
                    )
                    // Releases outside the pane (drag ended over the sidebar,
                    // footer, …) must still end the drag.
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, _: &MouseUpEvent, _, _| {
                            this.drag_anchor = None;
                            this.minimap_scrub = false;
                        }),
                    )
                    .child(
                        uniform_list("diff", data.rows.len(), move |range, _window, cx| {
                            let this = entity.read(cx);
                            match this.active_data() {
                                Some(data) => {
                                    let sel = data.selection;
                                    range
                                        .filter_map(|ix| data.rows.get(ix).map(|row| (ix, row)))
                                        .map(|(ix, row)| {
                                            let row_sel = sel.and_then(|sel| {
                                                row_selection_range(&sel, ix, row)
                                                    .filter(|range| !range.is_empty())
                                                    .map(|range| (sel.side, range))
                                            });
                                            render_row(ix, row, row_sel, &entity)
                                        })
                                        .collect()
                                }
                                None => Vec::new(),
                            }
                        })
                        .track_scroll(data.scroll.clone())
                        .with_horizontal_sizing_behavior(match data.mode {
                            ViewMode::Unified => ListHorizontalSizingBehavior::Unconstrained,
                            ViewMode::Split => ListHorizontalSizingBehavior::FitList,
                        })
                        .h_full()
                        .flex_1()
                        .min_w_0(),
                    )
                    // Between the list and the Scrollbar, which paints over
                    // the column's right edge. Nothing is mounted when hidden
                    // — zero cost.
                    .when(self.minimap_visible, |pane| {
                        pane.child(self.render_minimap(cx))
                    })
                    .child(Scrollbar::new(&data.scroll))
                    // Hover "+" (add comment) overlay, absolutely positioned
                    // at the hovered row's y like the minimap viewport.
                    .children(self.render_plus(cx))
                    .when_some(self.render_hover(), |pane, hover| pane.child(hover))
                    .into_any_element(),
            },
        };
        pane
    }
}
