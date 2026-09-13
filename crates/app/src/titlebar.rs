use crate::items::{dir_name, ItemData, ItemState, Source};
use crate::theme;
use crate::{app_title, ReviewApp};
use gpui::{div, prelude::*, px, Context, Hsla, SharedString};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    tag::Tag,
    Icon, IconName, Sizable as _, TitleBar,
};

/// while a review is still required (a green check once it isn't), plus a
/// passed/total CI count when the PR has any checks at all. Used in both the
/// subscribed-PRs feed (before a PR is opened) and the open-items list
/// (after), so it takes plain fields rather than `PrSummary`/`PrMeta`.
pub(crate) fn review_ci_indicator(review_decision: &str, checks: &[gh::CheckRun]) -> gpui::AnyElement {
    let (icon, icon_color) = if review_decision == "REVIEW_REQUIRED" {
        (IconName::TriangleAlert, theme::peach())
    } else {
        (IconName::CircleCheck, theme::green())
    };
    let checks_text = (!checks.is_empty()).then(|| {
        let total = checks.len();
        let passed = checks.iter().filter(|c| c.passed()).count();
        let color = if passed == total {
            theme::green()
        } else {
            theme::red()
        };
        (color, format!("{passed}/{total}"))
    });
    div()
        .flex()
        .items_center()
        .gap_1()
        .flex_shrink_0()
        .child(Icon::new(icon).xsmall().text_color(icon_color))
        .when_some(checks_text, |row, (color, label)| {
            row.child(
                div()
                    .text_size(px(11.))
                    .text_color(Hsla::from(color))
                    .child(SharedString::from(label)),
            )
        })
        .into_any_element()
}

pub(crate) fn pr_titlebar_content(meta: &gh::PrMeta, cx: &mut Context<ReviewApp>) -> gpui::AnyElement {
    let (state_color, state_label) = if meta.is_draft {
        (theme::overlay0(), "draft")
    } else {
        match meta.state.as_str() {
            "OPEN" => (theme::green(), "open"),
            "MERGED" => (theme::mauve(), "merged"),
            "CLOSED" => (theme::red(), "closed"),
            other => (theme::overlay0(), other),
        }
    };
    let state: Hsla = state_color.into();
    // The PR's overall review decision, when it has one.
    let decision = match meta.review_decision.as_str() {
        "APPROVED" => Some((theme::green(), "approved")),
        "CHANGES_REQUESTED" => Some((theme::red(), "changes requested")),
        "REVIEW_REQUIRED" => Some((theme::peach(), "review required")),
        _ => None,
    };
    let ci = gh::ci_summary(&meta.status_check_rollup).map(|(passed, total, state)| {
        let color = match state {
            gh::CiState::Passed => theme::green(),
            gh::CiState::InProgress => theme::peach(),
            gh::CiState::Failed => theme::red(),
        };
        (color, format!("{passed}/{total}"))
    });
    let url = meta.url.clone();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(state.opacity(0.15), state, state.opacity(0.4))
                        .small()
                        .child(SharedString::from(state_label.to_string())),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(meta.title.clone())),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .child(SharedString::from(format!("#{}", meta.number))),
                )
                .child(
                    div()
                        .text_color(theme::subtext())
                        .whitespace_nowrap()
                        .child(SharedString::from(format!("by {}", meta.author.login))),
                )
                .when_some(decision, |row, (color, label)| {
                    let tint: Hsla = color.into();
                    row.child(
                        Tag::custom(tint.opacity(0.15), tint, tint.opacity(0.4))
                            .small()
                            .child(SharedString::from(label.to_string())),
                    )
                })
                .when_some(ci, |row, (color, label)| {
                    let tint: Hsla = color.into();
                    row.child(
                        Tag::custom(tint.opacity(0.15), tint, tint.opacity(0.4))
                            .small()
                            .child(SharedString::from(label)),
                    )
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(
                    div()
                        .text_color(theme::overlay0())
                        .child(SharedString::from(format!(
                            "{} ← {}",
                            meta.base_ref_name, meta.head_ref_name
                        ))),
                )
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", meta.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", meta.deletions))),
                )
                .child(
                    Button::new("open-in-browser")
                        .icon(IconName::ExternalLink)
                        .ghost()
                        .xsmall()
                        .on_click(move |_, _, cx| cx.open_url(&url)),
                )
                .when(meta.state == "OPEN", |row| {
                    row.child(
                        Button::new("submit-review")
                            .label("Review")
                            .primary()
                            .xsmall()
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_review(window, cx);
                            })),
                    )
                }),
        )
        .into_any_element()
}

pub(crate) fn local_titlebar_content(
    item_id: u64,
    src: &git::LocalSource,
    data: &ItemData,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    let blue: Hsla = theme::blue().into();
    let repo_root = src.repo_root.clone();
    div()
        .flex()
        .items_center()
        .flex_1()
        .min_w_0()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .min_w_0()
                .flex_1()
                .child(
                    Tag::custom(blue.opacity(0.15), blue, blue.opacity(0.4))
                        .small()
                        .child(SharedString::from("local")),
                )
                .child(
                    div()
                        .font_weight(gpui::FontWeight::BOLD)
                        .truncate()
                        .child(SharedString::from(dir_name(&src.repo_root))),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .flex_shrink_0()
                .pr_3()
                .child(
                    div()
                        .id("local-base-picker")
                        .px_1()
                        .rounded_sm()
                        .cursor_pointer()
                        .text_color(theme::overlay0())
                        .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.6)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_local_base_palette(item_id, repo_root.clone(), window, cx);
                        }))
                        .child(SharedString::from(format!("{} ← {}", src.base_label, src.branch))),
                )
                .child(
                    div()
                        .text_color(theme::green())
                        .child(SharedString::from(format!("+{}", data.additions))),
                )
                .child(
                    div()
                        .text_color(theme::red())
                        .child(SharedString::from(format!("−{}", data.deletions))),
                )
                .child(
                    Button::new("copy-local-review-prompt")
                        .label("Copy prompt")
                        .primary()
                        .xsmall()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.copy_local_review_prompt(cx);
                        })),
                ),
        )
        .into_any_element()
}

impl ReviewApp {
    pub(crate) fn render_titlebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let content: gpui::AnyElement = match self.active_item() {
            None => app_title(None),
            Some(item) => match &item.state {
                ItemState::Ready(data) => match &item.source {
                    Source::Pr(_) => match &data.pr_meta {
                        Some(meta) => pr_titlebar_content(meta, cx),
                        None => app_title(None),
                    },
                    Source::Local(src) => local_titlebar_content(item.id, src, data, cx),
                },
                ItemState::Loading => app_title(Some(format!("loading {}…", item.primary()))),
                ItemState::Failed(_) => app_title(Some(format!("{} — failed", item.primary()))),
            },
        };
        let note: Option<SharedString> = self.active_item().and_then(|item| {
            if item.reloading {
                Some("reloading…".into())
            } else {
                item.refresh_error
                    .as_ref()
                    .map(|err| SharedString::from(format!("refresh failed: {err}")))
            }
        });
        TitleBar::new()
            .text_size(px(13.))
            .child(content)
            .when_some(self.render_lsp_status(cx), |bar, status| bar.child(status))
            .when_some(note, |bar, note| {
                bar.child(
                    div()
                        .max_w(px(280.))
                        .truncate()
                        .text_color(theme::overlay0())
                        .pr_3()
                        .child(note),
                )
            })
    }

}
