# Codex chat alongside Claude

## What

Add OpenAI Codex's `codex app-server` as a second chat backend, selectable next
to Claude Code, reusing the existing chat UI (`ChatState`/`ChatMessage`
rendering) unchanged.

## Why

`cmd-j` chat is currently hardcoded to the `claude` CLI. Some reviewers use
Codex instead of or alongside Claude; picking a backend per session (or
per-message) would let them stay in lgtm instead of switching tools.

## Decided (feature-planning, 2026-09-11)

Full feature-plan: `/Users/emil.fleron/.claude/plans/humming-wobbling-trinket.md`.

- Backend toggle lives inside the chat panel itself (not the titlebar), is per-item, and resets to Claude every time an item is opened or the app restarts. No disk persistence, no per-message selection.
- Switching backend clears the item's transcript and starts a new conversation. If a reply is streaming when the switch happens, it's stopped first (same as hitting stop), then the switch proceeds.
- Codex gets the same diff/selection context and cross-turn memory Claude gets today.
- Codex failures render the same inline red error text Claude failures do, with no retry and no fallback to the other backend. No upfront check that the `codex` CLI is installed.
- Stopping an in-flight Codex reply shows the same "stopped" marker Claude shows today.
- Cost line is omitted for Codex messages that don't report a cost, rather than showing a placeholder.
- Codex is read-only (no file edits, no writing shell commands), matching Claude's read-only intent, but the mechanism differs: Claude enforces confinement to the item's directory and blocks web access via a tool allowlist, while Codex's read-only shell can reach files outside the item's directory and make outbound network requests (e.g. `curl`). Accepted knowingly as a real safety gap between the two backends, not closed in this feature.

## How

Today: `crates/claude/src/lib.rs` spawns `claude -p <prompt> --output-format
stream-json --verbose --include-partial-messages --permission-mode dontAsk`,
reads stdout line by line, and turns each NDJSON line into one of three
`ChatEvent` variants (`TextDelta`, `Completed { session_id, cost_usd, is_error,
text }`, `Failed`). `crates/app/src/main.rs` calls `claude::chat(&prompt,
&opts, &cancel_bg, |event| { .. })` at one call site (~line 6677) inside a
`cx.spawn` async block. There is no provider abstraction; `ChatState` and the
`claude::` module path are assumed everywhere a chat runs.

`codex app-server` does not speak this protocol. It's JSON-RPC 2.0 over
stdio, a long-lived process with request/response/notification framing and an
explicit session/turn lifecycle, structurally like LSP, not like Claude's
flat NDJSON stream. The repo already has a JSON-RPC-over-stdio client for
exactly this shape of problem: `crates/app/src/lsp_client.rs` (built for
`rust-analyzer`/Bifrost). It also already has the "pick one of two backends"
pattern: `LspBackend` (`lsp_client.rs:31-35`, a `#[default]` two-variant enum
with a `label()` for the status chip, wired into `main.rs` at lines 37, 2414,
2649).

Steps:
1. New `crates/codex` crate exposing the same shape as `crates/claude`:
   `ChatOptions`, `ChatEvent` (reuse `claude::ChatEvent` or an identical
   type), and a `chat(...)` function with the same signature. Internally it
   drives Codex's JSON-RPC session/turn lifecycle over `lsp_client.rs`'s
   stdio transport instead of line-parsing NDJSON. This is real protocol
   work: Codex's exact method names and session/turn JSON shapes need
   pulling from Codex's own `app-server` docs; nothing in this repo names
   them yet.
2. Add a `ChatBackend { Claude, Codex }` enum next to `LspBackend`'s pattern,
   stored on `ReviewApp` state, with a status-chip-style UI toggle.
3. Swap the single call site in `main.rs` (~line 6677) to dispatch on
   `ChatBackend` and call either `claude::chat` or `codex::chat`. No other
   line in `main.rs` needs to change, since both produce the same
   `ChatEvent` stream the rendering code already consumes.

Effort: medium, not surgical. The transport pattern and the two-backend UI
pattern both already exist in-repo; the new work is the Codex protocol
mapping in step 1. A general plugin system for arbitrary backends beyond
Claude/Codex is out of scope here.
