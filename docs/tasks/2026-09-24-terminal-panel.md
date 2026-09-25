# Terminal panel for interactive claude/codex review

## Outcome
In the Review view, `cmd-e` opens a terminal panel on the right side of the window, in the same 380px slot the chat (`cmd-j`) and PR conversation (`cmd-g`) use. Inside it runs the real interactive `claude` TUI (or `codex`, via a chip in the panel header), started in a checkout of the item under review:

- Local item (`lgtm ~/src/repo`): cwd is the repo root.
- PR item (`lgtm owner/repo#123`): cwd is the PR-head checkout under `~/.cache/lgtm/worktrees/owner__repo__pr123__<oid12>`, the same one the LSP already builds.

The session starts with the same context header the chat sends on its first turn appended to the system prompt (PRs: repo, number, title, author, state and the PR description; local items: repo, branch and base), and nothing else: you type `/code-review` or any skill yourself. Your own claude/codex settings decide permissions. Skills, subagents and permission prompts all work, since it is the normal TUI.

Each sidebar item keeps its own session. Switch items and the other session keeps running in the background; switch back and it is where you left it. Pressing `cmd-e` again hides the panel without killing the session. `cmd-w` on the item kills that process; quitting lgtm closes every PTY, which hangs up (SIGHUP) the agents. When the agent exits, the panel shows "Session ended" with a Restart button.

## Context
- `crates/app/src/app.rs:800-838` builds the main row: sidebar, diff pane (`flex_1`, key context `ReviewApp`), then the right-side panel. The right panel sits outside the `ReviewApp` key context on purpose (comment at 819-823) so typing in it never fires single-letter diff shortcuts. `escape` is only bound in the `ReviewApp` and `Palette` contexts, so it reaches a focused terminal. Only one right panel shows at a time: `ReviewApp.chat_visible` (`app.rs:100`) and `pr_conversation_visible` (`app.rs:104`) clear each other (`chat.rs:339`, `pr_conversation.rs:165`).
- `crates/app/src/chat.rs` is the headless chat. `CHAT_WIDTH = 380.0` (`chat.rs:26`). `ChatBackend { Claude, Codex }` (`chat.rs:44-67`) with `label()`/`toggled()`, shown as a chip by `render_chat_backend` (`chat.rs:708`). Per-item state lives on `ItemData` (`crates/app/src/items.rs:40-118`) and background work finds its item via `chat_mut(item_id)` (`chat.rs:323`). `pr_chat_header` (`chat.rs:136`) and `local_chat_header` (`chat.rs:155`) build the context header text.
- `crates/app/src/pr_conversation.rs` (commit 369b57a) is the template for a new right panel: `toggle_pr_conversation` (`:150-215`), a render fn using the same div shell (`w(px(CHAT_WIDTH)).flex_shrink_0().h_full().bg(theme::mantle()).border_l_1()`), an action + binding in `crates/app/src/main.rs` (`actions!` at `:53-104`, bindings at `:192-205`), handler wiring at `app.rs:637-639`.
- `crates/app/src/lsp.rs`: `lsp_root_for_source(source, pr_meta)` (`:28`) returns `repo_root` for local items and, for PRs, calls `materialize_pr_worktree(loc, head_oid)` (`:41`), which makes a blobless partial clone checked out at the PR head in `~/.cache/lgtm/worktrees/<owner>__<repo>__pr<N>__<oid12>` (reuses an existing dir, clones via a `tmp-<pid>` dir otherwise). `restart_lsp_for_item` (`:~235-300`) runs on every item load and refresh: it sets `data.lsp_loading = true`, calls `lsp_root_for_source` then `LspSession::start` on the background executor, and in its result handler (after an `lsp_gen` check) sets `lsp_loading = false` and `data.lsp = Some(LspHandle { root, .. })` or `data.lsp_error`.
- `crates/app/src/items.rs:652-675` `close_item` removes the item from `self.items`, dropping its `ItemData`. Refresh mutates `ItemData` in place (`items.rs:300-313`), so per-item state survives `r`.
- `crates/app/src/dispatch.rs:180-192` launches an agent in Ghostty through a login shell, `/bin/zsh -lic '<bin> "$0"' <prompt>`, so a Finder-launched app still gets the user's PATH. Argv builders there are pure fns with unit tests (`dispatch.rs:517+`).
- `crates/app/src/theme.rs`: Catppuccin Mocha colours as free fns returning `Rgba`. No teal/sky/pink/surface1/surface2 yet. Mono font `MONO = "Menlo"` (`main.rs:48`).
- `cached_prs::git_env()` (`cached_prs.rs:21`) is set process-wide in `main.rs` (`GIT_TERMINAL_PROMPT=0`, gh credential helper). A PTY child inherits it; that is fine for git calls the agent makes.
- `Quit` is `cx.quit()` (`main.rs:234`), which ends the process without running entity Drop impls.

Terms:
- **PTY** (pseudo-terminal): the OS device pair that lets a program like `claude` believe it runs in a real terminal. `portable-pty` opens one and spawns the child on it.
- **gpui-terminal**: crate `gpui-terminal = "0.1"` (github.com/zortax/gpui-terminal, MIT/Apache, depends on `gpui ^0.2.2` like lgtm, `alacritty_terminal ^0.25.1`). Per its README, `TerminalView::new(writer, reader, config, cx)` renders a terminal fed by any `Read`/`Write`, with `with_resize_callback(|cols, rows|)` and `with_exit_callback`. Known gaps: no mouse selection, no scrollback navigation. Not yet built locally; step 1 verifies the API.
- **TUI**: the full-screen interactive UI `claude` and `codex` draw when run without `-p`/`exec`.

## Approach
New module `crates/app/src/terminal.rs`, modelled on `pr_conversation.rs`.

State:
- `ReviewApp.terminal_visible: bool`, cleared by and clearing the other two panel flags, so the three panels share one slot.
- `ItemData.terminal: TerminalState` per item: `backend: ChatBackend` (reused enum, default Claude), `session: Option<TerminalSession>`, `status: Idle | WaitingForCheckout | Preparing | Running | Exited(Option<i32>) | Failed(String)`.
- `TerminalSession` owns the `Entity<TerminalView>`, the PTY master (for resize) and the child. Its `Drop` sends SIGKILL to the child and moves the child handle to a detached `std::thread` that calls `wait()`, so the UI thread never blocks and no zombie is left behind. `close_item` drops `ItemData`, so `cmd-w` ends the process. On quit Drop does not run; the kernel closes the PTY master and the child gets SIGHUP. Step 1 confirms claude and codex die on SIGHUP; if either does not, add an explicit kill of all sessions in the `Quit` handler.

Launch (first `cmd-e` on an item without a live session, or Restart):
1. Resolve cwd without racing the LSP. Two concurrent `materialize_pr_worktree` calls for one PR share the same `tmp-<pid>` dir and break each other, and `restart_lsp_for_item` already runs one on every load and refresh. So:
   - Local item: `repo_root`, launch now.
   - PR item, `data.lsp` is `Some(handle)`: use `handle.root`, launch now.
   - PR item, `data.lsp_loading` is true: status = `WaitingForCheckout`, panel shows "Preparing checkout…". In the LSP result handler (after the `lsp_gen` check), if the item's terminal status is `WaitingForCheckout`, call the launch fn again; it now lands in one of the other branches.
   - PR item, LSP finished with an error or never started: status = `Preparing`, call `lsp_root_for_source(&source, pr_meta)` on the background executor. No LSP clone is in flight in this state, and `materialize_pr_worktree` reuses the dir if the LSP got as far as cloning.
   - PR item with no `pr_meta` yet: the panel opens showing "PR still loading", status stays `Idle`, no auto-start. The user presses Restart once it has loaded.
2. Build argv with a new pure fn `terminal_argv(backend, context) -> Vec<String>`, with its own `"claude"`/`"codex"` mapping (a `bin()` on `ChatBackend`; `dispatch::Agent::bin()` is private and keyed on a different enum):
   - claude: `/bin/zsh -lic 'exec claude --append-system-prompt "$0"' <context>`
   - codex: `/bin/zsh -lic 'exec codex -c developer_instructions="$0"' <context>`. `$0` quoting mirrors `dispatch.rs` so the context never goes through shell parsing. The exact TOML quoting codex needs for `-c` is settled in step 1.
   - context = `pr_chat_header(meta)` or `local_chat_header(src)` (existing fns), no patch attached; the agent can run `git diff`/`gh pr diff` itself in the checkout.
3. Open a PTY with `portable-pty` sized from the panel (cols/rows from 380px and the cell size gpui-terminal reports; the resize callback forwards to `master.resize`), spawn argv with cwd and `TERM=xterm-256color`. Hand the reader/writer to `TerminalView::new`. Status = Running.
4. Exit callback sets status to Exited via `this.update`, addressing the item by id (same pattern as `chat_mut`).

Render: header row with title "Terminal", the backend chip (same look as `render_chat_backend`; clicking kills the session and relaunches with the toggled backend), and a Restart button when Idle/Exited/Failed. Body is the `TerminalView`, or a `centered_message` for WaitingForCheckout/Preparing/Failed/Exited/"PR still loading". The panel only renders in `TopView::Review`.

Focus and keys: opening the panel focuses the TerminalView. It lives outside the `ReviewApp` key context, so diff letter keys never fire. Global `cmd-*` bindings (`cmd-k`, `cmd-w`, `cmd-e`, `cmd-b`, `cmd-j`, `cmd-g`) still win. Escape goes to the terminal (claude uses Esc to interrupt), so leaving the panel is `cmd-e` or clicking the diff. Clicking the diff pane refocuses `focus_handle` as today.

Palette: build a `ColorPalette` from `theme.rs`, adding the missing Catppuccin Mocha colours as new theme fns (teal `0x94e2d5`, sky `0x89dceb`, pink `0xf5c2e7`, surface1 `0x45475a`, surface2 `0x585b70`). Font `MONO`, size matching the diff font size.

## Not doing
- No popout window; the app stays single-window.
- No resizable or wider panel; 380px like chat. Width is a later change.
- No changes to or removal of the headless chat (`cmd-j`), its crates, or the selection-rides-along feature.
- No permission flags: no `--allowed-tools`, `--permission-mode`, or codex sandbox flags. The user's own config decides.
- No auto-run skill or initial prompt.
- No mouse selection or copy. Scrollback comes from the Fleron/gpui-terminal fork (pinned rev).
- No persistence of sessions across lgtm restarts, no `claude --resume` wiring.
- No terminal in the Tracker view.
- No external-terminal (Ghostty) fallback.
- No shell tab: the PTY runs the agent directly; when it exits there is no shell prompt.
- A PR refresh that moves the head to a new commit does not move a running session; it keeps its old checkout dir until restarted.

## Decisions
D1. gpui-terminal 0.1 over gpui_xterm (needs the gpui-kit 0.6 upgrade, Chinese-only docs) — no mouse selection. Scrollback and mouse-wheel scrolling come from the Fleron/gpui-terminal fork (`scrollback` branch, pinned by rev in `crates/app/Cargo.toml`).
D2. 380px width, same as chat — about 45 columns at Menlo 13; claude/codex TUIs wrap heavily and some layouts may look cramped.
D3. User's default permissions — a review session can edit files or run commands the user approves (or pre-approved in their settings) inside the checkout; for PRs that is the cache clone, for local items it is the user's real working tree.
D4. Reuse the LSP PR checkout as cwd and wait for the LSP when it is mid-start — on a fresh PR the terminal starts only after the language server is up, not just after the clone; the terminal and the LSP share one dir, so an agent that edits or checks out other refs there changes what the LSP sees, and deleting the cached PR from the sidebar pulls the dir out from under a live session.
D5. Sessions live per item until cmd-w or quit — several idle claude/codex processes can pile up if many items are open.
D6. Launch via `/bin/zsh -lic` like dispatch — assumes zsh profile files provide PATH, matching today's Ghostty dispatch.
D7. Reuse `ChatBackend` for the terminal chip — the enum now serves two panels; renaming later touches both.
D8. Global cmd bindings beat the terminal — cmd-k, cmd-w, cmd-b etc. never reach claude/codex; none of them are needed by those TUIs.
D9. SIGKILL on cmd-w, no graceful shutdown — claude/codex get no chance to flush anything on item close; their session logs may miss the last turn.

## Steps
1. Spike, throwaway, in the worktree: add `gpui-terminal = "0.1"` and `portable-pty = "0.9"` to `crates/app/Cargo.toml`, temporarily render a `TerminalView` running `claude` in the chat slot at 380px. → check:
   - `cargo build -p lgtm` succeeds and the crate API matches the plan (`TerminalView::new(writer, reader, config, cx)`, resize and exit callbacks).
   - On a local repo the claude TUI shows; typing and Enter work, `/help` renders, Esc interrupts, colours look right.
   - Hide the panel while claude streams a long answer, wait 30s, re-show: still responsive (the PTY keeps draining while unmounted).
   - Quit lgtm: `pgrep -fl claude` shows the child gone (SIGHUP works).
   - `codex -c developer_instructions="…"` starts and repeats the instruction when asked.
   If the TUI is unusable or unmounted views stop draining the PTY, stop and report before step 2.
2. Add `terminal_argv(backend, context)` pure fn and `ChatBackend::bin()` in `crates/app/src/terminal.rs` / `chat.rs`, with unit tests for both backends and a context containing quotes, `$`, backticks and newlines. → check: `cargo test -p lgtm terminal` passes.
3. Add `TerminalState`/`TerminalSession` (Drop SIGKILLs the child, reaps it on a detached thread), `ItemData.terminal`, `ReviewApp.terminal_visible`, the `ToggleTerminal` action with global `cmd-e` binding and handler wiring in `app.rs`, and mutual exclusion in all three panel toggles. → check: `cargo build -p lgtm`; cmd-j / cmd-g / cmd-e switch the slot between the three panels.
4. Launch flow: cwd resolution per Approach step 1 (including the `WaitingForCheckout` hook in the LSP result handler), PTY spawn, resize and exit callbacks. → check: cmd-e on a local item starts claude in `repo_root` (`! pwd` in claude shows it); on a freshly opened PR item, cmd-e pressed immediately shows "Preparing checkout…", then claude in `~/.cache/lgtm/worktrees/...`, and the LSP status still reaches ready (no clone clash).
5. Header: backend chip (kill + relaunch), Restart button, status messages; palette from `theme.rs` with the added colours. → check: the chip relaunches as codex; `/exit` in claude shows "Session ended" and Restart brings it back.
6. Remove the step 1 spike wiring. → check: `git diff` shows no spike code in `chat.rs`; `cargo test` and `cargo clippy --workspace --all-targets` are clean.

## Validation
`/Users/emil.fleron/Documents/Tools/lgtm` `cargo test` → all tests pass, including the new `terminal_argv` tests.
`/Users/emil.fleron/Documents/Tools/lgtm` `cargo clippy --workspace --all-targets` → no warnings.

Manual scenario, proving it does more than compile:
1. `cargo run -p lgtm -- ~/Documents/Tools/lgtm owner/repo#<open PR>` (two items).
2. On the PR item press cmd-e right away. The panel shows "Preparing checkout…", then the claude TUI. Ask "what is the cwd and what PR am I reviewing?" → it answers with the `~/.cache/lgtm/worktrees/...` path and the PR number/title from the injected context. The titlebar LSP status reaches ready.
3. Type `/code-review` (or another installed skill) → the skill runs; its tool permission prompts appear in the panel and accept keyboard answers.
4. Switch to the local item, cmd-e → a separate claude session starts in the repo root. Switch back to the PR item → the first session is still there, mid-conversation.
5. Press j/k while the terminal is focused → the characters go to claude, the diff does not scroll. Click the diff, press j → the diff scrolls.
6. Click the chip → the session restarts as codex in the same cwd.
7. Hide the panel with cmd-e while claude is answering, wait, press cmd-e → the answer finished and the session responds.
8. cmd-w on the PR item → `pgrep -fl claude` no longer lists that session's process, and the window did not freeze. Quit lgtm → no claude/codex children remain.
