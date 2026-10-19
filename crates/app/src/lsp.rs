use crate::app::PendingCheckoutTask;
use crate::comments::comment_anchor;
use crate::items::{ItemData, ItemState, Source};
use crate::lsp_client::{
    trace as lsp_trace, DefinitionTarget, HoverResult, LspPosition, LspProgress, LspSession,
};
use crate::review_checkouts::{CheckoutOperation, ReviewCheckoutLease};
use crate::selection::{RowCol, SelSide};
use crate::terminal::TerminalStatus;
use crate::theme;
use crate::{
    line_content, row_height, text_size, NavBack, NavForward, ReviewApp, Row,
    MAX_SOURCE_HIGHLIGHT_BYTES, MAX_SYNTAX_LINE_BYTES, MONO,
};
use anyhow::Context as _;
use gpui::{
    anchored, deferred, div, font, point, prelude::*, px, uniform_list, Context, Hsla,
    ListHorizontalSizingBehavior, Pixels, Point, ScrollStrategy, SharedString, TextRun,
    UniformListScrollHandle, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants as _},
    tooltip::Tooltip,
    Disableable as _, Icon, IconName, Sizable as _,
};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
pub(crate) struct LspHandle {
    pub(crate) root: PathBuf,
    pub(crate) session: LspSession,
    pub(crate) progress: Arc<Mutex<LspProgress>>,
    // Keep this after the session so dropping a handle reaps its server first.
    _operation: Option<Arc<CheckoutOperation>>,
}

fn stop_lsp_blocking(handle: LspHandle) {
    handle.session.shutdown_blocking();
    drop(handle);
}

pub(crate) fn stop_lsp(
    handle: LspHandle,
    pending_task: PendingCheckoutTask,
    cx: &mut Context<ReviewApp>,
) {
    cx.background_spawn(async move {
        stop_lsp_blocking(handle);
        drop(pending_task);
    })
    .detach();
}

pub(crate) fn release_checkout(
    lease: ReviewCheckoutLease,
    pending_task: PendingCheckoutTask,
    cx: &mut Context<ReviewApp>,
) {
    lease.mark_closing();
    let app = cx.weak_entity();
    cx.spawn(async move |_, cx| {
        let result = cx.background_spawn(async move { lease.teardown() }).await;
        if let Err(err) = result {
            eprintln!("lgtm: review checkout cleanup deferred: {err:#}");
        }
        drop(pending_task);
        app.update(cx, |app, cx| app.refresh_cached_prs(cx)).ok();
    })
    .detach();
}

pub(crate) fn release_checkout_after_sessions(
    lease: ReviewCheckoutLease,
    lsp: Option<LspHandle>,
    terminal: Option<crate::terminal::TerminalSession>,
    pending_task: PendingCheckoutTask,
    cx: &mut Context<ReviewApp>,
) {
    lease.mark_closing();
    let app = cx.weak_entity();
    cx.spawn(async move |_, cx| {
        let result = cx
            .background_spawn(async move {
                if let Some(lsp) = lsp {
                    stop_lsp_blocking(lsp);
                }
                if let Some(terminal) = terminal {
                    terminal.stop_blocking();
                }
                lease.teardown()
            })
            .await;
        if let Err(err) = result {
            eprintln!("lgtm: review checkout cleanup deferred: {err:#}");
        }
        drop(pending_task);
        app.update(cx, |app, cx| app.refresh_cached_prs(cx)).ok();
    })
    .detach();
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct SymbolTarget {
    pub(crate) position: LspPosition,
    pub(crate) row: usize,
    pub(crate) col: usize,
}

#[derive(Clone)]
pub(crate) struct HoverState {
    pub(crate) result: Option<HoverResult>,
    pub(crate) loading: bool,
    pub(crate) mouse: Point<Pixels>,
}

#[derive(Clone)]
pub(crate) struct SourceViewState {
    pub(crate) target: DefinitionTarget,
    pub(crate) lines: Vec<SharedString>,
    /// Tree-sitter spans per line (line-relative byte ranges), index-aligned
    /// with `lines`; empty when the language is unknown or the file is too big.
    pub(crate) syntax: Vec<Vec<(Range<usize>, syntax::Token)>>,
    pub(crate) scroll: UniformListScrollHandle,
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) enum NavLocation {
    Diff { row: usize },
    Source { target: DefinitionTarget },
}

fn utf16_col_for_monospace_col(text: &str, col: usize) -> u32 {
    text.chars().take(col).map(|ch| ch.len_utf16() as u32).sum()
}

pub(crate) fn identifier_start_at(text: &str, col: usize) -> Option<usize> {
    let chars = text.chars().collect::<Vec<_>>();
    let is_identifier = |ch: char| ch == '_' || ch.is_alphanumeric();
    let mut cursor = col.min(chars.len());
    if chars.get(cursor).is_none_or(|ch| !is_identifier(*ch)) {
        if chars.get(cursor).is_none_or(|ch| ch.is_whitespace()) {
            return None;
        }
        cursor = cursor.checked_sub(1)?;
        if !is_identifier(chars[cursor]) {
            return None;
        }
    }
    while cursor > 0 && is_identifier(chars[cursor - 1]) {
        cursor -= 1;
    }
    Some(cursor)
}

/// Short status for a still-loading LSP. A real sub-100% percentage is genuine
/// indexing progress and worth showing; at 100% (or once the phase count is
/// gone) the server is usually still analyzing before hovers resolve — so show
/// "analyzing…" rather than a stuck "100%".
pub(crate) fn lsp_loading_label(percentage: Option<u32>) -> String {
    match percentage {
        Some(pct) if pct < 100 => format!("{pct}%"),
        _ => "analyzing…".to_string(),
    }
}

fn lsp_warmup_position(data: &ItemData) -> Option<LspPosition> {
    for (row_ix, row) in data.rows.iter().enumerate() {
        let (line, text) = match row {
            Row::Line { new_no, text, .. } => ((*new_no)? - 1, text.as_ref()),
            Row::SplitLine {
                right: Some(cell), ..
            } => (cell.no - 1, cell.text.as_ref()),
            _ => continue,
        };
        let file_ix = data.file_rows.iter().rposition(|&row| row <= row_ix)?;
        let path = data.diff.files.get(file_ix)?.new_path.as_ref()?.clone();
        let col = first_lsp_identifier_col(text)?;
        return Some(LspPosition {
            path,
            line,
            character: col,
        });
    }
    None
}

fn first_lsp_identifier_col(text: &str) -> Option<u32> {
    let mut chars = text.char_indices().peekable();
    while let Some((start_byte, ch)) = chars.next() {
        if !(ch == '_' || ch.is_ascii_alphabetic()) {
            continue;
        }
        let mut end_byte = start_byte + ch.len_utf8();
        while let Some(&(next_byte, next)) = chars.peek() {
            if next == '_' || next.is_ascii_alphanumeric() {
                end_byte = next_byte + next.len_utf8();
                chars.next();
            } else {
                break;
            }
        }
        let ident = &text[start_byte..end_byte];
        if !RUST_LSP_WARMUP_KEYWORDS.contains(&ident) {
            return Some(utf16_col_for_monospace_col(
                text,
                text[..start_byte].chars().count(),
            ));
        }
    }
    None
}

const RUST_LSP_WARMUP_KEYWORDS: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while",
];

impl ReviewApp {
    /// Switch the active item's LSP backend (Bifrost ↔ rust-analyzer) and
    /// restart its session. Driven by clicking the "LSP" status chip.
    pub(crate) fn toggle_lsp_backend(&mut self, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(self.active) else {
            return;
        };
        let id = item.id;
        let Some(data) = self.active_data_mut() else {
            return;
        };
        data.lsp_backend = data.lsp_backend.toggled();
        self.restart_lsp_for_item(id, cx);
        cx.notify();
    }

    pub(crate) fn restart_lsp_for_item(&mut self, id: u64, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let manager = self.checkouts.clone();
        let (
            source,
            pr_meta,
            warmup,
            backend,
            progress,
            lease,
            checkout_request,
            old_checkout,
            old_lsp,
            gen,
        ) = {
            let Some(item) = self.items.iter_mut().find(|item| item.id == id) else {
                return;
            };
            let ItemState::Ready(data) = &mut item.state else {
                return;
            };
            item.lsp_gen += 1;
            let gen = item.lsp_gen;
            let backend = data.lsp_backend;
            let old_lsp = data.lsp.take();
            data.lsp_error = None;
            data.lsp_loading = true;
            let progress = Arc::new(Mutex::new(LspProgress {
                title: Some("Indexing workspace".to_string()),
                message: Some(format!("Starting {}", backend.label())),
                percentage: Some(0),
                done: false,
            }));
            data.lsp_progress = Some(Arc::clone(&progress));
            if let Some(cancel) = data.hover_cancel.take() {
                cancel.store(true, Ordering::Release);
            }
            data.hover = None;
            data.source_view = None;
            let source = item.source.clone();
            let pr_meta = data.pr_meta.clone();
            let warmup = lsp_warmup_position(data);
            let mut old_checkout = None;
            let (lease, checkout_request) = match (&source, pr_meta.as_ref()) {
                (Source::Pr(loc), Some(meta)) if !meta.head_ref_oid.is_empty() => {
                    if item
                        .checkout
                        .as_ref()
                        .is_some_and(|checkout| checkout.head_oid() == meta.head_ref_oid)
                    {
                        (item.checkout.clone(), None)
                    } else {
                        old_checkout = item.checkout.take();
                        if let Some(checkout) = old_checkout.as_ref() {
                            checkout.mark_closing();
                        }
                        (None, Some((loc.clone(), meta.head_ref_oid.clone())))
                    }
                }
                (Source::Pr(_), _) => (None, None),
                (Source::Local(_), _) => (None, None),
            };
            (
                source,
                pr_meta,
                warmup,
                backend,
                progress,
                lease,
                checkout_request,
                old_checkout,
                old_lsp,
                gen,
            )
        };
        self.pending_checkout_tasks.cancel_superseded(id, gen);
        let mut old_cleanup = (old_checkout.is_some() || old_lsp.is_some())
            .then(|| self.pending_checkout_tasks.begin_cleanup(id));
        let Some(pending_task) = self.pending_checkout_tasks.begin(id, gen) else {
            return;
        };
        let old_lsp = match (old_checkout, old_lsp) {
            (Some(checkout), Some(lsp)) => {
                release_checkout_after_sessions(
                    checkout,
                    Some(lsp),
                    None,
                    old_cleanup.take().expect("checkout cleanup guard"),
                    cx,
                );
                None
            }
            (Some(checkout), None) => {
                release_checkout(
                    checkout,
                    old_cleanup.take().expect("checkout cleanup guard"),
                    cx,
                );
                None
            }
            (None, Some(lsp)) => Some(lsp),
            (None, None) => None,
        };
        let old_lsp_cleanup = old_lsp
            .as_ref()
            .map(|_| old_cleanup.take().expect("LSP cleanup guard"));
        let progress_for_start = Arc::clone(&progress);
        let manager_for_start = manager.clone();
        let pending_cancel = pending_task.cancel_token();
        cx.spawn(async move |this, cx| {
            let (result, pending_task) = cx
                .background_spawn(async move {
                    let result: anyhow::Result<(Option<ReviewCheckoutLease>, LspHandle)> = (|| {
                        if let Some(old_lsp) = old_lsp {
                            stop_lsp_blocking(old_lsp);
                        }
                        drop(old_lsp_cleanup);
                        let acquired_checkout = checkout_request.is_some();
                        if pending_cancel.load(Ordering::Acquire) {
                            anyhow::bail!("LSP startup canceled");
                        }
                        let mut lease = match checkout_request {
                            Some((loc, head_oid)) => Some(manager_for_start.acquire_cancellable(
                                &loc,
                                &head_oid,
                                pending_cancel.as_ref(),
                            )?),
                            None => lease,
                        };
                        let startup = (|| {
                            if let Some(lease) = lease.as_ref() {
                                pending_task.register_lease(lease);
                            }
                            if pending_task.is_cancelled() {
                                anyhow::bail!("LSP startup canceled");
                            }
                            let operation = lease
                                .as_ref()
                                .map(|lease| lease.operation("lsp startup"))
                                .transpose()?
                                .map(Arc::new);
                            let cancel = operation
                                .as_ref()
                                .map(|operation| operation.cancel_token())
                                .or_else(|| Some(Arc::clone(&pending_cancel)));
                            let root = match &source {
                                Source::Local(src) => src.repo_root.clone(),
                                Source::Pr(_loc) => {
                                    let _meta = pr_meta.as_ref().context(
                                        "PR metadata missing; cannot materialize LSP checkout",
                                    )?;
                                    let lease =
                                        lease.as_ref().context("PR checkout lease missing")?;
                                    manager_for_start.materialize(lease)?
                                }
                            };
                            let session = LspSession::start_cancellable(
                                root.clone(),
                                backend,
                                Arc::clone(&progress_for_start),
                                warmup,
                                cancel,
                            )?;
                            Ok::<LspHandle, anyhow::Error>(LspHandle {
                                root,
                                session,
                                progress: progress_for_start,
                                _operation: operation,
                            })
                        })();
                        if startup.is_err() && acquired_checkout {
                            if let Some(lease) = lease.take() {
                                let _ = lease.teardown();
                            }
                        }
                        let checkout = acquired_checkout.then_some(lease).flatten();
                        Ok(startup.map(|handle| (checkout, handle))?)
                    })(
                    );
                    (result, pending_task)
                })
                .await;
            let mut result = Some(result);
            let cleanup = this
                .update(cx, |app, cx| {
                    if app.closing {
                        return result.take();
                    }
                    let Some(item) = app.items.iter_mut().find(|item| item.id == id) else {
                        return result.take();
                    };
                    if item.lsp_gen != gen {
                        return result.take();
                    }
                    let ItemState::Ready(data) = &mut item.state else {
                        return result.take();
                    };
                    data.lsp_loading = false;
                    match result.take().expect("LSP startup result is consumed once") {
                        Ok((lease, handle)) => {
                            item.checkout = lease.or_else(|| item.checkout.clone());
                            data.lsp_progress = Some(Arc::clone(&handle.progress));
                            data.lsp = Some(handle);
                            data.lsp_error = None;
                        }
                        Err(err) => {
                            data.lsp = None;
                            data.lsp_error = Some(format!("{err:#}").into());
                        }
                    }
                    if data.terminal.status == TerminalStatus::WaitingForCheckout {
                        app.launch_terminal(id, cx);
                    }
                    cx.notify();
                    None
                })
                .ok()
                .flatten()
                .or_else(|| result.take());
            if let Some(Ok((lease, handle))) = cleanup {
                cx.background_spawn(async move {
                    stop_lsp_blocking(handle);
                    if let Some(lease) = lease {
                        let _ = lease.teardown();
                    }
                })
                .await;
            }
            drop(pending_task);
        })
        .detach();
        cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let keep_going = this
                .update(cx, |app, cx| {
                    let Some(item) = app.items.iter().find(|item| item.id == id) else {
                        return false;
                    };
                    if item.lsp_gen != gen {
                        return false;
                    }
                    let ItemState::Ready(data) = &item.state else {
                        return false;
                    };
                    if !data.lsp_loading {
                        return false;
                    }
                    cx.notify();
                    true
                })
                .unwrap_or(false);
            if !keep_going {
                break;
            }
        })
        .detach();
        cx.notify();
    }

    /// The row + split half under the pointer if it can take a new comment:
    /// the pointer is inside the diff list, and the row has a line number on
    /// that side. PR items also need a known head oid for GitHub anchoring.
    pub(crate) fn hover_target(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<(usize, SelSide)> {
        if self.palette.is_some() {
            return None;
        }
        let char_width = self.char_width(window);
        let item = self.active_item()?;
        let ItemState::Ready(data) = &item.state else {
            return None;
        };
        // New comments post against the head oid; without one (older gh
        // missing headRefOid) the affordance stays off entirely.
        if matches!(&item.source, Source::Pr(_)) {
            if data
                .pr_meta
                .as_ref()
                .is_none_or(|meta| meta.head_ref_oid.is_empty())
            {
                return None;
            }
        }
        let bounds = data.scroll.0.borrow().base_handle.bounds();
        if !bounds.contains(&position) {
            return None;
        }
        let (side, hit) = self.pane_hit(position, char_width, None)?;
        let data = self.active_data()?;
        comment_anchor(&data.rows, hit.row, side).map(|_| (hit.row, side))
    }

    pub(crate) fn symbol_target_at(
        &mut self,
        position: Point<Pixels>,
        window: &Window,
    ) -> Option<SymbolTarget> {
        if self.palette.is_some() || self.composer.is_some() || self.review.is_some() {
            return None;
        }
        let bounds = self.active_data()?.scroll.0.borrow().base_handle.bounds();
        if !bounds.contains(&position) {
            return None;
        }
        let (side, row, text_x) = self.pane_text_hit(position, None)?;
        let text = match self.active_data()?.rows.get(row)? {
            Row::Line { text, .. } if side == SelSide::Unified => text.as_ref(),
            Row::SplitLine {
                left: Some(cell), ..
            } if side == SelSide::Left => cell.text.as_ref(),
            Row::SplitLine {
                right: Some(cell), ..
            } if side == SelSide::Right => cell.text.as_ref(),
            _ => return None,
        };
        let shaped = window.text_system().shape_line(
            SharedString::from(text.to_string()),
            px(text_size()),
            &[TextRun {
                len: text.len(),
                font: font(MONO),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            None,
        );
        let byte_col = shaped.index_for_x(text_x.max(px(0.)))?;
        let col = text.get(..byte_col)?.chars().count();
        self.symbol_target_for_hit(side, RowCol { row, col })
    }

    pub(crate) fn symbol_target_for_hit(&self, side: SelSide, hit: RowCol) -> Option<SymbolTarget> {
        let data = self.active_data()?;
        let file_ix = data.file_rows.iter().rposition(|&row| row <= hit.row)?;
        let file = data.diff.files.get(file_ix)?;
        let path = file.new_path.as_ref()?.clone();
        let (line, text) = match data.rows.get(hit.row)? {
            Row::Line { new_no, text, .. } if side == SelSide::Unified => {
                ((*new_no)? - 1, text.as_ref())
            }
            Row::SplitLine {
                right: Some(cell), ..
            } if side == SelSide::Right => (cell.no - 1, cell.text.as_ref()),
            _ => return None,
        };
        let col = identifier_start_at(text, hit.col)?;
        let identifier = text
            .chars()
            .skip(col)
            .take_while(|ch| *ch == '_' || ch.is_alphanumeric())
            .collect::<String>();
        if path.ends_with(".rs") && RUST_LSP_WARMUP_KEYWORDS.contains(&identifier.as_str()) {
            return None;
        }
        let character = utf16_col_for_monospace_col(text, col);
        Some(SymbolTarget {
            position: LspPosition {
                path,
                line,
                character,
            },
            row: hit.row,
            col,
        })
    }

    pub(crate) fn request_hover(
        &mut self,
        target: SymbolTarget,
        mouse: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let checkout = self
            .items
            .get(self.active)
            .and_then(|item| item.checkout.clone());
        let Some(data) = self.active_data_mut() else {
            return;
        };
        data.hover_gen += 1;
        let gen = data.hover_gen;
        lsp_trace(format_args!(
            "ui hover gen={gen} {}:{}:{} ready={}",
            target.position.path,
            target.position.line + 1,
            target.position.character + 1,
            data.lsp.is_some()
        ));
        if let Some(cancel) = data.hover_cancel.take() {
            cancel.store(true, Ordering::Release);
        }
        let canceled = Arc::new(AtomicBool::new(false));
        data.hover_cancel = Some(Arc::clone(&canceled));
        data.last_symbol_target = Some(target.clone());
        let Some(handle) = data.lsp.clone() else {
            if data.lsp_loading {
                let waiting_target = target.clone();
                data.hover = Some(HoverState {
                    result: None,
                    loading: true,
                    mouse,
                });
                cx.notify();
                cx.spawn(async move |this, cx| loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(150))
                        .await;
                    let keep_waiting = this
                        .update(cx, |app, cx| {
                            let Some(data) = app.active_data() else {
                                return false;
                            };
                            if data.hover_gen != gen
                                || data.last_symbol_target.as_ref() != Some(&waiting_target)
                            {
                                return false;
                            }
                            if data.lsp.is_some() {
                                app.request_hover(waiting_target.clone(), mouse, cx);
                                return false;
                            }
                            data.lsp_loading
                        })
                        .unwrap_or(false);
                    if !keep_waiting {
                        break;
                    }
                })
                .detach();
            }
            return;
        };
        let operation = match checkout.as_ref() {
            Some(lease) => match lease.operation("hover request") {
                Ok(operation) => Some(operation),
                Err(_) => return,
            },
            None => None,
        };
        data.hover = None;
        cx.spawn(async move |this, cx| {
            let _operation = operation;
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let should_request = this
                .update(cx, |app, _cx| {
                    let Some(data) = app.active_data() else {
                        return false;
                    };
                    if data.hover_gen != gen || data.last_symbol_target.as_ref() != Some(&target) {
                        lsp_trace(format_args!("ui hover debounce canceled gen={gen}"));
                        return false;
                    }
                    true
                })
                .unwrap_or(false);
            if !should_request {
                return;
            }
            let position = target.position.clone();
            let result = cx
                .background_spawn(async move {
                    let result = handle.session.hover(position, canceled);
                    drop(handle);
                    result
                })
                .await;
            this.update(cx, |app, cx| {
                let Some(data) = app.active_data_mut() else {
                    return;
                };
                if data.hover_gen != gen || data.last_symbol_target.as_ref() != Some(&target) {
                    return;
                }
                match result {
                    Ok(Some(result)) => {
                        lsp_trace(format_args!("ui hover content gen={gen}"));
                        data.hover = Some(HoverState {
                            result: Some(result),
                            loading: false,
                            mouse,
                        });
                    }
                    Ok(None) => {
                        lsp_trace(format_args!("ui hover none gen={gen}"));
                        data.hover = None;
                    }
                    Err(err) => {
                        lsp_trace(format_args!("ui hover error gen={gen}: {err:#}"));
                        data.hover = None;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn go_to_last_symbol(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self
            .active_data()
            .and_then(|data| data.last_symbol_target.clone())
        else {
            return;
        };
        self.request_definition(target, cx);
    }

    pub(crate) fn request_definition(&mut self, origin: SymbolTarget, cx: &mut Context<Self>) {
        let Some(handle) = self.active_data().and_then(|data| data.lsp.clone()) else {
            return;
        };
        let checkout = self
            .items
            .get(self.active)
            .and_then(|item| item.checkout.clone());
        let operation = match checkout.as_ref() {
            Some(lease) => match lease.operation("definition request") {
                Ok(operation) => Some(operation),
                Err(_) => return,
            },
            None => None,
        };
        if let Some(data) = self.active_data_mut() {
            data.cursor = origin.row.min(data.rows.len().saturating_sub(1));
        }
        cx.spawn(async move |this, cx| {
            let _operation = operation;
            let position = origin.position.clone();
            let result = cx
                .background_spawn(async move {
                    let result = handle.session.definition(position);
                    drop(handle);
                    result
                })
                .await;
            this.update(cx, |app, cx| {
                let target = match result {
                    Ok(mut targets) => targets.drain(..).next(),
                    Err(err) => {
                        lsp_trace(format_args!("ui definition error: {err:#}"));
                        return;
                    }
                };
                let Some(target) = target else {
                    return;
                };
                app.open_source_target(target, true, cx);
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_source_target(
        &mut self,
        target: DefinitionTarget,
        push_history: bool,
        cx: &mut Context<Self>,
    ) {
        let (lines, syntax) = match self.read_lsp_source_lines(&target) {
            Ok(loaded) => loaded,
            Err(err) => {
                lsp_trace(format_args!("ui definition source error: {err:#}"));
                return;
            }
        };
        let current = self.current_nav_location();
        let scroll = UniformListScrollHandle::new();
        if !lines.is_empty() {
            scroll.scroll_to_item(
                (target.start_line as usize).min(lines.len() - 1),
                ScrollStrategy::Center,
            );
        }
        if let Some(data) = self.active_data_mut() {
            if push_history {
                if let Some(current) = current {
                    data.nav_back.push(current);
                    data.nav_forward.clear();
                }
            }
            data.source_view = Some(SourceViewState {
                target,
                lines,
                syntax,
                scroll,
            });
            data.hover = None;
            cx.notify();
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn read_lsp_source_lines(
        &self,
        target: &DefinitionTarget,
    ) -> anyhow::Result<(Vec<SharedString>, Vec<Vec<(Range<usize>, syntax::Token)>>)> {
        let data = self.active_data().context("no active item")?;
        let handle = data.lsp.as_ref().context("LSP is not ready")?;
        // Targets outside the workspace (dependency / stdlib sources) carry an
        // absolute path; in-repo ones are relative to the LSP root.
        let target_path = Path::new(&target.path);
        let path = if target_path.is_absolute() {
            target_path.to_path_buf()
        } else {
            handle.root.join(target_path)
        };
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let lines = text
            .lines()
            .map(|line| SharedString::from(line.to_string()))
            .collect();
        // Tree-sitter highlighting keyed off the file's extension, guarded on
        // total size (this runs synchronously when the view opens). Line-
        // relative spans, index-aligned with `lines`.
        let syntax = syntax::language_for_path(&target.path)
            .filter(|_| text.len() <= MAX_SOURCE_HIGHLIGHT_BYTES)
            .map(|lang| syntax::highlight_lines(lang, &text))
            .unwrap_or_default();
        Ok((lines, syntax))
    }

    pub(crate) fn current_nav_location(&self) -> Option<NavLocation> {
        let data = self.active_data()?;
        if let Some(source) = &data.source_view {
            Some(NavLocation::Source {
                target: source.target.clone(),
            })
        } else {
            Some(NavLocation::Diff { row: data.cursor })
        }
    }

    pub(crate) fn nav_back(&mut self, cx: &mut Context<Self>) {
        self.navigate_history(true, cx);
    }

    pub(crate) fn nav_forward(&mut self, cx: &mut Context<Self>) {
        self.navigate_history(false, cx);
    }

    pub(crate) fn navigate_history(&mut self, backward: bool, cx: &mut Context<Self>) {
        let Some(current) = self.current_nav_location() else {
            return;
        };
        let next = {
            let Some(data) = self.active_data_mut() else {
                return;
            };
            let stack = if backward {
                &mut data.nav_back
            } else {
                &mut data.nav_forward
            };
            let Some(next) = stack.pop() else {
                return;
            };
            if backward {
                data.nav_forward.push(current);
            } else {
                data.nav_back.push(current);
            }
            next
        };
        self.restore_nav_location(next, cx);
    }

    pub(crate) fn restore_nav_location(&mut self, location: NavLocation, cx: &mut Context<Self>) {
        match location {
            NavLocation::Diff { row } => {
                if let Some(data) = self.active_data_mut() {
                    data.source_view = None;
                }
                self.jump(row, cx);
            }
            NavLocation::Source { target } => self.open_source_target(target, false, cx),
        }
    }

    pub(crate) fn render_lsp_status(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let data = self.active_data()?;
        let backend = data.lsp_backend;
        // The chip always shows the active backend (so it reads as a control);
        // a status suffix reflects loading/error, and the color tracks it.
        let (suffix, color) = if let Some(err) = &data.lsp_error {
            (format!(" · error: {err}"), theme::red())
        } else if data.lsp_loading {
            let percentage = data
                .lsp_progress
                .as_ref()
                .and_then(|progress| progress.lock().ok().map(|progress| progress.clone()))
                .and_then(|progress| progress.percentage);
            (
                format!(" · {}", lsp_loading_label(percentage)),
                theme::blue(),
            )
        } else {
            (String::new(), theme::overlay0())
        };
        let text = SharedString::from(format!("LSP: {}{suffix}", backend.label()));
        let other = backend.toggled().label();
        Some(
            div()
                .id("lsp-backend-toggle")
                .flex_shrink_0()
                // Keep the chip off the window's right edge (it's the rightmost
                // titlebar element when there's no refresh note).
                .mr_2()
                .flex()
                .items_center()
                .gap_1()
                .pl_2()
                .pr_1()
                .rounded_sm()
                .border_1()
                .border_color(Hsla::from(color).opacity(0.45))
                .bg(Hsla::from(color).opacity(0.1))
                .text_size(px(11.))
                .text_color(color)
                .cursor_pointer()
                // Stronger border + fill on hover so it reads as a button.
                .hover(|style| {
                    style
                        .bg(Hsla::from(color).opacity(0.2))
                        .border_color(Hsla::from(color).opacity(0.8))
                })
                .tooltip(move |window, cx| {
                    Tooltip::new(format!("Switch LSP to {other}")).build(window, cx)
                })
                .on_click(cx.listener(|this, _, _, cx| this.toggle_lsp_backend(cx)))
                .child(text)
                // The caret signals it's a switchable control (like a dropdown);
                // the Icon inherits the chip's text color.
                .child(Icon::new(IconName::ChevronDown).xsmall())
                .into_any_element(),
        )
    }

    pub(crate) fn render_hover(&self) -> Option<gpui::AnyElement> {
        let data = self.active_data()?;
        let hover = data.hover.as_ref()?;
        let definition_hint = if cfg!(target_os = "macos") {
            "Cmd+click to go to definition"
        } else {
            "Ctrl+click to go to definition"
        };
        let text = if hover.loading {
            let percentage = data
                .lsp_progress
                .as_ref()
                .and_then(|progress| progress.lock().ok()?.percentage);
            SharedString::from(format!("LSP: {}", lsp_loading_label(percentage)))
        } else {
            SharedString::from(hover.result.as_ref()?.text.clone())
        };
        // Anchor just below-right of the cursor; `anchored().snap_to_window`
        // measures the card and slides it back from the window edges so it never
        // spills off-screen and clips its own content. `deferred` paints it above
        // everything and outside any ancestor's clip rect.
        Some(
            deferred(
                anchored()
                    .snap_to_window_with_margin(px(8.))
                    .position(hover.mouse)
                    .offset(point(px(14.), px(18.)))
                    .child(
                        div()
                            .max_w(px(520.))
                            .max_h(px(260.))
                            .overflow_hidden()
                            .rounded_sm()
                            .border_1()
                            .border_color(theme::surface0())
                            .bg(theme::mantle())
                            .shadow_lg()
                            .flex()
                            .flex_col()
                            .font_family(MONO)
                            .text_size(px(12.))
                            .line_height(px(18.))
                            .text_color(theme::text())
                            // The body clips within the card's max height; the
                            // hint stays pinned so it's never pushed off the
                            // bottom by a long rust-analyzer hover.
                            .child(div().p_3().min_h_0().overflow_hidden().child(text))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .border_t_1()
                                    .border_color(theme::surface0())
                                    .px_3()
                                    .py_1()
                                    .text_size(px(10.))
                                    .text_color(theme::overlay0())
                                    .child(definition_hint),
                            ),
                    ),
            )
            .into_any_element(),
        )
    }

    pub(crate) fn render_source_view(
        &self,
        source: &SourceViewState,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let lines = source.lines.clone();
        let syntax = source.syntax.clone();
        let target = source.target.clone();
        let can_back = self
            .active_data()
            .is_some_and(|data| !data.nav_back.is_empty());
        let can_forward = self
            .active_data()
            .is_some_and(|data| !data.nav_forward.is_empty());
        div()
            .size_full()
            .flex()
            .flex_col()
            .font_family(MONO)
            .text_size(px(text_size()))
            .line_height(px(row_height()))
            .child(
                div()
                    .h(px(30.))
                    .flex_shrink_0()
                    .px_3()
                    .flex()
                    .items_center()
                    .bg(theme::mantle())
                    .border_b_1()
                    .border_color(theme::surface0())
                    .text_color(theme::subtext())
                    .gap_1()
                    .child(
                        Button::new("source-nav-back")
                            .icon(IconName::ArrowLeft)
                            .ghost()
                            .xsmall()
                            .disabled(!can_back)
                            .tooltip_with_action("Back", &NavBack, Some("ReviewApp"))
                            .on_click(cx.listener(|this, _, _, cx| this.nav_back(cx))),
                    )
                    .child(
                        Button::new("source-nav-forward")
                            .icon(IconName::ArrowRight)
                            .ghost()
                            .xsmall()
                            .disabled(!can_forward)
                            .tooltip_with_action("Forward", &NavForward, Some("ReviewApp"))
                            .on_click(cx.listener(|this, _, _, cx| this.nav_forward(cx))),
                    )
                    .child(div().min_w_0().truncate().child(SharedString::from(format!(
                        "{}:{}",
                        target.path,
                        target.start_line + 1
                    )))),
            )
            .child(
                uniform_list("source", lines.len(), move |range, _window, _cx| {
                    range
                        .map(|ix| {
                            let mut row = div().h(px(row_height())).flex().items_center();
                            if ix as u32 == target.start_line {
                                row = row.bg(Hsla::from(theme::blue()).opacity(0.16));
                            }
                            row.child(
                                div()
                                    .w(px(64.))
                                    .flex_shrink_0()
                                    .pr_2()
                                    .text_color(theme::overlay0())
                                    .flex()
                                    .justify_end()
                                    .child(SharedString::from((ix + 1).to_string())),
                            )
                            .child(div().whitespace_nowrap().text_color(theme::text()).child({
                                // Long lines stay plain, like the diff.
                                let spans = if lines[ix].len() > MAX_SYNTAX_LINE_BYTES {
                                    &[][..]
                                } else {
                                    syntax.get(ix).map(Vec::as_slice).unwrap_or(&[])
                                };
                                line_content(&lines[ix], spans, &[], None, None)
                            }))
                            .into_any_element()
                        })
                        .collect()
                })
                .track_scroll(source.scroll.clone())
                .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                .flex_1()
                .min_h_0(),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static LSP_ENV_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

    #[test]
    fn identifier_hover_hit_snaps_to_the_token_start() {
        let text = "    if cfg.font.mono_family.is_empty() {";
        assert_eq!(identifier_start_at(text, 7), Some(7));
        assert_eq!(identifier_start_at(text, 9), Some(7));
        assert_eq!(identifier_start_at(text, 10), Some(7));
        assert_eq!(identifier_start_at(text, 11), Some(11));
        assert_eq!(identifier_start_at(text, 14), Some(11));
        assert_eq!(identifier_start_at(text, 15), Some(11));
        assert_eq!(identifier_start_at(text, 6), None);
    }

    #[test]
    #[cfg(unix)]
    fn pending_lsp_close_cancels_startup_and_waits_for_reap() {
        use crate::app::PendingCheckoutTasks;
        use crate::lsp_client::LspBackend;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let _guard = LSP_ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "lgtm-lsp-pending-close-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("lsp-started");
        let server = root.join("blocked-bifrost.sh");
        fs::write(
            &server,
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > \"$LGTM_LSP_STARTED\"\nwhile IFS= read -r line; do sleep 1; done\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&server).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&server, permissions).unwrap();
        let previous_server = std::env::var_os("LGTM_BIFROST");
        let previous_marker = std::env::var_os("LGTM_LSP_STARTED");
        std::env::set_var("LGTM_BIFROST", &server);
        std::env::set_var("LGTM_LSP_STARTED", &marker);

        let tasks = Arc::new(PendingCheckoutTasks::default());
        let pending = tasks.begin(1, 1).unwrap();
        let cancel = pending.cancel_token();
        let (started, started_result) = mpsc::channel();
        let startup_root = root.clone();
        let startup_thread = thread::spawn(move || {
            let result = LspSession::start_cancellable(
                startup_root,
                LspBackend::Bifrost,
                Arc::new(Mutex::new(LspProgress::default())),
                None,
                Some(cancel),
            );
            started.send(result).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "fake LSP did not start");

        let (closed, closed_result) = mpsc::channel();
        tasks.cancel_item(1);
        let close_tasks = Arc::clone(&tasks);
        let close_thread = thread::spawn(move || {
            close_tasks.wait_for_close();
            closed.send(()).unwrap();
        });
        assert!(
            closed_result
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "close gate ignored pending startup"
        );
        let startup_result = started_result
            .recv_timeout(Duration::from_secs(5))
            .expect("LSP startup cancellation hung");
        assert!(
            startup_result.is_err(),
            "blocked LSP startup unexpectedly succeeded"
        );
        drop(startup_result);
        drop(pending);
        closed_result
            .recv_timeout(Duration::from_secs(5))
            .expect("close gate did not finish after startup reap");
        close_thread.join().unwrap();
        startup_thread.join().unwrap();

        let pid = fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "LSP child still alive");
        if let Some(previous_server) = previous_server {
            std::env::set_var("LGTM_BIFROST", previous_server);
        } else {
            std::env::remove_var("LGTM_BIFROST");
        }
        if let Some(previous_marker) = previous_marker {
            std::env::set_var("LGTM_LSP_STARTED", previous_marker);
        } else {
            std::env::remove_var("LGTM_LSP_STARTED");
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[cfg(unix)]
    fn lsp_handle_keeps_checkout_until_pending_startup_is_stopped() {
        use crate::app::PendingCheckoutTasks;
        use crate::lsp_client::LspBackend;
        use crate::review_checkouts::ReviewCheckoutManager;
        use std::fs;
        use std::os::unix::fs::PermissionsExt;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let _guard = LSP_ENV_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap();
        let root = std::env::temp_dir().join(format!(
            "lgtm-lsp-handle-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let marker = root.join("lsp-started");
        let server = root.join("bifrost-lsp.sh");
        fs::write(
            &server,
            r###"#!/bin/sh
printf '%s\n' "$$" > "$LGTM_LSP_STARTED"
request=0
while IFS= read -r line; do
  line=$(printf '%s' "$line" | tr -d '\r')
  case "$line" in
    Content-Length:*) length=${line#Content-Length: } ;;
    '')
      body=$(dd bs=1 count="$length" 2>/dev/null)
      case "$request" in
        0)
          response='{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}'
          printf 'Content-Length: %s\r\n\r\n%s' "${#response}" "$response"
          progress='{"jsonrpc":"2.0","method":"$/progress","params":{"value":{"kind":"end"}}}'
          printf 'Content-Length: %s\r\n\r\n%s' "${#progress}" "$progress"
          ;;
        1)
          response='{"jsonrpc":"2.0","id":2,"result":[]}'
          printf 'Content-Length: %s\r\n\r\n%s' "${#response}" "$response"
          ;;
      esac
      request=$((request + 1))
      ;;
  esac
done
"###,
        )
        .unwrap();
        let mut permissions = fs::metadata(&server).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&server, permissions).unwrap();
        let previous_server = std::env::var_os("LGTM_BIFROST");
        let previous_marker = std::env::var_os("LGTM_LSP_STARTED");
        std::env::set_var("LGTM_BIFROST", &server);
        std::env::set_var("LGTM_LSP_STARTED", &marker);

        let manager = ReviewCheckoutManager::at_root(root.join("cache"), None);
        let lease = manager
            .acquire(
                &gh::PrLocator {
                    owner: "owner".to_string(),
                    repo: "repo".to_string(),
                    number: 1,
                },
                "0123456789abcdef0123456789abcdef01234567",
            )
            .unwrap();
        let tasks = Arc::new(PendingCheckoutTasks::default());
        let pending = tasks.begin(1, 1).unwrap();
        pending.register_lease(&lease);
        let operation = Arc::new(lease.operation("lsp startup").unwrap());
        let progress = Arc::new(Mutex::new(LspProgress::default()));
        let session = LspSession::start(root.clone(), LspBackend::Bifrost, progress.clone(), None)
            .expect("fake LSP should initialize");
        let old_pid = fs::read_to_string(&marker)
            .expect("fake LSP did not start")
            .trim()
            .parse::<libc::pid_t>()
            .unwrap();

        let handle = LspHandle {
            root: root.clone(),
            session,
            progress,
            _operation: Some(operation),
        };
        tasks.cancel_superseded(1, 2);
        let (done, result) = mpsc::channel();
        let teardown_lease = lease.clone();
        let teardown_manager = manager.clone();
        thread::spawn(move || {
            done.send(teardown_manager.teardown(&teardown_lease))
                .unwrap();
        });
        assert!(
            result.recv_timeout(Duration::from_millis(100)).is_err(),
            "checkout teardown raced pending LSP startup result"
        );
        let (close_done, close_result) = mpsc::channel();
        let close_tasks = Arc::clone(&tasks);
        thread::spawn(move || {
            close_tasks.wait_for_close();
            close_done.send(()).unwrap();
        });
        assert!(
            close_result
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "head supersession released the pending startup gate"
        );

        let shutdown_started = Instant::now();
        stop_lsp_blocking(handle.clone());
        assert!(shutdown_started.elapsed() < Duration::from_secs(2));
        assert!(
            result.recv_timeout(Duration::from_millis(100)).is_err(),
            "checkout teardown released before the startup guard"
        );
        handle.session.shutdown_blocking();
        assert_ne!(unsafe { libc::kill(old_pid, 0) }, 0, "old LSP still alive");
        let replacement_marker = root.join("lsp-replacement-started");
        std::env::set_var("LGTM_LSP_STARTED", &replacement_marker);
        let replacement = LspSession::start(
            root.clone(),
            LspBackend::Bifrost,
            Arc::new(Mutex::new(LspProgress::default())),
            None,
        )
        .expect("replacement LSP should initialize after old server reaped");
        assert!(replacement_marker.is_file());
        replacement.shutdown_blocking();
        if let Some(previous_server) = previous_server {
            std::env::set_var("LGTM_BIFROST", previous_server);
        } else {
            std::env::remove_var("LGTM_BIFROST");
        }
        if let Some(previous_marker) = previous_marker {
            std::env::set_var("LGTM_LSP_STARTED", previous_marker);
        } else {
            std::env::remove_var("LGTM_LSP_STARTED");
        }
        drop(handle);
        drop(pending);
        assert!(
            result.recv_timeout(Duration::from_secs(2)).is_ok(),
            "checkout teardown did not observe the stopped LSP"
        );
        close_result
            .recv_timeout(Duration::from_secs(2))
            .expect("pending startup gate did not close after reap");
        fs::remove_dir_all(root).unwrap();
    }
}
