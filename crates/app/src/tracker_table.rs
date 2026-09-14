//! Tracker view rendering: left sidebar (milestones, filter, queue table)
//! and the middle main pane (per-status groups). The issue panel lives in
//! `tracker_panel.rs`.

use crate::comments::now_unix;
use crate::tracker::{
    assignee_chip_label, milestone_summary, oi, queue_other_groups, row_style, tint, type_icon,
    FocusedColumn, TrackerItem,
};
use crate::urgency::due_countdown;
use crate::{centered_message, theme, ReviewApp};
use gpui::{div, prelude::*, px, Context, SharedString};
use gpui_component::input::Input;
use gpui_component::Sizable as _;

fn cell(width: f32, grow: bool) -> gpui::Div {
    let cell = div().flex_shrink_0().overflow_hidden();
    if grow {
        cell.flex_1().min_w(px(40.))
    } else {
        cell.w(px(width))
    }
}

fn header_row(cells: Vec<(f32, bool, gpui::AnyElement)>) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .py_1()
        .text_size(px(11.))
        .text_color(theme::overlay0())
        .border_b_1()
        .border_color(theme::surface0())
        .children(cells.into_iter().map(|(w, grow, child)| cell(w, grow).child(child)))
}

/// Title with the issue's labels on a second, indented line (same
/// treatment as the review sidebar's PR rows).
fn title_cell(title: &str, labels: &[gh::IssueLabel], color: gpui::Rgba) -> gpui::AnyElement {
    let tags = labels.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ");
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .child(div().truncate().text_color(color).child(SharedString::from(title.to_string())))
        .when(!labels.is_empty(), |d| {
            d.child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pl_2()
                    .text_size(px(10.))
                    .text_color(theme::subtext())
                    .child(oi("tag-16", theme::subtext()))
                    .child(div().truncate().child(SharedString::from(tags))),
            )
        })
        .into_any_element()
}

fn text_cell(text: impl Into<SharedString>, color: gpui::Rgba) -> gpui::AnyElement {
    div()
        .truncate()
        .text_color(color)
        .child(text.into())
        .into_any_element()
}

impl ReviewApp {
    pub(crate) fn render_tracker_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.tracker.loaded && !self.tracker.loading {
            self.tracker_load(cx);
        }
        let milestones = milestone_summary(&self.tracker.items);
        let filter = self.tracker_filter_text(cx);
        let ready_count = crate::tracker::queue_ready_rows(
            &self.tracker.items,
            &self.tracker.config,
            &self.tracker.selected_milestones,
            &filter,
        )
        .into_iter()
        .filter(|it| self.tracker_assignee_matches(it))
        .count();
        let other_rows: Vec<&TrackerItem> = crate::tracker::queue_other_rows(
            &self.tracker.items,
            &self.tracker.config,
            &self.tracker.selected_milestones,
            &filter,
        )
        .into_iter()
        .filter(|it| self.tracker_assignee_matches(it))
        .collect();
        let other_groups = queue_other_groups(&other_rows);
        let now = now_unix();
        let rows = self.tracker_queue_rows(cx);
        let selected_ix = self.tracker.queue_selected;
        let focused = self.tracker.focus == FocusedColumn::Queue;

        let milestone_chips = div().flex().flex_wrap().gap_2().px_2().py_1().children(
            milestones.into_iter().map(|(title, done, total)| {
                let on = self.tracker.selected_milestones.contains(&title);
                let click_title = title.clone();
                div()
                    .id(SharedString::from(format!("ms-{title}")))
                    .px_2()
                    .py_0p5()
                    .rounded_sm()
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |d| d.bg(theme::surface0()).text_color(theme::text()))
                    .when(!on, |d| d.text_color(theme::overlay0()))
                    .child(SharedString::from(format!("{title} {done}/{total}")))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.tracker.selected_milestones.contains(&click_title) {
                            this.tracker.selected_milestones.remove(&click_title);
                        } else {
                            this.tracker.selected_milestones.insert(click_title.clone());
                        }
                        cx.notify();
                    }))
            }),
        );

        let table_header = header_row(vec![
            (30., false, text_cell("ID", theme::overlay0())),
            (18., false, div().into_any_element()),
            (0., true, text_cell("Title", theme::overlay0())),
            (26., false, text_cell("Age", theme::overlay0())),
            (14., false, oi("list-ordered-16", theme::overlay0())),
            (36., false, oi("milestone-16", theme::overlay0())),
            (36., false, oi("calendar-16", theme::overlay0())),
            (30., false, oi("flame-16", theme::overlay0())),
        ]);

        let table_rows = div().flex().flex_col().children(rows.iter().enumerate().map(|(ix, item)| {
            self.render_queue_row(item, ix, ix == selected_ix && focused, now, cx)
        }));

        let divider = div()
            .id("queue-divider")
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .text_size(px(11.))
            .text_color(theme::overlay0())
            .cursor_pointer()
            .child(SharedString::from(format!(
                "{} {}",
                if self.tracker.todo_backlog_expanded { "▾" } else { "▸" },
                other_groups
                    .iter()
                    .map(|(name, count)| format!("{} {count}", name.to_lowercase()))
                    .collect::<Vec<_>>()
                    .join(" · ")
            )))
            .when(!other_groups.is_empty(), |d| {
                d.on_click(cx.listener(|this, _, _, cx| {
                    this.tracker.todo_backlog_expanded = !this.tracker.todo_backlog_expanded;
                    cx.notify();
                }))
            });

        div()
            .w(px(400.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(11.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .pt_2()
                    .text_size(px(11.))
                    .text_color(theme::overlay0())
                    .child(oi("milestone-16", theme::overlay0()))
                    .child(SharedString::from("Milestones")),
            )
            .child(milestone_chips)
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .pt_2()
                    .child(div().text_size(px(11.)).text_color(theme::overlay0()).child(SharedString::from("Next up")))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme::overlay0())
                            .child(SharedString::from(format!("ready {ready_count}"))),
                    ),
            )
            .child(div().px_2().py_1().child(Input::new(&self.tracker.filter_input).small()))
            .child(table_header)
            .child(div().id("queue-table-scroll").flex_1().min_h(px(0.)).overflow_y_scroll().child(table_rows))
            .child(divider)
            .when_some(self.tracker.error.clone(), |d, err| {
                d.child(div().px_2().py_1().text_color(theme::red()).child(err))
            })
    }

    fn render_queue_row(
        &self,
        item: &TrackerItem,
        row_ix: usize,
        selected: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let style = row_style(item, now);
        let (icon, color) = type_icon(item.detail.issue_type.as_deref(), &self.tracker.config);
        let number = item.number;
        let due_text = item.due.map(|d| due_countdown(d, now)).unwrap_or_else(|| "–".to_string());
        let priority = match item.priority {
            Some(crate::urgency::Priority::High) => "H",
            Some(crate::urgency::Priority::Medium) => "M",
            Some(crate::urgency::Priority::Low) => "L",
            None => "–",
        };
        let milestone = item.detail.milestone.as_ref().map(|m| m.title.clone()).unwrap_or_else(|| "–".to_string());
        let age = crate::comments::short_age(&item.detail.created_at, now);
        let age = age.trim_end_matches(" ago").to_string();

        div()
            .id(SharedString::from(format!("queue-row-{number}")))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(tint(theme::green(), 0.16))
                    .border_l_2()
                    .border_color(theme::green())
            })
            .when(style.italic, |d| d.italic())
            .child(cell(30., false).text_color(style.text).child(SharedString::from(number.to_string())))
            .child(cell(18., false).child(oi(icon, color)))
            .child(cell(0., true).child(title_cell(&item.detail.title, &item.detail.labels, style.text)))
            .child(cell(26., false).text_color(style.text).child(SharedString::from(age)))
            .child(cell(14., false).text_color(style.text).child(SharedString::from(priority)))
            .child(cell(36., false).text_color(style.text).child(div().truncate().child(SharedString::from(milestone))))
            .child(cell(36., false).text_color(style.due_urgency_text).child(SharedString::from(due_text)))
            .child(cell(30., false).text_color(style.due_urgency_text).child(SharedString::from(format!("{:.1}", item.urgency))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tracker.focus = FocusedColumn::Queue;
                this.tracker.queue_selected = row_ix;
                this.tracker.panel_stack = vec![number];
                cx.notify();
            }))
    }

    pub(crate) fn render_tracker_pane(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.tracker.loading && self.tracker.items.is_empty() {
            return centered_message("loading tracker…".into(), theme::overlay0());
        }
        if self.tracker.loaded && self.tracker.board.is_none() {
            let message = self
                .tracker
                .error
                .clone()
                .unwrap_or_else(|| "no tracker data".into());
            return centered_message(message, theme::red());
        }
        let groups = self.tracker_flight_groups(cx);
        let now = now_unix();
        let focused = self.tracker.focus == FocusedColumn::Flight;
        let flat_selected = self.tracker.flight_selected;
        let colors = [theme::blue(), theme::mauve(), theme::peach(), theme::green(), theme::red()];
        let mut row_ix = 0usize;

        let header = header_row(vec![
            (36., false, text_cell("ID", theme::overlay0())),
            (18., false, div().into_any_element()),
            (0., true, text_cell("Title", theme::overlay0())),
            (90., false, oi("issue-tracks-16", theme::overlay0())),
            (56., false, oi("git-pull-request-16", theme::overlay0())),
            (70., false, oi("person-16", theme::overlay0())),
            (36., false, oi("flame-16", theme::overlay0())),
        ]);

        let mut body = div().id("flight-table-scroll").flex_1().min_h(px(0.)).overflow_y_scroll().flex().flex_col();
        for (group_ix, (name, rows)) in groups.iter().enumerate() {
            let color = colors[group_ix % colors.len()];
            body = body.child(
                div()
                    .mt_2()
                    .px_2()
                    .py_1()
                    .text_size(px(11.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .border_l_2()
                    .border_color(color)
                    .bg(tint(color, 0.12))
                    .text_color(color)
                    .flex()
                    .gap_2()
                    .child(SharedString::from(name.clone()))
                    .child(div().text_color(theme::subtext()).font_weight(gpui::FontWeight::NORMAL).child(SharedString::from(rows.len().to_string()))),
            );
            for item in rows {
                let selected = focused && row_ix == flat_selected;
                body = body.child(self.render_flight_row(item, row_ix, selected, now, cx));
                row_ix += 1;
            }
        }
        if groups.is_empty() {
            body = body.child(centered_message("nothing in flight".into(), theme::overlay0()));
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .text_size(px(11.))
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_flight_row(
        &self,
        item: &TrackerItem,
        row_ix: usize,
        selected: bool,
        now: i64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let style = row_style(item, now);
        let (icon, color) = type_icon(item.detail.issue_type.as_deref(), &self.tracker.config);
        let number = item.number;
        let (done, total) = (item.detail.sub_issues_summary.completed, item.detail.sub_issues_summary.total);
        let sub_issues: gpui::AnyElement = if total > 0 {
            let pct = (done as f32 / total as f32 * 100.0).round();
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w(px(36.))
                        .h(px(4.))
                        .rounded_full()
                        .bg(theme::surface0())
                        .child(div().h_full().rounded_full().bg(theme::green()).w(gpui::relative(pct / 100.0))),
                )
                .child(SharedString::from(format!("{done}/{total}")))
                .into_any_element()
        } else {
            text_cell("–", theme::overlay0())
        };
        let pr: gpui::AnyElement = match item.detail.linked_prs.first() {
            Some(pr) => {
                let (pr_icon, pr_color) = if pr.state.eq_ignore_ascii_case("merged") {
                    ("git-merge-16", theme::mauve())
                } else if pr.is_draft {
                    ("git-pull-request-draft-16", theme::overlay0())
                } else {
                    ("git-pull-request-16", theme::green())
                };
                let url = pr.url.clone();
                div()
                    .id(SharedString::from(format!("pr-{}", pr.number)))
                    .flex()
                    .items_center()
                    .gap_1()
                    .cursor_pointer()
                    .child(oi(pr_icon, pr_color))
                    .child(SharedString::from(format!("#{}", pr.number)))
                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                    .into_any_element()
            }
            None => text_cell("–", theme::overlay0()),
        };
        let assignee = item.detail.assignees.first().cloned().unwrap_or_else(|| "–".to_string());

        div()
            .id(SharedString::from(format!("flight-row-{number}")))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .cursor_pointer()
            .when(selected, |d| {
                d.bg(tint(theme::green(), 0.16))
                    .border_l_2()
                    .border_color(theme::green())
            })
            .when(style.italic, |d| d.italic())
            .child(cell(36., false).text_color(style.text).child(SharedString::from(number.to_string())))
            .child(cell(18., false).child(oi(icon, color)))
            .child(cell(0., true).child(title_cell(&item.detail.title, &item.detail.labels, style.text)))
            .child(cell(90., false).text_color(style.text).child(sub_issues))
            .child(cell(56., false).text_color(style.text).child(pr))
            .child(cell(70., false).text_color(style.text).child(div().truncate().child(SharedString::from(assignee))))
            .child(cell(36., false).text_color(style.due_urgency_text).child(SharedString::from(format!("{:.1}", item.urgency))))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tracker.focus = FocusedColumn::Flight;
                this.tracker.flight_selected = row_ix;
                this.tracker.panel_stack = vec![number];
                cx.notify();
            }))
    }
}

impl ReviewApp {
    /// Titlebar content for the Tracker view: repo name and per-group
    /// counts (`4 doing · 2 in review · 11 ready · 20 later`).
    pub(crate) fn render_tracker_titlebar_content(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        if self.tracker.owner.is_empty() {
            return crate::app_title(None);
        }
        let filter = self.tracker_filter_text(cx);
        let flight_counts: Vec<String> = self
            .tracker_flight_groups(cx)
            .into_iter()
            .map(|(name, rows)| format!("{} {}", rows.len(), name.to_lowercase()))
            .collect();
        let ready = crate::tracker::queue_ready_rows(
            &self.tracker.items,
            &self.tracker.config,
            &self.tracker.selected_milestones,
            &filter,
        )
        .into_iter()
        .filter(|it| self.tracker_assignee_matches(it))
        .count();
        let later = crate::tracker::queue_other_rows(
            &self.tracker.items,
            &self.tracker.config,
            &self.tracker.selected_milestones,
            &filter,
        )
        .into_iter()
        .filter(|it| self.tracker_assignee_matches(it))
        .count();
        let mut parts = flight_counts;
        parts.push(format!("{ready} ready"));
        parts.push(format!("{later} later"));
        div()
            .flex()
            .items_center()
            .gap_2()
            .flex_1()
            .min_w_0()
            .child(div().font_weight(gpui::FontWeight::BOLD).child(SharedString::from(format!(
                "{}/{}",
                self.tracker.owner, self.tracker.repo
            ))))
            .child(
                div()
                    .text_color(theme::subtext())
                    .truncate()
                    .child(SharedString::from(parts.join(" · "))),
            )
            .into_any_element()
    }

    pub(crate) fn render_tracker_assignee_chip(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let label = assignee_chip_label(self.tracker.config.assignee_filter);
        div()
            .id("tracker-assignee-chip")
            .flex_shrink_0()
            .mr_2()
            .px_2()
            .rounded_sm()
            .border_1()
            .border_color(theme::surface0())
            .text_size(px(11.))
            .text_color(theme::overlay0())
            .cursor_pointer()
            .child(SharedString::from(label))
            .on_click(cx.listener(|this, _, _, cx| this.tracker_cycle_assignee(cx)))
            .into_any_element()
    }
}

/// A tag-colour pill, shared by the sidebar/main tables' tags column and
/// the issue panel's label row.
pub(crate) fn label_pill(name: &str, color_hex: &str) -> impl IntoElement {
    let color = parse_hex_color(color_hex).unwrap_or_else(theme::overlay0);
    div()
        .px_2()
        .rounded_full()
        .text_size(px(11.))
        .border_1()
        .border_color(tint(color, 0.5))
        .text_color(theme::text())
        .child(SharedString::from(name.to_string()))
}

fn parse_hex_color(hex: &str) -> Option<gpui::Rgba> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(gpui::rgb(value))
}

