use crate::items::{dir_name, ItemState, Source};
use crate::subscriptions::parse_repo_slug;
use crate::theme;
use crate::{PaletteBack, PaletteDown, PaletteUp, ReviewApp, ViewMode};
use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
use gpui::{
    div, prelude::*, px, uniform_list, Context, Hsla, MouseButton, ScrollStrategy, SharedString,
    Window,
};
use gpui_component::input::{Escape as InputEscape, Input};
use gpui_component::Sizable as _;
use std::path::PathBuf;

/// The cmd-k command palette: a staged flow for opening things.
pub(crate) enum PaletteStep {
    /// Step 1: pick what to open.
    Sources { selected: usize },
    /// Step 2 (GitHub path): type `owner/repo`.
    RepoInput { error: Option<SharedString> },
    /// Step 2 (subscription path): type `owner/repo`.
    SubscribeRepoInput { error: Option<SharedString> },
    /// Step 3 (GitHub path): pick a PR from the repo's open list.
    PrList { repo: String, prs: PrListState },
    /// Local path: pick the base ref for an already-open local item.
    LocalBaseList {
        item_id: u64,
        repo_root: PathBuf,
        current: String,
        bases: BaseListState,
    },
}

pub(crate) enum PrListState {
    Loading,
    Loaded {
        all: Vec<gh::PrSummary>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

pub(crate) enum BaseListState {
    Loading,
    Loaded {
        all: Vec<String>,
        /// Indices into `all`, fuzzy-filtered by the query, best match first.
        filtered: Vec<usize>,
        /// Index into `filtered`.
        selected: usize,
    },
    Failed(String),
}

const PALETTE_SOURCES: [&str; 3] = [
    "Open GitHub pull request",
    "Subscribe to GitHub repository",
    "Open local folder",
];
const SOURCE_PR: usize = 0;
const SOURCE_SUBSCRIBE: usize = 1;
const SOURCE_FOLDER: usize = 2;
const PALETTE_ROW_HEIGHT: f32 = 30.0;
pub(crate) const SIDEBAR_MAX_LIST_HEIGHT: f32 = 240.0;

/// Step-1 options matching the query (case-insensitive substring), as indices
/// into PALETTE_SOURCES. Empty query keeps both.
pub(crate) fn filtered_sources(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    PALETTE_SOURCES
        .iter()
        .enumerate()
        .filter(|(_, label)| q.is_empty() || label.to_lowercase().contains(&q))
        .map(|(ix, _)| ix)
        .collect()
}

/// Fuzzy-filter PRs against `#number title author branch`, best score first;
/// an empty query keeps gh's original (most recently updated) order.
pub(crate) fn filter_prs(all: &[gh::PrSummary], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(ix, pr)| {
            let haystack = format!(
                "#{} {} {} {}",
                pr.number, pr.title, pr.author.login, pr.head_ref_name
            );
            matcher
                .fuzzy_match(&haystack, query)
                .map(|score| (score, ix))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

pub(crate) fn filter_base_refs(all: &[String], query: &str) -> Vec<usize> {
    let query = query.trim();
    if query.is_empty() {
        return (0..all.len()).collect();
    }
    let matcher = SkimMatcherV2::default();
    let mut scored: Vec<(i64, usize)> = all
        .iter()
        .enumerate()
        .filter_map(|(ix, base)| matcher.fuzzy_match(base, query).map(|score| (score, ix)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, ix)| ix).collect()
}

/// One row of the palette's PR list: state dot, #number, title, author, head
/// branch. Clicking opens the PR just like enter does.
fn palette_pr_row(
    pr: &gh::PrSummary,
    pos: usize,
    selected: bool,
    entity: gpui::Entity<ReviewApp>,
) -> gpui::AnyElement {
    let dot = if pr.is_draft {
        theme::overlay0()
    } else {
        theme::green()
    };
    div()
        .id(("palette-pr", pos))
        .mx_1()
        .px_2()
        .h(px(PALETTE_ROW_HEIGHT))
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .when(selected, |row| row.bg(theme::surface0()))
        .when(!selected, |row| {
            row.hover(|style| style.bg(Hsla::from(theme::surface0()).opacity(0.5)))
        })
        .on_click(move |_, window, cx| {
            entity.update(cx, |this, cx| this.palette_open_pr_row(pos, window, cx));
        })
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
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(format!("#{}", pr.number))),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme::text())
                .child(SharedString::from(pr.title.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .text_color(theme::subtext())
                .child(SharedString::from(pr.author.login.clone())),
        )
        .child(
            div()
                .flex_shrink_0()
                .max_w(px(140.))
                .truncate()
                .text_color(theme::overlay0())
                .child(SharedString::from(pr.head_ref_name.clone())),
        )
        .into_any_element()
}

impl ReviewApp {
    /// Reset the palette's shared input for the step being entered and keep
    /// keyboard focus in it (the palette traps focus while open).
    pub(crate) fn set_palette_input(
        &mut self,
        value: &str,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_input.update(cx, |state, cx| {
            state.set_value(value.to_string(), window, cx);
            state.set_placeholder(placeholder, window, cx);
            state.focus(window, cx);
        });
    }

    pub(crate) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = Some(PaletteStep::Sources { selected: 0 });
        self.palette_gen += 1;
        self.set_palette_input("", "type to filter…", window, cx);
        cx.notify();
    }

    pub(crate) fn close_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.palette = None;
        self.palette_gen += 1;
        window.focus(&self.focus_handle);
        cx.notify();
    }

    /// Esc: one step back, or close from step 1.
    pub(crate) fn palette_back(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.palette {
            None | Some(PaletteStep::Sources { .. }) => self.close_palette(window, cx),
            Some(PaletteStep::RepoInput { .. }) => {
                self.palette = Some(PaletteStep::Sources { selected: 0 });
                self.palette_gen += 1;
                self.set_palette_input("", "type to filter…", window, cx);
                cx.notify();
            }
            Some(PaletteStep::SubscribeRepoInput { .. }) => {
                self.palette = Some(PaletteStep::Sources { selected: 0 });
                self.palette_gen += 1;
                self.set_palette_input("", "type to filter…", window, cx);
                cx.notify();
            }
            Some(PaletteStep::PrList { repo, .. }) => {
                let repo = repo.clone();
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input(&repo, "owner/repo", window, cx);
                cx.notify();
            }
            Some(PaletteStep::LocalBaseList { .. }) => self.close_palette(window, cx),
        }
    }

    pub(crate) fn palette_move(&mut self, delta: isize, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                if len > 0 {
                    *selected = (*selected as isize + delta).clamp(0, len as isize - 1) as usize;
                }
            }
            Some(PaletteStep::PrList {
                prs:
                    PrListState::Loaded {
                        filtered, selected, ..
                    },
                ..
            }) => {
                if !filtered.is_empty() {
                    *selected =
                        (*selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
                    self.palette_scroll
                        .scroll_to_item(*selected, ScrollStrategy::Top);
                }
            }
            Some(PaletteStep::LocalBaseList {
                bases: BaseListState::Loaded { filtered, selected, .. },
                ..
            }) => {
                if !filtered.is_empty() {
                    *selected =
                        (*selected as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
                    self.palette_scroll
                        .scroll_to_item(*selected, ScrollStrategy::Top);
                }
            }
            _ => return,
        }
        cx.notify();
    }

    pub(crate) fn palette_query_changed(&mut self, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().to_string();
        match &mut self.palette {
            Some(PaletteStep::Sources { selected }) => {
                let len = filtered_sources(&query).len();
                *selected = (*selected).min(len.saturating_sub(1));
            }
            Some(PaletteStep::RepoInput { error }) => *error = None,
            Some(PaletteStep::SubscribeRepoInput { error }) => *error = None,
            Some(PaletteStep::PrList {
                prs:
                    PrListState::Loaded {
                        all,
                        filtered,
                        selected,
                    },
                ..
            }) => {
                *filtered = filter_prs(all, &query);
                *selected = 0;
                self.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            Some(PaletteStep::LocalBaseList {
                bases: BaseListState::Loaded { all, filtered, selected },
                ..
            }) => {
                *filtered = filter_base_refs(all, &query);
                *selected = 0;
                self.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
            }
            _ => return,
        }
        cx.notify();
    }

    /// Enter, on whichever step is showing.
    pub(crate) fn palette_confirm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.palette_input.read(cx).value().trim().to_string();
        match &self.palette {
            Some(PaletteStep::Sources { selected }) => {
                if let Some(&opt) = filtered_sources(&query).get(*selected) {
                    self.palette_activate_source(opt, window, cx);
                }
            }
            Some(PaletteStep::RepoInput { .. }) => match parse_repo_slug(&query) {
                Ok((owner, repo)) => self.palette_fetch_prs(owner, repo, window, cx),
                Err(msg) => {
                    if let Some(PaletteStep::RepoInput { error }) = &mut self.palette {
                        *error = Some(msg.into());
                        cx.notify();
                    }
                }
            },
            Some(PaletteStep::SubscribeRepoInput { .. }) => match parse_repo_slug(&query) {
                Ok((owner, repo)) => self.subscribe_repo(owner, repo, window, cx),
                Err(msg) => {
                    if let Some(PaletteStep::SubscribeRepoInput { error }) = &mut self.palette {
                        *error = Some(msg.into());
                        cx.notify();
                    }
                }
            },
            Some(PaletteStep::PrList {
                prs: PrListState::Loaded { selected, .. },
                ..
            }) => {
                let selected = *selected;
                self.palette_open_pr_row(selected, window, cx);
            }
            Some(PaletteStep::LocalBaseList {
                bases: BaseListState::Loaded { selected, .. },
                ..
            }) => {
                let selected = *selected;
                self.palette_open_base_row(selected, window, cx);
            }
            _ => {}
        }
    }

    pub(crate) fn palette_activate_source(&mut self, opt: usize, window: &mut Window, cx: &mut Context<Self>) {
        match opt {
            SOURCE_PR => {
                self.palette = Some(PaletteStep::RepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input("", "owner/repo", window, cx);
                cx.notify();
            }
            SOURCE_SUBSCRIBE => {
                self.palette = Some(PaletteStep::SubscribeRepoInput { error: None });
                self.palette_gen += 1;
                self.set_palette_input("", "owner/repo", window, cx);
                cx.notify();
            }
            SOURCE_FOLDER => {
                // The palette closes when the native dialog opens.
                self.close_palette(window, cx);
                self.prompt_open_folder(cx);
            }
            _ => {}
        }
    }

    /// Step 2 → step 3: show "loading…" and fetch the open-PR list on the
    /// background executor. The generation guard drops the result if the user
    /// closed the palette or navigated away before it landed.
    pub(crate) fn palette_fetch_prs(
        &mut self,
        owner: String,
        repo: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_gen += 1;
        let gen = self.palette_gen;
        self.palette = Some(PaletteStep::PrList {
            repo: format!("{owner}/{repo}"),
            prs: PrListState::Loading,
        });
        self.set_palette_input("", "search pull requests…", window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::list_prs(&owner, &repo) })
                .await;
            this.update(cx, |app, cx| {
                if app.palette_gen != gen {
                    return;
                }
                let query = app.palette_input.read(cx).value().to_string();
                let Some(PaletteStep::PrList { prs, .. }) = &mut app.palette else {
                    return;
                };
                *prs = match fetched {
                    Ok(all) => {
                        let filtered = filter_prs(&all, &query);
                        PrListState::Loaded {
                            all,
                            filtered,
                            selected: 0,
                        }
                    }
                    Err(err) => PrListState::Failed(format!("{err:#}")),
                };
                app.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_local_base_palette(
        &mut self,
        item_id: u64,
        repo_root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self
            .items
            .iter()
            .find(|item| item.id == item_id)
            .and_then(|item| match &item.source {
                Source::Local(src) => Some(src.base_label.clone()),
                Source::Pr(_) => None,
            })
            .unwrap_or_else(|| "HEAD".to_string());
        self.palette_gen += 1;
        let gen = self.palette_gen;
        self.palette = Some(PaletteStep::LocalBaseList {
            item_id,
            repo_root: repo_root.clone(),
            current,
            bases: BaseListState::Loading,
        });
        self.set_palette_input("", "search base refs…", window, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { git::list_base_refs(&repo_root) })
                .await;
            this.update(cx, |app, cx| {
                if app.palette_gen != gen {
                    return;
                }
                let query = app.palette_input.read(cx).value().to_string();
                let Some(PaletteStep::LocalBaseList { bases, .. }) = &mut app.palette else {
                    return;
                };
                *bases = match fetched {
                    Ok(all) => {
                        let filtered = filter_base_refs(&all, &query);
                        BaseListState::Loaded {
                            all,
                            filtered,
                            selected: 0,
                        }
                    }
                    Err(err) => BaseListState::Failed(format!("{err:#}")),
                };
                app.palette_scroll.scroll_to_item(0, ScrollStrategy::Top);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the PR at `pos` in the filtered list as a new review item.
    pub(crate) fn palette_open_pr_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PaletteStep::PrList {
            repo,
            prs: PrListState::Loaded { all, filtered, .. },
        }) = &self.palette
        else {
            return;
        };
        let Some(&ix) = filtered.get(pos) else {
            return;
        };
        let Some((owner, repo)) = repo.split_once('/') else {
            return;
        };
        let source = Source::Pr(gh::PrLocator {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number: all[ix].number,
        });
        self.close_palette(window, cx);
        self.open_item(source, cx);
    }

    pub(crate) fn palette_open_base_row(&mut self, pos: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(PaletteStep::LocalBaseList {
            item_id,
            repo_root,
            bases: BaseListState::Loaded { all, filtered, .. },
            ..
        }) = &self.palette
        else {
            return;
        };
        let Some(&ix) = filtered.get(pos) else {
            return;
        };
        let (item_id, repo_root, base_ref) = (*item_id, repo_root.clone(), all[ix].clone());
        self.close_palette(window, cx);
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let mode = match &item.state {
            ItemState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        let branch = match &item.source {
            Source::Local(src) => src.branch.clone(),
            Source::Pr(_) => dir_name(&repo_root),
        };
        let source = Source::Local(git::LocalSource {
            repo_root,
            branch,
            base_ref: Some(base_ref.clone()),
            base_label: base_ref,
            base_oid: None,
        });
        item.source = source.clone();
        item.refresh_error = None;
        match item.state {
            ItemState::Failed(_) => item.state = ItemState::Loading,
            ItemState::Loading => {}
            ItemState::Ready(_) => item.reloading = true,
        }
        Self::spawn_fetch(item_id, source, mode, cx);
        cx.notify();
    }

    pub(crate) fn render_palette(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let Some(step) = &self.palette else {
            return div().into_any_element();
        };
        let query = self.palette_input.read(cx).value().to_string();

        let header: Option<SharedString> = match step {
            PaletteStep::Sources { .. } => None,
            PaletteStep::RepoInput { .. } => Some("Open GitHub pull request".into()),
            PaletteStep::SubscribeRepoInput { .. } => {
                Some("Subscribe to GitHub repository".into())
            }
            PaletteStep::PrList { repo, .. } => Some(repo.clone().into()),
            PaletteStep::LocalBaseList { .. } => Some("Choose local diff base".into()),
        };

        let body: gpui::AnyElement = match step {
            PaletteStep::Sources { selected } => {
                let filtered = filtered_sources(&query);
                let selected = *selected;
                let mut list = div().py_1().flex().flex_col();
                if filtered.is_empty() {
                    list = list.child(
                        div()
                            .px_3()
                            .py_2()
                            .text_color(theme::overlay0())
                            .child(SharedString::from("no matches")),
                    );
                }
                for (pos, &opt) in filtered.iter().enumerate() {
                    list = list.child(
                        div()
                            .id(("palette-source", opt))
                            .mx_1()
                            .px_2()
                            .h(px(PALETTE_ROW_HEIGHT))
                            .rounded_md()
                            .flex()
                            .items_center()
                            .cursor_pointer()
                            .when(pos == selected, |row| row.bg(theme::surface0()))
                            .when(pos != selected, |row| {
                                row.hover(|style| {
                                    style.bg(Hsla::from(theme::surface0()).opacity(0.5))
                                })
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.palette_activate_source(opt, window, cx)
                            }))
                            .child(
                                div()
                                    .text_color(theme::text())
                                    .child(SharedString::from(PALETTE_SOURCES[opt])),
                            ),
                    );
                }
                list.into_any_element()
            }
            PaletteStep::RepoInput { error } | PaletteStep::SubscribeRepoInput { error } => {
                let (text, color): (SharedString, gpui::Rgba) = match error {
                    Some(err) => (err.clone(), theme::red()),
                    None if matches!(step, PaletteStep::RepoInput { .. }) => (
                        "enter to list open pull requests · esc to go back".into(),
                        theme::overlay0(),
                    ),
                    None => ("enter to subscribe · esc to go back".into(), theme::overlay0()),
                };
                div()
                    .px_3()
                    .py_2()
                    .text_color(color)
                    .child(text)
                    .into_any_element()
            }
            PaletteStep::PrList { prs, .. } => match prs {
                PrListState::Loading => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading pull requests…"))
                    .into_any_element(),
                PrListState::Failed(msg) => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::red())
                    .child(SharedString::from(msg.clone()))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } if filtered.is_empty() => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("no matching pull requests"))
                    .into_any_element(),
                PrListState::Loaded { filtered, .. } => {
                    let entity = cx.entity();
                    let count = filtered.len();
                    div()
                        .py_1()
                        .h(px(count.min(10) as f32 * PALETTE_ROW_HEIGHT + 8.))
                        .child(
                            uniform_list("palette-prs", count, move |range, _window, cx| {
                                let this = entity.read(cx);
                                let Some(PaletteStep::PrList {
                                    prs:
                                        PrListState::Loaded {
                                            all,
                                            filtered,
                                            selected,
                                        },
                                    ..
                                }) = &this.palette
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|pos| Some((pos, &all[*filtered.get(pos)?])))
                                    .map(|(pos, pr)| {
                                        palette_pr_row(pr, pos, pos == *selected, entity.clone())
                                    })
                                    .collect()
                            })
                            .track_scroll(self.palette_scroll.clone())
                            .h_full(),
                        )
                        .into_any_element()
                }
            },
            PaletteStep::LocalBaseList { bases, .. } => match bases {
                BaseListState::Loading => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("loading base refs…"))
                    .into_any_element(),
                BaseListState::Failed(msg) => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::red())
                    .child(SharedString::from(msg.clone()))
                    .into_any_element(),
                BaseListState::Loaded { filtered, .. } if filtered.is_empty() => div()
                    .px_3()
                    .py_2()
                    .text_color(theme::overlay0())
                    .child(SharedString::from("no matching base refs"))
                    .into_any_element(),
                BaseListState::Loaded { filtered, .. } => {
                    let entity = cx.entity();
                    let count = filtered.len();
                    div()
                        .py_1()
                        .h(px(count.min(10) as f32 * PALETTE_ROW_HEIGHT + 8.))
                        .child(
                            uniform_list("palette-bases", count, move |range, _window, cx| {
                                let this = entity.read(cx);
                                let Some(PaletteStep::LocalBaseList {
                                    current,
                                    bases: BaseListState::Loaded { all, filtered, selected },
                                    ..
                                }) = &this.palette
                                else {
                                    return Vec::new();
                                };
                                range
                                    .filter_map(|pos| {
                                        let base = all.get(*filtered.get(pos)?)?;
                                        Some((pos, base.clone(), base == current))
                                    })
                                    .map(|(pos, base, is_current)| {
                                        div()
                                            .id(("palette-base", pos))
                                            .mx_1()
                                            .px_2()
                                            .h(px(PALETTE_ROW_HEIGHT))
                                            .rounded_md()
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .cursor_pointer()
                                            .when(pos == *selected, |row| row.bg(theme::surface0()))
                                            .when(pos != *selected, |row| {
                                                row.hover(|style| {
                                                    style.bg(Hsla::from(theme::surface0()).opacity(0.5))
                                                })
                                            })
                                            .on_click({
                                                let entity = entity.clone();
                                                move |_, window, cx| {
                                                    entity.update(cx, |this, cx| {
                                                        this.palette_open_base_row(pos, window, cx)
                                                    });
                                                }
                                            })
                                            .child(
                                                div()
                                                    .w(px(8.))
                                                    .h(px(8.))
                                                    .flex_shrink_0()
                                                    .rounded_full()
                                                    .bg(if is_current {
                                                        theme::green()
                                                    } else {
                                                        theme::overlay0()
                                                    }),
                                            )
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .text_color(theme::text())
                                                    .child(SharedString::from(base)),
                                            )
                                            .into_any_element()
                                    })
                                    .collect()
                            })
                            .track_scroll(self.palette_scroll.clone())
                            .h_full(),
                        )
                        .into_any_element()
                }
            },
        };

        // Backdrop: dims and occludes everything (sidebar included); click
        // closes. The panel swallows its own mouse-downs so clicks inside
        // don't bubble to the backdrop.
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
                    this.close_palette(window, cx);
                }),
            )
            .child(
                div()
                    .key_context("Palette")
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_action(cx.listener(|this, _: &PaletteUp, _, cx| this.palette_move(-1, cx)))
                    .on_action(cx.listener(|this, _: &PaletteDown, _, cx| this.palette_move(1, cx)))
                    .on_action(cx.listener(|this, _: &PaletteBack, window, cx| {
                        this.palette_back(window, cx)
                    }))
                    // The palette input propagates Escape when it has nothing
                    // of its own to dismiss; intercept it here before the
                    // root's handler steals focus back to the diff.
                    .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                        this.palette_back(window, cx)
                    }))
                    .w(px(560.))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme::surface0())
                    .bg(theme::mantle())
                    .shadow_lg()
                    .text_size(px(13.))
                    .flex()
                    .flex_col()
                    .overflow_hidden()
                    .when_some(header, |panel, header| {
                        panel.child(
                            div()
                                .px_3()
                                .pt_2()
                                .text_size(px(11.))
                                .text_color(theme::overlay0())
                                .child(header),
                        )
                    })
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(theme::surface0())
                            .child(Input::new(&self.palette_input).small()),
                    )
                    .child(body),
            )
            .into_any_element()
    }

}
