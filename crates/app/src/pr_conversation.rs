use crate::chat::CHAT_WIDTH;
use crate::comments::{now_unix, short_age};
use crate::items::ItemState;
use crate::theme;
use crate::{centered_message, ReviewApp, TopView};
use gpui::{div, prelude::*, px, Context, Hsla, Rgba, SharedString, Window};
use gpui_component::text::TextView;

// --- Read-only PR conversation ----------------------------------------------

/// One bubble in the conversation feed, borrowed from the item's loaded data.
enum ConversationEntry<'a> {
    Description,
    Comment(&'a gh::IssueComment),
    Review(&'a gh::PrReview),
}

/// The PR description, then its top-level comments and review verdicts in
/// chronological order. The description is pinned first rather than sorted in:
/// `gh::PrMeta` carries no creation time, and it predates everything else
/// anyway. Reviews without a verdict worth reading (`PENDING`, `DISMISSED`,
/// and bodiless `COMMENTED` ones, which are inline-only) are dropped.
fn build_timeline<'a>(
    comments: &'a [gh::IssueComment],
    reviews: &'a [gh::PrReview],
) -> Vec<ConversationEntry<'a>> {
    let mut rest: Vec<(&str, ConversationEntry<'a>)> = comments
        .iter()
        .map(|c| (c.created_at.as_str(), ConversationEntry::Comment(c)))
        .chain(
            reviews
                .iter()
                .filter(|r| match r.state.as_str() {
                    "APPROVED" | "CHANGES_REQUESTED" => true,
                    "COMMENTED" => !r.body.trim().is_empty(),
                    _ => false,
                })
                .map(|r| {
                    (
                        r.submitted_at.as_deref().unwrap_or(""),
                        ConversationEntry::Review(r),
                    )
                }),
        )
        .collect();
    // Both timestamps are ISO-8601 UTC, so they sort lexicographically.
    rest.sort_by(|(a, _), (b, _)| a.cmp(b));
    std::iter::once(ConversationEntry::Description)
        .chain(rest.into_iter().map(|(_, entry)| entry))
        .collect()
}

/// Label and color for a review verdict, or None for the states
/// [`build_timeline`] already filters out.
fn verdict(state: &str) -> Option<(&'static str, Rgba)> {
    match state {
        "APPROVED" => Some(("Approved", theme::green())),
        "CHANGES_REQUESTED" => Some(("Changes requested", theme::red())),
        "COMMENTED" => Some(("Commented", theme::overlay0())),
        _ => None,
    }
}

/// One bubble: author, verdict badge and age on a header line, then the body
/// as markdown. `id` must be unique per item and position (see the call site).
fn render_entry(
    entry: &ConversationEntry,
    meta: &gh::PrMeta,
    now: i64,
    id: SharedString,
    window: &mut Window,
    cx: &mut gpui::App,
) -> gpui::AnyElement {
    let (author, when, badge, body) = match entry {
        ConversationEntry::Description => (
            meta.author.login.clone(),
            None,
            None,
            if meta.body.trim().is_empty() {
                "*(no description)*".to_string()
            } else {
                meta.body.clone()
            },
        ),
        ConversationEntry::Comment(comment) => (
            comment.author.clone(),
            Some(short_age(&comment.created_at, now)),
            None,
            comment.body.clone(),
        ),
        ConversationEntry::Review(review) => (
            review.user.login.clone(),
            review.submitted_at.as_deref().map(|at| short_age(at, now)),
            verdict(&review.state),
            review.body.clone(),
        ),
    };

    let mut head = div().flex().items_center().gap_2().child(
        div()
            .font_weight(gpui::FontWeight::BOLD)
            .text_color(theme::text())
            .child(SharedString::from(author)),
    );
    if let Some((label, color)) = badge {
        head = head.child(
            div()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(Hsla::from(color).opacity(0.45))
                .bg(Hsla::from(color).opacity(0.1))
                .text_size(px(10.))
                .text_color(color)
                .child(SharedString::from(label)),
        );
    }
    head = head.child(div().flex_1());
    if let Some(when) = when {
        head = head.child(
            div()
                .flex_shrink_0()
                .text_size(px(10.))
                .text_color(theme::overlay0())
                .child(SharedString::from(when)),
        );
    }

    let mut bubble = div()
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .gap_1()
        .bg(theme::base())
        .rounded_md()
        .px_2()
        .py_1()
        .text_color(theme::text())
        .child(head);
    if !body.trim().is_empty() {
        bubble = bubble.child(TextView::markdown(id, body, window, cx));
    }
    bubble.into_any_element()
}

impl ReviewApp {
    /// `cmd-g`: toggle the PR conversation panel. Opening needs a loaded PR
    /// item — local diffs have no conversation — but closing always works.
    pub(crate) fn toggle_pr_conversation(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.top_view != TopView::Review {
            return;
        }
        // Only opening is gated: switching to a local item leaves the panel
        // showing its placeholder, and cmd-g still has to close it.
        if !self.pr_conversation_visible
            && self
                .active_data()
                .is_none_or(|data| data.pr_meta.is_none())
        {
            return;
        }
        self.pr_conversation_visible = !self.pr_conversation_visible;
        if self.pr_conversation_visible {
            // Chat, the terminal and this panel share the slot right of the
            // diff.
            self.chat_visible = false;
            self.terminal_visible = false;
            // cmd-g is global, so it also fires while the palette has focus.
            // Drop the palette instead of leaving it rendered but dead.
            self.palette = None;
            self.palette_gen += 1;
        }
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// The right-side conversation panel: header, then one bubble per entry
    /// with its body rendered as markdown. Read-only; nothing here mutates the
    /// item.
    pub(crate) fn render_pr_conversation(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let panel = div()
            .id("pr-conversation")
            .w(px(CHAT_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_l_1()
            .border_color(theme::surface0())
            .text_size(px(13.))
            // Every TextView focuses itself when clicked, which would leave
            // the diff pane's single-key bindings dead until the user clicked
            // back onto the diff. Take focus back on the way out.
            .on_click(cx.listener(|this, _, window: &mut Window, cx| {
                window.focus(&this.focus_handle);
                cx.notify();
            }));

        let loaded = self.active_item().and_then(|item| match &item.state {
            ItemState::Ready(data) => data.pr_meta.as_ref().map(|meta| (item.id, meta, &**data)),
            _ => None,
        });
        let Some((item_id, meta, data)) = loaded else {
            return panel
                .child(centered_message(
                    "open a pull request to see its conversation".into(),
                    theme::overlay0(),
                ))
                .into_any_element();
        };

        let header = div()
            .h(px(34.))
            .flex_shrink_0()
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .border_b_1()
            .border_color(theme::surface0())
            .child(
                div()
                    .flex_shrink_0()
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .child(SharedString::from(format!("#{}", meta.number))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme::subtext())
                    .child(SharedString::from(meta.title.clone())),
            );

        let now = now_unix();
        let mut column = div().w_full().flex().flex_col().gap_3().p_3();
        for (ix, entry) in build_timeline(&data.pr_comments, &data.pr_reviews)
            .into_iter()
            .enumerate()
        {
            // TextView keys its parse state by this id, so it carries the item
            // id: bare positional ids would show the previous PR's text for a
            // beat after switching items.
            let id = SharedString::from(format!("pr-conv-{item_id}-{ix}"));
            column = column.child(render_entry(&entry, meta, now, id, window, cx));
        }

        panel
            .child(header)
            .child(
                div()
                    .id(SharedString::from(format!(
                        "pr-conversation-body-{item_id}"
                    )))
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(column),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comment(created_at: &str) -> gh::IssueComment {
        gh::IssueComment {
            author: "octocat".into(),
            body: "hi".into(),
            created_at: created_at.into(),
        }
    }

    fn review(state: &str, body: &str, submitted_at: &str) -> gh::PrReview {
        gh::PrReview {
            id: 1,
            user: gh::Author {
                login: "octocat".into(),
            },
            body: body.into(),
            state: state.into(),
            submitted_at: Some(submitted_at.into()),
        }
    }

    #[test]
    fn timeline_pins_description_then_merges_by_time() {
        let comments = [comment("2026-01-02T00:00:00Z")];
        let reviews = [
            review("APPROVED", "", "2026-01-03T00:00:00Z"),
            review("CHANGES_REQUESTED", "no", "2026-01-01T00:00:00Z"),
        ];
        let timeline = build_timeline(&comments, &reviews);
        let shape: Vec<&str> = timeline
            .iter()
            .map(|entry| match entry {
                ConversationEntry::Description => "desc",
                ConversationEntry::Comment(_) => "comment",
                ConversationEntry::Review(r) => r.state.as_str(),
            })
            .collect();
        assert_eq!(
            shape,
            vec!["desc", "CHANGES_REQUESTED", "comment", "APPROVED"]
        );
    }

    #[test]
    fn timeline_drops_pending_dismissed_and_bodiless_comment_reviews() {
        let reviews = [
            review("PENDING", "draft", "2026-01-01T00:00:00Z"),
            review("DISMISSED", "gone", "2026-01-02T00:00:00Z"),
            review("COMMENTED", "   ", "2026-01-03T00:00:00Z"),
            review("COMMENTED", "a note", "2026-01-04T00:00:00Z"),
        ];
        let timeline = build_timeline(&[], &reviews);
        assert_eq!(timeline.len(), 2);
        assert!(matches!(timeline[1], ConversationEntry::Review(r) if r.body == "a note"));
    }
}
