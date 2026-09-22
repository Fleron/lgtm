use crate::comments::{group_comments, CommentIndex, LocalReview, COMMENT_WRAP_CHARS};
use crate::diff::{
    build_rows, is_comment_row, nth_noncomment_row, row_height, run_upgrade, FileUpgrade,
    UpgradeJob, UpgradeSource,
};
use crate::lsp_client::{LspBackend, LspProgress};
use crate::minimap::{minimap_rows, minimap_runs, MinimapLayout, MinimapRow};
use crate::selection::Selection;
use crate::theme;
use crate::tree::{build_tree, TreeEntry, TreeEntryKind};
use crate::{
    chat_scratch_root, ChatState, HoverState, LspHandle, NavLocation, ReviewApp, Row,
    SourceViewState, SymbolTarget, ViewMode,
};
use anyhow::anyhow;
use diff_core::{FileStatus, PrDiff};
use gpui::{point, prelude::*, px, Context, SharedString, UniformListScrollHandle, Window};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Where a review item's diff comes from.
#[derive(Clone)]
pub(crate) enum Source {
    Pr(gh::PrLocator),
    Local(git::LocalSource),
}

pub(crate) enum ItemState {
    Loading,
    Ready(Box<ItemData>),
    Failed(String),
}

/// Everything the diff pane needs for one loaded item. Each item owns its
/// scroll handle so per-item scroll position survives switching.
pub(crate) struct ItemData {
    pub(crate) pr_meta: Option<gh::PrMeta>,
    pub(crate) diff: PrDiff,
    /// The raw unified patch this diff was parsed from, kept for chat
    /// context (capped at [`MAX_CHAT_PATCH_BYTES`] when sent).
    pub(crate) patch: String,
    /// Top-level PR conversation comments, oldest first; empty for local
    /// items. Rendered by the `cmd-g` panel.
    pub(crate) pr_comments: Vec<gh::IssueComment>,
    /// Submitted PR reviews (verdict + summary body); empty for local items.
    pub(crate) pr_reviews: Vec<gh::PrReview>,
    /// Per-item chat transcript + session; survives refresh, dies with the
    /// item.
    pub(crate) chat: ChatState,
    pub(crate) mode: ViewMode,
    pub(crate) rows: Vec<Row>,
    pub(crate) file_rows: Vec<usize>,
    pub(crate) hunk_rows: Vec<usize>,
    /// Minimap model, index-aligned with `rows`; rebuilt with them.
    pub(crate) minimap: Vec<MinimapRow>,
    /// Coalesced minimap quad runs for one pane height, computed lazily on
    /// paint and reused until the height changes or the rows are rebuilt.
    pub(crate) minimap_cache: RefCell<Option<(f32, Rc<MinimapLayout>)>>,
    pub(crate) cursor: usize,
    pub(crate) scroll: UniformListScrollHandle,
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    /// Mouse text selection, in display-row space. Per item (survives item
    /// switching); cleared on view-mode toggle and refresh, where row indices
    /// change meaning.
    pub(crate) selection: Option<Selection>,
    /// Phase-2 upgrades by file index into `diff.files`: whole-file span
    /// tables, full new-side lines, and expanded gaps. The re-diffed hunks
    /// themselves replace `diff.files[ix].hunks`. Reset on refresh.
    pub(crate) upgrades: HashMap<usize, FileUpgrade>,
    /// Review threads grouped by anchor; Some for PR items (possibly empty),
    /// and for local items with in-memory draft comments.
    pub(crate) comments: Option<CommentIndex>,
    pub(crate) local_review: Option<LocalReview>,
    /// `c` toggles the comment rows; the file-header counts always show.
    pub(crate) comments_visible: bool,
    /// Columns comment bodies are currently wrapped at, derived from the
    /// measured pane width (see `comment_wrap_cols`). Changing it re-wraps the
    /// bodies, which changes the row count, so render rebuilds the rows when
    /// the pane resizes past a column boundary.
    pub(crate) comment_wrap: usize,
    /// Users offered by the composer's `@`-mention autocomplete. Shared live
    /// with any open `MentionProvider`; seeded from PR participants, then
    /// filled from the repo's mentionable set (see `mentions_fetched`).
    pub(crate) mentions: Rc<RefCell<Vec<gh::Mention>>>,
    /// The one-time background mentionable-user fetch has been kicked off.
    pub(crate) mentions_fetched: bool,
    /// Sidebar file tree, rebuilt whenever the diff itself changes (load,
    /// refresh, blob upgrade) — but not on view-mode toggles: entries map to
    /// file indices, not row indices, so they survive row rebuilds.
    pub(crate) tree: Vec<TreeEntry>,
    /// Collapsed directory paths, preserved across rebuilds where the
    /// directory still exists.
    pub(crate) collapsed: HashSet<String>,
    pub(crate) tree_scroll: UniformListScrollHandle,
    /// The file last auto-centered in the tree, so follow-the-diff only
    /// scrolls the tree when the viewport's file changes — never while the
    /// user scrolls the tree themselves.
    pub(crate) tree_last_file: Option<usize>,
    pub(crate) lsp: Option<LspHandle>,
    pub(crate) lsp_progress: Option<Arc<Mutex<LspProgress>>>,
    pub(crate) lsp_loading: bool,
    pub(crate) lsp_error: Option<SharedString>,
    /// Which server backs this item's hover/goto; the "LSP" status chip toggles
    /// it (Bifrost ↔ rust-analyzer) and restarts the session. Per-item so a
    /// buildable Rust review can use rust-analyzer without affecting others.
    pub(crate) lsp_backend: LspBackend,
    pub(crate) hover: Option<HoverState>,
    pub(crate) hover_gen: u64,
    pub(crate) hover_cancel: Option<Arc<AtomicBool>>,
    pub(crate) last_symbol_target: Option<SymbolTarget>,
    pub(crate) source_view: Option<SourceViewState>,
    pub(crate) nav_back: Vec<NavLocation>,
    pub(crate) nav_forward: Vec<NavLocation>,
}

impl ItemData {
    /// Install freshly built display rows, keeping the minimap model in sync
    /// (every row rebuild goes through here).
    pub(crate) fn set_rows(&mut self, (rows, file_rows, hunk_rows): (Vec<Row>, Vec<usize>, Vec<usize>)) {
        self.minimap = minimap_rows(&rows);
        self.minimap_cache.replace(None);
        self.rows = rows;
        self.file_rows = file_rows;
        self.hunk_rows = hunk_rows;
    }

    /// Rebuild the display rows after only the comment rows changed
    /// (visibility toggle, comment refetch), keeping the viewport anchored:
    /// the first visible non-comment row stays put even though comment rows
    /// above it appeared or disappeared.
    pub(crate) fn rebuild_rows_anchored(&mut self) {
        let offset = self.scroll.0.borrow().base_handle.offset();
        let top_px = f32::from(-offset.y).max(0.);
        let top_row =
            ((top_px / row_height()).floor() as usize).min(self.rows.len().saturating_sub(1));
        let frac = top_px - top_row as f32 * row_height();
        let count_noncomment =
            |rows: &[Row]| rows.iter().filter(|row| !is_comment_row(row)).count();
        let top_base = count_noncomment(&self.rows[..top_row]);
        let cursor_base = count_noncomment(&self.rows[..self.cursor.min(self.rows.len())]);
        self.set_rows(build_rows(
            &self.diff,
            self.mode,
            &self.upgrades,
            self.comments.as_ref(),
            self.comments_visible,
            self.comment_wrap,
        ));
        self.selection = None;
        self.cursor = nth_noncomment_row(&self.rows, cursor_base);
        let new_top = nth_noncomment_row(&self.rows, top_base);
        self.scroll
            .0
            .borrow()
            .base_handle
            .set_offset(point(offset.x, px(-(new_top as f32 * row_height() + frac))));
    }

    /// The minimap quad runs for this pane height, from the cache when the
    /// height hasn't changed since the last paint.
    pub(crate) fn minimap_layout(&self, pane_px: f32) -> Rc<MinimapLayout> {
        let mut cache = self.minimap_cache.borrow_mut();
        if let Some((h, layout)) = &*cache {
            if *h == pane_px {
                return layout.clone();
            }
        }
        let layout = Rc::new(minimap_runs(&self.minimap, pane_px));
        *cache = Some((pane_px, layout.clone()));
        layout
    }

    /// Rebuild the sidebar file tree from the current diff, keeping collapse
    /// state for directories that still exist.
    pub(crate) fn rebuild_tree(&mut self) {
        let paths: Vec<&str> = self.diff.files.iter().map(|f| f.display_path()).collect();
        let tree = build_tree(&paths);
        self.collapsed.retain(|path| {
            tree.iter()
                .any(|e| matches!(&e.kind, TreeEntryKind::Dir { path: p } if p == path))
        });
        self.tree = tree;
        self.tree_last_file = None;
    }
}

pub(crate) struct ReviewItem {
    pub(crate) id: u64,
    pub(crate) source: Source,
    pub(crate) state: ItemState,
    /// A refresh is in flight while the old data stays visible.
    pub(crate) reloading: bool,
    pub(crate) refresh_error: Option<SharedString>,
    /// Bumped whenever fresh data is installed; an in-flight Phase-2 upgrade
    /// only lands if the generation it captured is still current.
    pub(crate) upgrade_gen: u64,
    /// Bumped whenever LSP root/session startup is restarted.
    pub(crate) lsp_gen: u64,
}

impl ReviewItem {
    pub(crate) fn primary(&self) -> SharedString {
        match &self.source {
            Source::Pr(loc) => format!("{}#{}", loc.repo_slug(), loc.number).into(),
            Source::Local(src) => src.branch.clone().into(),
        }
    }

    pub(crate) fn secondary(&self) -> SharedString {
        match &self.source {
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) => data
                    .pr_meta
                    .as_ref()
                    .map(|meta| meta.title.clone())
                    .unwrap_or_default()
                    .into(),
                _ => SharedString::default(),
            },
            Source::Local(src) => {
                format!("{} ← {}", dir_name(&src.repo_root), src.base_label).into()
            }
        }
    }

    pub(crate) fn dot_color(&self) -> gpui::Rgba {
        match &self.source {
            Source::Local(_) => theme::blue(),
            Source::Pr(_) => match &self.state {
                ItemState::Ready(data) if data.pr_meta.as_ref().is_some_and(|m| m.is_draft) => {
                    theme::overlay0()
                }
                ItemState::Ready(data) => match data.pr_meta.as_ref().map(|m| m.state.as_str()) {
                    Some("OPEN") => theme::green(),
                    Some("MERGED") => theme::mauve(),
                    Some("CLOSED") => theme::red(),
                    _ => theme::overlay0(),
                },
                ItemState::Failed(_) => theme::red(),
                ItemState::Loading => theme::overlay0(),
            },
        }
    }

    /// Swap fetched data in. On refresh (already Ready) scroll position,
    /// cursor, and view mode are preserved.
    pub(crate) fn install(&mut self, loaded: Loaded) {
        let Loaded {
            meta,
            diff,
            patch,
            mut rows,
            mut file_rows,
            mut hunk_rows,
            mode,
            comments,
            pr_comments,
            pr_reviews,
        } = loaded;
        let (additions, deletions) = diff
            .files
            .iter()
            .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
        let pr_meta = match meta {
            LoadedMeta::Pr(meta) => Some(meta),
            LoadedMeta::Local(src) => {
                // Branch/base may have moved since open; keep the label fresh.
                self.source = Source::Local(src);
                None
            }
        };
        self.refresh_error = None;
        match &mut self.state {
            ItemState::Ready(data) => {
                // Fresh patch-derived data: any previous upgrade (and its
                // expanded gaps) is stale — the upgrade re-runs from scratch.
                data.upgrades.clear();
                data.comments = match &data.local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                if data.local_review.is_some() || data.mode != mode || !data.comments_visible {
                    // Background rows assumed the fetched comments. Local
                    // drafts are reattached here, and view/comment toggles may
                    // have changed while the refresh ran.
                    (rows, file_rows, hunk_rows) = build_rows(
                        &diff,
                        data.mode,
                        &data.upgrades,
                        data.comments.as_ref(),
                        data.comments_visible,
                        data.comment_wrap,
                    );
                }
                if pr_meta.is_some() {
                    data.pr_meta = pr_meta;
                }
                data.diff = diff;
                data.patch = patch;
                data.pr_comments = pr_comments;
                data.pr_reviews = pr_reviews;
                data.set_rows((rows, file_rows, hunk_rows));
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.additions = additions;
                data.deletions = deletions;
                data.selection = None;
                data.rebuild_tree();
            }
            _ => {
                let minimap = minimap_rows(&rows);
                let local_review = matches!(&self.source, Source::Local(_))
                    .then(LocalReview::default);
                let comments = match &local_review {
                    Some(local) => Some(local.index()),
                    None => comments,
                };
                let mut data = Box::new(ItemData {
                    pr_meta,
                    diff,
                    patch,
                    pr_comments,
                    pr_reviews,
                    chat: ChatState::new(),
                    mode,
                    rows,
                    file_rows,
                    hunk_rows,
                    minimap,
                    minimap_cache: RefCell::new(None),
                    cursor: 0,
                    scroll: UniformListScrollHandle::new(),
                    additions,
                    deletions,
                    selection: None,
                    upgrades: HashMap::new(),
                    comments,
                    local_review,
                    comments_visible: true,
                    comment_wrap: COMMENT_WRAP_CHARS,
                    mentions: Rc::new(RefCell::new(Vec::new())),
                    mentions_fetched: false,
                    tree: Vec::new(),
                    collapsed: HashSet::new(),
                    tree_scroll: UniformListScrollHandle::new(),
                    tree_last_file: None,
                    lsp: None,
                    lsp_progress: None,
                    lsp_loading: false,
                    lsp_error: None,
                    lsp_backend: LspBackend::default(),
                    hover: None,
                    hover_gen: 0,
                    hover_cancel: None,
                    last_symbol_target: None,
                    source_view: None,
                    nav_back: Vec::new(),
                    nav_forward: Vec::new(),
                });
                data.rebuild_tree();
                self.state = ItemState::Ready(data);
            }
        }
    }
}

pub(crate) fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

pub(crate) enum LoadedMeta {
    Pr(gh::PrMeta),
    Local(git::LocalSource),
}

pub(crate) struct Loaded {
    pub(crate) meta: LoadedMeta,
    pub(crate) diff: PrDiff,
    /// The raw unified patch the diff was parsed from (chat context).
    pub(crate) patch: String,
    pub(crate) rows: Vec<Row>,
    pub(crate) file_rows: Vec<usize>,
    pub(crate) hunk_rows: Vec<usize>,
    pub(crate) mode: ViewMode,
    /// Some (possibly empty) for PR items, None for local ones.
    pub(crate) comments: Option<CommentIndex>,
    /// Empty for local items.
    pub(crate) pr_comments: Vec<gh::IssueComment>,
    /// Empty for local items.
    pub(crate) pr_reviews: Vec<gh::PrReview>,
}

/// Blocking fetch + parse + row building for one item; runs on the background
/// executor, so subprocess waits and tree-sitter work stay off the main thread.
/// PR items fetch meta, patch, review comments, and the conversation
/// (top-level comments + reviews) concurrently.
pub(crate) fn fetch_item(source: &Source, mode: ViewMode) -> anyhow::Result<Loaded> {
    let (meta, patch, comments, pr_comments, pr_reviews) = match source {
        Source::Pr(loc) => {
            let meta_loc = loc.clone();
            let meta_thread = std::thread::spawn(move || gh::fetch_meta(&meta_loc));
            let comments_loc = loc.clone();
            let comments_thread =
                std::thread::spawn(move || gh::fetch_review_comments(&comments_loc));
            let pr_comments_loc = loc.clone();
            let pr_comments_thread =
                std::thread::spawn(move || gh::fetch_pr_comments(&pr_comments_loc));
            let pr_reviews_loc = loc.clone();
            let pr_reviews_thread =
                std::thread::spawn(move || gh::fetch_pr_reviews(&pr_reviews_loc));
            let patch = gh::fetch_patch(loc)?;
            let meta = meta_thread
                .join()
                .map_err(|_| anyhow!("gh metadata fetch panicked"))??;
            let comments = comments_thread
                .join()
                .map_err(|_| anyhow!("gh comments fetch panicked"))??;
            let pr_comments = pr_comments_thread
                .join()
                .map_err(|_| anyhow!("gh PR comments fetch panicked"))??;
            let pr_reviews = pr_reviews_thread
                .join()
                .map_err(|_| anyhow!("gh PR reviews fetch panicked"))??;
            (
                LoadedMeta::Pr(meta),
                patch,
                Some(group_comments(comments)),
                pr_comments,
                pr_reviews,
            )
        }
        Source::Local(src) => {
            let src = git::resolve_local_with_base(&src.repo_root, src.base_ref.as_deref())?;
            let patch = git::diff_patch(&src)?;
            (LoadedMeta::Local(src), patch, None, Vec::new(), Vec::new())
        }
    };
    let diff = diff_core::parse_patch(&patch);
    let (rows, file_rows, hunk_rows) =
        build_rows(&diff, mode, &HashMap::new(), comments.as_ref(), true, COMMENT_WRAP_CHARS);
    Ok(Loaded {
        meta,
        diff,
        patch,
        rows,
        file_rows,
        hunk_rows,
        mode,
        comments,
        pr_comments,
        pr_reviews,
    })
}

impl ReviewApp {
    pub(crate) fn open_item(&mut self, source: Source, cx: &mut Context<Self>) {
        let id = self.next_id;
        self.next_id += 1;
        self.items.push(ReviewItem {
            id,
            source: source.clone(),
            state: ItemState::Loading,
            reloading: false,
            refresh_error: None,
            upgrade_gen: 0,
            lsp_gen: 0,
        });
        self.active = self.items.len() - 1;
        Self::spawn_fetch(id, source, ViewMode::Split, cx);
        cx.notify();
    }

    pub(crate) fn spawn_fetch(id: u64, source: Source, mode: ViewMode, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { fetch_item(&source, mode) })
                .await;
            this.update(cx, |app, cx| {
                let mut restart_lsp = false;
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                item.reloading = false;
                match fetched {
                    Ok(loaded) => {
                        item.install(loaded);
                        restart_lsp = true;
                        // Phase 2: after the instant patch-derived paint,
                        // upgrade every eligible file to full contents in the
                        // background. The bumped generation cancels any
                        // still-running upgrade from before a refresh.
                        item.upgrade_gen += 1;
                        let gen = item.upgrade_gen;
                        if let ItemState::Ready(data) = &item.state {
                            let jobs: Vec<UpgradeJob> = data
                                .diff
                                .files
                                .iter()
                                .enumerate()
                                .filter(|(_, f)| {
                                    f.status != FileStatus::Binary && !f.hunks.is_empty()
                                })
                                .map(|(ix, f)| UpgradeJob {
                                    file_ix: ix,
                                    old_path: f.old_path.clone(),
                                    new_path: f.new_path.clone(),
                                    status: f.status,
                                })
                                .collect();
                            let source = match &item.source {
                                Source::Pr(loc) => data
                                    .pr_meta
                                    .as_ref()
                                    .filter(|meta| {
                                        !meta.base_ref_oid.is_empty()
                                            && !meta.head_ref_oid.is_empty()
                                    })
                                    .map(|meta| UpgradeSource::Pr {
                                        loc: loc.clone(),
                                        base_oid: meta.base_ref_oid.clone(),
                                        head_oid: meta.head_ref_oid.clone(),
                                    }),
                                Source::Local(src) => Some(UpgradeSource::Local(src.clone())),
                            };
                            if let (Some(source), false) = (source, jobs.is_empty()) {
                                Self::spawn_upgrade(id, gen, source, jobs, cx);
                            }
                        }
                    }
                    Err(err) => {
                        let msg = format!("{err:#}");
                        match &item.state {
                            // Refresh failure: keep the stale-but-useful data.
                            ItemState::Ready(_) => item.refresh_error = Some(msg.into()),
                            _ => item.state = ItemState::Failed(msg),
                        }
                    }
                }
                if restart_lsp {
                    app.restart_lsp_for_item(id, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Phase 2, per item: fetch every file's full contents, re-diff, and
    /// re-highlight in the background, then rebuild rows once and swap
    /// atomically. Scroll keeps its pixel offset (the re-diff normally
    /// reproduces the patch's hunks, so drift is small); the selection is
    /// cleared because row indices shift.
    pub(crate) fn spawn_upgrade(
        id: u64,
        gen: u64,
        source: UpgradeSource,
        jobs: Vec<UpgradeJob>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let upgraded = cx
                .background_spawn(async move { run_upgrade(&source, jobs) })
                .await;
            if upgraded.is_empty() {
                return;
            }
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                    return;
                };
                if item.upgrade_gen != gen {
                    return;
                }
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                for file in upgraded {
                    let Some(target) = data.diff.files.get_mut(file.file_ix) else {
                        continue;
                    };
                    target.hunks = file.hunks;
                    target.additions = file.additions;
                    target.deletions = file.deletions;
                    data.upgrades.insert(file.file_ix, file.upgrade);
                }
                (data.additions, data.deletions) = data
                    .diff
                    .files
                    .iter()
                    .fold((0, 0), |(a, d), f| (a + f.additions, d + f.deletions));
                // Comment anchors live in the same absolute line-number space
                // the re-diff produces, so threads re-insert at the re-diffed
                // rows without translation.
                data.set_rows(build_rows(
                    &data.diff,
                    data.mode,
                    &data.upgrades,
                    data.comments.as_ref(),
                    data.comments_visible,
                    data.comment_wrap,
                ));
                data.cursor = data.cursor.min(data.rows.len().saturating_sub(1));
                data.selection = None;
                // Paths can't change in an upgrade, but stats did; rebuilding
                // keeps the tree in lockstep with every row rebuild.
                data.rebuild_tree();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn submit_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.open_input.read(cx).value().trim().to_string();
        if value.is_empty() {
            return;
        }
        let parsed = if Path::new(&value).is_dir() {
            git::resolve_local(Path::new(&value)).map(Source::Local)
        } else {
            gh::resolve_pr_arg(&value).map(Source::Pr)
        };
        match parsed {
            Ok(source) => {
                self.open_error = None;
                self.open_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.open_item(source, cx);
                window.focus(&self.focus_handle);
            }
            Err(err) => self.open_error = Some(format!("{err:#}").into()),
        }
        cx.notify();
    }

    pub(crate) fn activate(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.items.len() {
            self.active = ix;
            window.focus(&self.focus_handle);
            cx.notify();
        }
    }

    pub(crate) fn close_item(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.items.len() {
            return;
        }
        // Stop any in-flight chat run and drop its scratch dir (best-effort;
        // the path is ours and never the local repo root).
        let item = &self.items[ix];
        if let ItemState::Ready(data) = &item.state {
            data.chat.cancel.store(true, Ordering::Relaxed);
        }
        let _ = std::fs::remove_dir_all(chat_scratch_root(item.id));
        let was_pr = matches!(item.source, Source::Pr(_));
        self.items.remove(ix);
        if self.active > ix || self.active >= self.items.len() {
            self.active = self.active.saturating_sub(1);
        }
        // A PR reviewed this session may have just materialized its worktree;
        // rescan so it (re)appears in the sidebar's cached list now that it's
        // closed, rather than only after a restart.
        if was_pr {
            self.refresh_cached_prs(cx);
        }
        cx.notify();
    }

    pub(crate) fn cycle_items(&mut self, delta: isize, cx: &mut Context<Self>) {
        let len = self.items.len() as isize;
        if len > 0 {
            self.active = (self.active as isize + delta).rem_euclid(len) as usize;
            cx.notify();
        }
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        if matches!(item.state, ItemState::Loading) || item.reloading {
            return;
        }
        let mode = match &item.state {
            ItemState::Ready(data) => data.mode,
            _ => ViewMode::Split,
        };
        match item.state {
            ItemState::Failed(_) => item.state = ItemState::Loading,
            _ => item.reloading = true,
        }
        item.refresh_error = None;
        let (id, source) = (item.id, item.source.clone());
        Self::spawn_fetch(id, source, mode, cx);
        cx.notify();
    }

    /// Refetch only the review comments (not meta/patch), regroup, and
    /// rebuild the rows with the viewport anchored — the counterpart of a
    /// full refresh for the post-comment path.
    pub(crate) fn refetch_comments(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_review_comments(&loc) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                match fetched {
                    Ok(comments) => {
                        data.comments = Some(group_comments(comments));
                        data.rebuild_rows_anchored();
                    }
                    Err(err) => {
                        item.refresh_error =
                            Some(format!("comment refresh failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Refetch only the PR meta — the post-review counterpart of
    /// `refetch_comments`, so the titlebar reflects the new review decision
    /// without reloading the whole diff.
    pub(crate) fn refetch_meta(&mut self, item_id: u64, loc: gh::PrLocator, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let fetched = cx
                .background_spawn(async move { gh::fetch_meta(&loc) })
                .await;
            this.update(cx, |app, cx| {
                let Some(item) = app.items.iter_mut().find(|item| item.id == item_id) else {
                    return;
                };
                let ItemState::Ready(data) = &mut item.state else {
                    return;
                };
                match fetched {
                    Ok(meta) => data.pr_meta = Some(meta),
                    Err(err) => {
                        item.refresh_error = Some(format!("meta refresh failed: {err:#}").into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

}
