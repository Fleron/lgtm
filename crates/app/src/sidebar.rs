use crate::cached_prs::scan_cached_prs;
use crate::diff::row_height;
use crate::items::{ItemData, ItemState, Source};
use crate::subscriptions::{pr_key, save_subscribed_repos, SubscribedRepo};
use crate::titlebar::review_ci_indicator;
use crate::tree::{
    fuzzy_file_matches, status_style, visible_entries, TreeEntryKind, TreeListRow, TREE_ROW_HEIGHT,
};
use crate::{theme, ReviewApp, SIDEBAR_MAX_LIST_HEIGHT};
use diff_core::FileDiff;
use gpui::{
    div, prelude::*, px, uniform_list, Context, Hsla, ScrollStrategy, SharedString, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    input::{Escape as InputEscape, Input},
    scroll::{Scrollbar, ScrollbarShow},
    IconName, Sizable as _,
};
use std::collections::HashSet;
use std::time::Duration;

fn render_tree_row(
    row: TreeListRow,
    pos: usize,
    current: bool,
    data: &ItemData,
    entity: &gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let stats = |file: &FileDiff| {
        div()
            .flex()
            .items_center()
            .gap_1()
            .flex_shrink_0()
            .text_size(px(10.))
            .child(
                div()
                    .text_color(Hsla::from(theme::green()).opacity(0.7))
                    .child(SharedString::from(format!("+{}", file.additions))),
            )
            .child(
                div()
                    .text_color(Hsla::from(theme::red()).opacity(0.7))
                    .child(SharedString::from(format!("−{}", file.deletions))),
            )
    };
    let entity = entity.clone();
    let base = div()
        .id(("tree-row", pos))
        .h(px(TREE_ROW_HEIGHT))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .pr_2()
        .cursor_pointer()
        .when(current, |row| row.bg(theme::surface0()))
        .when(!current, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        });
    match row {
        TreeListRow::Entry(entry_ix) => {
            let entry = &data.tree[entry_ix];
            let indent = px(8. + entry.depth as f32 * 12.);
            let base = base.pl(indent).on_click(move |_, window, cx| {
                entity.update(cx, |this, cx| this.tree_entry_clicked(entry_ix, window, cx));
            });
            match &entry.kind {
                TreeEntryKind::Dir { path } => {
                    let chevron = if data.collapsed.contains(path) {
                        "▸"
                    } else {
                        "▾"
                    };
                    base.child(
                        div()
                            .w(px(12.))
                            .flex_shrink_0()
                            .text_color(theme::overlay0())
                            .child(SharedString::from(chevron)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::overlay0())
                            .child(entry.name.clone()),
                    )
                    .into_any_element()
                }
                TreeEntryKind::File { file_ix } => {
                    let file = &data.diff.files[*file_ix];
                    base.child(div().w(px(12.)).flex_shrink_0()) // aligns with dir chevrons
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(status_style(file.status).1)
                                .child(entry.name.clone()),
                        )
                        .child(stats(file))
                        .into_any_element()
                }
            }
        }
        TreeListRow::FilteredFile(file_ix) => {
            let file = &data.diff.files[file_ix];
            base.pl_2()
                .on_click(move |_, window, cx| {
                    entity.update(cx, |this, cx| this.jump_to_file(file_ix, window, cx));
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(status_style(file.status).1)
                        .child(SharedString::from(file.display_path().to_string())),
                )
                .child(stats(file))
                .into_any_element()
        }
    }
}

impl ReviewApp {
    /// Rescan the worktree cache (off the UI thread) and repopulate the
    /// sidebar's cached-PR list.
    pub(crate) fn refresh_cached_prs(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let cached = cx.background_spawn(async move { scan_cached_prs() }).await;
            this.update(cx, |app, cx| {
                app.cached_prs = cached;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn start_subscribed_pr_poll(&mut self, cx: &mut Context<Self>) {
        self.refresh_subscribed_prs(cx);
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_secs(30))
                .await;
            if this
                .update(cx, |app, cx| app.refresh_subscribed_prs(cx))
                .is_err()
            {
                break;
            }
        })
        .detach();
    }

    /// Refresh every subscribed repository as one guarded batch. The list for
    /// each repository is replaced only after a successful fetch; failures
    /// keep the last successful list visible and attach an error to it.
    pub(crate) fn refresh_subscribed_prs(&mut self, cx: &mut Context<Self>) {
        if self.subscribed_refreshing || self.subscribed_repos.is_empty() {
            return;
        }
        self.subscribed_refreshing = true;
        let repos: Vec<(String, String)> = self
            .subscribed_repos
            .iter()
            .map(|repo| (repo.owner.clone(), repo.repo.clone()))
            .collect();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move {
                    repos
                        .into_iter()
                        .map(|(owner, repo)| {
                            let result = gh::list_prs(&owner, &repo)
                                .map_err(|err| format!("{err:#}"));
                            (owner, repo, result)
                        })
                        .collect::<Vec<_>>()
                })
                .await;
            this.update(cx, |app, cx| {
                app.subscribed_refreshing = false;
                for (owner, repo, result) in fetched {
                    let Some(subscription) = app.subscribed_repos.iter_mut().find(|subscription| {
                        subscription.owner == owner && subscription.repo == repo
                    }) else {
                        continue;
                    };
                    match result {
                        Ok(prs) => {
                            subscription.prs = prs;
                            subscription.refresh_error = None;
                        }
                        Err(error) => subscription.refresh_error = Some(error),
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn subscribe_repo(
        &mut self,
        owner: String,
        repo: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .subscribed_repos
            .iter()
            .any(|subscription| {
                subscription.owner.eq_ignore_ascii_case(&owner)
                    && subscription.repo.eq_ignore_ascii_case(&repo)
            })
        {
            self.subscribed_repos.push(SubscribedRepo {
                owner,
                repo,
                prs: Vec::new(),
                refresh_error: None,
            });
            save_subscribed_repos(&self.subscribed_repos);
        }
        self.sidebar_visible = true;
        self.close_palette(window, cx);
        self.refresh_subscribed_prs(cx);
        cx.notify();
    }

    /// Open a cached PR (activating it if already open), from a sidebar click.
    pub(crate) fn open_cached_pr(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(loc) = self.cached_prs.get(ix).map(|cached| cached.loc.clone()) else {
            return;
        };
        if let Some(existing) = self.items.iter().position(|item| {
            matches!(&item.source, Source::Pr(l)
                if l.owner.eq_ignore_ascii_case(&loc.owner)
                    && l.repo.eq_ignore_ascii_case(&loc.repo)
                    && l.number == loc.number)
        }) {
            self.activate(existing, window, cx);
            return;
        }
        self.open_item(Source::Pr(loc), cx);
    }

    /// Open a subscribed feed PR, activating the existing review item when it
    /// is already open so the sidebar never creates duplicate reviews.
    pub(crate) fn open_subscribed_pr(
        &mut self,
        loc: gh::PrLocator,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(existing) = self.items.iter().position(|item| {
            matches!(&item.source, Source::Pr(l)
                if l.owner.eq_ignore_ascii_case(&loc.owner)
                    && l.repo.eq_ignore_ascii_case(&loc.repo)
                    && l.number == loc.number)
        }) {
            self.activate(existing, window, cx);
            return;
        }
        self.open_item(Source::Pr(loc), cx);
    }

    /// Delete a cached PR's worktree(s) from disk and drop it from the list.
    pub(crate) fn delete_cached_pr(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.cached_prs.len() {
            return;
        }
        for dir in &self.cached_prs[ix].dirs {
            let _ = std::fs::remove_dir_all(dir);
        }
        self.cached_prs.remove(ix);
        cx.notify();
    }

    /// Jump the diff to `file_ix`'s header row and hand focus to the diff
    /// pane (like clicking a sidebar item does).
    pub(crate) fn jump_to_file(&mut self, file_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(&row) = self
            .active_data()
            .and_then(|data| data.file_rows.get(file_ix))
        else {
            return;
        };
        window.focus(&self.focus_handle);
        self.jump(row, cx);
    }

    /// Click on an unfiltered tree row: directories toggle collapse, files
    /// jump the diff.
    pub(crate) fn tree_entry_clicked(&mut self, entry_ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(data) = self.active_data_mut() else {
            return;
        };
        match data.tree.get(entry_ix).map(|entry| &entry.kind) {
            Some(TreeEntryKind::Dir { path }) => {
                let path = path.clone();
                if !data.collapsed.remove(&path) {
                    data.collapsed.insert(path);
                }
                cx.notify();
            }
            Some(&TreeEntryKind::File { file_ix }) => self.jump_to_file(file_ix, window, cx),
            None => {}
        }
    }

    /// Enter in the tree filter: jump to the best match and return focus to
    /// the diff (the filter stays, like GitHub's tree).
    pub(crate) fn tree_filter_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let Some(data) = self.active_data() else {
            return;
        };
        if query.is_empty() {
            return;
        }
        let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
        if let Some(file_ix) = fuzzy_file_matches(&paths, &query).into_iter().next() {
            self.jump_to_file(file_ix, window, cx);
        }
    }

    pub(crate) fn render_sidebar(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        // Five normal two-line entries fit before this area scrolls, leaving
        // the rest of the sidebar for the active item's file tree.
        let mut list = div()
            .id("sidebar-items")
            .max_h(px(SIDEBAR_MAX_LIST_HEIGHT))
            .flex_shrink_0()
            .overflow_y_scroll()
            .track_scroll(&self.subscribed_scroll)
            .py_1();
        for (ix, item) in self.items.iter().enumerate() {
            let active = ix == self.active;
            let dot: Hsla = item.dot_color().into();
            let status: gpui::AnyElement = match &item.state {
                ItemState::Ready(data) => {
                    let review_ci = data.pr_meta.as_ref().map(|meta| {
                        review_ci_indicator(&meta.review_decision, &meta.status_check_rollup)
                    });
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .flex_shrink_0()
                        .text_size(px(11.))
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
                        .children(review_ci)
                        .into_any_element()
                }
                ItemState::Loading => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading…"))
                    .into_any_element(),
                ItemState::Failed(_) => div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme::red())
                    .child(SharedString::from("failed"))
                    .into_any_element(),
            };
            let secondary = item.secondary();
            let entry = div()
                .id(("item", ix))
                .group("sidebar-item")
                .mx_1()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(active, |entry| entry.bg(theme::surface0()))
                .when(!active, |entry| {
                    entry.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                })
                .on_click(cx.listener(move |this, _, window, cx| this.activate(ix, window, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .w(px(8.))
                                .h(px(8.))
                                .flex_shrink_0()
                                .rounded_full()
                                .bg(dot),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(theme::text())
                                .child(item.primary()),
                        )
                        .child(status)
                        .child(
                            div()
                                .flex_shrink_0()
                                .opacity(0.)
                                .group_hover("sidebar-item", |style| style.opacity(1.))
                                .child(
                                    Button::new(("close-item", ix))
                                        .icon(IconName::Close)
                                        .ghost()
                                        .xsmall()
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.close_item(ix, cx)
                                        })),
                                ),
                        ),
                )
                .when(!secondary.is_empty(), |entry| {
                    entry.child(
                        div()
                            .pl(px(16.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme::subtext())
                            .child(secondary),
                    )
                });
            list = list.child(entry);
        }

        // --- subscribed GitHub PRs ---
        // Feed rows remain lightweight: only the list summary is retained,
        // and opening one goes through the normal review-item path.
        let open_pr_keys: HashSet<(String, String, u64)> = self
            .items
            .iter()
            .filter_map(|item| match &item.source {
                Source::Pr(loc) => Some(pr_key(&loc.owner, &loc.repo, loc.number)),
                Source::Local(_) => None,
            })
            .collect();
        let subscribed_feed: Vec<(gh::PrLocator, gh::PrSummary)> = self
            .subscribed_repos
            .iter()
            .flat_map(|subscription| {
                subscription.prs.iter().filter_map(|pr| {
                    let key = pr_key(&subscription.owner, &subscription.repo, pr.number);
                    (!open_pr_keys.contains(&key)).then(|| {
                        (
                            gh::PrLocator {
                                owner: subscription.owner.clone(),
                                repo: subscription.repo.clone(),
                                number: pr.number,
                            },
                            pr.clone(),
                        )
                    })
                })
            })
            .collect();
        let subscribed_pr_keys: HashSet<(String, String, u64)> = self
            .subscribed_repos
            .iter()
            .flat_map(|subscription| {
                subscription
                    .prs
                    .iter()
                    .map(|pr| pr_key(&subscription.owner, &subscription.repo, pr.number))
            })
            .collect();
        let subscribed_feed_count = subscribed_feed.len();
        if !self.subscribed_repos.is_empty() {
            let refresh_failed = self
                .subscribed_repos
                .iter()
                .any(|subscription| subscription.refresh_error.is_some());
            list = list.child(
                div()
                    .mx_1()
                    .mt_2()
                    .px_2()
                    .pb_1()
                    .text_size(px(10.))
                    .text_color(if refresh_failed {
                        theme::red()
                    } else {
                        theme::overlay0()
                    })
                    .child(SharedString::from(if refresh_failed {
                        "SUBSCRIBED PRS · REFRESH FAILED"
                    } else {
                        "SUBSCRIBED PRS"
                    })),
            );
            for (loc, pr) in subscribed_feed {
                let label: SharedString = format!("{}#{}", loc.repo_slug(), pr.number).into();
                let title: SharedString = pr.title.clone().into();
                let click_loc = loc.clone();
                let entry = div()
                    .id(SharedString::from(format!(
                        "subscribed-pr-{}#{}",
                        loc.repo_slug(),
                        pr.number
                    )))
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_subscribed_pr(click_loc.clone(), window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(8.))
                                    .h(px(8.))
                                    .flex_shrink_0()
                                    .rounded_full()
                                    .bg(if pr.is_draft {
                                        theme::overlay0()
                                    } else {
                                        theme::green()
                                    }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_color(theme::text())
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_color(theme::subtext())
                                    .child(SharedString::from(pr.author.login.clone())),
                            )
                            .child(review_ci_indicator(
                                &pr.review_decision,
                                &pr.status_check_rollup,
                            )),
                    )
                    .child(
                        div()
                            .pl(px(16.))
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme::subtext())
                            .child(title),
                    );
                list = list.child(entry);
            }
        }

        // --- cached PRs (past reviews with an on-disk worktree) ---
        // Skip any that are already open above, so each PR shows once.
        let cached: Vec<(usize, SharedString)> = self
            .cached_prs
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let key = pr_key(&c.loc.owner, &c.loc.repo, c.loc.number);
                !open_pr_keys.contains(&key) && !subscribed_pr_keys.contains(&key)
            })
            .map(|(ix, c)| (ix, SharedString::from(format!("{}#{}", c.loc.repo_slug(), c.loc.number))))
            .collect();
        let cached_count = cached.len();
        if !cached.is_empty() {
            list = list.child(
                div()
                    .mx_1()
                    .mt_2()
                    .px_2()
                    .pb_1()
                    .text_size(px(10.))
                    .text_color(theme::overlay0())
                    .child(SharedString::from("CACHED PRS")),
            );
            for (ix, label) in cached {
                let entry = div()
                    .id(("cached-pr", ix))
                    .group("cached-pr")
                    .mx_1()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_cached_pr(ix, window, cx)
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(12.))
                                    .text_color(theme::subtext())
                                    .child(label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .opacity(0.)
                                    .group_hover("cached-pr", |style| style.opacity(1.))
                                    .child(
                                        Button::new(("delete-cached", ix))
                                            .icon(IconName::Close)
                                            .ghost()
                                            .xsmall()
                                            .tooltip("Delete cached worktree")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.delete_cached_pr(ix, cx)
                                            })),
                                    ),
                            ),
                    );
                list = list.child(entry);
            }
        }

        let section_count = usize::from(!self.subscribed_repos.is_empty())
            + usize::from(cached_count > 0);
        let sidebar_row_count =
            self.items.len() + subscribed_feed_count + cached_count + section_count;
        let list = div()
            .relative()
            .w_full()
            .max_h(px(SIDEBAR_MAX_LIST_HEIGHT))
            .flex_shrink_0()
            .child(list)
            .when(sidebar_row_count > 5, |area| {
                area.child(
                    Scrollbar::vertical(&self.subscribed_scroll)
                        .scrollbar_show(ScrollbarShow::Always),
                )
            });

        // --- file tree for the active item ---
        let query = self.tree_filter_input.read(cx).value().trim().to_string();
        let mut tree_rows: Vec<TreeListRow> = Vec::new();
        let mut current_file = None;
        let mut current_row = None;
        if let Some(data) = self.active_data() {
            if query.is_empty() {
                tree_rows = visible_entries(&data.tree, &data.collapsed)
                    .into_iter()
                    .map(TreeListRow::Entry)
                    .collect();
            } else {
                let paths: Vec<&str> = data.diff.files.iter().map(|f| f.display_path()).collect();
                tree_rows = fuzzy_file_matches(&paths, &query)
                    .into_iter()
                    .map(TreeListRow::FilteredFile)
                    .collect();
            }
            // Follow the diff: highlight the file whose header row is at (or
            // scrolled past) the top of the viewport. A pending scroll_to_item
            // (from ]/[ or a tree click) hasn't reached the offset yet, so it
            // takes precedence; otherwise the same offset/row_height() math the
            // selection hit test uses.
            let scroll = data.scroll.0.borrow();
            let top_row = match &scroll.deferred_scroll_to_item {
                Some(deferred) => deferred.item_index,
                None => (f32::from(-scroll.base_handle.offset().y) / row_height()).max(0.) as usize,
            };
            drop(scroll);
            current_file = data.file_rows.iter().rposition(|&ix| ix <= top_row);
            current_row = current_file.and_then(|file| {
                tree_rows.iter().position(|row| match row {
                    TreeListRow::Entry(ix) => matches!(
                        data.tree[*ix].kind,
                        TreeEntryKind::File { file_ix } if file_ix == file
                    ),
                    TreeListRow::FilteredFile(file_ix) => *file_ix == file,
                })
            });
        }
        // Keep the highlighted file in view — but only when it changes, so
        // the user's own tree scrolling is never fought.
        if let Some(file) = current_file {
            if let Some(data) = self.active_data_mut() {
                if data.tree_last_file != Some(file) {
                    data.tree_last_file = Some(file);
                    if let Some(pos) = current_row {
                        data.tree_scroll.scroll_to_item(pos, ScrollStrategy::Center);
                    }
                }
            }
        }
        let tree_scroll = self.active_data().map(|data| data.tree_scroll.clone());
        let entity = cx.entity();
        let tree_list: gpui::AnyElement = match tree_scroll {
            Some(scroll) if !tree_rows.is_empty() => {
                uniform_list("file-tree", tree_rows.len(), move |range, _window, cx| {
                    let this = entity.read(cx);
                    let Some(data) = this.active_data() else {
                        return Vec::new();
                    };
                    range
                        .filter_map(|pos| tree_rows.get(pos).map(|row| (pos, *row)))
                        .map(|(pos, row)| {
                            render_tree_row(row, pos, current_row == Some(pos), data, &entity)
                        })
                        .collect()
                })
                .track_scroll(scroll)
                .h_full()
                .into_any_element()
            }
            Some(_) if !query.is_empty() => div()
                .px_3()
                .py_2()
                .text_color(theme::overlay0())
                .child(SharedString::from("no matching files"))
                .into_any_element(),
            _ => div().into_any_element(),
        };

        div()
            .w(px(260.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_r_1()
            .border_color(theme::surface0())
            .text_size(px(12.))
            .child(
                div()
                    .p_2()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(Input::new(&self.open_input).small())
                    .when_some(self.open_error.clone(), |area, err| {
                        area.child(div().text_size(px(11.)).text_color(theme::red()).child(err))
                    }),
            )
            .child(list)
            .child(div().h(px(1.)).flex_shrink_0().bg(theme::surface0()))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    // The filter input propagates Escape when it has nothing
                    // of its own to dismiss; catch it here (before the root's
                    // handler) to clear the query and return to the diff.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.tree_filter_input
                            .update(cx, |state, cx| state.set_value("", window, cx));
                        window.focus(&this.focus_handle);
                        cx.notify();
                    }))
                    .child(
                        div()
                            .p_2()
                            .child(Input::new(&self.tree_filter_input).small()),
                    )
                    .child(div().flex_1().min_h_0().child(tree_list)),
            )
    }
}
