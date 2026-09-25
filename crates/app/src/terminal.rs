use crate::chat::{backend_chip, local_chat_header, pr_chat_header, ChatBackend, CHAT_WIDTH};
use crate::items::{ItemState, Source};
use crate::lsp::lsp_root_for_source;
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
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        // portable-pty's `kill` sends SIGHUP, which the agent may handle and
        // outlive; SIGKILL can't be caught. portable-pty spawns the child
        // with setsid, so its pid is also the group id and the negative pid
        // takes the agent's own subprocesses with it. The blocking reap runs
        // off the UI thread so closing an item never stalls a frame.
        if let Some(pid) = child.process_id() {
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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
    app: WeakEntity<ReviewApp>,
    item_id: u64,
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
                    app.update(cx, |app, cx| app.terminal_exited(item_id, cx))
                        .ok();
                });
            })
    });
    Ok(TerminalSession {
        view,
        writer,
        child: Some(child),
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
        let Some(item) = self.items.iter_mut().find(|item| item.id == item_id) else {
            return;
        };
        let ItemState::Ready(data) = &mut item.state else {
            return;
        };
        data.terminal.session = None;
        let cwd = match (&item.source, &data.pr_meta, &data.lsp) {
            (Source::Local(src), _, _) => src.repo_root.clone(),
            (Source::Pr(_), None, _) => {
                data.terminal.status = TerminalStatus::Idle;
                cx.notify();
                return;
            }
            (Source::Pr(_), Some(_), Some(handle)) => handle.root.clone(),
            (Source::Pr(_), Some(_), None) if data.lsp_loading => {
                data.terminal.status = TerminalStatus::WaitingForCheckout;
                cx.notify();
                return;
            }
            (Source::Pr(_), Some(meta), None) => {
                let (source, meta) = (item.source.clone(), meta.clone());
                data.terminal.status = TerminalStatus::Preparing;
                cx.spawn(async move |this, cx| {
                    let root = cx
                        .background_spawn(async move { lsp_root_for_source(&source, Some(&meta)) })
                        .await;
                    this.update(cx, |app, cx| {
                        let Some(terminal) = app.terminal_mut(item_id) else {
                            return;
                        };
                        if terminal.status != TerminalStatus::Preparing {
                            return;
                        }
                        match root {
                            Ok(root) => app.spawn_terminal(item_id, &root, cx),
                            Err(err) => {
                                terminal.status = TerminalStatus::Failed(format!("{err:#}"));
                                cx.notify();
                            }
                        }
                    })
                    .ok();
                })
                .detach();
                cx.notify();
                return;
            }
        };
        self.spawn_terminal(item_id, &cwd, cx);
    }

    fn spawn_terminal(&mut self, item_id: u64, cwd: &Path, cx: &mut Context<Self>) {
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
        match open_session(&argv, cwd, app, item_id, cx) {
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

    fn terminal_exited(&mut self, item_id: u64, cx: &mut Context<Self>) {
        let Some(terminal) = self.terminal_mut(item_id) else {
            return;
        };
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
            .w(px(CHAT_WIDTH))
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
}
