//! Tracker view (app layer): state, `gh` project loading, urgency wiring
//! and key handling. Rendering lives in `tracker_table.rs` (sidebar + main
//! pane) and `tracker_panel.rs` (issue panel); this file owns the data and
//! the actions that mutate it.

use crate::comments::{now_unix, parse_iso_utc};
use crate::dispatch::{claude_dirs, skill_names, Agent, SkillProvider};
use crate::items::Source;
use crate::theme;
use crate::urgency::{
    due_state, load_tracker_config, parse_iso_date, save_tracker_config, urgency, AssigneeFilter,
    Column, DueState, Priority, TrackerConfig, UrgencyInput,
};
use crate::ReviewApp;
use anyhow::{Context as _, Result};
use gh::{IssueDetail, ProjectBoard, ProjectField, ProjectItem};
use gpui::{prelude::*, AssetSource, Context, Entity, ScrollHandle, SharedString, Subscription, Window};
use gpui_component::input::{InputEvent, InputState};
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::rc::Rc;

/// One project-board item, enriched with the issue detail and the
/// precomputed urgency score.
pub(crate) struct TrackerItem {
    /// Projects v2 item id (not the issue number) — needed for field writes.
    pub(crate) project_item_id: String,
    pub(crate) number: u64,
    /// Status option name; empty when the item has no Status set.
    pub(crate) status: String,
    pub(crate) due: Option<i64>,
    pub(crate) priority: Option<Priority>,
    pub(crate) detail: IssueDetail,
    pub(crate) urgency: f64,
}

/// Which column currently has keyboard focus. `Panel` is the open issue's
/// sub-issues list — only reachable when the panel is open on a parent
/// issue (one that has a sub-issues section at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusedColumn {
    Queue,
    Flight,
    Panel,
}

/// What the toggled composer input is currently being used for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ComposerMode {
    NewIssue { parent: Option<u64> },
    EditDescription,
    EditChildTitle(u64),
}

/// The floating "dispatch an agent" card. One at a time; opening it for
/// another issue replaces the previous one.
pub(crate) struct DispatchCard {
    pub(crate) number: u64,
    pub(crate) agent: Agent,
    pub(crate) input: Entity<InputState>,
    pub(crate) error: Option<SharedString>,
    pub(crate) _subscription: Subscription,
}

pub(crate) struct TrackerState {
    pub(crate) loaded: bool,
    pub(crate) loading: bool,
    pub(crate) error: Option<SharedString>,
    pub(crate) owner: String,
    pub(crate) repo: String,
    pub(crate) me: Option<String>,
    pub(crate) board: Option<ProjectBoard>,
    pub(crate) status_field: Option<ProjectField>,
    pub(crate) priority_field_id: Option<String>,
    pub(crate) due_field_id: Option<String>,
    pub(crate) items: Vec<TrackerItem>,
    pub(crate) config: TrackerConfig,
    /// Bumped on every load; a background fetch only lands if its generation
    /// is still current (mirrors `items.rs`'s upgrade_gen protocol).
    pub(crate) gen: u64,
    pub(crate) focus: FocusedColumn,
    pub(crate) queue_selected: usize,
    pub(crate) flight_selected: usize,
    pub(crate) queue_scroll: ScrollHandle,
    pub(crate) flight_scroll: ScrollHandle,
    pub(crate) todo_backlog_expanded: bool,
    pub(crate) selected_milestones: BTreeSet<String>,
    pub(crate) filter_input: Entity<InputState>,
    /// Issue numbers opened in the right panel, oldest first; the last
    /// entry is the open issue, everything before it is the breadcrumb.
    pub(crate) panel_stack: Vec<u64>,
    pub(crate) panel_expanded: bool,
    /// Issue number the status menu is open for — the panel header's own
    /// issue, or a highlighted sub-issue row.
    pub(crate) status_menu_target: Option<u64>,
    /// Index into the open issue's `sub_issues`, highlighted when
    /// `focus == FocusedColumn::Panel`.
    pub(crate) panel_child_selected: usize,
    pub(crate) comment_input: Entity<InputState>,
    pub(crate) composer_mode: Option<ComposerMode>,
    pub(crate) composer_input: Option<Entity<InputState>>,
    pub(crate) composer_subscription: Option<Subscription>,
    pub(crate) dispatch: Option<DispatchCard>,
}

fn status_opt(status: &str) -> Option<&str> {
    if status.is_empty() {
        None
    } else {
        Some(status)
    }
}

fn parse_priority(raw: &str) -> Option<Priority> {
    match raw {
        "High" => Some(Priority::High),
        "Medium" => Some(Priority::Medium),
        "Low" => Some(Priority::Low),
        _ => None,
    }
}

impl TrackerItem {
    pub(crate) fn column(&self, config: &TrackerConfig) -> Column {
        config.column_for(status_opt(&self.status))
    }

    pub(crate) fn is_blocked(&self) -> bool {
        self.status == "Blocked"
    }
}

impl TrackerState {
    pub(crate) fn new(
        window: &mut Window,
        cx: &mut Context<ReviewApp>,
    ) -> (Self, Vec<Subscription>) {
        let filter_input = cx.new(|cx| InputState::new(window, cx).placeholder("filter…"));
        let comment_input =
            cx.new(|cx| InputState::new(window, cx).auto_grow(1, 5).placeholder("write a comment…"));
        let subscriptions = vec![
            cx.subscribe_in(&filter_input, window, |_this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&comment_input, window, |this, _, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { secondary: true }) {
                    this.tracker_submit_comment(window, cx);
                }
            }),
        ];
        let config = load_tracker_config().unwrap_or_else(|| TrackerConfig::seed(&[]));
        let state = TrackerState {
            loaded: false,
            loading: false,
            error: None,
            owner: String::new(),
            repo: String::new(),
            me: None,
            board: None,
            status_field: None,
            priority_field_id: None,
            due_field_id: None,
            items: Vec::new(),
            config,
            gen: 0,
            focus: FocusedColumn::Queue,
            queue_selected: 0,
            flight_selected: 0,
            queue_scroll: ScrollHandle::new(),
            flight_scroll: ScrollHandle::new(),
            todo_backlog_expanded: false,
            selected_milestones: BTreeSet::new(),
            filter_input,
            panel_stack: Vec::new(),
            panel_expanded: false,
            status_menu_target: None,
            panel_child_selected: 0,
            comment_input,
            composer_mode: None,
            composer_input: None,
            composer_subscription: None,
            dispatch: None,
        };
        (state, subscriptions)
    }

    pub(crate) fn open_issue(&self) -> Option<u64> {
        self.panel_stack.last().copied()
    }

    pub(crate) fn item(&self, number: u64) -> Option<&TrackerItem> {
        self.items.iter().find(|it| it.number == number)
    }
}

/// Issues per aliased GraphQL request in the detail phase.
const DETAIL_BATCH: usize = 20;

struct LoadedTracker {
    board: ProjectBoard,
    status_field: Option<ProjectField>,
    priority_field_id: Option<String>,
    due_field_id: Option<String>,
    items: Vec<TrackerItem>,
    me: Option<String>,
}

fn load_tracker_data(owner: &str, repo: &str) -> Result<LoadedTracker> {
    let me = gh::current_user_login().ok();
    let boards = gh::list_project_boards(owner, repo)?;
    let board = boards
        .into_iter()
        .next()
        .with_context(|| format!("no Projects v2 board linked to {owner}/{repo}"))?;
    let fields = gh::project_fields(owner, board.number)?;
    let status_field = fields.iter().find(|f| f.name == "Status").cloned();
    let priority_field_id = fields.iter().find(|f| f.name == "Priority").map(|f| f.id.clone());
    let due_field_id = fields.iter().find(|f| f.name == "Due").map(|f| f.id.clone());
    let project_items: Vec<ProjectItem> = gh::project_items(owner, board.number)?;
    let mut items = Vec::with_capacity(project_items.len());
    // Details (labels, sub-issues, PRs, comments) arrive in a second phase;
    // until then the row shows what the project item already carries.
    for pi in project_items {
        let detail = IssueDetail {
            title: pi.content.title.clone(),
            ..IssueDetail::default()
        };
        items.push(TrackerItem {
            project_item_id: pi.id,
            number: pi.content.number,
            status: pi.status,
            due: if pi.due.is_empty() {
                None
            } else {
                parse_iso_date(&pi.due)
            },
            priority: parse_priority(&pi.priority),
            detail,
            urgency: 0.0,
        });
    }
    Ok(LoadedTracker {
        board,
        status_field,
        priority_field_id,
        due_field_id,
        items,
        me,
    })
}

impl ReviewApp {
    /// Repo the tracker follows: the first subscribed repo, or the active
    /// item's repo when it's a PR and nothing is subscribed.
    fn tracker_repo(&self) -> Option<(String, String)> {
        if let Some(repo) = self.subscribed_repos.first() {
            return Some((repo.owner.clone(), repo.repo.clone()));
        }
        match self.active_item().map(|item| &item.source) {
            Some(Source::Pr(loc)) => Some((loc.owner.clone(), loc.repo.clone())),
            _ => None,
        }
    }

    pub(crate) fn tracker_load(&mut self, cx: &mut Context<Self>) {
        let Some((owner, repo)) = self.tracker_repo() else {
            self.tracker.loaded = true;
            self.tracker.error =
                Some("no subscribed repo configured — subscribe to one from the Review sidebar".into());
            return;
        };
        self.tracker.loading = true;
        self.tracker.error = None;
        self.tracker.gen += 1;
        let gen = self.tracker.gen;
        let owner_bg = owner.clone();
        let repo_bg = repo.clone();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { load_tracker_data(&owner_bg, &repo_bg) })
                .await;
            this.update(cx, |app, cx| {
                if app.tracker.gen != gen {
                    return;
                }
                app.tracker.loading = false;
                app.tracker.loaded = true;
                match result {
                    Ok(loaded) => {
                        app.tracker.owner = owner;
                        app.tracker.repo = repo;
                        app.tracker.me = loaded.me;
                        if app.tracker.config.status_columns.is_empty() {
                            let options: Vec<String> = loaded
                                .status_field
                                .as_ref()
                                .map(|f| f.options.iter().map(|o| o.name.clone()).collect())
                                .unwrap_or_default();
                            app.tracker.config = TrackerConfig::seed(&options);
                            save_tracker_config(&app.tracker.config);
                        }
                        app.tracker.board = Some(loaded.board);
                        app.tracker.status_field = loaded.status_field;
                        app.tracker.priority_field_id = loaded.priority_field_id;
                        app.tracker.due_field_id = loaded.due_field_id;
                        app.tracker.items = loaded.items;
                        app.tracker.queue_selected = 0;
                        app.tracker.flight_selected = 0;
                        app.recompute_tracker_urgency();
                        app.tracker_load_details(gen, cx);
                    }
                    Err(err) => {
                        app.tracker.error = Some(format!("tracker load failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Phase two of a load: fetch issue details in batches and merge each
    /// batch into the items as it lands, so the board is usable before the
    /// last one arrives. Stops if a newer load has started.
    fn tracker_load_details(&mut self, gen: u64, cx: &mut Context<Self>) {
        let owner = self.tracker.owner.clone();
        let repo = self.tracker.repo.clone();
        let numbers: Vec<u64> = self.tracker.items.iter().map(|it| it.number).collect();
        cx.spawn(async move |this, cx| {
            for chunk in numbers.chunks(DETAIL_BATCH) {
                let (owner_bg, repo_bg, chunk_bg) = (owner.clone(), repo.clone(), chunk.to_vec());
                let result = cx
                    .background_spawn(async move { gh::issue_details(&owner_bg, &repo_bg, &chunk_bg) })
                    .await;
                let keep_going = this
                    .update(cx, |app, cx| {
                        if app.tracker.gen != gen {
                            return false;
                        }
                        match result {
                            Ok(details) => {
                                for (number, detail) in details {
                                    if let Some(item) = app.tracker.items.iter_mut().find(|it| it.number == number) {
                                        item.detail = detail;
                                    }
                                }
                                app.recompute_tracker_urgency();
                            }
                            Err(err) => {
                                app.tracker.error = Some(format!("issue details failed: {err:#}").into());
                            }
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !keep_going {
                    return;
                }
            }
        })
        .detach();
    }

    pub(crate) fn recompute_tracker_urgency(&mut self) {
        let now = now_unix();
        let TrackerState { config, items, .. } = &mut self.tracker;
        for item in items.iter_mut() {
            let input = UrgencyInput {
                due: item.due,
                now,
                blocking_count: 0,
                priority: item.priority,
                active: config.column_for(status_opt(&item.status)) == Column::Flight,
                created_at: parse_iso_utc(&item.detail.created_at).unwrap_or(now),
                has_milestone: item.detail.milestone.is_some(),
                milestone_due: item
                    .detail
                    .milestone
                    .as_ref()
                    .and_then(|m| m.due_on.as_deref())
                    .and_then(parse_iso_utc),
                has_labels: !item.detail.labels.is_empty(),
                has_comments: !item.detail.comments.is_empty(),
                blocked: item.status == "Blocked",
            };
            item.urgency = urgency(&input, &config.coefficients);
        }
    }

    pub(crate) fn tracker_assignee_matches(&self, item: &TrackerItem) -> bool {
        assignee_matches(item, self.tracker.config.assignee_filter, self.tracker.me.as_deref())
    }

    pub(crate) fn tracker_cycle_assignee(&mut self, cx: &mut Context<Self>) {
        self.tracker.config.assignee_filter = match self.tracker.config.assignee_filter {
            AssigneeFilter::Me => AssigneeFilter::MeAndUnassigned,
            AssigneeFilter::MeAndUnassigned => AssigneeFilter::All,
            AssigneeFilter::All => AssigneeFilter::Me,
        };
        save_tracker_config(&self.tracker.config);
        cx.notify();
    }

    pub(crate) fn tracker_filter_text(&self, cx: &Context<Self>) -> String {
        self.tracker.filter_input.read(cx).value().trim().to_string()
    }

    /// Queue rows in display order: the Ready table, plus the collapsed
    /// other-queue-statuses rows when the divider is expanded.
    pub(crate) fn tracker_queue_rows(&self, cx: &Context<Self>) -> Vec<&TrackerItem> {
        let filter = self.tracker_filter_text(cx);
        let mut rows = queue_ready_rows(
            &self.tracker.items,
            &self.tracker.config,
            &self.tracker.selected_milestones,
            &filter,
        );
        if self.tracker.todo_backlog_expanded {
            rows.extend(queue_other_rows(
                &self.tracker.items,
                &self.tracker.config,
                &self.tracker.selected_milestones,
                &filter,
            ));
        }
        rows.retain(|item| self.tracker_assignee_matches(item));
        rows
    }

    pub(crate) fn tracker_flight_groups(&self, cx: &Context<Self>) -> Vec<(String, Vec<&TrackerItem>)> {
        let filter = self.tracker_filter_text(cx);
        let mut groups = flight_groups(
            &self.tracker.items,
            &self.tracker.config,
            self.tracker.status_field.as_ref(),
            &self.tracker.selected_milestones,
            &filter,
        );
        for (_, rows) in &mut groups {
            rows.retain(|item| self.tracker_assignee_matches(item));
        }
        groups.retain(|(_, rows)| !rows.is_empty());
        groups
    }

    /// Every visible flight row, board-group order then urgency order —
    /// the flat list `j`/`k` walk while the middle column has focus.
    fn tracker_flight_rows_flat(&self, cx: &Context<Self>) -> Vec<u64> {
        self.tracker_flight_groups(cx)
            .into_iter()
            .flat_map(|(_, rows)| rows.into_iter().map(|it| it.number))
            .collect()
    }

    fn tracker_queue_rows_flat(&self, cx: &Context<Self>) -> Vec<u64> {
        self.tracker_queue_rows(cx).into_iter().map(|it| it.number).collect()
    }

    /// Whether the open issue has a sub-issues section at all (parents
    /// only — a sub-issue's own panel doesn't get one), the same test
    /// `tracker_panel.rs` uses to decide whether to render it.
    pub(crate) fn tracker_panel_has_sub_issues_section(&self) -> bool {
        let Some(number) = self.tracker.open_issue() else {
            return false;
        };
        let Some(item) = self.tracker.item(number) else {
            return false;
        };
        self.tracker.panel_stack.len() <= 1 && item.detail.parent.is_none()
    }

    fn tracker_panel_children(&self) -> Vec<gh::SubIssue> {
        self.tracker
            .open_issue()
            .and_then(|number| self.tracker.item(number))
            .map(|item| item.detail.sub_issues.clone())
            .unwrap_or_default()
    }

    pub(crate) fn tracker_move(&mut self, delta: i64, cx: &mut Context<Self>) {
        if self.tracker.focus == FocusedColumn::Panel {
            let children = self.tracker_panel_children();
            if children.is_empty() {
                return;
            }
            let next = (self.tracker.panel_child_selected as i64 + delta)
                .clamp(0, children.len() as i64 - 1);
            self.tracker.panel_child_selected = next as usize;
            cx.notify();
            return;
        }
        let rows = match self.tracker.focus {
            FocusedColumn::Queue => self.tracker_queue_rows_flat(cx),
            FocusedColumn::Flight => self.tracker_flight_rows_flat(cx),
            FocusedColumn::Panel => unreachable!(),
        };
        if rows.is_empty() {
            return;
        }
        let selected = match self.tracker.focus {
            FocusedColumn::Queue => &mut self.tracker.queue_selected,
            FocusedColumn::Flight => &mut self.tracker.flight_selected,
            FocusedColumn::Panel => unreachable!(),
        };
        let next = ((*selected as i64 + delta).clamp(0, rows.len() as i64 - 1)) as usize;
        *selected = next;
        match self.tracker.focus {
            FocusedColumn::Queue => self.tracker.queue_scroll.scroll_to_item(next),
            FocusedColumn::Flight => {
                let child_ix = self.flight_child_index(next, cx);
                self.tracker.flight_scroll.scroll_to_item(child_ix);
            }
            FocusedColumn::Panel => unreachable!(),
        }
        cx.notify();
    }

    /// The flight pane's scroll container holds a header child before each
    /// group's rows, so a flat row index maps to a later child index.
    fn flight_child_index(&self, flat: usize, cx: &Context<Self>) -> usize {
        let mut seen = 0;
        for (group_ix, (_, rows)) in self.tracker_flight_groups(cx).iter().enumerate() {
            if flat < seen + rows.len() {
                return flat + group_ix + 1;
            }
            seen += rows.len();
        }
        flat
    }

    pub(crate) fn tracker_next_column(&mut self, cx: &mut Context<Self>) {
        self.tracker.focus = match self.tracker.focus {
            FocusedColumn::Queue => FocusedColumn::Flight,
            FocusedColumn::Flight if self.tracker_panel_has_sub_issues_section() => {
                self.tracker.panel_child_selected = 0;
                FocusedColumn::Panel
            }
            FocusedColumn::Flight => FocusedColumn::Queue,
            FocusedColumn::Panel => FocusedColumn::Queue,
        };
        cx.notify();
    }

    fn tracker_selected_number(&self, cx: &Context<Self>) -> Option<u64> {
        let rows = match self.tracker.focus {
            FocusedColumn::Queue => self.tracker_queue_rows_flat(cx),
            FocusedColumn::Flight => self.tracker_flight_rows_flat(cx),
            FocusedColumn::Panel => return None,
        };
        let selected = match self.tracker.focus {
            FocusedColumn::Queue => self.tracker.queue_selected,
            FocusedColumn::Flight => self.tracker.flight_selected,
            FocusedColumn::Panel => return None,
        };
        rows.get(selected).copied()
    }

    /// `↵`: with the panel's sub-issues list focused, drill into the
    /// highlighted child; otherwise open the panel on the focused row.
    pub(crate) fn tracker_open(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.tracker.focus == FocusedColumn::Panel {
            if let Some(child) = self.tracker_panel_children().get(self.tracker.panel_child_selected) {
                let number = child.number;
                self.tracker_drill_into(number, cx);
            }
            return;
        }
        if let Some(number) = self.tracker_selected_number(cx) {
            self.tracker.panel_stack = vec![number];
            self.tracker.status_menu_target = None;
            self.tracker.panel_child_selected = 0;
            cx.notify();
        }
    }

    pub(crate) fn tracker_drill_into(&mut self, number: u64, cx: &mut Context<Self>) {
        self.tracker.panel_stack.push(number);
        self.tracker.status_menu_target = None;
        self.tracker.panel_child_selected = 0;
        if self.tracker.focus == FocusedColumn::Panel && self.tracker_panel_children().is_empty() {
            self.tracker.focus = FocusedColumn::Queue;
        }
        cx.notify();
    }

    pub(crate) fn tracker_back(&mut self, cx: &mut Context<Self>) {
        if self.tracker.panel_stack.len() > 1 {
            self.tracker.panel_stack.pop();
            self.tracker.status_menu_target = None;
            self.tracker.panel_child_selected = 0;
            cx.notify();
        }
    }

    /// `space` with the panel's sub-issues list focused: open the status
    /// menu on the highlighted child, or report it's not a board item.
    pub(crate) fn tracker_panel_space(&mut self, cx: &mut Context<Self>) {
        if self.tracker.focus != FocusedColumn::Panel {
            return;
        }
        let Some(child) = self.tracker_panel_children().get(self.tracker.panel_child_selected).cloned() else {
            return;
        };
        if self.tracker.item(child.number).is_none() {
            self.tracker.error = Some(format!("#{} is not on the board", child.number).into());
            cx.notify();
            return;
        }
        self.tracker.status_menu_target = Some(child.number);
        cx.notify();
    }

    /// `e` with the panel's sub-issues list focused: edit the highlighted
    /// child's title inline.
    pub(crate) fn tracker_panel_edit_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tracker.focus != FocusedColumn::Panel {
            return;
        }
        let Some(child) = self.tracker_panel_children().get(self.tracker.panel_child_selected).cloned() else {
            return;
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(child.title.clone()));
        let subscription = cx.subscribe_in(&input, window, move |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: false }) {
                this.tracker_submit_composer(window, cx);
            }
        });
        self.tracker.composer_mode = Some(ComposerMode::EditChildTitle(child.number));
        self.tracker.composer_input = Some(input.clone());
        self.tracker.composer_subscription = Some(subscription);
        input.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// `+` with the panel's sub-issues list focused: same "add sub-issue"
    /// composer the header's plus glyph opens.
    pub(crate) fn tracker_panel_add_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tracker.focus != FocusedColumn::Panel {
            return;
        }
        if let Some(number) = self.tracker.open_issue() {
            self.tracker_open_new_issue_composer(Some(number), window, cx);
        }
    }

    pub(crate) fn tracker_close_panel(&mut self, cx: &mut Context<Self>) {
        if self.tracker.composer_mode.is_some() {
            self.tracker_close_composer(cx);
            return;
        }
        if self.tracker.status_menu_target.is_some() {
            self.tracker.status_menu_target = None;
            cx.notify();
            return;
        }
        self.tracker.panel_stack.clear();
        cx.notify();
    }

    pub(crate) fn tracker_first_status_in(&self, column: Column) -> Option<String> {
        let field = self.tracker.status_field.as_ref()?;
        field
            .options
            .iter()
            .find(|o| self.tracker.config.column_for(Some(&o.name)) == column)
            .map(|o| o.name.clone())
    }

    pub(crate) fn tracker_set_first_flight(&mut self, cx: &mut Context<Self>) {
        if let Some(number) = self.tracker_selected_number(cx) {
            if let Some(status) = self.tracker_first_status_in(Column::Flight) {
                self.tracker_set_status(number, status, cx);
            }
        }
    }

    pub(crate) fn tracker_set_first_hidden(&mut self, cx: &mut Context<Self>) {
        if let Some(number) = self.tracker_selected_number(cx) {
            if let Some(status) = self.tracker_first_status_in(Column::Hidden) {
                self.tracker_set_status(number, status, cx);
            }
        }
    }

    pub(crate) fn tracker_set_named_status(&mut self, name: &str, cx: &mut Context<Self>) {
        let Some(number) = self.tracker_selected_number(cx) else {
            return;
        };
        let has_option = self
            .tracker
            .status_field
            .as_ref()
            .is_some_and(|f| f.options.iter().any(|o| o.name == name));
        if has_option {
            self.tracker_set_status(number, name.to_string(), cx);
        }
    }

    /// Set `number`'s Status field, optimistically, reverting with an
    /// inline error (same treatment as chat errors) if the write fails.
    pub(crate) fn tracker_set_status(&mut self, number: u64, new_status: String, cx: &mut Context<Self>) {
        let Some(board) = self.tracker.board.clone() else {
            return;
        };
        let Some(status_field) = self.tracker.status_field.clone() else {
            return;
        };
        let Some(option) = status_field.options.iter().find(|o| o.name == new_status).cloned() else {
            return;
        };
        let Some(item) = self.tracker.items.iter_mut().find(|it| it.number == number) else {
            return;
        };
        let old_status = item.status.clone();
        if old_status == new_status {
            return;
        }
        item.status = new_status.clone();
        let item_id = item.project_item_id.clone();
        self.recompute_tracker_urgency();
        self.tracker.error = None;
        self.tracker.status_menu_target = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let project_id = board.id.clone();
            let field_id = status_field.id.clone();
            let option_id = option.id.clone();
            let result = cx
                .background_spawn(async move { gh::set_project_field(&project_id, &item_id, &field_id, &option_id) })
                .await;
            this.update(cx, |app, cx| {
                if let Err(err) = result {
                    if let Some(item) = app.tracker.items.iter_mut().find(|it| it.number == number) {
                        item.status = old_status;
                    }
                    app.recompute_tracker_urgency();
                    app.tracker.error = Some(format!("status update failed: {err:#}").into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn tracker_open_selected_on_github(&mut self, cx: &mut Context<Self>) {
        let number = self.tracker.open_issue().or_else(|| self.tracker_selected_number(cx));
        let Some(number) = number else {
            return;
        };
        let url = match self.tracker.item(number) {
            Some(item) if !item.detail.linked_prs.is_empty() => item.detail.linked_prs[0].url.clone(),
            Some(item) => item.detail.url.clone(),
            None => return,
        };
        cx.open_url(&url);
    }

    /// Title of `number`, whether it is a board item or only a sub-issue of
    /// one (children are not necessarily on the project board themselves).
    fn tracker_issue_title(&self, number: u64) -> Option<String> {
        if let Some(item) = self.tracker.item(number) {
            return Some(item.detail.title.clone());
        }
        self.tracker
            .items
            .iter()
            .flat_map(|item| &item.detail.sub_issues)
            .find(|sub| sub.number == number)
            .map(|sub| sub.title.clone())
    }

    /// `⌘d`: dispatch for the open issue, else the focused row.
    pub(crate) fn tracker_dispatch_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let child = if self.tracker.focus == FocusedColumn::Panel {
            self.tracker_panel_children()
                .get(self.tracker.panel_child_selected)
                .map(|sub| sub.number)
        } else {
            None
        };
        let number = child
            .or_else(|| self.tracker.open_issue())
            .or_else(|| self.tracker_selected_number(cx));
        if let Some(number) = number {
            self.tracker_open_dispatch(number, window, cx);
        }
    }

    pub(crate) fn tracker_open_dispatch(
        &mut self,
        number: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(title) = self.tracker_issue_title(number) else {
            return;
        };
        let seed = format!("Issue #{number}: {title}\n");
        self.tracker_close_composer(cx);
        let checkout = self
            .tracker
            .config
            .dispatch_root
            .as_ref()
            .map(|root| root.join(&self.tracker.repo));
        let names = skill_names(&claude_dirs(checkout.as_deref()));
        let input = cx.new(|cx| {
            let mut state = InputState::new(window, cx).auto_grow(3, 12).default_value(seed);
            state.lsp.completion_provider = Some(Rc::new(SkillProvider { names }));
            state
        });
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
                match event {
                    InputEvent::PressEnter { secondary: true } => {
                        this.tracker_submit_dispatch(window, cx);
                    }
                    InputEvent::Change => {
                        if let Some(card) = &mut this.tracker.dispatch {
                            card.error = None;
                            cx.notify();
                        }
                    }
                    _ => {}
                }
            });
        input.update(cx, |state, cx| {
            state.set_cursor_position(lsp_types::Position::new(1, 0), window, cx);
        });
        self.tracker.dispatch = Some(DispatchCard {
            number,
            agent: Agent::Claude,
            input,
            error: None,
            _subscription: subscription,
        });
        cx.notify();
    }

    pub(crate) fn tracker_close_dispatch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.tracker.dispatch = None;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    pub(crate) fn tracker_toggle_dispatch_agent(&mut self, cx: &mut Context<Self>) {
        if let Some(card) = &mut self.tracker.dispatch {
            card.agent = card.agent.toggled();
            card.error = None;
            cx.notify();
        }
    }

    pub(crate) fn tracker_submit_dispatch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = &self.tracker.dispatch else {
            return;
        };
        let Some(root) = self.tracker.config.dispatch_root.clone() else {
            return;
        };
        let (agent, prompt) = (card.agent, card.input.read(cx).value().trim().to_string());
        match crate::dispatch::dispatch(&root, &self.tracker.repo, agent, &prompt) {
            Ok(()) => self.tracker_close_dispatch(window, cx),
            Err(err) => {
                if let Some(card) = &mut self.tracker.dispatch {
                    card.error = Some(err.into());
                }
                cx.notify();
            }
        }
    }

    pub(crate) fn tracker_close_composer(&mut self, cx: &mut Context<Self>) {
        self.tracker.composer_mode = None;
        self.tracker.composer_input = None;
        self.tracker.composer_subscription = None;
        cx.notify();
    }
}

fn assignee_matches(item: &TrackerItem, filter: AssigneeFilter, me: Option<&str>) -> bool {
    match (filter, me) {
        (AssigneeFilter::All, _) | (_, None) => true,
        (AssigneeFilter::Me, Some(me)) => item.detail.assignees.iter().any(|a| a == me),
        (AssigneeFilter::MeAndUnassigned, Some(me)) => {
            item.detail.assignees.is_empty() || item.detail.assignees.iter().any(|a| a == me)
        }
    }
}

pub(crate) fn assignee_chip_label(filter: AssigneeFilter) -> &'static str {
    match filter {
        AssigneeFilter::Me => "me",
        AssigneeFilter::MeAndUnassigned => "me + unassigned",
        AssigneeFilter::All => "all",
    }
}

/// The queue table's main rows: items in the Queue column with status
/// `Ready` (or no status at all), matching `filter` and `milestones`.
pub(crate) fn queue_ready_rows<'a>(
    items: &'a [TrackerItem],
    config: &TrackerConfig,
    milestones: &BTreeSet<String>,
    filter: &str,
) -> Vec<&'a TrackerItem> {
    let mut rows: Vec<&TrackerItem> = items
        .iter()
        .filter(|it| it.column(config) == Column::Queue)
        .filter(|it| it.status.is_empty() || it.status == "Ready")
        .filter(|it| milestone_matches(it, milestones))
        .filter(|it| text_matches(it, filter))
        .collect();
    sort_by_urgency_desc(&mut rows);
    rows
}

/// The collapsed "todo 8 · backlog 12" divider: Queue-column items whose
/// status isn't `Ready`, grouped by status name with counts.
pub(crate) fn queue_other_groups(rows: &[&TrackerItem]) -> Vec<(String, usize)> {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for item in rows {
        match counts.iter_mut().find(|(name, _)| *name == item.status) {
            Some((_, count)) => *count += 1,
            None => counts.push((item.status.clone(), 1)),
        }
    }
    counts
}

/// Queue-column items whose status isn't `Ready` (the expanded rows behind
/// the collapsed divider), matching the same filters as the main table.
pub(crate) fn queue_other_rows<'a>(
    items: &'a [TrackerItem],
    config: &TrackerConfig,
    milestones: &BTreeSet<String>,
    filter: &str,
) -> Vec<&'a TrackerItem> {
    let mut rows: Vec<&TrackerItem> = items
        .iter()
        .filter(|it| it.column(config) == Column::Queue)
        .filter(|it| !it.status.is_empty() && it.status != "Ready")
        .filter(|it| milestone_matches(it, milestones))
        .filter(|it| text_matches(it, filter))
        .collect();
    sort_by_urgency_desc(&mut rows);
    rows
}

/// One middle-pane group: a flight status name and its rows, sorted by
/// urgency desc, in board order (empty groups dropped).
pub(crate) fn flight_groups<'a>(
    items: &'a [TrackerItem],
    config: &TrackerConfig,
    status_field: Option<&ProjectField>,
    milestones: &BTreeSet<String>,
    filter: &str,
) -> Vec<(String, Vec<&'a TrackerItem>)> {
    let names: Vec<String> = match status_field {
        Some(field) => field.options.iter().map(|o| o.name.clone()).collect(),
        None => {
            let mut seen = Vec::new();
            for item in items {
                if item.column(config) == Column::Flight && !seen.contains(&item.status) {
                    seen.push(item.status.clone());
                }
            }
            seen
        }
    };
    names
        .into_iter()
        .filter_map(|name| {
            if config.column_for(Some(&name)) != Column::Flight {
                return None;
            }
            let mut rows: Vec<&TrackerItem> = items
                .iter()
                .filter(|it| it.status == name)
                .filter(|it| milestone_matches(it, milestones))
                .filter(|it| text_matches(it, filter))
                .collect();
            if rows.is_empty() {
                return None;
            }
            sort_by_urgency_desc(&mut rows);
            Some((name, rows))
        })
        .collect()
}

fn sort_by_urgency_desc(rows: &mut [&TrackerItem]) {
    rows.sort_by(|a, b| b.urgency.partial_cmp(&a.urgency).unwrap_or(std::cmp::Ordering::Equal));
}

fn milestone_matches(item: &TrackerItem, milestones: &BTreeSet<String>) -> bool {
    if milestones.is_empty() {
        return true;
    }
    item.detail
        .milestone
        .as_ref()
        .is_some_and(|m| milestones.contains(&m.title))
}

fn text_matches(item: &TrackerItem, filter: &str) -> bool {
    filter.is_empty() || item.detail.title.to_lowercase().contains(&filter.to_lowercase())
}

/// `(title, done, total)` for every milestone that has open issues on the
/// board, skipping ones where every issue is already closed (closed
/// milestones aren't shown per the tracker doc).
pub(crate) fn milestone_summary(items: &[TrackerItem]) -> Vec<(String, u32, u32)> {
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::BTreeMap<String, (u32, u32)> = std::collections::BTreeMap::new();
    for item in items {
        let Some(milestone) = &item.detail.milestone else {
            continue;
        };
        if !order.contains(&milestone.title) {
            order.push(milestone.title.clone());
        }
        let entry = counts.entry(milestone.title.clone()).or_insert((0, 0));
        entry.1 += 1;
        if item.detail.state.eq_ignore_ascii_case("closed") {
            entry.0 += 1;
        }
    }
    order
        .into_iter()
        .filter_map(|title| {
            let (done, total) = counts.get(&title).copied().unwrap_or((0, 0));
            (done < total).then_some((title, done, total))
        })
        .collect()
}

/// The type name → (octicon file stem, colour) mapping the mockup uses:
/// `Bug` gets its own icon, every other named type (and no type) shares
/// `issue-opened-16` and picks its colour from `tracker.json`.
pub(crate) fn type_icon(issue_type: Option<&str>, config: &TrackerConfig) -> (&'static str, gpui::Rgba) {
    match issue_type {
        None => ("issue-opened-16", theme::green()),
        Some(name) if name.eq_ignore_ascii_case("bug") => ("bug-16", theme::red()),
        Some(name) => (
            "issue-opened-16",
            config
                .type_colors
                .get(name)
                .map(|c| color_by_name(c))
                .unwrap_or_else(theme::green),
        ),
    }
}

/// `color` at `alpha` opacity, for a translucent background/border tint —
/// `Rgba` itself has no `opacity()`, only `Hsla` does.
pub(crate) fn tint(color: gpui::Rgba, alpha: f32) -> gpui::Hsla {
    gpui::Hsla::from(color).opacity(alpha)
}

pub(crate) fn color_by_name(name: &str) -> gpui::Rgba {
    match name {
        "red" => theme::red(),
        "blue" => theme::blue(),
        "mauve" => theme::mauve(),
        "peach" => theme::peach(),
        "green" => theme::green(),
        _ => theme::overlay0(),
    }
}

/// A sub-issue row's single state icon. GitHub's sub-issues API only gives
/// `state` (open/closed), not the child's own project Status — so when the
/// child is itself a tracked board item this looks its Status column up to
/// tell ready/in-flight apart; otherwise open falls back to "ready".
pub(crate) fn sub_issue_icon(
    sub: &gh::SubIssue,
    items: &[TrackerItem],
    config: &TrackerConfig,
) -> (&'static str, gpui::Rgba) {
    if sub.state.eq_ignore_ascii_case("closed") {
        return ("issue-closed-16", theme::mauve());
    }
    match items.iter().find(|it| it.number == sub.number) {
        Some(it) => match it.column(config) {
            Column::Queue if !it.status.is_empty() && it.status != "Ready" => {
                ("issue-draft-16", theme::overlay0())
            }
            Column::Queue => ("issue-opened-16", theme::green()),
            Column::Flight => ("issue-opened-16", theme::blue()),
            Column::Hidden => ("issue-closed-16", theme::mauve()),
        },
        None => ("issue-opened-16", theme::green()),
    }
}

/// Row colouring: overdue (red, whole row), due-soon (yellow-ish, due +
/// urgency cells only), blocked (italic overlay grey), else normal text.
pub(crate) struct RowStyle {
    pub(crate) text: gpui::Rgba,
    pub(crate) due_urgency_text: gpui::Rgba,
    pub(crate) italic: bool,
}

pub(crate) fn row_style(item: &TrackerItem, now: i64) -> RowStyle {
    if item.is_blocked() {
        return RowStyle {
            text: theme::overlay0(),
            due_urgency_text: theme::overlay0(),
            italic: true,
        };
    }
    match item.due.map(|due| due_state(due, now)) {
        Some(DueState::Overdue) => RowStyle {
            text: theme::red(),
            due_urgency_text: theme::red(),
            italic: false,
        },
        Some(DueState::Soon) => RowStyle {
            text: theme::text(),
            due_urgency_text: theme::yellow(),
            italic: false,
        },
        _ => RowStyle {
            text: theme::text(),
            due_urgency_text: theme::text(),
            italic: false,
        },
    }
}

/// Octicon SVGs from `crates/app/assets/octicons/`, chained in front of
/// `gpui_component_assets::Assets` so `gpui::svg().path("octicons/…")`
/// resolves while every other built-in icon keeps working.
pub(crate) struct TrackerAssets;

macro_rules! octicon {
    ($name:literal) => {
        (
            concat!("octicons/", $name, ".svg"),
            include_str!(concat!("../assets/octicons/", $name, ".svg")),
        )
    };
}

const OCTICONS: &[(&str, &str)] = &[
    octicon!("bug-16"),
    octicon!("calendar-16"),
    octicon!("comment-16"),
    octicon!("filter-16"),
    octicon!("flame-16"),
    octicon!("git-merge-16"),
    octicon!("git-pull-request-16"),
    octicon!("git-pull-request-draft-16"),
    octicon!("issue-closed-16"),
    octicon!("issue-draft-16"),
    octicon!("issue-opened-16"),
    octicon!("issue-tracked-by-16"),
    octicon!("issue-tracks-16"),
    octicon!("link-external-16"),
    octicon!("list-ordered-16"),
    octicon!("milestone-16"),
    octicon!("pencil-16"),
    octicon!("person-16"),
    octicon!("plus-16"),
    octicon!("screen-full-16"),
    octicon!("tag-16"),
    octicon!("terminal-16"),
    octicon!("x-16"),
];

impl AssetSource for TrackerAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        if let Some((_, content)) = OCTICONS.iter().find(|(p, _)| *p == path) {
            return Ok(Some(Cow::Borrowed(content.as_bytes())));
        }
        gpui_component_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        let mut names: Vec<SharedString> = OCTICONS
            .iter()
            .filter(|(p, _)| p.starts_with(path))
            .map(|(p, _)| (*p).into())
            .collect();
        names.extend(gpui_component_assets::Assets.list(path)?);
        Ok(names)
    }
}

/// One octicon, sized and coloured like the mockup's `.oi` glyphs.
pub(crate) fn oi(name: &str, color: gpui::Rgba) -> gpui::AnyElement {
    oi_sized(name, color, 13.)
}

/// The per-row dispatch affordance: the terminal octicon, shown while the
/// row is `visible` (hovered via `group`, or selected) and inert otherwise.
pub(crate) fn dispatch_icon(
    number: u64,
    group: &SharedString,
    visible: bool,
    cx: &mut Context<ReviewApp>,
) -> gpui::AnyElement {
    gpui::div()
        .id(SharedString::from(format!("dispatch-{group}")))
        .cursor_pointer()
        .opacity(if visible { 1. } else { 0. })
        .group_hover(group.clone(), |style| style.opacity(1.))
        .child(oi("terminal-16", theme::overlay0()))
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(cx.listener(move |this, _, window, cx| {
            this.tracker_open_dispatch(number, window, cx);
        }))
        .into_any_element()
}

/// Builder for the one-item right-click menu that dispatches `number`.
pub(crate) fn dispatch_menu(
    number: u64,
    cx: &Context<ReviewApp>,
) -> impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static {
    let app = cx.entity().downgrade();
    move |menu, _, _| {
        let app = app.clone();
        menu.item(PopupMenuItem::new("Dispatch").on_click(move |_, window, cx| {
            let _ = app.update(cx, |this, cx| this.tracker_open_dispatch(number, window, cx));
        }))
    }
}

pub(crate) fn oi_sized(name: &str, color: gpui::Rgba, size: f32) -> gpui::AnyElement {
    gpui::svg()
        .path(format!("octicons/{name}.svg"))
        .size(gpui::px(size))
        .text_color(color)
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(number: u64, status: &str, urgency: f64) -> TrackerItem {
        TrackerItem {
            project_item_id: format!("PI_{number}"),
            number,
            status: status.to_string(),
            due: None,
            priority: None,
            detail: IssueDetail {
                title: format!("issue {number}"),
                ..Default::default()
            },
            urgency,
        }
    }

    fn config() -> TrackerConfig {
        TrackerConfig::seed(&["Backlog".into(), "Todo".into(), "Ready".into(), "Doing".into(), "Done".into()])
    }

    #[test]
    fn queue_ready_rows_sorts_by_urgency_desc() {
        let items = vec![item(1, "Ready", 1.0), item(2, "Ready", 5.0), item(3, "Ready", 3.0)];
        let rows = queue_ready_rows(&items, &config(), &BTreeSet::new(), "");
        assert_eq!(rows.iter().map(|it| it.number).collect::<Vec<_>>(), vec![2, 3, 1]);
    }

    #[test]
    fn queue_ready_rows_excludes_other_queue_statuses() {
        let items = vec![item(1, "Ready", 1.0), item(2, "Todo", 2.0), item(3, "Backlog", 3.0)];
        let rows = queue_ready_rows(&items, &config(), &BTreeSet::new(), "");
        assert_eq!(rows.iter().map(|it| it.number).collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn queue_other_groups_counts_by_status() {
        let items = vec![item(1, "Todo", 1.0), item(2, "Todo", 1.0), item(3, "Backlog", 1.0)];
        let groups = queue_other_groups(&queue_other_rows(&items, &config(), &BTreeSet::new(), ""));
        assert_eq!(groups, vec![("Todo".to_string(), 2), ("Backlog".to_string(), 1)]);
    }

    #[test]
    fn flight_groups_drops_empty_groups_and_sorts_within() {
        let items = vec![item(1, "Doing", 1.0), item(2, "Doing", 4.0)];
        let groups = flight_groups(&items, &config(), None, &BTreeSet::new(), "");
        assert_eq!(groups.len(), 1);
        let (name, rows) = &groups[0];
        assert_eq!(name, "Doing");
        assert_eq!(rows.iter().map(|it| it.number).collect::<Vec<_>>(), vec![2, 1]);
    }

    #[test]
    fn text_filter_matches_case_insensitively() {
        let items = vec![item(1, "Ready", 1.0)];
        assert_eq!(queue_ready_rows(&items, &config(), &BTreeSet::new(), "ISSUE").len(), 1);
        assert_eq!(queue_ready_rows(&items, &config(), &BTreeSet::new(), "nope").len(), 0);
    }

    #[test]
    fn milestone_summary_skips_fully_closed_milestones() {
        let mut open = item(1, "Ready", 1.0);
        open.detail.milestone = Some(gh::IssueMilestone {
            title: "v1".into(),
            due_on: None,
        });
        let mut closed = item(2, "Done", 1.0);
        closed.detail.state = "CLOSED".into();
        closed.detail.milestone = Some(gh::IssueMilestone {
            title: "v0".into(),
            due_on: None,
        });
        let summary = milestone_summary(&[open, closed]);
        assert_eq!(summary, vec![("v1".to_string(), 0, 1)]);
    }
}
