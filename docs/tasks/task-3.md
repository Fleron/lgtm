# Switchable Claude/Codex chat via Codex app-server

## Goal

Add Codex as a switchable provider in LGTM's existing review chat while preserving the current Claude experience. Use one long-lived, multiplexed Codex app-server connection so Codex can stream replies and support clean cancellation without starting a new process for every message.

Use the [official Codex app-server documentation](https://learn.chatgpt.com/docs/app-server) as the protocol source of truth. The implementation must use the documented stable stdio/JSONL surface and must not infer behavior from a CLI TUI or undocumented wire fields.

## Behaviours / Scope

- Keep Claude as the default provider and preserve its current prompt, read-only tool policy, streaming cadence, transcript rendering, selection context, `⌘⏎` send, Escape/Stop cancellation, item switching, and error handling.
- Add a small Claude/Codex selector in the existing chat header. Switching provider changes which provider-specific conversation is shown for the active review item; switching back restores the other conversation.
- Keep independent in-memory transcripts and session identifiers for Claude and Codex for every review item. A Codex thread must never receive Claude history, and changing providers must not transfer messages or context between them.
- Start Codex lazily on the first Codex send. Maintain one shared `codex app-server` process for the application and multiplex one app-server thread per review item through it.
- Use the app-server's configured default Codex model. Do not add a model selector or hard-code a model override.
- Reuse the existing `chat_prompt` and review context construction: send the PR/local header and capped unified patch once on the first Codex turn for that item, include the current selection in the same way as Claude, and send follow-up questions without duplicating the first-turn context.
- Render only Codex agent text in the existing transcript. Do not add tool cards, approval UI, model controls, login UI, or a second chat surface.

## Protocol lifecycle

Implement the adapter as a small protocol/process layer in the mandatory `crates/codex` crate, mirroring the narrow headless boundary of `crates/claude`; keep JSON-RPC details out of the GPUI event handlers.

1. On the first Codex send, spawn exactly one `codex app-server` child with stdin/stdout pipes and the default stdio transport (newline-delimited JSON). Drain stderr independently so a noisy process cannot block the reader. Do not spawn it during application startup or for each turn.
2. Allocate monotonically increasing numeric JSON-RPC request IDs. Route response objects by their numeric `id` to the pending request, and route notifications by method/params to the event pump. Never assume responses arrive in request order.
3. Immediately after connecting, send one `initialize` request with LGTM client metadata, wait for its response, then send the `initialized` notification. Do not send `thread/start` or any turn request before this handshake; do not set `capabilities.experimentalApi`.
4. When a Codex conversation is first used for a review item, prepare its required review cwd before sending anything that starts a turn. Send `thread/start` with that cwd, `approvalPolicy: "never"`, and `sandbox: "read-only"`, then record the returned thread ID in that item's Codex state. There is one thread per review item, not one thread per message.
5. For each user message, send `turn/start` for that item's thread with one text input, the same cwd, `approvalPolicy: "never"`, and `sandboxPolicy: {"type": "readOnly", "networkAccess": false}`. Use the default model and the same first-turn context rules above. Route all resulting events by `threadId`/`turnId` so switching the visible item cannot append text to the wrong transcript.
6. Consume `item/agentMessage/delta` notifications and batch their text into the existing assistant bubble at the current UI cadence. Treat `item/completed` for the agent message and `turn/completed` as authoritative terminal signals; support `completed`, `interrupted`, and `failed` states. Surface protocol or turn errors in the existing error treatment and always clear the in-flight state.
7. On Stop or Escape, send `turn/interrupt` with the exact thread and active turn IDs. Treat the resulting `turn/completed` with interrupted status as a normal cancellation, and make cancellation idempotent if the turn has already completed.
8. Handle server-initiated JSON-RPC requests on the same reader. Respond using the request's exact ID and these stable safe responses; never accept one and never wait for a UI prompt:
   - `item/commandExecution/requestApproval` and `item/fileChange/requestApproval`: `{"id": <same id>, "result": {"decision": "decline"}}`.
   - `item/permissions/requestApproval`: `{"id": <same id>, "result": {"permissions": {}, "scope": "turn"}}`.
   - `mcpServer/elicitation/request`: `{"id": <same id>, "result": {"action": "decline", "content": null}}`.
   - Unknown server requests: `{"id": <same id>, "error": {"code": -32601, "message": "Method not supported"}}`.
   Do not add handlers for experimental server request methods. `approvalPolicy: "never"` should make approval requests unusual, but the defensive handler must prevent a hang or accidental side effect if one appears.
9. If stdout reaches EOF, the child exits, or the reader encounters malformed protocol data, fail any in-flight turn, invalidate the connection generation, and make the UI recoverable. Start a replacement process only when Codex is used again. After the replacement handshake, attempt `thread/resume` for each still-known in-memory Codex thread before sending its next turn; if resume fails because the thread is unavailable, start a fresh thread for that item and send its first-turn context again rather than mixing histories. Do not route late events from the old process.

## Read-only context policy

Codex is being used to inspect a review, not to modify the checkout. Use the stable app-server read-only fields on every thread and turn. `sandbox: "read-only"` on `thread/start` and `sandboxPolicy: {"type": "readOnly", "networkAccess": false}` on every `turn/start` prevent sandboxed command/file changes, but they permit broad filesystem reads. The cwd selects the working directory only; it is not a read boundary. `networkAccess: false` governs network access by sandboxed agent commands/tools, not the app-server's required OpenAI service traffic.

- Every `thread/start` payload must include the review cwd, `approvalPolicy: "never"`, and `sandbox: "read-only"`.
- Every `turn/start` payload must include that same cwd, `approvalPolicy: "never"`, and `sandboxPolicy: {"type": "readOnly", "networkAccess": false}`. Do not substitute legacy readable-root fields or claim that these stable fields provide path isolation.
- For a local review, use the actual local repository cwd. For a PR review, always create and use a dedicated per-item materialized scratch cwd when files are available, or a dedicated empty per-item scratch cwd when no PR files are materialized. Never fall back to LGTM's process cwd or repository cwd for a PR.
- Do not use beta permission-profile or permission-profile discovery/list APIs in this stable-only implementation. Strict read-root isolation is a non-goal and a follow-up requiring those beta profiles or host OS sandboxing; document the broad-read limitation rather than implying stronger isolation.
- Keep authentication delegated to the user's existing Codex CLI configuration. Surface missing binary/authentication errors clearly, but do not implement a login flow.

## Affected areas

- `Cargo.toml`: add the mandatory `crates/codex` adapter crate to the workspace.
- `crates/codex/Cargo.toml` and `crates/codex/src/lib.rs`: process ownership, JSONL read/write, typed protocol messages/events, request correlation, lifecycle/restart handling, interruption, and safe server-request replies.
- `crates/app/Cargo.toml`: must depend on the mandatory `crates/codex` adapter.
- `crates/app/src/main.rs`: provider selection, provider-specific `ChatState`, shared prompt/context dispatch, Codex event routing, cancellation, selector/header rendering, and restart/error state. Reuse existing `ChatMessage`, `chat_prompt`, `ExplorePlan`, and scroll/pump behavior where possible.
- `crates/claude/src/lib.rs`: preserve its public behavior and existing tests; change it only if a minimal provider-neutral extraction is necessary.

## Explicit decisions

- Integration boundary: use app-server over local stdio JSONL, not `codex exec`, a network listener, or a separate process per turn.
- Process ownership: one lazy app-server per LGTM process, shared by all Codex review-item threads; serialize writes and multiplex reads.
- Conversation ownership: one in-memory Codex thread and transcript per review item, independent from that item's Claude state. Do not expose or persist a session-history UI.
- Model and auth: app-server's default model and the user's existing Codex CLI authentication; no model picker and no login screen.
- Safety: stable `thread/start` read-only fields (`cwd`, `approvalPolicy: "never"`, `sandbox: "read-only"`) and stable `turn/start` read-only fields (`cwd`, `approvalPolicy: "never"`, `sandboxPolicy: {"type": "readOnly", "networkAccess": false}`), plus exact automatic denial for unexpected stable approval/permission/elicitation requests. These fields prevent sandboxed command/file writes but do not isolate filesystem reads.
- API surface: stable documented methods only; do not opt into experimental app-server APIs, and fail closed when the installed server cannot provide the required stable behavior.
- UX: use the existing chat panel and transcript; provider switching is the only new control. If a provider is mid-turn, keep its cancellation semantics intact and do not silently reassign the run to the other provider.

## Acceptance criteria

- Claude remains behaviorally unchanged, including existing tests and normal chat flows.
- The chat header offers Claude and Codex, with Claude selected by default; selecting either provider shows and updates only that provider's in-memory transcript for the active review item.
- The first Codex turn includes the existing review header/patch once, follow-ups do not duplicate it, and selected diff context remains available as it is for Claude.
- Codex uses one lazily created app-server process, one thread per review item, numeric request correlation, and correctly routed streamed deltas for multiple open items.
- Initialization always follows `initialize` response then `initialized`; no pre-handshake request is sent.
- Completed, failed, interrupted, spawn, parse, authentication, and EOF/restart cases leave the UI in a non-stuck state with a useful error or stopped marker.
- Stop/Escape interrupts the active Codex turn and does not cancel a different review item's turn.
- Approval, file-change, permission, and elicitation requests cannot grant access, write files, or block waiting for a user; sandboxed command/tool operations cannot use the network under `networkAccess: false`, while the app-server's required OpenAI service traffic remains available. The exact safe result payloads use the matching JSON-RPC request IDs, and unknown requests receive a matching `-32601` method-not-supported error. Experimental app-server APIs are not implemented.
- Codex uses the stable read-only fields on every thread and turn. Stable read-only prevents sandboxed command/file writes but permits broad filesystem reads, and cwd is not treated as a read boundary. Local reviews use the actual repository cwd; PR reviews always use a dedicated materialized or empty per-item scratch cwd and never LGTM's process/repository cwd. Strict read-root isolation is explicitly deferred to beta permission profiles or host OS sandboxing.
- The installed Codex version is verified to support the documented stable fields and lifecycle; an incompatible version fails before the turn with an actionable upgrade error.
- No Codex session identifiers or transcripts are written by LGTM to disk, no provider history is transferred, and a process restart does not lose the rest of the app's UI state; available in-memory threads are resumed or safely restarted as specified.

## Staged implementation

1. Add the small Codex adapter and pure protocol models. Implement JSONL framing, numeric correlation, initialization, thread/turn requests, delta/terminal parsing, interrupt, server-request denial, and EOF handling with fake-stream unit tests.
2. Refactor app chat state minimally so Claude and Codex have separate provider state while retaining the existing Claude path and prompt helpers.
3. Add lazy app-server startup, thread creation/resume, multiplexed background event pumping, first-turn context dispatch, and safe cancellation/restart routing.
4. Add the provider selector and reuse the existing chat transcript/header/input UI. Keep non-agent items out of the transcript and keep the default provider Claude.
5. Run automated tests, then perform the manual safety/recovery checks below before handoff.

## Tests

- Adapter unit tests with fixture/fake JSONL streams for initialize/initialized ordering, numeric out-of-order response correlation, thread/start and thread/resume, turn/start, agent-message deltas, authoritative completion/error handling, turn/interrupt, malformed input, EOF, and stale-generation suppression.
- Adapter unit tests assert `thread/start` carries `cwd`, `approvalPolicy: "never"`, and `sandbox: "read-only"`, while every `turn/start` carries `cwd`, `approvalPolicy: "never"`, and `sandboxPolicy: {"type":"readOnly","networkAccess":false}`. Assert no beta permission-profile/list or legacy readable-root fields are sent, and assert the stable read-only contract discloses broad filesystem reads rather than treating cwd as a boundary. Assert exact denial responses: `{"decision":"decline"}` for command/file-change approval, `{"permissions":{},"scope":"turn"}` for permission approval, `{"action":"decline","content":null}` for elicitation, and `-32601` for unknown requests, all with matching request IDs.
- App unit tests cover first-turn-only `chat_prompt` context, independent Claude/Codex transcripts and sessions, provider switching, item-to-thread routing, cancellation, and restart fallback.
- Manual verification with a real authenticated Codex installation: open both a local review and PR, switch providers, send follow-ups, include a selected range, switch items while a turn streams, press Stop/Escape, and confirm smooth transcript updates and stable scroll behavior.
- Manual safety verification with a real installed Codex checks `codex --version` and exercises the stable app-server lifecycle/field compatibility before any turn. Confirm `thread/start` and every `turn/start` carry the exact stable sandbox fields; local reviews use the repository cwd, and PR reviews use dedicated materialized or empty per-item scratch cwds without falling back to LGTM's process/repository cwd. Confirm sandboxed commands cannot modify files, broad filesystem reads remain possible, cwd is not presented as a boundary, and `networkAccess: false` applies to sandboxed commands/tools while required app-server OpenAI service traffic still works. Exercise or simulate command/file-change/permission/elicitation requests and confirm the exact denial payloads, matching IDs, and no hanging UI.
- Manual recovery verification kills or disconnects `codex app-server`, confirms the active turn fails cleanly, sends a later Codex message, and verifies handshake plus thread resume or safe fresh-thread/context fallback. Repeat with an installed Codex version that rejects the documented stable fields and confirm the turn is refused with an actionable upgrade/compatibility error.
- Verify with:

  - `cargo fmt --check`
  - `cargo test -p codex`
  - `cargo test -p lgtm`
  - `cargo test --workspace`

## Non-goals

- Changing Claude's CLI integration or existing Claude behavior.
- Building an LGTM plugin, MCP server, tool integration, or Codex login/authentication UI.
- Adding a model selector, effort selector, tool cards, approval dialogs, or arbitrary Codex tool execution.
- Transferring conversation history between Claude and Codex.
- Persisting Codex sessions/transcripts in LGTM or adding a session-history browser.
- Opting into experimental app-server APIs, WebSocket transport, dynamic tools, plugins, or MCP.
- Strict read-root isolation through beta permission profiles or host OS sandboxing; this remains a follow-up to the stable broad-read policy.
- Integrating `codex exec` or spawning one Codex process per message.
