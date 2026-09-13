//! Headless Q&A via `codex app-server` (OpenAI Codex), same public shape as
//! `crates/claude` so the two backends are interchangeable at the call site.
//!
//! Unlike Claude's flat NDJSON stream, `codex app-server` speaks JSON-RPC 2.0
//! over stdio with an explicit thread/turn lifecycle (`initialize` handshake,
//! `thread/start` or `thread/resume`, `turn/start`, streaming
//! `item/agentMessage/delta` notifications, a terminal `turn/completed`).
//! Framing is newline-delimited JSON, one message per line, no
//! `Content-Length` headers (unlike `crates/app/src/lsp_client.rs`'s LSP
//! transport, which this crate is otherwise modeled on).
//!
//! Method names and JSON shapes below were taken from `codex app-server
//! generate-ts` / `generate-json-schema` run locally against the installed
//! `codex` binary (codex-cli 0.153.4), then confirmed against a live
//! `initialize` -> `thread/start` -> `turn/start` exchange with that same
//! binary. Generating directly from the installed binary avoids drift
//! against a version-mismatched upstream doc.
//!
//! Sandbox mode is locked to `read-only` and approval policy to `never` on
//! every `thread/start`/`thread/resume`, so a turn can never block on an
//! approval prompt; any approval request the server sends anyway is denied
//! (see `crates/app/src/lsp_client.rs`'s `handle_server_message` for the
//! analogous "always answer server-initiated requests" pattern).

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

/// How long to wait for the `initialize`/`thread/start (or resume)`/
/// `turn/start` request-response round trips before giving up. Generous
/// because `thread/start` can shell out to load MCP servers.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How often the turn-streaming loop wakes up to check `cancel`, since a
/// blocking read can't be interrupted while the model is thinking.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Default)]
pub struct ChatOptions {
    /// Resume this thread (`thread/resume`); None starts a fresh one
    /// (`thread/start`).
    pub session: Option<String>,
    /// Sent as `developerInstructions` on thread start/resume (additive
    /// developer-level context), mirroring claude's
    /// `--append-system-prompt`.
    pub system_prompt: Option<String>,
    /// Working directory for the thread. Advisory only: unlike claude's
    /// `explore_dir`, Codex's read-only sandbox is not confined to this
    /// directory (accepted gap, see docs/tasks/codex-chat.md).
    pub explore_dir: Option<PathBuf>,
}

/// Same event shape as `claude::chat` (`TextDelta`/`Completed`/`Failed`), so
/// the app's chat rendering code doesn't need to know which backend ran.
/// Codex's app-server protocol doesn't report a per-turn cost, so `Completed`
/// is always built with `cost_usd: None`.
pub use claude::ChatEvent;

/// One parsed server notification, decoupled from `ChatEvent` so mapping is
/// a pure `&str -> Option<Parsed>` function tests can exercise directly.
#[derive(Debug, Clone, PartialEq)]
enum Parsed {
    TextDelta(String),
    TurnCompleted { is_error: bool, text: String },
    Failed(String),
}

/// Parse one server notification's `method` and `params` into an event, or
/// None for everything we don't act on (hooks, token usage, rate limits,
/// item lifecycle notifications other than the agent message delta, a
/// retrying `error`, etc).
fn parse_notification(method: &str, params: &Value) -> Option<Parsed> {
    match method {
        "item/agentMessage/delta" => {
            Some(Parsed::TextDelta(params.get("delta")?.as_str()?.to_string()))
        }
        "turn/completed" => {
            let turn = params.get("turn")?;
            let is_error = turn.get("status")?.as_str()? == "failed";
            let text = turn
                .get("items")?
                .as_array()?
                .iter()
                .rev()
                .find_map(|item| {
                    (item.get("type")?.as_str()? == "agentMessage")
                        .then(|| item.get("text")?.as_str().map(str::to_string))
                        .flatten()
                })
                .unwrap_or_default();
            Some(Parsed::TurnCompleted { is_error, text })
        }
        "error" => {
            if params.get("willRetry").and_then(Value::as_bool).unwrap_or(false) {
                return None;
            }
            Some(Parsed::Failed(
                params.get("error")?.get("message")?.as_str()?.to_string(),
            ))
        }
        _ => None,
    }
}

fn initialize_params() -> Value {
    json!({
        "clientInfo": { "name": "lgtm", "title": null, "version": env!("CARGO_PKG_VERSION") },
        "capabilities": null,
    })
}

/// Fields shared by `thread/start` and `thread/resume`: sandbox and
/// approval policy are pinned every time (rather than only on first start)
/// so a resumed thread can't silently inherit a non-read-only setting from
/// `config.toml`.
fn thread_config_fields(opts: &ChatOptions) -> Value {
    json!({
        "cwd": opts.explore_dir.as_ref().map(|dir| dir.display().to_string()),
        "sandbox": "read-only",
        "approvalPolicy": "never",
        "developerInstructions": opts.system_prompt,
    })
}

fn thread_start_params(opts: &ChatOptions) -> Value {
    thread_config_fields(opts)
}

fn thread_resume_params(session: &str, opts: &ChatOptions) -> Value {
    let mut params = thread_config_fields(opts);
    params["threadId"] = json!(session);
    params
}

fn turn_start_params(thread_id: &str, prompt: &str) -> Value {
    json!({
        "threadId": thread_id,
        "input": [{ "type": "text", "text": prompt, "text_elements": [] }],
    })
}

fn turn_interrupt_params(thread_id: &str, turn_id: &str) -> Value {
    json!({ "threadId": thread_id, "turnId": turn_id })
}

/// Build the JSON-RPC response to a server-initiated request (`id` and
/// `method` both present) that declines it: `denied`/`decline` for the
/// approval request types the schema defines a decision enum for, a
/// JSON-RPC error for everything else (permission-profile requests, tool
/// calls, elicitations, ...), since those have no decline-shaped response
/// and must not be answered with a guessed payload.
fn deny_response(id: Value, method: &str) -> Value {
    let result = match method {
        "execCommandApproval" | "applyPatchApproval" => Some(json!({
            "decision": { "denied": { "rejection": "lgtm runs Codex read-only and declines approval requests" } }
        })),
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            Some(json!({ "decision": "decline" }))
        }
        _ => None,
    };
    match result {
        Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        None => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32000, "message": format!("lgtm declines unsupported codex request: {method}") },
        }),
    }
}

fn send_request(stdin: &mut ChildStdin, id: u64, method: &str, params: Value) -> Result<()> {
    write_line(stdin, &json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
}

fn send_notification(stdin: &mut ChildStdin, method: &str, params: Value) -> Result<()> {
    write_line(stdin, &json!({ "jsonrpc": "2.0", "method": method, "params": params }))
}

fn write_line(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    writeln!(stdin, "{value}")?;
    stdin.flush()?;
    Ok(())
}

/// Block until a response with the given `id` arrives, ignoring any
/// notifications or other in-flight responses seen along the way. Used only
/// during the handshake, which is not cancellable (it's expected to be
/// fast; the long-running, cancellable phase is the turn itself).
fn wait_for_response(rx: &mpsc::Receiver<String>, id: u64, timeout: Duration) -> Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            bail!("codex app-server timed out waiting for response id={id}");
        }
        match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if value.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(err) = value.get("error") {
                    bail!("codex app-server request id={id} failed: {err}");
                }
                return Ok(value.get("result").cloned().unwrap_or(Value::Null));
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                bail!("codex app-server exited before responding to id={id}")
            }
        }
    }
}

/// Stream a single turn's notifications, emitting `ChatEvent`s as they
/// arrive. Returns `Ok(true)` once a terminal event (`Completed`/`Failed`)
/// has been emitted, `Ok(false)` if `cancel` fired first (interrupt sent,
/// no terminal event, matching claude's cancel contract).
fn stream_turn(
    rx: &mpsc::Receiver<String>,
    stdin: &mut ChildStdin,
    next_id: &mut u64,
    thread_id: &str,
    turn_id: &str,
    cancel: &AtomicBool,
    on_event: &mut impl FnMut(ChatEvent),
) -> Result<bool> {
    loop {
        if cancel.load(Ordering::Relaxed) {
            let id = *next_id;
            *next_id += 1;
            let _ = send_request(
                stdin,
                id,
                "turn/interrupt",
                turn_interrupt_params(thread_id, turn_id),
            );
            return Ok(false);
        }
        match rx.recv_timeout(CANCEL_POLL_INTERVAL) {
            Ok(line) => {
                let Ok(value) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let Some(method) = value.get("method").and_then(Value::as_str) else {
                    continue;
                };
                if let Some(id) = value.get("id").cloned() {
                    // Server-initiated request (an approval prompt, despite
                    // approvalPolicy "never", or a tool/elicitation request
                    // we don't support). Always reply so the turn can't
                    // stall waiting for a response we'd never send.
                    let _ = write_line(stdin, &deny_response(id, method));
                    continue;
                }
                let params = value.get("params").unwrap_or(&Value::Null);
                match parse_notification(method, params) {
                    Some(Parsed::TextDelta(text)) => on_event(ChatEvent::TextDelta(text)),
                    Some(Parsed::TurnCompleted { is_error, text }) => {
                        on_event(ChatEvent::Completed {
                            session_id: thread_id.to_string(),
                            cost_usd: None,
                            is_error,
                            text,
                        });
                        return Ok(true);
                    }
                    Some(Parsed::Failed(message)) => {
                        on_event(ChatEvent::Failed(message));
                        return Ok(true);
                    }
                    None => {}
                }
            }
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                bail!("codex app-server exited before the turn completed")
            }
        }
    }
}

/// Last `max` bytes of `text` (on a char boundary), for error surfacing.
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Drive the handshake and one turn to completion or cancellation. Kept
/// separate from [`chat`] so every error path (bad handshake, dead
/// process) funnels through one `?`-propagating function, with cleanup and
/// `Failed` conversion handled once by the caller.
fn drive(
    stdin: &mut ChildStdin,
    rx: &mpsc::Receiver<String>,
    prompt: &str,
    opts: &ChatOptions,
    cancel: &AtomicBool,
    on_event: &mut impl FnMut(ChatEvent),
) -> Result<bool> {
    let mut next_id: u64 = 1;

    let mut request = |stdin: &mut ChildStdin, method: &str, params: Value| -> Result<Value> {
        let id = next_id;
        next_id += 1;
        send_request(stdin, id, method, params)?;
        wait_for_response(rx, id, HANDSHAKE_TIMEOUT)
    };

    request(stdin, "initialize", initialize_params())?;
    send_notification(stdin, "initialized", json!({}))?;

    let thread_result = match &opts.session {
        Some(session) => request(stdin, "thread/resume", thread_resume_params(session, opts))?,
        None => request(stdin, "thread/start", thread_start_params(opts))?,
    };
    let thread_id = thread_result
        .get("thread")
        .and_then(|thread| thread.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("codex thread/start response missing thread.id"))?
        .to_string();

    let turn_result = request(stdin, "turn/start", turn_start_params(&thread_id, prompt))?;
    let turn_id = turn_result
        .get("turn")
        .and_then(|turn| turn.get("id"))
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("codex turn/start response missing turn.id"))?
        .to_string();

    stream_turn(rx, stdin, &mut next_id, &thread_id, &turn_id, cancel, on_event)
}

/// Run one prompt against `codex app-server`, streaming events to
/// `on_event`.
///
/// Blocking: call from a background thread/executor. Spawns one
/// `codex app-server` process per call and kills it before returning.
/// Every run ends with a terminal event -- `Completed` or `Failed` --
/// except when `cancel` is set, which sends `turn/interrupt`, kills the
/// child, and returns without a terminal event. `Err` is reserved for not
/// being able to spawn `codex` at all.
pub fn chat(
    prompt: &str,
    opts: &ChatOptions,
    cancel: &AtomicBool,
    mut on_event: impl FnMut(ChatEvent),
) -> Result<()> {
    let mut child: Child = Command::new("codex")
        .arg("app-server")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| anyhow!("failed to run codex (is Codex CLI installed?): {err}"))?;

    let mut stderr = child.stderr.take().expect("stderr was piped");
    let stderr_thread = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    let stdout = child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel::<String>();
    let reader_thread = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut stdin = child.stdin.take().expect("stdin was piped");
    // Guards against a panicking `on_event` leaking the child process: if
    // `drive` unwinds, this still runs (armed by default) and kills it.
    // Disarmed on a normal return since the explicit kill below covers that
    // path.
    struct KillOnDrop<'a> {
        child: &'a mut Child,
        armed: bool,
    }
    impl Drop for KillOnDrop<'_> {
        fn drop(&mut self) {
            if self.armed {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }
    let mut guard = KillOnDrop { child: &mut child, armed: true };
    let outcome = drive(&mut stdin, &rx, prompt, opts, cancel, &mut on_event);
    guard.armed = false;
    drop(guard);
    drop(stdin);

    let _ = child.kill();
    let _ = child.wait();
    let _ = reader_thread.join();
    let stderr_text = stderr_thread.join().unwrap_or_default();

    if let Err(err) = outcome {
        if !cancel.load(Ordering::Relaxed) {
            let detail = tail(stderr_text.trim(), 2000);
            on_event(ChatEvent::Failed(if detail.is_empty() {
                err.to_string()
            } else {
                format!("{err}: {detail}")
            }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_deltas() {
        let line = r#"{"method":"item/agentMessage/delta","params":{"threadId":"t","turnId":"u","itemId":"m","delta":"Hello"}}"#;
        let value: Value = serde_json::from_str(line).unwrap();
        let method = value["method"].as_str().unwrap();
        assert_eq!(
            parse_notification(method, &value["params"]),
            Some(Parsed::TextDelta("Hello".into()))
        );
    }

    #[test]
    fn parses_turn_completed_from_final_agent_message() {
        let line = r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","status":"completed","items":[{"type":"userMessage","id":"m0","content":[]},{"type":"agentMessage","id":"m1","text":"final text"}]}}}"#;
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            parse_notification(value["method"].as_str().unwrap(), &value["params"]),
            Some(Parsed::TurnCompleted {
                is_error: false,
                text: "final text".into(),
            })
        );
    }

    #[test]
    fn parses_failed_turn() {
        let line = r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":"u","status":"failed","items":[]}}}"#;
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            parse_notification(value["method"].as_str().unwrap(), &value["params"]),
            Some(Parsed::TurnCompleted {
                is_error: true,
                text: String::new(),
            })
        );
    }

    #[test]
    fn non_retrying_error_fails_but_retrying_error_is_ignored() {
        let fatal = r#"{"method":"error","params":{"error":{"message":"boom"},"willRetry":false,"threadId":"t","turnId":"u"}}"#;
        let value: Value = serde_json::from_str(fatal).unwrap();
        assert_eq!(
            parse_notification(value["method"].as_str().unwrap(), &value["params"]),
            Some(Parsed::Failed("boom".into()))
        );

        let retrying = r#"{"method":"error","params":{"error":{"message":"transient"},"willRetry":true,"threadId":"t","turnId":"u"}}"#;
        let value: Value = serde_json::from_str(retrying).unwrap();
        assert_eq!(
            parse_notification(value["method"].as_str().unwrap(), &value["params"]),
            None
        );
    }

    #[test]
    fn skips_unrelated_notifications() {
        let line = r#"{"method":"thread/tokenUsage/updated","params":{}}"#;
        let value: Value = serde_json::from_str(line).unwrap();
        assert_eq!(
            parse_notification(value["method"].as_str().unwrap(), &value["params"]),
            None
        );
    }

    #[test]
    fn deny_response_declines_known_approval_requests_and_errors_on_the_rest() {
        assert_eq!(
            deny_response(json!(7), "execCommandApproval")["result"]["decision"]["denied"]
                .is_object(),
            true
        );
        assert_eq!(
            deny_response(json!(8), "applyPatchApproval")["result"]["decision"]["denied"]
                .is_object(),
            true
        );
        assert_eq!(
            deny_response(json!(9), "item/commandExecution/requestApproval")["result"]["decision"],
            json!("decline")
        );
        assert_eq!(
            deny_response(json!(10), "item/fileChange/requestApproval")["result"]["decision"],
            json!("decline")
        );
        let unsupported = deny_response(json!(11), "item/permissions/requestApproval");
        assert!(unsupported.get("result").is_none());
        assert_eq!(unsupported["error"]["code"], json!(-32000));
        assert_eq!(unsupported["id"], json!(11));
    }
}
