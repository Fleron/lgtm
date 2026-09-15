use crate::comments::{CommentThread, LocalReview};
use crate::items::{dir_name, ItemState, Source};
use crate::selection::selection_info;
use crate::theme;
use crate::{centered_message, row_height, ReviewApp, TopView};
use gpui::{
    div, prelude::*, px, Context, Hsla, ScrollHandle, ScrollWheelEvent, SharedString,
    Subscription, Window,
};
use gpui_component::{
    button::Button,
    input::{Escape as InputEscape, Input, InputEvent, InputState},
    tooltip::Tooltip,
    Icon, IconName, Sizable as _,
};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// --- Chat with Claude -------------------------------------------------------

/// Width of the right-side chat panel.
const CHAT_WIDTH: f32 = 380.0;
/// The unified patch included in a session's first message is capped here.
pub(crate) const MAX_CHAT_PATCH_BYTES: usize = 200 * 1024;
/// Files above this size are skipped when materializing an exploration dir.
pub(crate) const MAX_EXPLORE_FILE_BYTES: usize = 1024 * 1024;
/// Reviewer persona appended to the system prompt on a session's first turn.
const CHAT_SYSTEM_PROMPT: &str =
    "You are reviewing this diff. Be concrete; cite file:line for claims about the code.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChatRole {
    User,
    Assistant,
}

/// Which backend answers chat turns for an item. Per-item (lives on
/// `ChatState`) and resets to `Claude` whenever the item is (re)opened; no
/// persistence, modeled on `LspBackend`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum ChatBackend {
    #[default]
    Claude,
    Codex,
}

impl ChatBackend {
    /// Short name for the status chip.
    pub(crate) fn label(self) -> &'static str {
        match self {
            ChatBackend::Claude => "Claude",
            ChatBackend::Codex => "Codex",
        }
    }

    /// The other backend, for the click-to-switch toggle.
    pub(crate) fn toggled(self) -> Self {
        match self {
            ChatBackend::Claude => ChatBackend::Codex,
            ChatBackend::Codex => ChatBackend::Claude,
        }
    }
}

pub(crate) struct ChatMessage {
    pub(crate) role: ChatRole,
    pub(crate) text: String,
    /// Total cost of the run that produced this assistant message.
    pub(crate) cost: Option<f64>,
    /// "› included selection: path:lines" marker under a user message.
    pub(crate) note: Option<String>,
    /// The run behind this assistant message failed (rendered in red).
    pub(crate) error: bool,
}

/// Per-item chat state; lives on ItemData so transcripts follow the item and
/// die with it. The InputState is created lazily (it needs a Window) the
/// first time the panel renders for this item.
pub(crate) struct ChatState {
    pub(crate) messages: Vec<ChatMessage>,
    pub(crate) session_id: Option<String>,
    pub(crate) in_flight: bool,
    /// Set to stop the current run; claude::chat kills the child on it.
    /// Replaced (not reset) per send so a stale run can't clear a new one.
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) input: Option<gpui::Entity<InputState>>,
    pub(crate) _input_sub: Option<Subscription>,
    pub(crate) scroll: ScrollHandle,
    /// Auto-scroll to the bottom on new content, editor-style: sticky until
    /// the user scrolls up, re-sticks when they scroll back to the bottom.
    pub(crate) stick_to_bottom: bool,
    /// Exploration dir passed to claude, fixed at session start: the repo
    /// root for local items, a materialized scratch dir for PR items with
    /// blob-upgraded contents, None otherwise.
    pub(crate) explore_dir: Option<std::path::PathBuf>,
    /// Which backend answers this item's chat turns; the chat panel's status
    /// chip toggles it and starts a new conversation.
    pub(crate) backend: ChatBackend,
}

impl ChatState {
    pub(crate) fn new() -> Self {
        Self {
            messages: Vec::new(),
            session_id: None,
            in_flight: false,
            cancel: Arc::new(AtomicBool::new(false)),
            input: None,
            _input_sub: None,
            scroll: ScrollHandle::new(),
            stick_to_bottom: true,
            explore_dir: None,
            backend: ChatBackend::default(),
        }
    }
}

/// `text` truncated to at most `cap` bytes on a char boundary, plus whether
/// anything was cut.
pub(crate) fn truncate_str(text: &str, cap: usize) -> (&str, bool) {
    if text.len() <= cap {
        return (text, false);
    }
    let mut cut = cap;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    (&text[..cut], true)
}

/// First-message context header for a PR item.
pub(crate) fn pr_chat_header(meta: &gh::PrMeta) -> String {
    let body = if meta.body.trim().is_empty() {
        "(no description)"
    } else {
        meta.body.trim()
    };
    format!(
        "PR under review: \"{}\" — {}\nAuthor: {} · state: {} · {} ← {}\n\nPR description:\n{}",
        meta.title,
        meta.url,
        meta.author.login,
        meta.state,
        meta.base_ref_name,
        meta.head_ref_name,
        body,
    )
}

/// First-message context header for a local item.
pub(crate) fn local_chat_header(src: &git::LocalSource) -> String {
    format!(
        "Local diff under review: repo {}, branch {} against {}.",
        dir_name(&src.repo_root),
        src.branch,
        src.base_label,
    )
}

/// The full prompt for one turn. `context` (header + raw patch) is only
/// present on a session's first message; the patch is capped at
/// [`MAX_CHAT_PATCH_BYTES`] with the truncation noted in the prompt.
pub(crate) fn chat_prompt(
    context: Option<(&str, &str)>,
    selection_block: Option<&str>,
    question: &str,
) -> String {
    let mut out = String::new();
    if let Some((header, patch)) = context {
        out.push_str(header);
        out.push_str("\n\nThe unified diff under review:\n```diff\n");
        let (patch, truncated) = truncate_str(patch, MAX_CHAT_PATCH_BYTES);
        out.push_str(patch);
        if !patch.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("```\n");
        if truncated {
            out.push_str("(patch truncated at 200KB — ask about specific files if needed)\n");
        }
        out.push('\n');
    }
    if let Some(block) = selection_block {
        out.push_str(block);
        out.push('\n');
    }
    out.push_str(question);
    out
}

pub(crate) fn local_review_prompt(src: &git::LocalSource, review: &LocalReview) -> String {
    let threads = local_review_threads(review)
        .iter()
        .map(format_local_thread_prompt)
        .collect::<Vec<_>>()
        .join("\n=====\n");
    format!(
        "Diff: {} ← {}. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\n{}",
        src.base_label, src.branch, threads
    )
}

fn local_review_threads(review: &LocalReview) -> Vec<CommentThread> {
    let mut comments = review.comments.clone();
    comments.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    let mut threads = Vec::new();
    let mut thread_of: HashMap<u64, usize> = HashMap::new();
    for comment in comments {
        match comment.in_reply_to_id {
            None => {
                thread_of.insert(comment.id, threads.len());
                threads.push(CommentThread {
                    root: comment,
                    replies: Vec::new(),
                });
            }
            Some(parent) => {
                if let Some(&ix) = thread_of.get(&parent) {
                    thread_of.insert(comment.id, ix);
                    threads[ix].replies.push(comment);
                }
            }
        }
    }
    threads
}

fn format_local_thread_prompt(thread: &CommentThread) -> String {
    let file = if thread.root.path.is_empty() {
        "<unknown file>"
    } else {
        &thread.root.path
    };
    let mut sections = vec![
        local_comment_location(file, thread.root.side.as_deref(), thread.root.line),
        thread.root.body.clone(),
    ];
    for (ix, reply) in thread.replies.iter().enumerate() {
        let author = if reply.user.login.trim().is_empty() {
            "Unknown"
        } else {
            reply.user.login.as_str()
        };
        sections.push(format!("Reply {} ({author})", ix + 1));
        sections.push(reply.body.clone());
    }
    sections.join("\n")
}

fn local_comment_location(file: &str, side: Option<&str>, line: Option<u64>) -> String {
    let line = line.unwrap_or(0);
    match side {
        Some("LEFT") => format!("{file}:L{line}"),
        _ => format!("{file}:R{line}"),
    }
}

/// Scratch dir for one item's materialized exploration files. Includes the
/// pid: item ids restart at 0 every run, and stale dirs from another process
/// must never be reused.
pub(crate) fn chat_scratch_root(item_id: u64) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("lgtm-chat-{}-{item_id}", std::process::id()))
}

/// A repo-relative path mapped under `root`, preserving the layout. Rejects
/// absolute paths and any non-normal component (`..`, `.`) so materialized
/// files can't escape the scratch dir.
pub(crate) fn scratch_path(root: &Path, rel: &str) -> Option<std::path::PathBuf> {
    let rel = Path::new(rel);
    if rel.as_os_str().is_empty()
        || !rel
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return None;
    }
    Some(root.join(rel))
}

/// Write blob-upgraded new-side files into `root`, best-effort: oversized
/// files, unsafe paths, and individual write failures are skipped. Returns
/// the root when it could be created at all.
pub(crate) fn materialize_files(root: &Path, files: &[(String, String)]) -> Option<std::path::PathBuf> {
    std::fs::create_dir_all(root).ok()?;
    for (rel, content) in files {
        if content.len() > MAX_EXPLORE_FILE_BYTES {
            continue;
        }
        let Some(path) = scratch_path(root, rel) else {
            continue;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&path, content);
    }
    Some(root.to_path_buf())
}

/// How the next chat run gets its exploration dir, decided at send time.
pub(crate) enum ExplorePlan {
    /// No tools: patch-only context.
    None,
    /// An existing directory (local repo root, or an already-materialized
    /// scratch dir from an earlier turn).
    Dir(std::path::PathBuf),
    /// PR item, first turn with blob upgrades: write these (path, content)
    /// pairs under `root` on the background executor, then use it.
    Materialize {
        root: std::path::PathBuf,
        files: Vec<(String, String)>,
    },
}

    /// The chat state of the item with `item_id`, if it is still open and
    /// loaded. Chat tasks address items by id so streams land on the right
    /// transcript regardless of switching/closing.
impl ReviewApp {
    pub(crate) fn chat_mut(&mut self, item_id: u64) -> Option<&mut ChatState> {
        match &mut self.items.iter_mut().find(|item| item.id == item_id)?.state {
            ItemState::Ready(data) => Some(&mut data.chat),
            _ => None,
        }
    }

    /// `cmd-j`: toggle the chat panel; opening focuses the active item's
    /// chat input.
    pub(crate) fn toggle_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.top_view != TopView::Review {
            return;
        }
        self.chat_visible = !self.chat_visible;
        if self.chat_visible {
            // The chat input can't take focus under the palette (same as the
            // cmd-t open input).
            self.palette = None;
            self.palette_gen += 1;
            self.ensure_chat_input(window, cx);
            if let Some(input) = self.active_data().and_then(|data| data.chat.input.clone()) {
                input.update(cx, |state, cx| state.focus(window, cx));
            }
        } else {
            window.focus(&self.focus_handle);
        }
        cx.notify();
    }

    /// Create the active item's chat InputState on first use (it needs a
    /// Window, which ItemData construction doesn't have).
    pub(crate) fn ensure_chat_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.active_data() {
            Some(data) if data.chat.input.is_none() => {}
            _ => return,
        }
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(2, 6)
                .placeholder("ask about this diff…")
        });
        // cmd-enter sends — same secondary-enter pattern as the comment
        // composer (the newline it inserts first is trimmed on send).
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: true }) {
                this.send_chat(window, cx);
            }
        });
        if let Some(data) = self.active_data_mut() {
            data.chat.input = Some(input);
            data.chat._input_sub = Some(sub);
        }
    }

    /// Stop the active item's streaming run, if any. Returns whether there
    /// was one (escape falls through to other meanings when there wasn't).
    pub(crate) fn cancel_chat(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(data) = self.active_data_mut() else {
            return false;
        };
        if !data.chat.in_flight {
            return false;
        }
        // claude::chat kills the child; the pump's finish path then clears
        // in_flight and marks the message stopped.
        data.chat.cancel.store(true, Ordering::Relaxed);
        cx.notify();
        true
    }

    /// Switch the active item's chat backend (Claude ↔ Codex), stopping any
    /// in-flight run first and starting a fresh conversation. Driven by
    /// clicking the backend status chip in the chat panel.
    pub(crate) fn toggle_chat_backend(&mut self, cx: &mut Context<Self>) {
        self.cancel_chat(cx);
        let Some(data) = self.active_data_mut() else {
            return;
        };
        let chat = &mut data.chat;
        chat.backend = chat.backend.toggled();
        chat.messages.clear();
        chat.session_id = None;
        // The stopped run's Completed/Failed event can still land after
        // this: in_flight = false makes apply_chat_events/finish_chat
        // early-return instead of stamping the old run's session id (or a
        // stray "— stopped") onto the fresh transcript.
        chat.in_flight = false;
        cx.notify();
    }

    /// Send the chat input's text: append the user message (with a selection
    /// marker when one is included), stream the reply on the background
    /// executor, and batch deltas back at ~50ms into the transcript.
    pub(crate) fn send_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.items.get_mut(self.active) else {
            return;
        };
        let item_id = item.id;
        let source = item.source.clone();
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        if data.chat.in_flight {
            return;
        }
        let Some(input) = data.chat.input.clone() else {
            return;
        };
        let text = input.read(cx).value().trim().to_string();
        if text.is_empty() {
            return;
        }

        let first = data.chat.session_id.is_none();
        let sel_info = data
            .selection
            .and_then(|sel| selection_info(&sel, &data.rows, &data.file_rows, &data.diff));
        let sel_block = sel_info.as_ref().map(|info| info.block());
        // The transcript shows only the typed text; the context header,
        // patch, and selection ride along invisibly in the real prompt.
        let header = first.then(|| match (&source, &data.pr_meta) {
            (Source::Pr(_), Some(meta)) => pr_chat_header(meta),
            (Source::Pr(loc), None) => {
                format!("PR under review: {}#{}", loc.repo_slug(), loc.number)
            }
            (Source::Local(src), _) => local_chat_header(src),
        });
        let prompt = chat_prompt(
            header.as_deref().map(|h| (h, data.patch.as_str())),
            sel_block.as_deref(),
            &text,
        );

        let explore = match &data.chat.explore_dir {
            Some(dir) => ExplorePlan::Dir(dir.clone()),
            None => match &source {
                Source::Local(src) => ExplorePlan::Dir(src.repo_root.clone()),
                // PR with blob-upgraded contents: materialize the new-side
                // files once, at session start.
                Source::Pr(_) if first && !data.upgrades.is_empty() => {
                    let files = data
                        .upgrades
                        .iter()
                        .filter_map(|(&ix, upgrade)| {
                            let path = data.diff.files.get(ix)?.new_path.clone()?;
                            if upgrade.new_lines.is_empty() {
                                return None;
                            }
                            let mut content = upgrade
                                .new_lines
                                .iter()
                                .map(|line| line.as_ref())
                                .collect::<Vec<&str>>()
                                .join("\n");
                            content.push('\n');
                            Some((path, content))
                        })
                        .collect();
                    ExplorePlan::Materialize {
                        root: chat_scratch_root(item_id),
                        files,
                    }
                }
                Source::Pr(_) => ExplorePlan::None,
            },
        };

        let session = data.chat.session_id.clone();
        let system_prompt = first.then(|| CHAT_SYSTEM_PROMPT.to_string());
        let backend = data.chat.backend;
        let cancel = Arc::new(AtomicBool::new(false));
        data.chat.cancel = cancel.clone();
        data.chat.in_flight = true;
        data.chat.stick_to_bottom = true;
        data.chat.messages.push(ChatMessage {
            role: ChatRole::User,
            text,
            cost: None,
            note: sel_info.as_ref().map(|info| info.note()),
            error: false,
        });
        data.chat.messages.push(ChatMessage {
            role: ChatRole::Assistant,
            text: String::new(),
            cost: None,
            note: None,
            error: false,
        });
        data.chat.scroll.scroll_to_bottom();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        cx.notify();

        cx.spawn(async move |this, cx| {
            let explore_dir = match explore {
                ExplorePlan::None => None,
                ExplorePlan::Dir(dir) => Some(dir),
                ExplorePlan::Materialize { root, files } => {
                    cx.background_spawn(async move { materialize_files(&root, &files) })
                        .await
                }
            };
            if let Some(dir) = explore_dir.clone() {
                this.update(cx, |app, _| {
                    if let Some(chat) = app.chat_mut(item_id) {
                        chat.explore_dir = Some(dir);
                    }
                })
                .ok();
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let cancel_bg = cancel.clone();
            let task = cx.background_spawn(async move {
                match backend {
                    ChatBackend::Claude => {
                        let opts = claude::ChatOptions {
                            session,
                            system_prompt,
                            explore_dir,
                        };
                        claude::chat(&prompt, &opts, &cancel_bg, |event| {
                            let _ = tx.send(event);
                        })
                    }
                    ChatBackend::Codex => {
                        let opts = codex::ChatOptions {
                            session,
                            system_prompt,
                            explore_dir,
                        };
                        codex::chat(&prompt, &opts, &cancel_bg, |event| {
                            let _ = tx.send(event);
                        })
                    }
                }
            });
            // Throttled pump: every ~50ms drain whatever streamed in and
            // apply it as one entity update — never one update per token.
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(50))
                    .await;
                let mut delta = String::new();
                let mut terminals = Vec::new();
                let mut disconnected = false;
                loop {
                    match rx.try_recv() {
                        Ok(claude::ChatEvent::TextDelta(text)) => delta.push_str(&text),
                        Ok(event) => terminals.push(event),
                        Err(std::sync::mpsc::TryRecvError::Empty) => break,
                        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                            disconnected = true;
                            break;
                        }
                    }
                }
                if !delta.is_empty() || !terminals.is_empty() {
                    let alive = this
                        .update(cx, |app, cx| {
                            app.apply_chat_events(item_id, &delta, terminals, cx)
                        })
                        .is_ok();
                    if !alive {
                        // App gone: make sure the subprocess dies too.
                        cancel.store(true, Ordering::Relaxed);
                        break;
                    }
                }
                if disconnected {
                    break;
                }
            }
            let result = task.await;
            this.update(cx, |app, cx| {
                app.finish_chat(item_id, result.err().map(|err| format!("{err:#}")), cx);
            })
            .ok();
        })
        .detach();
    }

    /// Fold one pump batch into the transcript: text deltas append to the
    /// trailing assistant message, Completed installs the authoritative text
    /// + cost + session id, Failed marks the message as an error.
    pub(crate) fn apply_chat_events(
        &mut self,
        item_id: u64,
        delta: &str,
        terminals: Vec<claude::ChatEvent>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat_mut(item_id) else {
            return;
        };
        if !chat.in_flight {
            return;
        }
        if !delta.is_empty() {
            if let Some(msg) = chat
                .messages
                .last_mut()
                .filter(|msg| msg.role == ChatRole::Assistant)
            {
                msg.text.push_str(delta);
            }
        }
        for event in terminals {
            let msg = chat
                .messages
                .last_mut()
                .filter(|msg| msg.role == ChatRole::Assistant);
            match event {
                claude::ChatEvent::Completed {
                    session_id,
                    cost_usd,
                    is_error,
                    text,
                } => {
                    if !session_id.is_empty() {
                        chat.session_id = Some(session_id);
                    }
                    chat.in_flight = false;
                    if let Some(msg) = msg {
                        if !text.is_empty() {
                            msg.text = text;
                        }
                        msg.cost = cost_usd;
                        msg.error = is_error;
                    }
                }
                claude::ChatEvent::Failed(reason) => {
                    chat.in_flight = false;
                    if let Some(msg) = msg {
                        if !msg.text.is_empty() {
                            msg.text.push_str("\n\n");
                        }
                        msg.text.push_str("chat failed: ");
                        msg.text.push_str(&reason);
                        msg.error = true;
                    }
                }
                claude::ChatEvent::TextDelta(_) => {}
            }
        }
        if chat.stick_to_bottom {
            chat.scroll.scroll_to_bottom();
        }
        cx.notify();
    }

    /// Close out a run that ended without a terminal event: user-cancelled,
    /// or `claude` couldn't be spawned at all.
    pub(crate) fn finish_chat(&mut self, item_id: u64, spawn_error: Option<String>, cx: &mut Context<Self>) {
        let Some(chat) = self.chat_mut(item_id) else {
            return;
        };
        if !chat.in_flight {
            return;
        }
        chat.in_flight = false;
        if let Some(msg) = chat
            .messages
            .last_mut()
            .filter(|msg| msg.role == ChatRole::Assistant)
        {
            match spawn_error {
                Some(err) => {
                    msg.text = err;
                    msg.error = true;
                }
                None => {
                    if !msg.text.is_empty() {
                        msg.text.push(' ');
                    }
                    msg.text.push_str("— stopped");
                }
            }
        }
        cx.notify();
    }

    /// Status-chip-style backend toggle for the chat panel header, matching
    /// `render_lsp_status`'s visual treatment. Clicking it stops any
    /// in-flight run, clears the transcript, and switches backend.
    pub(crate) fn render_chat_backend(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let backend = self
            .active_data()
            .map(|data| data.chat.backend)
            .unwrap_or_default();
        let color = theme::overlay0();
        let text = SharedString::from(backend.label());
        let other = backend.toggled().label();
        div()
            .id("chat-backend-toggle")
            .flex_shrink_0()
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
            .hover(|style| {
                style
                    .bg(Hsla::from(color).opacity(0.2))
                    .border_color(Hsla::from(color).opacity(0.8))
            })
            .tooltip(move |window, cx| {
                Tooltip::new(format!("Switch chat to {other}")).build(window, cx)
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_chat_backend(cx)))
            .child(text)
            .child(Icon::new(IconName::ChevronDown).xsmall())
            .into_any_element()
    }

    /// The right-side chat panel: header (+ Stop while streaming), the
    /// scrollable transcript, and the multi-line input. Streaming behavior
    /// (auto-scroll, live deltas, cancel) is verified manually.
    pub(crate) fn render_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) -> gpui::AnyElement {
        self.ensure_chat_input(window, cx);
        let panel = div()
            .w(px(CHAT_WIDTH))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_l_1()
            .border_color(theme::surface0())
            .text_size(px(13.))
            // The chat input propagates Escape when it has nothing of its
            // own to dismiss: stop a streaming run, else return to the diff.
            .on_action(cx.listener(|this, _: &InputEscape, window, cx| {
                if !this.cancel_chat(cx) {
                    window.focus(&this.focus_handle);
                }
            }));
        let Some(data) = self.active_data() else {
            return panel
                .child(centered_message(
                    "open an item to chat about it".into(),
                    theme::overlay0(),
                ))
                .into_any_element();
        };
        let chat = &data.chat;

        let mut header = div()
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
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .child(SharedString::from("chat")),
            )
            .child(self.render_chat_backend(cx))
            .child(div().flex_1());
        if chat.in_flight {
            header = header.child(Button::new("chat-stop").label("Stop").small().on_click(
                cx.listener(|this, _, _, cx| {
                    this.cancel_chat(cx);
                }),
            ));
        }

        let mut column = div().w_full().flex().flex_col().gap_3().p_3();
        if chat.messages.is_empty() {
            column = column.child(
                div()
                    .text_color(theme::overlay0())
                    .child(SharedString::from(format!(
                        "Ask {} about this diff. Select text in the diff to include it. ⌘⏎ sends.",
                        chat.backend.label(),
                    ))),
            );
        }
        let last = chat.messages.len().saturating_sub(1);
        for (ix, msg) in chat.messages.iter().enumerate() {
            match msg.role {
                ChatRole::User => {
                    let mut wrap = div().w_full().flex().flex_col().items_end().gap_1().child(
                        div()
                            .max_w(px(CHAT_WIDTH - 64.))
                            .bg(theme::surface0())
                            .rounded_md()
                            .px_2()
                            .py_1()
                            .text_color(theme::text())
                            .child(SharedString::from(msg.text.clone())),
                    );
                    if let Some(note) = &msg.note {
                        wrap = wrap.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme::overlay0())
                                .truncate()
                                .child(SharedString::from(note.clone())),
                        );
                    }
                    column = column.child(wrap);
                }
                ChatRole::Assistant => {
                    let mut text = msg.text.clone();
                    if chat.in_flight && ix == last {
                        text.push_str(" ▌");
                    }
                    let mut wrap = div().w_full().min_w_0().flex().flex_col().gap_1().child(
                        div()
                            .text_color(if msg.error {
                                theme::red()
                            } else {
                                theme::text()
                            })
                            .child(SharedString::from(text)),
                    );
                    if let Some(cost) = msg.cost {
                        wrap = wrap.child(
                            div()
                                .text_size(px(10.))
                                .text_color(theme::overlay0())
                                .child(SharedString::from(format!("${cost:.4}"))),
                        );
                    }
                    column = column.child(wrap);
                }
            }
        }
        let messages = div()
            .id("chat-messages")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&chat.scroll)
            // Stick-to-bottom the way editors do it: scrolling up unsticks,
            // scrolling back to (near) the bottom re-sticks.
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, _| {
                let Some(data) = this.active_data_mut() else {
                    return;
                };
                let chat = &mut data.chat;
                let dy = f32::from(event.delta.pixel_delta(px(row_height())).y);
                if dy > 0. {
                    chat.stick_to_bottom = false;
                } else {
                    let scrolled = -f32::from(chat.scroll.offset().y);
                    let max = f32::from(chat.scroll.max_offset().height);
                    chat.stick_to_bottom = scrolled >= max - 8.;
                }
            }))
            .child(column);

        let sel_hint = data
            .selection
            .and_then(|sel| selection_info(&sel, &data.rows, &data.file_rows, &data.diff))
            .map(|info| {
                format!(
                    "will include selection {}:{}-{}",
                    info.path, info.lo, info.hi
                )
            });
        let mut input_area = div()
            .p_2()
            .flex_shrink_0()
            .border_t_1()
            .border_color(theme::surface0())
            .flex()
            .flex_col()
            .gap_1();
        if let Some(hint) = sel_hint {
            input_area = input_area.child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::blue())
                    .truncate()
                    .child(SharedString::from(hint)),
            );
        }
        if let Some(input) = &chat.input {
            input_area = input_area.child(Input::new(input));
        }
        input_area = input_area.child(
            div()
                .text_size(px(10.))
                .text_color(theme::overlay0())
                .child(SharedString::from(if chat.in_flight {
                    "streaming… esc to stop"
                } else {
                    "⌘⏎ to send"
                })),
        );

        panel
            .child(header)
            .child(messages)
            .child(input_area)
            .into_any_element()
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::CommentSide;

    #[test]
    fn chat_prompt_first_message_carries_header_patch_and_selection() {
        let prompt = chat_prompt(
            Some(("HEADER", "diff --git a/x b/x\n+new line\n")),
            Some("Selected text (x:1-1, RIGHT (new) side):\n```\nnew line\n```\n"),
            "why?",
        );
        assert!(prompt.starts_with("HEADER\n\nThe unified diff under review:\n```diff\n"));
        assert!(prompt.contains("+new line\n```\n"));
        assert!(!prompt.contains("truncated"));
        assert!(prompt.contains("Selected text (x:1-1"));
        assert!(prompt.ends_with("why?"));
        // Later turns: no context block, just selection (if any) + question.
        let followup = chat_prompt(None, None, "and this?");
        assert_eq!(followup, "and this?");
    }

    #[test]
    fn chat_prompt_truncates_patch_at_cap_on_char_boundary() {
        // 'a' then 2-byte 'é's: char boundaries sit at odd offsets, so the
        // even cap falls mid-char and must be shaved back to a boundary.
        let patch = format!("a{}", "é".repeat(MAX_CHAT_PATCH_BYTES));
        let prompt = chat_prompt(Some(("H", &patch)), None, "q");
        assert!(prompt.contains("(patch truncated at 200KB"));
        let (cut, truncated) = truncate_str(&patch, MAX_CHAT_PATCH_BYTES);
        assert!(truncated);
        assert_eq!(cut.len(), MAX_CHAT_PATCH_BYTES - 1); // boundary shaved one byte
        assert!(cut.is_char_boundary(cut.len()));
        let (all, truncated) = truncate_str("abc", 10);
        assert_eq!((all, truncated), ("abc", false));
    }

    #[test]
    fn chat_headers_describe_the_item() {
        let meta = gh::PrMeta {
            number: 7,
            title: "Fix the frobnicator".into(),
            author: gh::Author {
                login: "alice".into(),
            },
            state: "OPEN".into(),
            is_draft: false,
            url: "https://github.com/o/r/pull/7".into(),
            body: "It was broken.\n".into(),
            base_ref_name: "main".into(),
            head_ref_name: "fix".into(),
            base_ref_oid: String::new(),
            head_ref_oid: String::new(),
            additions: 1,
            deletions: 2,
            changed_files: 3,
            review_decision: String::new(),
            status_check_rollup: Vec::new(),
        };
        let header = pr_chat_header(&meta);
        assert!(header.contains("\"Fix the frobnicator\""));
        assert!(header.contains("https://github.com/o/r/pull/7"));
        assert!(header.contains("alice"));
        assert!(header.contains("OPEN"));
        assert!(header.contains("main ← fix"));
        assert!(header.contains("It was broken."));
        // Empty body → explicit placeholder, so the model doesn't guess.
        let meta = gh::PrMeta {
            body: "  \n".into(),
            ..meta
        };
        assert!(pr_chat_header(&meta).contains("(no description)"));

        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "origin/main".into(),
            base_oid: None,
        };
        let header = local_chat_header(&src);
        assert!(header.contains("myrepo"));
        assert!(header.contains("feature"));
        assert!(header.contains("origin/main"));
    }

    #[test]
    fn local_review_prompt_matches_difit_comment_format() {
        let src = git::LocalSource {
            repo_root: "/tmp/myrepo".into(),
            branch: "feature".into(),
            base_ref: None,
            base_label: "upstream/main".into(),
            base_oid: None,
        };
        let mut review = LocalReview::default();
        review.add_comment(
            None,
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "why remove this?\nplease explain".to_string(),
        );
        review.add_comment(
            Some(1),
            "src/lib.rs".to_string(),
            CommentSide::Right,
            7,
            None,
            "because this path handles nil".to_string(),
        );
        review.add_comment(
            None,
            "src/main.rs".to_string(),
            CommentSide::Left,
            12,
            None,
            "second thread".to_string(),
        );

        assert_eq!(
            local_review_prompt(&src, &review),
            "Diff: upstream/main ← feature. Locations are GitHub diff-style: path:Rline for right/new, path:Lline for left/old.\n\nsrc/lib.rs:R7\nwhy remove this?\nplease explain\nReply 1 (you)\nbecause this path handles nil\n=====\nsrc/main.rs:L12\nsecond thread"
        );
    }

    #[test]
    fn scratch_paths_stay_inside_the_root() {
        let root = Path::new("/tmp/lgtm-chat-1-2");
        assert_eq!(
            scratch_path(root, "src/main.rs"),
            Some(root.join("src/main.rs"))
        );
        assert_eq!(
            scratch_path(root, "deep/a/b/c.txt"),
            Some(root.join("deep/a/b/c.txt"))
        );
        // Escapes and non-normal components are refused.
        assert_eq!(scratch_path(root, "../evil"), None);
        assert_eq!(scratch_path(root, "a/../../evil"), None);
        assert_eq!(scratch_path(root, "/abs/path"), None);
        assert_eq!(scratch_path(root, "./x"), None);
        assert_eq!(scratch_path(root, ""), None);
    }

    #[test]
    fn materialize_writes_files_and_skips_oversized_and_unsafe() {
        let root = std::env::temp_dir().join(format!(
            "lgtm-chat-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let files = vec![
            ("src/lib.rs".to_string(), "fn a() {}\n".to_string()),
            ("../escape.rs".to_string(), "nope\n".to_string()),
            ("big.rs".to_string(), "x".repeat(MAX_EXPLORE_FILE_BYTES + 1)),
        ];
        let dir = materialize_files(&root, &files).unwrap();
        assert_eq!(dir, root);
        assert_eq!(
            std::fs::read_to_string(root.join("src/lib.rs")).unwrap(),
            "fn a() {}\n"
        );
        assert!(!root.join("big.rs").exists());
        assert!(!root.parent().unwrap().join("escape.rs").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
