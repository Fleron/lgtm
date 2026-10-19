use crate::app::PendingCheckoutTask;
use crate::chat::{backend_chip, local_chat_header, pr_chat_header, ChatBackend, CHAT_WIDTH};
use crate::items::{ItemState, Source};
use crate::review_checkouts::CheckoutOperation;
use crate::theme;
use crate::{centered_message, ReviewApp, TerminalCopy, TerminalPaste, TopView};
use gpui::{
    div, prelude::*, px, ClipboardItem, Context, Entity, Rgba, SharedString, WeakEntity, Window,
};
use gpui_component::{button::Button, Sizable as _};
use gpui_terminal::{ColorPalette, ColorPaletteBuilder, TerminalConfig, TerminalView};
use portable_pty::{native_pty_system, Child, CommandBuilder, PtyPair, PtySize};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

const TERMINAL_FONT: &str = "Iosevka Term";
const TERMINAL_FONT_PX: f32 = 11.0;

// --- Interactive claude/codex terminal ---------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum TerminalStatus {
    #[default]
    Idle,
    /// The item's LSP is still materializing the PR checkout; launch resumes
    /// from the LSP result handler.
    WaitingForCheckout,
    /// Materializing the PR checkout ourselves (the LSP errored or never ran).
    Preparing,
    Running,
    Exited(Option<i32>),
    Failed(String),
}

/// Per-item terminal; lives on `ItemData` so the session follows the item and
/// dies with it.
#[derive(Default)]
pub(crate) struct TerminalState {
    pub(crate) backend: ChatBackend,
    pub(crate) status: TerminalStatus,
    pub(crate) session: Option<TerminalSession>,
    pub(crate) generation: u64,
    /// A delayed launch finished while its panel showed; the next render
    /// focuses the terminal, since the launch itself had no Window.
    focus_pending: bool,
}

type SharedWriter = Arc<Mutex<Box<dyn std::io::Write + Send>>>;

fn write_pty(writer: &SharedWriter, bytes: &[u8]) {
    let mut writer = writer.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = writer.write_all(bytes);
    let _ = writer.flush();
}

/// The PTY writer handed to `TerminalView`, sharing the session's handle so
/// paste and option-key input write to the same stream.
struct PtyWriter(SharedWriter);

impl std::io::Write for PtyWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .flush()
    }
}

pub(crate) struct TerminalSession {
    view: Entity<TerminalView>,
    writer: SharedWriter,
    child: Option<Box<dyn Child + Send + Sync>>,
    _operation: Option<CheckoutOperation>,
}

fn stop_child(mut child: Box<dyn Child + Send + Sync>, operation: Option<CheckoutOperation>) {
    if let Some(pid) = child.process_id() {
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    }
    let _ = child.wait();
    drop(operation);
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let Some(child) = self.child.take() else {
            return;
        };
        stop_child(child, self._operation.take());
    }
}

impl TerminalSession {
    pub(crate) fn stop_blocking(mut self) {
        if let Some(child) = self.child.take() {
            stop_child(child, self._operation.take());
        }
    }
}

pub(crate) fn stop_terminal(
    session: TerminalSession,
    pending_task: PendingCheckoutTask,
    cx: &mut Context<ReviewApp>,
) {
    cx.background_spawn(async move {
        session.stop_blocking();
        drop(pending_task);
    })
    .detach();
}

/// `text` as a TOML basic string. codex parses `-c` values as TOML and falls
/// back to the raw text only when that fails, which mangles values that
/// happen to end in a quote.
fn toml_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// The agent's argv. `context` rides as its own element so `$0` receives it
/// verbatim, never parsed by the shell (same as dispatch's `open_args`).
pub(crate) fn terminal_argv(backend: ChatBackend, context: &str) -> Vec<String> {
    let (flag, arg) = match backend {
        ChatBackend::Claude => ("--append-system-prompt \"$0\"", context.to_string()),
        ChatBackend::Codex => ("-c \"developer_instructions=$0\"", toml_string(context)),
    };
    vec![
        "/bin/zsh".to_string(),
        "-lic".to_string(),
        format!("exec {} {flag}", backend.bin()),
        arg,
    ]
}

fn palette() -> ColorPalette {
    type Setter = fn(ColorPaletteBuilder, u8, u8, u8) -> ColorPaletteBuilder;
    let slots: [(Setter, Rgba); 19] = [
        (ColorPaletteBuilder::background, theme::mantle()),
        (ColorPaletteBuilder::foreground, theme::text()),
        (ColorPaletteBuilder::cursor, theme::text()),
        (ColorPaletteBuilder::black, theme::surface1()),
        (ColorPaletteBuilder::red, theme::red()),
        (ColorPaletteBuilder::green, theme::green()),
        (ColorPaletteBuilder::yellow, theme::yellow()),
        (ColorPaletteBuilder::blue, theme::blue()),
        (ColorPaletteBuilder::magenta, theme::pink()),
        (ColorPaletteBuilder::cyan, theme::teal()),
        (ColorPaletteBuilder::white, theme::subtext()),
        (ColorPaletteBuilder::bright_black, theme::surface2()),
        (ColorPaletteBuilder::bright_red, theme::red()),
        (ColorPaletteBuilder::bright_green, theme::green()),
        (ColorPaletteBuilder::bright_yellow, theme::yellow()),
        (ColorPaletteBuilder::bright_blue, theme::blue()),
        (ColorPaletteBuilder::bright_magenta, theme::pink()),
        (ColorPaletteBuilder::bright_cyan, theme::sky()),
        (ColorPaletteBuilder::bright_white, theme::text()),
    ];
    let channel = |value: f32| (value * 255.).round() as u8;
    slots
        .into_iter()
        .fold(ColorPalette::builder(), |builder, (set, color)| {
            set(
                builder,
                channel(color.r),
                channel(color.g),
                channel(color.b),
            )
        })
        .build()
}

/// Spawn `argv` on a fresh PTY in `cwd` and wrap it in a `TerminalView`.
fn open_session(
    argv: &[String],
    cwd: &Path,
    operation: Option<CheckoutOperation>,
    app: WeakEntity<ReviewApp>,
    item_id: u64,
    terminal_generation: u64,
    cx: &mut Context<ReviewApp>,
) -> anyhow::Result<TerminalSession> {
    // Iosevka's advance is 0.5em; the first paint measures the real cell and
    // resizes the PTY to fit.
    let cols = (CHAT_WIDTH / (TERMINAL_FONT_PX * 0.5)) as u16;
    let size = PtySize {
        rows: 40,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    };
    let PtyPair { master, slave } = native_pty_system().openpty(size)?;
    let mut cmd = CommandBuilder::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.cwd(cwd);
    cmd.env("TERM", "xterm-256color");
    let child = slave.spawn_command(cmd)?;
    // The master reads EOF only once every slave fd is closed; keeping ours
    // open would stop the exit callback from ever firing.
    drop(slave);
    let reader = master.try_clone_reader()?;
    let writer: SharedWriter = Arc::new(Mutex::new(master.take_writer()?));
    let key_writer = Arc::clone(&writer);
    let master = Mutex::new(master);
    let config = TerminalConfig {
        cols: size.cols.into(),
        rows: size.rows.into(),
        font_family: TERMINAL_FONT.into(),
        font_size: px(TERMINAL_FONT_PX),
        colors: palette(),
        ..TerminalConfig::default()
    };
    let view = cx.new(|cx| {
        TerminalView::new(PtyWriter(Arc::clone(&writer)), reader, config, cx)
            .with_resize_callback(move |cols, rows| {
                if let Ok(master) = master.lock() {
                    let _ = master.resize(PtySize {
                        rows: rows as u16,
                        cols: cols as u16,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            })
            // gpui-terminal types the bare letter for cmd-<key>, so unbound
            // cmd chords are dropped. It also sends ESC+key for option chords,
            // which loses the characters Nordic and German macOS layouts type
            // with option (@ $ [ ] { } | \ ~); those go out as typed.
            .with_key_handler(move |event| {
                let modifiers = event.keystroke.modifiers;
                if modifiers.platform {
                    return true;
                }
                match &event.keystroke.key_char {
                    Some(typed) if modifiers.alt && !modifiers.control => {
                        write_pty(&key_writer, typed.as_bytes());
                        true
                    }
                    _ => false,
                }
            })
            .with_exit_callback(move |_, cx| {
                let app = app.clone();
                cx.defer(move |cx| {
                    app.update(cx, |app, cx| {
                        app.terminal_exited(item_id, terminal_generation, cx)
                    })
                    .ok();
                });
            })
    });
    Ok(TerminalSession {
        view,
        writer,
        child: Some(child),
        _operation: operation,
    })
}

impl ReviewApp {
    fn terminal_mut(&mut self, item_id: u64) -> Option<&mut TerminalState> {
        match &mut self.items.iter_mut().find(|item| item.id == item_id)?.state {
            ItemState::Ready(data) => Some(&mut data.terminal),
            _ => None,
        }
    }

    /// `cmd-e`: toggle the terminal panel. Opening starts the active item's
    /// session if it has never run; hiding leaves it running.
    pub(crate) fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.top_view != TopView::Review {
            return;
        }
        self.terminal_visible = !self.terminal_visible;
        if self.terminal_visible {
            self.chat_visible = false;
            self.pr_conversation_visible = false;
            self.palette = None;
            self.palette_gen += 1;
            let idle = self
                .active_data()
                .is_some_and(|data| data.terminal.status == TerminalStatus::Idle);
            if let (true, Some(item_id)) = (idle, self.active_item().map(|item| item.id)) {
                self.launch_terminal(item_id, cx);
            }
        }
        self.focus_terminal(window, cx);
        cx.notify();
    }

    /// Focus the active item's running terminal while the panel shows, else
    /// the diff.
    fn focus_terminal(&self, window: &mut Window, cx: &mut Context<Self>) {
        let session = self
            .active_data()
            .filter(|data| self.terminal_visible && data.terminal.status == TerminalStatus::Running)
            .and_then(|data| data.terminal.session.as_ref());
        match session {
            Some(session) => window.focus(session.view.read(cx).focus_handle()),
            None => window.focus(&self.focus_handle),
        }
    }

    /// (Re)start the item's session: kill any current one, resolve the cwd,
    /// then spawn. PR items reuse the LSP's checkout rather than
    /// materializing it concurrently, since two `materialize_pr_worktree`
    /// runs for one PR share a tmp dir and break each other.
    pub(crate) fn launch_terminal(&mut self, item_id: u64, cx: &mut Context<Self>) {
        if self.closing {
            return;
        }
        let (terminal_generation, lsp_generation, old_session) = {
            let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
                return;
            };
            let ItemState::Ready(data) = &mut item.state else {
                return;
            };
            data.terminal.generation = data.terminal.generation.wrapping_add(1);
            (
                data.terminal.generation,
                item.lsp_gen,
                data.terminal.session.take(),
            )
        };
        let pending_tasks = Arc::clone(&self.pending_checkout_tasks);
        pending_tasks.cancel_terminal_superseded(item_id, terminal_generation);
        if let Some(session) = old_session {
            stop_terminal(session, pending_tasks.begin_cleanup(item_id), cx);
        }
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let cwd = match (&item.source, &data.pr_meta, &data.lsp) {
            (Source::Local(src), _, _) => src.repo_root.clone(),
            (Source::Pr(_), None, _) => {
                data.terminal.status = TerminalStatus::Idle;
                cx.notify();
                return;
            }
            (Source::Pr(_), Some(_), Some(handle)) => handle.root.clone(),
            (Source::Pr(_), Some(_), None)
                if item
                    .checkout
                    .as_ref()
                    .and_then(|lease| lease.path())
                    .is_some() =>
            {
                item.checkout
                    .as_ref()
                    .and_then(|lease| lease.path())
                    .unwrap()
            }
            (Source::Pr(_), Some(_), None) if data.lsp_loading => {
                data.terminal.status = TerminalStatus::WaitingForCheckout;
                cx.notify();
                return;
            }
            (Source::Pr(loc), Some(meta), None) => {
                let manager = self.checkouts.clone();
                let Some(pending_task) =
                    pending_tasks.begin_terminal(item_id, lsp_generation, terminal_generation)
                else {
                    return;
                };
                let pending_cancel = pending_task.cancel_token();
                let loc = loc.clone();
                let head_oid = meta.head_ref_oid.clone();
                let existing_lease = item.checkout.clone();
                data.terminal.status = TerminalStatus::Preparing;
                cx.spawn(async move |this, cx| {
                    let (result, pending_task) = cx
                        .background_spawn(async move {
                            let result = match existing_lease {
                                Some(lease) => {
                                    let result = (|| {
                                        let _operation = lease.operation("terminal preparation")?;
                                        manager.materialize(&lease)
                                    })();
                                    (result, Some(lease), false)
                                }
                                None => match manager.acquire_cancellable(
                                    &loc,
                                    &head_oid,
                                    pending_cancel.as_ref(),
                                ) {
                                    Err(err) => (Err(err), None, true),
                                    Ok(lease) => {
                                        pending_task.register_lease(&lease);
                                        let result = (|| {
                                            let _operation =
                                                lease.operation("terminal preparation")?;
                                            manager.materialize(&lease)
                                        })();
                                        (result, Some(lease), true)
                                    }
                                },
                            };
                            (result, pending_task)
                        })
                        .await;
                    let mut result = Some(result);
                    let stale_lease = this
                        .update(cx, |app, cx| {
                            let Some((root, lease, acquired)) = result.take() else {
                                return None;
                            };
                            if app.closing {
                                return lease;
                            }
                            let Some(item) = app.items.iter_mut().find(|item| item.id == item_id)
                            else {
                                return lease;
                            };
                            let ItemState::Ready(data) = &mut item.state else {
                                return acquired.then_some(lease).flatten();
                            };
                            let checkout_matches = item.checkout.as_ref().is_some_and(|current| {
                                lease.as_ref().is_some_and(|lease| {
                                    current.head_oid() == lease.head_oid()
                                        && current.path() == lease.path()
                                })
                            });
                            if item.lsp_gen != lsp_generation {
                                return (!checkout_matches || acquired).then_some(lease).flatten();
                            }
                            if data.terminal.generation != terminal_generation
                                || data.terminal.status != TerminalStatus::Preparing
                            {
                                return (!checkout_matches || acquired).then_some(lease).flatten();
                            }
                            match root {
                                Ok(root) => {
                                    if acquired {
                                        let Some(lease) = lease.as_ref() else {
                                            data.terminal.status = TerminalStatus::Failed(
                                                "cannot acquire PR checkout".into(),
                                            );
                                            cx.notify();
                                            return None;
                                        };
                                        item.checkout = Some(lease.clone());
                                    }
                                    if !checkout_matches && !acquired {
                                        data.terminal.status = TerminalStatus::Failed(
                                            "cannot acquire PR checkout".into(),
                                        );
                                        cx.notify();
                                        return None;
                                    }
                                    app.spawn_terminal(item_id, &root, terminal_generation, cx);
                                    None
                                }
                                Err(err) => {
                                    data.terminal.status =
                                        TerminalStatus::Failed(format!("{err:#}"));
                                    cx.notify();
                                    acquired.then_some(lease).flatten()
                                }
                            }
                        })
                        .ok()
                        .flatten();
                    let stale_lease =
                        stale_lease.or_else(|| result.take().and_then(|(_, lease, _)| lease));
                    if let Some(lease) = stale_lease {
                        let _ = cx.background_spawn(async move { lease.teardown() }).await;
                    }
                    drop(pending_task);
                })
                .detach();
                cx.notify();
                return;
            }
        };
        self.spawn_terminal(item_id, &cwd, terminal_generation, cx);
    }

    fn spawn_terminal(
        &mut self,
        item_id: u64,
        cwd: &Path,
        terminal_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let app = cx.weak_entity();
        let shown =
            self.terminal_visible && self.active_item().is_some_and(|item| item.id == item_id);
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        let context = match (&item.source, &data.pr_meta) {
            (Source::Local(src), _) => local_chat_header(src),
            (Source::Pr(_), Some(meta)) => pr_chat_header(meta),
            (Source::Pr(_), None) => return,
        };
        let argv = terminal_argv(data.terminal.backend, &context);
        let operation = match item.checkout.as_ref() {
            Some(lease) => match lease.operation("terminal startup") {
                Ok(operation) => Some(operation),
                Err(err) => {
                    data.terminal.status = TerminalStatus::Failed(format!("{err:#}"));
                    cx.notify();
                    return;
                }
            },
            None => None,
        };
        match open_session(&argv, cwd, operation, app, item_id, terminal_generation, cx) {
            Ok(session) => {
                data.terminal.focus_pending = shown
                    && matches!(
                        data.terminal.status,
                        TerminalStatus::WaitingForCheckout | TerminalStatus::Preparing
                    );
                data.terminal.session = Some(session);
                data.terminal.status = TerminalStatus::Running;
            }
            Err(err) => data.terminal.status = TerminalStatus::Failed(format!("{err:#}")),
        }
        cx.notify();
    }

    fn terminal_exited(&mut self, item_id: u64, terminal_generation: u64, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal_mut(item_id) else {
            return;
        };
        if terminal.generation != terminal_generation {
            return;
        }
        if terminal.status != TerminalStatus::Running {
            return;
        }
        let status = terminal
            .session
            .as_mut()
            .and_then(|session| session.child.as_mut()?.try_wait().ok().flatten());
        terminal.status = TerminalStatus::Exited(
            status.and_then(|status| status.signal().is_none().then(|| status.exit_code() as i32)),
        );
        cx.notify();
    }

    fn restart_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item_id) = self.active_item().map(|item| item.id) else {
            return;
        };
        self.launch_terminal(item_id, cx);
        self.focus_terminal(window, cx);
    }

    /// The chip kills the session and relaunches with the other backend. A
    /// launch that is still resolving its cwd just picks the new backend up.
    fn toggle_terminal_backend(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item_id) = self.active_item().map(|item| item.id) else {
            return;
        };
        let Some(terminal) = self.terminal_mut(item_id) else {
            return;
        };
        terminal.backend = terminal.backend.toggled();
        if matches!(
            terminal.status,
            TerminalStatus::WaitingForCheckout | TerminalStatus::Preparing
        ) {
            cx.notify();
            return;
        }
        self.launch_terminal(item_id, cx);
        self.focus_terminal(window, cx);
    }

    /// Bracketed paste, so the TUI takes multi-line text as one input. A
    /// large paste can fill the PTY buffer and block, so it writes off the UI
    /// thread; holding the lock for the whole write keeps keystrokes after it.
    fn paste_into_terminal(&self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let Some(session) = self
            .active_data()
            .and_then(|data| data.terminal.session.as_ref())
        else {
            return;
        };
        let writer = Arc::clone(&session.writer);
        session
            .view
            .update(cx, |view, cx| view.scroll_to_bottom_and_clear_selection(cx));
        let bytes = format!("\x1b[200~{}\x1b[201~", text.replace("\x1b[201~", ""));
        std::thread::spawn(move || write_pty(&writer, bytes.as_bytes()));
    }

    fn copy_from_terminal(&self, cx: &mut Context<Self>) {
        let Some(text) = self
            .active_data()
            .and_then(|data| data.terminal.session.as_ref())
            .and_then(|session| session.view.read(cx).selection_text())
        else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// The right-side terminal panel: header with the backend chip and
    /// Restart, then the live terminal or a status message.
    pub(crate) fn render_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let pending = self.active_data_mut().and_then(|data| {
            let terminal = &mut data.terminal;
            std::mem::take(&mut terminal.focus_pending)
                .then(|| {
                    terminal
                        .session
                        .as_ref()
                        .map(|session| session.view.clone())
                })
                .flatten()
        });
        if let Some(view) = pending {
            window.defer(cx, move |window, cx| {
                window.focus(view.read(cx).focus_handle());
            });
        }
        let panel = div()
            .id("terminal")
            .w(px(
                (f32::from(window.bounds().size.width) * 0.25).clamp(CHAT_WIDTH, 600.)
            ))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(theme::mantle())
            .border_l_1()
            .border_color(theme::surface0())
            .text_size(px(13.));
        let Some(item) = self.active_item() else {
            return panel
                .child(centered_message(
                    "open an item to start a terminal".into(),
                    theme::overlay0(),
                ))
                .into_any_element();
        };
        let data = match (&item.state, &item.source) {
            (ItemState::Ready(data), _) => data,
            (state, source) => {
                let text = match (state, source) {
                    (ItemState::Loading, Source::Pr(_)) => "PR still loading",
                    (ItemState::Loading, Source::Local(_)) => "still loading",
                    _ => "item failed to load",
                };
                return panel
                    .child(centered_message(text.into(), theme::overlay0()))
                    .into_any_element();
            }
        };
        let terminal = &data.terminal;

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
                    .child(SharedString::from("Terminal")),
            )
            .child(backend_chip(
                "terminal-backend-toggle",
                terminal.backend,
                format!("Restart as {}", terminal.backend.toggled().label()).into(),
                cx.listener(|this, _, window, cx| this.toggle_terminal_backend(window, cx)),
            ))
            .child(div().flex_1());
        let action = match terminal.status {
            TerminalStatus::Idle => Some("Start"),
            TerminalStatus::Exited(_) | TerminalStatus::Failed(_) => Some("Restart"),
            _ => None,
        };
        if let Some(label) = action {
            header = header.child(
                Button::new("terminal-restart")
                    .label(label)
                    .small()
                    .on_click(cx.listener(|this, _, window, cx| this.restart_terminal(window, cx))),
            );
        }

        let message = |text: String| centered_message(text.into(), theme::overlay0());
        let body = match (&terminal.status, &terminal.session) {
            (TerminalStatus::Running, Some(session)) => session.view.clone().into_any_element(),
            (TerminalStatus::Idle, _)
                if matches!(item.source, Source::Pr(_)) && data.pr_meta.is_none() =>
            {
                message("PR still loading".into())
            }
            (TerminalStatus::Idle, _) => message("session not started".into()),
            (TerminalStatus::WaitingForCheckout | TerminalStatus::Preparing, _) => {
                message("Preparing checkout…".into())
            }
            (TerminalStatus::Exited(Some(code)), _) if *code != 0 => {
                message(format!("Session ended (exit {code})"))
            }
            (TerminalStatus::Exited(_) | TerminalStatus::Running, _) => {
                message("Session ended".into())
            }
            (TerminalStatus::Failed(err), _) => centered_message(err.clone().into(), theme::red()),
        };

        panel
            .child(header)
            .child(
                div()
                    .key_context("Terminal")
                    .on_action(cx.listener(|this, _: &TerminalPaste, _, cx| {
                        this.paste_into_terminal(cx);
                    }))
                    .on_action(cx.listener(|this, _: &TerminalCopy, _, cx| {
                        this.copy_from_terminal(cx);
                    }))
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(body),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NASTY: &str = "PR \"x\" costs $5 `id`\n[section]\nkey = \"v\"";

    #[test]
    fn claude_argv_passes_context_verbatim_as_dollar_zero() {
        assert_eq!(
            terminal_argv(ChatBackend::Claude, NASTY),
            vec![
                "/bin/zsh",
                "-lic",
                "exec claude --append-system-prompt \"$0\"",
                NASTY,
            ]
        );
    }

    #[test]
    fn codex_argv_passes_context_as_a_toml_string() {
        assert_eq!(
            terminal_argv(ChatBackend::Codex, NASTY),
            vec![
                "/bin/zsh",
                "-lic",
                "exec codex -c \"developer_instructions=$0\"",
                r#""PR \"x\" costs $5 `id`\n[section]\nkey = \"v\"""#,
            ]
        );
    }

    #[test]
    fn codex_argv_escapes_backslashes_and_control_chars() {
        let context = |text| terminal_argv(ChatBackend::Codex, text).pop().unwrap();
        assert_eq!(context(""), "\"\"");
        assert_eq!(context("a\\b\tc\r\u{1b}é"), r#""a\\b\tc\r\u001Bé""#);
    }

    #[test]
    #[cfg(unix)]
    fn terminal_stop_reaps_process_group_before_checkout_teardown() {
        use crate::review_checkouts::ReviewCheckoutManager;
        use std::fs;
        use std::sync::mpsc;
        use std::thread;
        use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

        let root = std::env::temp_dir().join(format!(
            "lgtm-terminal-process-group-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
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
        let operation = lease.operation("terminal").unwrap();

        let PtyPair { master, slave } = native_pty_system()
            .openpty(PtySize {
                rows: 8,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .unwrap();
        let mut command = CommandBuilder::new("/bin/sh");
        command.args([
            "-c",
            "(sleep 1; printf survived > child-survived) & printf started > child-started; exit 0",
        ]);
        command.cwd(&root);
        let mut child = slave.spawn_command(command).unwrap();
        drop(slave);
        let started = root.join("child-started");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !started.is_file() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(started.is_file(), "terminal child did not start");
        let exited_deadline = Instant::now() + Duration::from_secs(2);
        while child.try_wait().unwrap().is_none() && Instant::now() < exited_deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            child.try_wait().unwrap().is_some(),
            "terminal owner did not exit before process-group stop"
        );

        lease.mark_closing();
        let (done, result) = mpsc::channel();
        let teardown_lease = lease.clone();
        let teardown_manager = manager.clone();
        thread::spawn(move || {
            done.send(teardown_manager.teardown(&teardown_lease))
                .unwrap();
        });
        assert!(
            result.recv_timeout(Duration::from_millis(100)).is_err(),
            "checkout teardown raced the running terminal child"
        );

        stop_child(child, Some(operation));
        drop(master);
        thread::sleep(Duration::from_millis(1_200));
        assert!(!root.join("child-survived").exists());
        assert!(
            result.recv_timeout(Duration::from_secs(2)).is_ok(),
            "checkout teardown did not observe terminal shutdown"
        );
        fs::remove_dir_all(root).unwrap();
    }
}
