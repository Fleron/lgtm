//! The tracker's right-hand issue panel: header/breadcrumb, status chip,
//! metadata, tags, description, sub-issues (parents only) and activity +
//! comment box. Sidebar/main-pane tables live in `tracker_table.rs`.

use crate::comments::short_age;
use crate::dispatch::Target;
use crate::tracker::{
    dispatch_icon, dispatch_menu, oi, sub_issue_icon, tint, type_icon, ComposerMode, FocusedColumn,
};
use crate::tracker_table::label_pill;
use crate::urgency::{due_countdown, Priority};
use crate::{centered_message, theme, ReviewApp};
use gpui::{div, prelude::*, px, Context, MouseButton, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::menu::ContextMenuExt as _;
use gpui_component::{Disableable as _, Sizable as _};

impl ReviewApp {
    pub(crate) fn render_tracker_panel(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(number) = self.tracker.open_issue() else {
            return div().into_any_element();
        };
        self.tracker_ensure_item_loaded(number, cx);
        let Some(item) = self.tracker.item(number) else {
            let (message, color) = self.tracker_panel_placeholder(number);
            return centered_message(message, color);
        };
        let parent_crumb = item.detail.parent.clone();
        let is_sub_issue = self.tracker.panel_stack.len() > 1 || parent_crumb.is_some();
        let (type_glyph, type_color) = type_icon(item.detail.issue_type.as_deref(), &self.tracker.config);
        let now = crate::comments::now_unix();

        let header = div()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(theme::surface0())
            .when_some(parent_crumb.clone(), |row, parent| {
                row.child(
                    div()
                        .id("tracker-breadcrumb")
                        .flex()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .text_color(theme::overlay0())
                        .child(oi("issue-tracked-by-16", theme::overlay0()))
                        .child(SharedString::from(format!("#{}", parent.number)))
                        .child(SharedString::from("›"))
                        .on_click(cx.listener(|this, _, _, cx| this.tracker_back(cx))),
                )
            })
            .child(oi(type_glyph, type_color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(SharedString::from(format!("#{number} {}", item.detail.title))),
            )
            .child(
                div()
                    .id("tracker-dispatch")
                    .cursor_pointer()
                    .child(oi("terminal-16", theme::overlay0()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.tracker_open_dispatch(number, window, cx);
                    })),
            )
            .child(
                div()
                    .id("tracker-open-github")
                    .cursor_pointer()
                    .child(oi("link-external-16", theme::overlay0()))
                    .on_click(cx.listener(|this, _, _, cx| this.tracker_open_selected_on_github(cx))),
            )
            .child(
                div()
                    .id("tracker-expand")
                    .cursor_pointer()
                    .child(oi("screen-full-16", theme::overlay0()))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tracker.panel_expanded = !this.tracker.panel_expanded;
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .id("tracker-close")
                    .cursor_pointer()
                    .child(oi("x-16", theme::overlay0()))
                    .on_click(cx.listener(|this, _, _, cx| this.tracker_close_panel(cx))),
            );

        let status_row = self.render_status_row(number, cx);

        let due_text = item.due.map(|d| due_countdown(d, now));
        let overdue = item
            .due
            .is_some_and(|d| d < now);
        let mut meta = div().flex().flex_wrap().gap_x_4().gap_y_1().px_3().text_color(theme::subtext());
        if let Some(countdown) = due_text {
            meta = meta.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .when(overdue, |d| d.text_color(theme::red()))
                    .child(oi("calendar-16", if overdue { theme::red() } else { theme::overlay0() }))
                    .child(SharedString::from(countdown)),
            );
        }
        if let Some(milestone) = &item.detail.milestone {
            meta = meta.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(oi("milestone-16", theme::overlay0()))
                    .child(SharedString::from(milestone.title.clone())),
            );
        }
        if let Some(assignee) = item.detail.assignees.first() {
            meta = meta.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(oi("person-16", theme::overlay0()))
                    .child(SharedString::from(assignee.clone())),
            );
        }
        if let Some(priority) = item.priority {
            let label = match priority {
                Priority::High => "high",
                Priority::Medium => "medium",
                Priority::Low => "low",
            };
            meta = meta.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(oi("list-ordered-16", theme::overlay0()))
                    .child(SharedString::from(label)),
            );
        }
        meta = meta.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(oi("flame-16", theme::overlay0()))
                .child(SharedString::from(format!("{:.1}", item.urgency))),
        );
        if let Some(pr) = item.detail.linked_prs.first() {
            meta = meta.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(oi("git-pull-request-16", theme::overlay0()))
                    .child(SharedString::from(format!("#{}", pr.number))),
            );
        }

        let tags = if item.detail.labels.is_empty() {
            div().into_any_element()
        } else {
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .px_3()
                .child(oi("tag-16", theme::overlay0()))
                .children(item.detail.labels.iter().map(|l| label_pill(&l.name, &l.color)))
                .into_any_element()
        };

        let description = self.render_description(number, cx);
        let sub_issues = if is_sub_issue {
            div().into_any_element()
        } else {
            self.render_sub_issues(number, cx)
        };
        let activity = self.render_activity(number, now);
        let comment_box = div()
            .px_3()
            .pb_2()
            .flex()
            .flex_col()
            .gap_1()
            .child(Input::new(&self.tracker.comment_input).small())
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::overlay0())
                    .child(SharedString::from("⌘↵ to post")),
            );

        let mut body = div()
            .id("tracker-panel-scroll")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_3()
            .p_3()
            .child(status_row)
            .child(meta)
            .child(tags)
            .child(description)
            .child(sub_issues)
            .child(activity)
            .child(comment_box);
        if let Some(err) = &self.tracker.error {
            body = body.child(div().px_3().text_color(theme::red()).child(err.clone()));
        }

        div()
            .w(if self.tracker.panel_expanded { px(640.) } else { px(390.) })
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::base())
            .border_l_1()
            .border_color(theme::surface0())
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// The card's top row: the terminal glyph, the agent chip, the target
    /// chip (hidden when no ssh hosts were found) and the issue number.
    fn render_dispatch_header(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.tracker.dispatch else {
            return div().into_any_element();
        };
        let label = card.agent.label();
        div()
            .flex()
            .items_center()
            .gap_2()
            .child(oi("terminal-16", theme::overlay0()))
            .child(
                div()
                    .id("dispatch-agent")
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .border_1()
                    .border_color(theme::surface0())
                    .cursor_pointer()
                    .child(SharedString::from(label))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.tracker_toggle_dispatch_agent(cx);
                    })),
            )
            .when(!card.hosts.is_empty(), |row| {
                row.child(self.render_dispatch_target_chip(cx))
            })
            .child(
                div()
                    .flex_1()
                    .text_color(theme::overlay0())
                    .truncate()
                    .child(SharedString::from(format!("#{}", card.number))),
            )
            .into_any_element()
    }

    /// The target chip: the agent chip's own markup, so the two match, over
    /// a click-toggled list of "Local" plus every discovered ssh host. The
    /// list is deferred so it paints above the input below it.
    fn render_dispatch_target_chip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.tracker.dispatch else {
            return div().into_any_element();
        };
        let chip = div()
            .id("dispatch-target")
            .px_2()
            .py_0p5()
            .rounded_md()
            .border_1()
            .border_color(theme::surface0())
            .cursor_pointer()
            .child(SharedString::from(card.target.label().to_string()))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                this.tracker_toggle_dispatch_target_menu(cx);
            }));
        if !card.target_menu_open {
            return div().child(chip).into_any_element();
        }
        let current = card.target.clone();
        let targets: Vec<Target> = std::iter::once(Target::Local)
            .chain(card.hosts.iter().cloned().map(Target::Remote))
            .collect();
        let menu = div()
            .absolute()
            .top_full()
            .left_0()
            .mt_1()
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(theme::surface0())
            .bg(theme::mantle())
            .occlude()
            .children(targets.into_iter().map(|target| {
                let on = target == current;
                let label = target.label().to_string();
                div()
                    .id(SharedString::from(format!("dispatch-target-{label}")))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::surface0()).text_color(theme::text()))
                    .when(!on, |d| d.text_color(theme::subtext()))
                    .child(SharedString::from(label))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tracker_set_dispatch_target(target.clone(), cx);
                    }))
            }));
        div()
            .relative()
            .child(chip)
            .child(gpui::deferred(menu))
            .into_any_element()
    }

    /// The floating "dispatch an agent" card, rendered at the window's
    /// top-centre over an invisible backdrop that closes it on click.
    pub(crate) fn render_dispatch_card(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(card) = &self.tracker.dispatch else {
            return div().into_any_element();
        };
        let needs_root = card.target == Target::Local
            && self.tracker.config.dispatch_root.is_none();
        div()
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(96.))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.tracker_close_dispatch(window, cx);
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.tracker_close_dispatch(window, cx);
                }),
            )
            .child(
                div()
                    .w(px(560.))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_mouse_down(MouseButton::Right, |_, _, cx| cx.stop_propagation())
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
                    .child(self.render_dispatch_header(cx))
                    .child(Input::new(&card.input))
                    .when(needs_root, |body| {
                        body.child(
                            div()
                                .text_color(theme::overlay0())
                                .child(SharedString::from(crate::dispatch::NO_ROOT_HINT)),
                        )
                    })
                    .when_some(card.error.clone(), |body, err| {
                        body.child(div().text_color(theme::red()).child(err))
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
                                    .child(SharedString::from("⌘⏎ to dispatch")),
                            )
                            .child(
                                Button::new("dispatch-submit")
                                    .label("Dispatch")
                                    .primary()
                                    .small()
                                    .disabled(needs_root)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.tracker_submit_dispatch(window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_status_row(&self, number: u64, cx: &mut Context<Self>) -> impl IntoElement {
        self.render_status_chip(number, "tracker-status-chip", cx)
    }

    /// A status chip that opens/closes the shared status menu targeting
    /// `number` — used for the panel header's own issue and (with a
    /// different element id) for a highlighted sub-issue row.
    fn render_status_chip(
        &self,
        number: u64,
        element_id: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let Some(item) = self.tracker.item(number) else {
            return div().into_any_element();
        };
        let status = if item.status.is_empty() { "No status".to_string() } else { item.status.clone() };
        let chip = div()
            .id(element_id)
            .px_2()
            .rounded_sm()
            .bg(tint(theme::blue(), 0.18))
            .text_color(theme::blue())
            .cursor_pointer()
            .child(SharedString::from(status))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tracker.status_menu_target = if this.tracker.status_menu_target == Some(number) {
                    None
                } else {
                    Some(number)
                };
                cx.notify();
            }));
        if self.tracker.status_menu_target != Some(number) {
            return div().child(chip).into_any_element();
        }
        let Some(field) = self.tracker.status_field.clone() else {
            return div().child(chip).into_any_element();
        };
        let current = item.status.clone();
        let menu = div()
            .mt_1()
            .flex()
            .flex_col()
            .rounded_sm()
            .border_1()
            .border_color(theme::surface0())
            .bg(theme::mantle())
            .children(field.options.iter().map(|option| {
                let name = option.name.clone();
                let on = name == current;
                div()
                    .id(SharedString::from(format!("status-option-{element_id}-{name}")))
                    .px_2()
                    .py_0p5()
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::surface0()).text_color(theme::text()))
                    .when(!on, |d| d.text_color(theme::subtext()))
                    .child(SharedString::from(name.clone()))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tracker_set_status(number, name.clone(), cx);
                    }))
            }));
        div().flex().flex_col().child(chip).child(menu).into_any_element()
    }

    fn render_description(&mut self, number: u64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let editing = matches!(self.tracker.composer_mode, Some(ComposerMode::EditDescription));
        let label = div()
            .flex()
            .items_center()
            .gap_1()
            .text_color(theme::overlay0())
            .child(SharedString::from("Description"))
            .when(!editing, |d| {
                d.child(
                    div()
                        .id("tracker-edit-description")
                        .cursor_pointer()
                        .child(oi("pencil-16", theme::overlay0()))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.tracker_open_edit_description(number, window, cx)
                        })),
                )
            });
        if editing {
            if let Some(input) = self.tracker.composer_input.clone() {
                return div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(label)
                    .child(Input::new(&input).small())
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("save-description")
                                    .label("save")
                                    .xsmall()
                                    .primary()
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.tracker_submit_composer(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("cancel-description")
                                    .label("cancel")
                                    .xsmall()
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.tracker_close_composer(cx))),
                            ),
                    )
                    .into_any_element();
            }
        }
        let body = self
            .tracker
            .item(number)
            .map(|it| it.detail.body.clone())
            .unwrap_or_default();
        let body = if body.is_empty() { "no description".to_string() } else { body };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(label)
            .child(div().text_color(theme::subtext()).child(SharedString::from(body)))
            .into_any_element()
    }

    fn render_sub_issues(&mut self, number: u64, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(item) = self.tracker.item(number) else {
            return div().into_any_element();
        };
        let (done, total) = (item.detail.sub_issues_summary.completed, item.detail.sub_issues_summary.total);
        let pct = if total > 0 { (done as f32 / total as f32 * 100.0).round() } else { 0.0 };
        let label = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .text_color(theme::overlay0())
                    .child(oi("issue-tracks-16", theme::overlay0()))
                    .child(SharedString::from("Sub-issues"))
                    .child(
                        div()
                            .w(px(48.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(theme::surface0())
                            .child(div().h_full().rounded_full().bg(theme::green()).w(gpui::relative(pct / 100.0))),
                    )
                    .child(SharedString::from(format!("{done}/{total}"))),
            )
            .child(
                div()
                    .id("tracker-add-sub-issue")
                    .cursor_pointer()
                    .child(oi("plus-16", theme::text()))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.tracker_open_new_issue_composer(Some(number), window, cx)
                    })),
            );

        let sub_issues = item.detail.sub_issues.clone();
        let panel_focused = self.tracker.focus == FocusedColumn::Panel;
        let highlighted = self.tracker.panel_child_selected;
        let rows = div().flex().flex_col().children(sub_issues.iter().enumerate().map(|(ix, sub)| {
            let (icon, color) = sub_issue_icon(sub, &self.tracker.items, &self.tracker.config);
            let sub_number = sub.number;
            let selected = panel_focused && ix == highlighted;
            let editing_title =
                matches!(self.tracker.composer_mode, Some(ComposerMode::EditChildTitle(n)) if n == sub_number);
            let title: gpui::AnyElement = if editing_title {
                self.tracker
                    .composer_input
                    .clone()
                    .map(|input| Input::new(&input).small().into_any_element())
                    .unwrap_or_else(|| div().into_any_element())
            } else {
                div().truncate().child(SharedString::from(sub.title.clone())).into_any_element()
            };
            let group = SharedString::from(format!("sub-issue-{sub_number}"));
            let mut row = div()
                .id(group.clone())
                .group(group.clone())
                .flex()
                .items_center()
                .gap_2()
                .py_0p5()
                .cursor_pointer()
                .when(selected, |d| {
                    d.bg(tint(theme::green(), 0.16)).border_l_2().border_color(theme::green())
                })
                .child(oi(icon, color))
                .child(SharedString::from(format!("#{sub_number}")))
                .child(div().flex_1().min_w_0().child(title));
            if self.tracker.status_menu_target == Some(sub_number) {
                row = row.child(self.render_status_chip(
                    sub_number,
                    "tracker-child-status-chip",
                    cx,
                ));
            }
            row = row.child(dispatch_icon(sub_number, &group, selected, cx));
            if !editing_title {
                row = row
                    .on_click(cx.listener(move |this, _, _, cx| this.tracker_drill_into(sub_number, cx)));
            }
            row.context_menu(dispatch_menu(sub_number, cx))
        }));

        let adding = matches!(self.tracker.composer_mode, Some(ComposerMode::NewIssue { parent: Some(p) }) if p == number);
        let mut column = div().flex().flex_col().gap_1().child(label).child(rows);
        if adding {
            if let Some(input) = self.tracker.composer_input.clone() {
                column = column.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(oi("plus-16", theme::overlay0()))
                        .child(Input::new(&input).small()),
                );
            }
        }
        column.into_any_element()
    }

    fn render_activity(&self, number: u64, now: i64) -> gpui::AnyElement {
        let Some(item) = self.tracker.item(number) else {
            return div().into_any_element();
        };
        let mut column = div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_color(theme::overlay0()).child(SharedString::from("Activity")));
        for comment in &item.detail.comments {
            column = column.child(
                div()
                    .flex()
                    .gap_2()
                    .text_color(theme::subtext())
                    .child(div().text_color(theme::text()).child(SharedString::from(comment.author.clone())))
                    .child(div().flex_1().min_w_0().truncate().child(SharedString::from(comment.body.clone())))
                    .child(
                        div()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(short_age(&comment.created_at, now))),
                    ),
            );
        }
        column.into_any_element()
    }

    pub(crate) fn tracker_open_edit_description(&mut self, number: u64, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(item) = self.tracker.item(number) else {
            return;
        };
        let body = item.detail.body.clone();
        let input = cx.new(|cx| InputState::new(window, cx).multi_line(true).default_value(body));
        let subscription = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: true }) {
                this.tracker_submit_composer(window, cx);
            }
        });
        self.tracker.composer_mode = Some(ComposerMode::EditDescription);
        self.tracker.composer_input = Some(input.clone());
        self.tracker.composer_subscription = Some(subscription);
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn tracker_open_new_issue_composer(
        &mut self,
        parent: Option<u64>,
        window: &mut gpui::Window,
        cx: &mut Context<Self>,
    ) {
        if self.tracker.open_issue().is_none() && parent.is_none() {
            // `n` outside the panel still needs a repo to file the issue
            // against.
            if self.tracker.owner.is_empty() {
                return;
            }
        }
        let placeholder = if parent.is_some() { "new sub-issue title…" } else { "new issue title…" };
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        let subscription = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: false }) {
                this.tracker_submit_composer(window, cx);
            }
        });
        self.tracker.composer_mode = Some(ComposerMode::NewIssue { parent });
        self.tracker.composer_input = Some(input.clone());
        self.tracker.composer_subscription = Some(subscription);
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn tracker_submit_composer(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(mode) = self.tracker.composer_mode else {
            return;
        };
        let Some(input) = self.tracker.composer_input.clone() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            self.tracker_close_composer(cx);
            return;
        }
        match mode {
            ComposerMode::EditDescription => {
                if let Some(number) = self.tracker.open_issue() {
                    self.tracker_update_description(number, text, cx);
                }
            }
            ComposerMode::NewIssue { parent: Some(parent) } => {
                self.tracker_add_sub_issue(parent, text, cx);
            }
            ComposerMode::NewIssue { parent: None } => {
                self.tracker_create_issue(text, cx);
            }
            ComposerMode::EditChildTitle(number) => {
                self.tracker_update_child_title(number, text, cx);
            }
        }
        let _ = window;
        self.tracker_close_composer(cx);
    }

    pub(crate) fn tracker_submit_comment(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let Some(number) = self.tracker.open_issue() else {
            return;
        };
        let text = self.tracker.comment_input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }
        self.tracker
            .comment_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        self.tracker.error = None;
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { gh::post_issue_comment(&owner, &repo, number, &text) })
                .await;
            this.update(cx, |app, cx| {
                match result {
                    Ok(()) => app.tracker_refresh_issue(number, cx),
                    Err(err) => {
                        app.tracker.error = Some(format!("comment failed: {err:#}").into());
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn tracker_update_description(&mut self, number: u64, body: String, cx: &mut Context<Self>) {
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        let old_body = self.tracker.item(number).map(|it| it.detail.body.clone()).unwrap_or_default();
        if let Some(item) = self.tracker.items.iter_mut().find(|it| it.number == number) {
            item.detail.body = body.clone();
        }
        self.tracker.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { gh::update_issue_body(&owner, &repo, number, &body) })
                .await;
            this.update(cx, |app, cx| {
                if let Err(err) = result {
                    if let Some(item) = app.tracker.items.iter_mut().find(|it| it.number == number) {
                        item.detail.body = old_body;
                    }
                    app.tracker.error = Some(format!("description update failed: {err:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Inline sub-issue title edit (`e` on a highlighted panel row): updates
    /// both the parent's `sub_issues` entry and the child's own row (it may
    /// also be a board item in its own right), reverting both on failure.
    fn tracker_update_child_title(&mut self, child_number: u64, title: String, cx: &mut Context<Self>) {
        let Some(parent_number) = self.tracker.open_issue() else {
            return;
        };
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        let mut old_title = String::new();
        if let Some(parent) = self.tracker.items.iter_mut().find(|it| it.number == parent_number) {
            if let Some(sub) = parent.detail.sub_issues.iter_mut().find(|s| s.number == child_number) {
                old_title = sub.title.clone();
                sub.title = title.clone();
            }
        }
        if let Some(child) = self.tracker.items.iter_mut().find(|it| it.number == child_number) {
            child.detail.title = title.clone();
        }
        self.tracker.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { gh::update_issue_title(&owner, &repo, child_number, &title) })
                .await;
            this.update(cx, |app, cx| {
                if let Err(err) = result {
                    if let Some(parent) = app.tracker.items.iter_mut().find(|it| it.number == parent_number) {
                        if let Some(sub) = parent.detail.sub_issues.iter_mut().find(|s| s.number == child_number) {
                            sub.title = old_title.clone();
                        }
                    }
                    if let Some(child) = app.tracker.items.iter_mut().find(|it| it.number == child_number) {
                        child.detail.title = old_title.clone();
                    }
                    app.tracker.error = Some(format!("title update failed: {err:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn tracker_add_sub_issue(&mut self, parent: u64, title: String, cx: &mut Context<Self>) {
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        self.tracker.error = None;
        cx.spawn(async move |this, cx| {
            let owner2 = owner.clone();
            let repo2 = repo.clone();
            let result = cx
                .background_spawn(async move {
                    let number = gh::create_issue(&owner2, &repo2, &title, "", &[], None)?;
                    gh::add_sub_issue(&owner2, &repo2, parent, number)?;
                    anyhow::Ok(number)
                })
                .await;
            this.update(cx, |app, cx| match result {
                Ok(number) => {
                    app.tracker_ensure_item_loaded(number, cx);
                    app.tracker_refresh_issue(parent, cx);
                }
                Err(err) => {
                    app.tracker.error = Some(format!("add sub-issue failed: {err:#}").into());
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn tracker_create_issue(&mut self, title: String, cx: &mut Context<Self>) {
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        let Some(board) = self.tracker.board.clone() else {
            return;
        };
        let first_queue_status = self.tracker_first_status_in(crate::urgency::Column::Queue);
        self.tracker.error = None;
        cx.spawn(async move |this, cx| {
            let owner2 = owner.clone();
            let repo2 = repo.clone();
            let result = cx
                .background_spawn(async move {
                    let number = gh::create_issue(&owner2, &repo2, &title, "", &[], None)?;
                    let issue_url = format!("https://github.com/{owner2}/{repo2}/issues/{number}");
                    gh::add_item_to_project(&owner2, board.number, &issue_url)?;
                    let items = gh::project_items(&owner2, board.number)?;
                    let project_item_id = items
                        .into_iter()
                        .find(|it| it.content.number == number)
                        .map(|it| it.id);
                    let detail = gh::issue_detail(&owner2, &repo2, number)?;
                    anyhow::Ok((number, project_item_id, detail))
                })
                .await;
            this.update(cx, |app, cx| {
                match result {
                    Ok((number, project_item_id, detail)) => {
                        if let Some(project_item_id) = project_item_id {
                            app.tracker.items.push(crate::tracker::TrackerItem {
                                project_item_id,
                                number,
                                status: String::new(),
                                due: None,
                                priority: None,
                                detail,
                                urgency: 0.0,
                            });
                            app.recompute_tracker_urgency();
                            if let Some(status) = first_queue_status {
                                app.tracker_set_status(number, status, cx);
                            }
                        }
                    }
                    Err(err) => {
                        app.tracker.error = Some(format!("create issue failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Re-fetch one issue's detail after a write that isn't safely
    /// optimistic locally (comments, sub-issue links): simpler and more
    /// reliable than hand-updating every derived field.
    fn tracker_refresh_issue(&mut self, number: u64, cx: &mut Context<Self>) {
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { gh::issue_detail(&owner, &repo, number) }).await;
            this.update(cx, |app, cx| {
                match result {
                    Ok(detail) => {
                        if let Some(item) = app.tracker.items.iter_mut().find(|it| it.number == number) {
                            item.detail = detail;
                        }
                        app.recompute_tracker_urgency();
                    }
                    Err(err) => {
                        app.tracker.error = Some(format!("refresh failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
