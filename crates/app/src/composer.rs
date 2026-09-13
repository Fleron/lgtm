use crate::comments::CommentSide;
use crate::items::{ItemState, Source};
use crate::theme;
use crate::{local_review_prompt, row_height, seed_mentions, MentionProvider, ReviewApp};
use gpui::{
    div, prelude::*, px, ClipboardItem, Context, Entity, Hsla, MouseButton, SharedString,
    Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    Disableable as _, Sizable as _,
};
use std::rc::Rc;

/// The floating comment/reply composer. One at a time, targeting a specific
/// anchor of a specific item (it survives item switches but only renders —
/// and can only post — for the item it was opened on).
pub(crate) struct Composer {
    pub(crate) item_id: u64,
    /// Some(root comment id) = reply to that thread; None = new top-level
    /// comment at (path, side, line) against `commit_id`.
    pub(crate) reply_to: Option<u64>,
    pub(crate) commit_id: String,
    pub(crate) path: String,
    pub(crate) side: CommentSide,
    pub(crate) line: u64,
    /// Set when the comment spans multiple lines (a drag-selection covered
    /// more than one row when "+" was clicked); anchors at `line`, the end.
    pub(crate) start_line: Option<u64>,
    /// Display row the composer is anchored beneath (best effort; goes stale
    /// harmlessly if rows rebuild while it is open).
    pub(crate) row_ix: usize,
    pub(crate) input: Entity<InputState>,
    pub(crate) error: Option<SharedString>,
    pub(crate) in_flight: bool,
    pub(crate) _subscription: Subscription,
}

/// The "submit review" modal: a verdict (approve / request changes /
/// comment) plus an optional body, targeting the item it was opened on.
pub(crate) struct ReviewDialog {
    pub(crate) item_id: u64,
    pub(crate) verdict: gh::ReviewVerdict,
    pub(crate) input: Entity<InputState>,
    pub(crate) error: Option<SharedString>,
    pub(crate) in_flight: bool,
    pub(crate) _subscription: Subscription,
}

impl ReviewApp {
    pub(crate) fn open_composer(
        &mut self,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        start_line: Option<u64>,
        row_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.get(self.active) else {
            return;
        };
        let item_id = item.id;
        let pr_loc = match &item.source {
            Source::Pr(loc) => Some(loc.clone()),
            Source::Local(_) => None,
        };
        let commit_id = self
            .active_data()
            .and_then(|data| data.pr_meta.as_ref())
            .map(|meta| meta.head_ref_oid.clone())
            .unwrap_or_default();
        if reply_to.is_none() {
            match &item.source {
                Source::Pr(_) if commit_id.is_empty() => return,
                Source::Pr(_) | Source::Local(_) => {}
            }
        }
        self.composer_gen += 1;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("leave a comment…")
        });
        // cmd-enter: the input's own `secondary-enter` binding emits
        // PressEnter { secondary: true } (after inserting a newline, which
        // submit trims away).
        let _subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: true }) {
                    this.submit_composer(window, cx);
                }
            });
        input.update(cx, |state, cx| state.focus(window, cx));
        // Wire up @-mention autocomplete for PR items: seed the pool with known
        // participants now, attach the provider, then fetch the full list once.
        if let Some(loc) = pr_loc {
            if let Some(data) = self.active_data() {
                let mentions = data.mentions.clone();
                seed_mentions(&mentions, data);
                input.update(cx, |state, _| {
                    state.lsp.completion_provider = Some(Rc::new(MentionProvider { users: mentions }));
                });
            }
            self.ensure_mentions(item_id, loc, cx);
        }
        self.composer = Some(Composer {
            item_id,
            reply_to,
            commit_id,
            path,
            side,
            line,
            start_line,
            row_ix,
            input,
            error: None,
            in_flight: false,
            _subscription,
        });
        cx.notify();
    }

    /// Fetch the repo's mentionable users once per item, merging them into the
    /// item's shared mention pool (which an open composer's completion provider
    /// reads live). Best-effort: on failure the seeded participants remain.
    pub(crate) fn ensure_mentions(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        if data.mentions_fetched {
            return;
        }
        data.mentions_fetched = true;
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_mentionable_users(&loc) })
                .await;
            let Ok(users) = fetched else {
                return;
            };
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &item.state else {
                    return;
                };
                let mut pool = data.mentions.borrow_mut();
                for user in users {
                    match pool
                        .iter_mut()
                        .find(|m| m.login.eq_ignore_ascii_case(&user.login))
                    {
                        // Upgrade a seeded (name-less) participant in place.
                        Some(existing) if existing.name.is_none() => existing.name = user.name,
                        Some(_) => {}
                        None => pool.push(user),
                    }
                }
                drop(pool);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn close_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.composer.take().is_some() {
            self.composer_gen += 1;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Submit the composer's comment (or reply): local items update in-memory
    /// drafts immediately; PR items post via gh on the background executor.
    pub(crate) fn submit_composer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(composer) = &self.composer else {
            return;
        };
        if composer.in_flight {
            return;
        }
        let body = composer.input.read(cx).value().trim().to_string();
        if body.is_empty() {
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == composer.item_id) else {
            return;
        };
        let item_id = item.id;
        let source = item.source.clone();
        let gen = self.composer_gen;
        let (reply_to, commit_id, path, side, line, start_line) = {
            let composer = self.composer.as_mut().unwrap();
            composer.in_flight = true;
            composer.error = None;
            (
                composer.reply_to,
                composer.commit_id.clone(),
                composer.path.clone(),
                composer.side,
                composer.line,
                composer.start_line,
            )
        };
        if let Source::Local(_) = source {
            self.add_local_comment(item_id, reply_to, path, side, line, start_line, body, cx);
            if self.composer_gen == gen {
                self.close_composer(window, cx);
            }
            return;
        }
        let Source::Pr(loc) = source else {
            return;
        };
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let post_loc = loc.clone();
            let result = cx
                .background_spawn(async move {
                    match reply_to {
                        Some(root_id) => gh::post_reply(&post_loc, root_id, &body),
                        None => gh::post_review_comment(
                            &post_loc,
                            &commit_id,
                            &path,
                            side.api_str(),
                            line,
                            start_line,
                            &body,
                        ),
                    }
                })
                .await;
            this.update_in(cx, |app, window, cx| {
                if app.composer_gen != gen {
                    return; // The composer was closed or retargeted meanwhile.
                }
                match result {
                    Ok(()) => {
                        app.close_composer(window, cx);
                        app.refetch_comments(item_id, loc, cx);
                    }
                    Err(err) => {
                        if let Some(composer) = &mut app.composer {
                            composer.in_flight = false;
                            composer.error = Some(format!("{err:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn add_local_comment(
        &mut self,
        item_id: u64,
        reply_to: Option<u64>,
        path: String,
        side: CommentSide,
        line: u64,
        start_line: Option<u64>,
        body: String,
        cx: &mut Context<Self>,
    ) {
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let Some(local) = &mut data.local_review else {
            return;
        };
        local.add_comment(reply_to, path, side, line, start_line, body);
        data.comments = Some(local.index());
        data.rebuild_rows_anchored();
        cx.notify();
    }

    pub(crate) fn copy_local_review_prompt(&self, cx: &mut Context<Self>) {
        let Some(item) = self.active_item() else {
            return;
        };
        let Source::Local(src) = &item.source else {
            return;
        };
        let ItemState::Ready(data) = &item.state else {
            return;
        };
        let Some(local) = &data.local_review else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(local_review_prompt(src, local)));
    }

    /// Open the "submit review" modal for the active item (PR items only).
    pub(crate) fn open_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.active_item() else {
            return;
        };
        let Source::Pr(_) = item.source else {
            return;
        };
        let ItemState::Ready(_) = item.state else {
            return;
        };
        let item_id = item.id;
        self.review_gen += 1;
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("leave a review comment… (optional when approving)")
        });
        let _subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: true }) {
                    this.submit_review(window, cx);
                }
            });
        input.update(cx, |state, cx| state.focus(window, cx));
        self.review = Some(ReviewDialog {
            item_id,
            verdict: gh::ReviewVerdict::Approve,
            input,
            error: None,
            in_flight: false,
            _subscription,
        });
        cx.notify();
    }

    pub(crate) fn close_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.review.take().is_some() {
            self.review_gen += 1;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    /// Submit the review via gh on the background executor. Success closes
    /// the dialog and refetches the PR meta (so the titlebar's decision tag
    /// updates); failure surfaces gh's stderr inline in the dialog.
    pub(crate) fn submit_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        if review.in_flight {
            return;
        }
        let body = review.input.read(cx).value().trim().to_string();
        let verdict = review.verdict;
        // GitHub rejects bodyless request-changes/comment reviews; fail
        // locally with a clearer message.
        if body.is_empty() && verdict != gh::ReviewVerdict::Approve {
            if let Some(review) = &mut self.review {
                review.error = Some("this review type needs a comment".into());
            }
            cx.notify();
            return;
        }
        let Some(item) = self.items.iter().find(|item| item.id == review.item_id) else {
            return;
        };
        let Source::Pr(loc) = &item.source else {
            return;
        };
        let loc = loc.clone();
        let item_id = item.id;
        let gen = self.review_gen;
        if let Some(review) = &mut self.review {
            review.in_flight = true;
            review.error = None;
        }
        cx.notify();
        cx.spawn_in(window, async move |this, cx| {
            let submit_loc = loc.clone();
            let result = cx
                .background_spawn(async move { gh::submit_review(&submit_loc, verdict, &body) })
                .await;
            this.update_in(cx, |app, window, cx| {
                if app.review_gen != gen {
                    return; // The dialog was closed and reopened meanwhile.
                }
                match result {
                    Ok(()) => {
                        app.close_review(window, cx);
                        app.refetch_meta(item_id, loc, cx);
                    }
                    Err(err) => {
                        if let Some(review) = &mut app.review {
                            review.in_flight = false;
                            review.error = Some(format!("{err:#}").into());
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The floating composer card: absolutely positioned at root level (so
    /// its input sits outside the "ReviewApp" key context and plain letters
    /// stay text), anchored near the target line's y, clamped into the pane.
    pub(crate) fn render_composer(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        let Some(composer) = &self.composer else {
            return empty();
        };
        // Only render for the item it was opened on, and only while active.
        if self.items.get(self.active).map(|item| item.id) != Some(composer.item_id) {
            return empty();
        }
        let Some(data) = self.active_data() else {
            return empty();
        };
        let (bounds, offset) = {
            let state = data.scroll.0.borrow();
            (state.base_handle.bounds(), state.base_handle.offset())
        };
        let pane_h = f32::from(bounds.size.height);
        let row_y = composer.row_ix as f32 * row_height() + f32::from(offset.y);
        let y = f32::from(bounds.top()) + (row_y + row_height()).clamp(8., (pane_h - 250.).max(8.));
        let x = (f32::from(bounds.left()) + 72.).min((f32::from(bounds.right()) - 528.).max(8.));
        let action = if composer.reply_to.is_some() {
            "Reply"
        } else {
            "Comment"
        };
        let line_label = match composer.start_line {
            Some(start) => format!("{start}-{}", composer.line),
            None => composer.line.to_string(),
        };
        let target = format!(
            "{}{}:{} ({})",
            if composer.reply_to.is_some() {
                "reply · "
            } else {
                ""
            },
            composer.path,
            line_label,
            composer.side.api_str()
        );
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(520.))
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            // The input propagates Escape when it has nothing of its own to
            // dismiss; catch it here (before the root's handler) to cancel.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                this.close_composer(window, cx);
            }))
            .rounded_lg()
            .border_1()
            .border_color(theme::surface0())
            .bg(theme::mantle())
            .shadow_lg()
            .p_2()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(12.))
            .child(
                div()
                    .text_color(theme::overlay0())
                    .truncate()
                    .child(SharedString::from(target)),
            )
            .child(Input::new(&composer.input))
            .when_some(composer.error.clone(), |card, err| {
                card.child(div().text_color(theme::red()).child(err))
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(11.))
                            .text_color(theme::overlay0())
                            .child(SharedString::from("⌘⏎ to submit")),
                    )
                    .child(
                        Button::new("composer-cancel")
                            .label("Cancel")
                            .ghost()
                            .small()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.close_composer(window, cx);
                            })),
                    )
                    .child(
                        Button::new("composer-submit")
                            .label(action)
                            .primary()
                            .small()
                            .disabled(composer.in_flight)
                            .loading(composer.in_flight)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_composer(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    /// The "submit review" modal: a dimming backdrop (click closes) over a
    /// centered card with the verdict picker, body input, and submit row.
    /// Root-level for the same reason as the composer: its input must sit
    /// outside the "ReviewApp" key context.
    pub(crate) fn render_review(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let empty = || div().into_any_element();
        let Some(review) = &self.review else {
            return empty();
        };
        // Only render for the item it was opened on, and only while active.
        if self.items.get(self.active).map(|item| item.id) != Some(review.item_id) {
            return empty();
        }
        let selected = review.verdict;
        let verdict_option =
            |label: &'static str, verdict: gh::ReviewVerdict, color: gpui::Rgba| {
                let tint: Hsla = color.into();
                div()
                    .id(label)
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .border_1()
                    .cursor_pointer()
                    .when(verdict == selected, |opt| {
                        opt.bg(tint.opacity(0.15))
                            .border_color(tint.opacity(0.6))
                            .text_color(color)
                    })
                    .when(verdict != selected, |opt| {
                        opt.border_color(theme::surface0())
                            .text_color(theme::subtext())
                            .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(review) = &mut this.review {
                            review.verdict = verdict;
                            review.error = None;
                            cx.notify();
                        }
                    }))
                    .child(SharedString::from(label))
            };
        let submit_label = match selected {
            gh::ReviewVerdict::Approve => "Approve",
            gh::ReviewVerdict::RequestChanges => "Request changes",
            gh::ReviewVerdict::Comment => "Comment",
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(120.))
            .bg(theme::palette_backdrop())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.close_review(window, cx);
                }),
            )
            .child(
                div()
                    .w(px(560.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // The input propagates Escape when it has nothing of its
                    // own to dismiss; catch it here to cancel the dialog.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.close_review(window, cx);
                    }))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::surface0())
                    .bg(theme::mantle())
                    .shadow_lg()
                    .p_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .text_size(px(12.))
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("Finish your review")),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(verdict_option(
                                "approve",
                                gh::ReviewVerdict::Approve,
                                theme::green(),
                            ))
                            .child(verdict_option(
                                "request changes",
                                gh::ReviewVerdict::RequestChanges,
                                theme::red(),
                            ))
                            .child(verdict_option(
                                "comment",
                                gh::ReviewVerdict::Comment,
                                theme::blue(),
                            )),
                    )
                    .child(Input::new(&review.input))
                    .when_some(review.error.clone(), |card, err| {
                        card.child(div().text_color(theme::red()).child(err))
                    })
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(11.))
                                    .text_color(theme::overlay0())
                                    .child(SharedString::from("⌘⏎ to submit")),
                            )
                            .child(
                                Button::new("review-cancel")
                                    .label("Cancel")
                                    .ghost()
                                    .small()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_review(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("review-submit")
                                    .label(submit_label)
                                    .primary()
                                    .small()
                                    .disabled(review.in_flight)
                                    .loading(review.in_flight)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.submit_review(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

}
